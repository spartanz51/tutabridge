//! In-app updates. GitHub Releases hosts a signed `latest.json`; the updater
//! plugin refuses any download whose signature does not match the public key
//! built into the app (`plugins.updater.pubkey` in `tauri.conf.json`).
//!
//! The update path does not go through Tuta, so an app that Tuta refuses for
//! being too old can still update itself.
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;
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

fn auto_update_enabled() -> bool {
    match config::load_config() {
        Ok(Some(cfg)) => cfg.auto_update,
        _ => true,
    }
}

/// Asks GitHub whether a newer version exists. Downloads nothing.
pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
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
    let Some(update) = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let version = update.version.clone();
    log::info!("Downloading TutaBridge {version}");
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
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
    tokio::time::sleep(FIRST_CHECK_DELAY).await;
    loop {
        if auto_update_enabled() {
            match install(&app).await {
                Ok(Some(_)) => return,
                Ok(None) => log::debug!("TutaBridge is up to date"),
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
