use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;
use tutabridge_core::bridge::{BridgeHandle, BridgeStats, BridgeStatus};
use tutabridge_core::config::{self, Config};
use tutabridge_core::tuta;

pub type BridgeState = Arc<Mutex<BridgeHandle>>;

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
