use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;
use tutabridge_core::backup;
use tutabridge_core::bridge::{BridgeHandle, BridgeStats, BridgeStatus};
use tutabridge_core::config::{self, Config};
use tutabridge_core::tuta;

pub type BridgeState = Arc<Mutex<BridgeHandle>>;

/// Progress event pushed to the UI during a backup (`bridge://backup-progress`).
#[derive(Clone, serde::Serialize)]
struct BackupProgressEvent {
    folder: String,
    done: usize,
    total: usize,
    finished: bool,
}

#[tauri::command]
pub async fn get_config() -> Result<Config, String> {
    match config::load_config() {
        Ok(Some(cfg)) => Ok(cfg),
        Ok(None) => Ok(Config::default()),
        Err(e) => Err(format!("Failed to load config: {e}")),
    }
}

#[tauri::command]
pub async fn save_config(config: Config) -> Result<(), String> {
    config::save_config(&config).map_err(|e| format!("Failed to save config: {e}"))
}

#[tauri::command]
pub async fn has_saved_session() -> Result<bool, String> {
    let cfg = match config::load_config() {
        Ok(Some(cfg)) if !cfg.email.is_empty() => cfg,
        _ => return Ok(false),
    };
    Ok(tuta::has_saved_session(&cfg.email))
}

#[tauri::command]
pub async fn start_bridge(
    password: Option<String>,
    state: State<'_, BridgeState>,
) -> Result<(), String> {
    let mut cfg = match config::load_config() {
        Ok(Some(cfg)) if !cfg.email.is_empty() => cfg,
        _ => return Err("No config found — save config first".into()),
    };

    config::ensure_bridge_password(&mut cfg).map_err(|e| format!("Bridge password setup failed: {e}"))?;

    let mut handle = state.lock().await;
    handle.start(cfg, password, None).await
}

#[tauri::command]
pub async fn stop_bridge(state: State<'_, BridgeState>) -> Result<(), String> {
    let mut handle = state.lock().await;
    handle.stop().await;
    Ok(())
}

#[tauri::command]
pub async fn get_status(state: State<'_, BridgeState>) -> Result<BridgeStatus, String> {
    let handle = state.lock().await;
    Ok(handle.status().await)
}

#[tauri::command]
pub async fn get_stats(state: State<'_, BridgeState>) -> Result<BridgeStats, String> {
    let handle = state.lock().await;
    Ok(handle.stats().await)
}

#[tauri::command]
pub async fn get_bridge_password() -> Result<Option<String>, String> {
    let cfg = config::load_config().map_err(|e| e.to_string())?;
    Ok(cfg.and_then(|c| c.bridge_password))
}

#[tauri::command]
pub async fn regenerate_bridge_password() -> Result<String, String> {
    let mut cfg = config::load_config()
        .map_err(|e| e.to_string())?
        .ok_or("No config found")?;
    config::regenerate_bridge_password(&mut cfg).map_err(|e| e.to_string())
}

/// Export every mail to `output_dir` as a tree of `.eml` files. Requires the
/// bridge to be running (reuses its live session + cache). Streams progress
/// via `bridge://backup-progress` events and resolves with the final stats.
#[tauri::command]
pub async fn export_mails(
    output_dir: String,
    app: AppHandle,
    state: State<'_, BridgeState>,
) -> Result<backup::BackupStats, String> {
    // Grab the live backend + cache, then drop the lock immediately — a
    // backup can run for minutes and must not block status/stats reads.
    let (backend, local_store) = {
        let handle = state.lock().await;
        handle
            .backend_and_store()
            .ok_or("Start the bridge before backing up")?
    };

    let out = std::path::Path::new(&output_dir);
    let stats = backup::export_eml(&*backend, &local_store, out, |p| {
        // Throttle: emit every 20 mails plus the last one of each folder.
        if p.done == p.total || p.done % 20 == 0 {
            let _ = app.emit(
                "bridge://backup-progress",
                BackupProgressEvent {
                    folder: p.folder.clone(),
                    done: p.done,
                    total: p.total,
                    finished: false,
                },
            );
        }
    })
    .await?;

    let _ = app.emit(
        "bridge://backup-progress",
        BackupProgressEvent {
            folder: String::new(),
            done: 0,
            total: 0,
            finished: true,
        },
    );
    Ok(stats)
}
