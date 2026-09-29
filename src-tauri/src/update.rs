//! In-app updates. GitHub Releases hosts a signed `latest.json`; the updater
//! plugin refuses any download whose signature does not match the public key
//! built into the app (`plugins.updater.pubkey` in `tauri.conf.json`).
//!
//! The update path does not go through Tuta, so an app that Tuta refuses for
//! being too old can still update itself.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    match config::load_config() {
        Ok(Some(cfg)) => cfg.auto_update,
        _ => true,
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
/// never do: the user gets a banner and decides. The Windows installer
/// manages its own rights.
fn bundle_writable() -> bool {
    if cfg!(target_os = "windows") {
        return true;
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
    let probe = dir.join(".tutabridge-update-probe");
    let writable = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    writable
}

/// One download at a time: the background loop and the settings button
/// can both ask for an install.
static INSTALLING: AtomicBool = AtomicBool::new(false);

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
        version: update.version.clone(),
        current: update.current_version.clone(),
        notes: update.body.clone(),
    }))
}

/// Downloads the newer version, verifies its signature and installs it. On
/// macOS and Linux the new version runs at the next launch; on Windows the
/// installer closes the app and relaunches it. Returns the installed version,
/// or `None` when the app is up to date.
pub async fn install(app: &AppHandle) -> Result<Option<String>, String> {
    if !self_updating() {
        return Err(PACKAGE_MANAGED.to_owned());
    }
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("An update is already being installed".to_owned());
    }
    let result = install_now(app).await;
    INSTALLING.store(false, Ordering::SeqCst);
    result
}

async fn install_now(app: &AppHandle) -> Result<Option<String>, String> {
    let Some(update) = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(describe)?
    else {
        return Ok(None);
    };
    let version = update.version.clone();
    log::info!("Downloading TutaBridge {version}");
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(describe)?;
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
        if auto_update_enabled() {
            match check(&app).await {
                Ok(None) => log::debug!("TutaBridge is up to date"),
                Ok(Some(_)) if bundle_writable() => match install(&app).await {
                    Ok(Some(_)) => return,
                    Ok(None) => {}
                    Err(e) => log::warn!("Update failed: {e}"),
                },
                Ok(Some(found)) => {
                    log::info!(
                        "TutaBridge {} is available; installing it needs more rights, so the app \
                         waits for the user",
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
pub fn get_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart()
}
