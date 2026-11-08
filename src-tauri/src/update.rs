//! In-app updates. GitHub Releases hosts a signed `latest.json`; the updater
//! plugin refuses any download whose signature does not match the public key
//! built into the app (`plugins.updater.pubkey` in `tauri.conf.json`).
//!
//! The update path does not go through Tuta, so an app that Tuta refuses for
//! being too old can still update itself.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Error, UpdaterExt};
use tutabridge_core::config;

/// How often a running app looks for a new version.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Lets the bridge start before the first check.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);

/// A version newer than the running one.
#[derive(Clone, serde::Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
}

/// A new version is installed, pushed to the UI as `bridge://update-ready`.
#[derive(Clone, serde::Serialize)]
struct UpdateReady {
    version: String,
}

/// A new version exists but installing it is left to the user, pushed to
/// the UI as `bridge://update-available`.
#[derive(Clone, serde::Serialize)]
struct UpdateAvailable {
    version: String,
}

fn auto_update_enabled() -> bool {
    let loaded = config::load_config();
    if loaded.is_err() {
        log::warn!("Automatic updates disabled: configuration could not be read");
    }
    enabled_by_config(loaded)
}

fn enabled_by_config(result: Result<Option<config::Config>, Box<dyn std::error::Error>>) -> bool {
    match result {
        Ok(Some(cfg)) => cfg.auto_update,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Whether this install can replace itself. The macOS and Windows bundles
/// can; on Linux only the AppImage can, a `.deb` or `.rpm` belongs to the
/// package manager and the release only publishes the AppImage for updates.
fn self_updating() -> bool {
    !cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some()
}

const PACKAGE_MANAGED: &str = "This install is updated by its package manager, not by the app";

/// Replacing the app needs write access to where it lives. Without it the
/// plugin asks for an administrator password, which a background task must
/// never do: the user gets a banner and decides. Windows also waits for a
/// click because its installer closes the running bridge.
fn bundle_writable() -> bool {
    if cfg!(target_os = "windows") {
        // NSIS closes the running bridge. Leave that interruption to the user.
        return false;
    }
    let location = if cfg!(target_os = "macos") {
        // .../TutaBridge.app/Contents/MacOS/tutabridge-gui: the folder holding the bundle.
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.ancestors().nth(4).map(Path::to_path_buf))
    } else {
        std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .and_then(|image| image.parent().map(Path::to_path_buf))
    };
    let Some(dir) = location else {
        return false;
    };
    directory_writable(&dir)
}

fn directory_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".tutabridge-update-probe-")
        .tempfile_in(dir)
        .is_ok()
}

/// One download at a time: the background loop and the settings button
/// can both ask for an install.
static INSTALLING: AtomicBool = AtomicBool::new(false);
static INSTALLED: OnceLock<String> = OnceLock::new();

struct InstallGuard<'a>(&'a AtomicBool);

impl<'a> InstallGuard<'a> {
    fn acquire(lock: &'a AtomicBool) -> Result<Self, String> {
        lock.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| Self(lock))
            .map_err(|_| "An update is already being installed".to_owned())
    }
}

impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The plugin reports a missing `latest.json` with the same words whether
/// the release has none (published before the updater existed) or GitHub
/// answered something else; both are worth saying.
fn describe(error: Error) -> String {
    match error {
        Error::ReleaseNotFound => {
            "GitHub returned no update information: the latest release has none, or the \
             answer was not the expected file"
                .to_owned()
        }
        Error::Network(reason) => format!("Could not reach GitHub: {reason}"),
        Error::Reqwest(e) => format!("Could not reach GitHub: {e}"),
        other => other.to_string(),
    }
}

/// Asks GitHub whether a newer version exists. Downloads nothing.
pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    if INSTALLED.get().is_some() {
        return Ok(None);
    }
    if !self_updating() {
        return Err(PACKAGE_MANAGED.to_owned());
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(describe)?;
    Ok(update.map(|update| UpdateInfo {
        version: display_version(&update.version),
        current: tutabridge_core::VERSION.to_owned(),
        notes: update.body.clone(),
    }))
}

/// Downloads the newer version, verifies its signature and installs it. On
/// macOS and Linux the new version runs at the next launch; on Windows the
/// installer closes the app and relaunches it. Returns the installed version,
/// or `None` when the app is up to date.
pub async fn install(app: &AppHandle) -> Result<Option<String>, String> {
    install_with_policy(app, false).await
}

async fn install_with_policy(app: &AppHandle, automatic: bool) -> Result<Option<String>, String> {
    if !self_updating() {
        return Err(PACKAGE_MANAGED.to_owned());
    }
    let _guard = InstallGuard::acquire(&INSTALLING)?;
    if let Some(version) = INSTALLED.get() {
        return Ok(Some(version.clone()));
    }
    if automatic && !auto_update_enabled() {
        return Ok(None);
    }
    let Some(update) = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(describe)?
    else {
        return Ok(None);
    };
    if automatic && !auto_update_enabled() {
        return Ok(None);
    }
    let version = display_version(&update.version);
    log::info!("Downloading TutaBridge {version}");
    let bytes = update.download(|_, _| {}, || {}).await.map_err(describe)?;
    // The setting may change while a large download is in flight.
    if automatic && (!auto_update_enabled() || !bundle_writable()) {
        return Ok(None);
    }
    update.install(bytes).map_err(describe)?;
    let _ = INSTALLED.set(version.clone());
    crate::tray::update_ready(app);
    log::info!("TutaBridge {version} is installed; it runs at the next launch");
    let _ = app.emit(
        "bridge://update-ready",
        UpdateReady {
            version: version.clone(),
        },
    );
    Ok(Some(version))
}

/// Background updates, as long as the setting allows them. The setting is
/// read again before every check, so turning it off needs no restart. Ends
/// after one install: the new version takes over at the next launch.
pub async fn run(app: AppHandle) {
    if !self_updating() {
        log::info!("{PACKAGE_MANAGED}");
        return;
    }
    tokio::time::sleep(FIRST_CHECK_DELAY).await;
    loop {
        if INSTALLED.get().is_some() {
            return;
        }
        if auto_update_enabled() {
            match check(&app).await {
                Ok(None) => log::debug!("TutaBridge is up to date"),
                Ok(Some(_)) if !auto_update_enabled() => {}
                Ok(Some(_)) if bundle_writable() => match install_with_policy(&app, true).await {
                    Ok(Some(_)) => return,
                    Ok(None) => {}
                    Err(e) => log::warn!("Update failed: {e}"),
                },
                Ok(Some(found)) => {
                    log::info!(
                        "TutaBridge {} is available; the app waits for the user to install it",
                        found.version
                    );
                    let _ = app.emit(
                        "bridge://update-available",
                        UpdateAvailable {
                            version: found.version,
                        },
                    );
                }
                Err(e) => log::warn!("Update check failed: {e}"),
            }
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    check(&app).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<Option<String>, String> {
    install(&app).await
}

#[tauri::command]
pub fn pending_update() -> Option<String> {
    INSTALLED.get().cloned()
}

#[tauri::command]
pub fn get_app_version() -> String {
    tutabridge_core::VERSION.to_owned()
}

/// The tag form of a bundle version. The release job writes `X.Y.Z-N` into
/// the bundles and the update manifest (`scripts/release-assets.py`); the
/// release it comes from is tagged `vX.Y.Z-rc.N`, and that is the form
/// shown everywhere else. Any other version is shown as it is.
fn display_version(bundle_version: &str) -> String {
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match bundle_version.split_once('-') {
        Some((release, build))
            if numeric(build)
                && release.split('.').count() == 3
                && release.split('.').all(numeric) =>
        {
            format!("{release}-rc.{build}")
        }
        _ => bundle_version.to_owned(),
    }
}

#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_versions_are_shown_as_their_tag() {
        assert_eq!(display_version("0.1.0-14"), "0.1.0-rc.14");
        assert_eq!(display_version("1.2.3-0"), "1.2.3-rc.0");
        assert_eq!(display_version("0.1.0"), "0.1.0");
        assert_eq!(display_version("0.1.0-rc.14"), "0.1.0-rc.14");
        assert_eq!(display_version("0.1.0-dev"), "0.1.0-dev");
        assert_eq!(display_version("0.1-14"), "0.1-14");
    }

    #[test]
    fn unreadable_config_disables_automatic_updates() {
        assert!(enabled_by_config(Ok(None)));
        assert!(enabled_by_config(Ok(Some(config::Config::default()))));
        let cfg = config::Config {
            auto_update: false,
            ..Default::default()
        };
        assert!(!enabled_by_config(Ok(Some(cfg))));
        assert!(!enabled_by_config(Err(std::io::Error::other(
            "invalid config"
        )
        .into())));
    }

    #[test]
    fn writable_probe_preserves_existing_files_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join(".tutabridge-update-probe");
        std::fs::write(&existing, b"keep me").unwrap();
        assert!(directory_writable(dir.path()));
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!directory_writable(&existing));
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
    }

    #[test]
    fn concurrent_installs_are_rejected_and_cancellation_releases_lock() {
        let lock = AtomicBool::new(false);
        let guard = InstallGuard::acquire(&lock).unwrap();
        assert!(InstallGuard::acquire(&lock).is_err());
        drop(guard);
        assert!(InstallGuard::acquire(&lock).is_ok());
        assert!(!lock.load(Ordering::SeqCst));
    }
}
