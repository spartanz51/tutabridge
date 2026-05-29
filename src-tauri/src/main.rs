#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use std::sync::Arc;
use commands::BridgeState;
use tauri::Manager;
use tokio::sync::Mutex;
use tutabridge_core::bridge::BridgeHandle;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .init();

    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install TLS crypto provider");

    let handle = BridgeHandle::new();
    let log_rx = handle.subscribe_logs();
    let stats_rx = handle.subscribe_stats();
    let shared = Arc::new(Mutex::new(handle));

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(shared as BridgeState)
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::has_saved_session,
            commands::start_bridge,
            commands::stop_bridge,
            commands::get_status,
            commands::get_stats,
            commands::get_bridge_password,
            commands::regenerate_bridge_password,
            commands::export_mails,
        ])
        .setup(|app| {
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                stream_logs(app_handle, log_rx).await;
            });

            let app_handle = app.handle().clone();
            let stats_state = app.state::<BridgeState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                stream_stats(app_handle, stats_rx, stats_state).await;
            });

            let state = app.state::<BridgeState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                auto_start(state).await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running TutaBridge");
}

async fn auto_start(state: Arc<Mutex<BridgeHandle>>) {
    use tutabridge_core::config;
    use tutabridge_core::tuta;

    let mut cfg = match config::load_config() {
        Ok(Some(cfg)) if !cfg.email.is_empty() => cfg,
        _ => return,
    };

    // Ensure bridge password exists (so the UI can display it)
    if let Err(e) = config::ensure_bridge_password(&mut cfg) {
        log::warn!("Bridge password setup failed: {e}");
    }

    if !tuta::has_saved_session(&cfg.email) {
        return;
    }

    let mut handle = state.lock().await;
    if let Err(e) = handle.start(cfg, None, None).await {
        log::warn!("Auto-start failed: {e}");
    }
}

async fn stream_stats(
    app: tauri::AppHandle,
    mut rx: tokio::sync::broadcast::Receiver<()>,
    state: BridgeState,
) {
    use tauri::Emitter;
    // Emit a single snapshot covering both bridge status and stats. Status
    // transitions (start / stop) and stats changes (new mail, ws state) all
    // pulse the same channel, so the UI replaces its periodic poll with one
    // listen per topic.
    async fn emit_snapshot(app: &tauri::AppHandle, state: &BridgeState) {
        let handle = state.lock().await;
        let status = handle.status().await;
        let stats = handle.stats().await;
        drop(handle);
        let _ = app.emit("bridge://stats", &stats);
        let _ = app.emit("bridge://status", &status);
    }

    emit_snapshot(&app, &state).await;
    loop {
        match rx.recv().await {
            Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                emit_snapshot(&app, &state).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn stream_logs(
    app: tauri::AppHandle,
    mut rx: tokio::sync::broadcast::Receiver<String>,
) {
    use tauri::Emitter;
    loop {
        match rx.recv().await {
            Ok(line) => {
                let _ = app.emit("bridge://log", &line);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let _ = app.emit("bridge://log", &format!("... skipped {n} log lines"));
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}
