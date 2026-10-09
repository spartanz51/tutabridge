#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod tray;

use commands::BridgeState;
use std::sync::Arc;
use tauri::{Manager, WebviewWindowBuilder};
use tokio::sync::Mutex;
use tutabridge_core::bridge::BridgeHandle;

fn main() {
    // Keep the bridge's own logs at debug but silence the HTTP/2 + TLS transport
    // firehose (h2/hyper/rustls/mio), which otherwise buries every useful line.
    // RUST_LOG still overrides this when set.
    const LOG_FILTER: &str = "info,tutabridge_core=debug,tutabridge_gui=debug,\
        h2=warn,hyper=warn,hyper_util=warn,rustls=warn,mio=warn,tokio_util=warn,want=warn";
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(LOG_FILTER)).init();

    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install TLS crypto provider");

    let handle = BridgeHandle::new();
    let log_rx = handle.subscribe_logs();
    let stats_rx = handle.subscribe_stats();
    let shared = Arc::new(Mutex::new(handle));

    tauri::Builder::default()
        // Reopening the launcher restores the background instance instead
        // of starting another bridge on the same IMAP/SMTP ports.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            tray::show_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(shared as BridgeState)
        .manage(commands::TotpState(std::sync::Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::has_saved_session,
            commands::start_bridge,
            commands::submit_totp,
            commands::stop_bridge,
            commands::get_status,
            commands::get_stats,
            commands::get_bridge_password,
            commands::regenerate_bridge_password,
            commands::export_mails,
            commands::get_mcp_client_config,
        ])
        .setup(|app| {
            create_main_window(app)?;
            if let Err(error) = tray::setup(app) {
                log::warn!("Tray unavailable; closing the window will exit: {error}");
            }

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
        .on_window_event(tray::on_window_event)
        .build(tauri::generate_context!())
        .expect("error building TutaBridge")
        .run(|_app, _event| {
            // The Dock can reopen an already-running macOS app too.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                tray::show_main_window(_app);
            }
        });
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

    // A 1s tick alongside the dirty pulses. Pulses give instant updates on
    // state changes (ws transitions, new mail); the tick keeps the time-based
    // uptime climbing and self-heals any pulse the UI missed during startup
    // (e.g. while the lock was held through an interactive 2FA wait).
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    emit_snapshot(&app, &state).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => emit_snapshot(&app, &state).await,
        }
    }
}

async fn stream_logs(app: tauri::AppHandle, mut rx: tokio::sync::broadcast::Receiver<String>) {
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

/// Opens the window at the same share of the screen everywhere: the
/// configured 700 × 520 is right on a laptop and a thumbnail on a 4K
/// monitor. Set the final size before creation: GTK resizes asynchronously,
/// so centering immediately after set_size() would use stale dimensions.
fn create_main_window(app: &tauri::App) -> tauri::Result<()> {
    let mut config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .expect("main window configuration is missing")
        .clone();
    // Wayland may have no primary monitor, and a hidden window cannot tell
    // us which monitor it is on yet. Enumerate outputs as a fallback.
    let monitor = match app.primary_monitor()? {
        Some(monitor) => Some(monitor),
        None => app.available_monitors()?.into_iter().next(),
    };
    if let Some(monitor) = monitor {
        let screen = monitor.size().to_logical::<f64>(monitor.scale_factor());
        config.width = (screen.width * 0.35).max(config.min_width.unwrap_or(0.0));
        config.height = (screen.height * 0.45).max(config.min_height.unwrap_or(0.0));
        // Tell the builder which monitor to center on, including when there
        // is no primary. Wayland leaves placement to the compositor.
        let origin = monitor.position().to_logical::<f64>(monitor.scale_factor());
        config.x = Some(origin.x);
        config.y = Some(origin.y);
    }
    WebviewWindowBuilder::from_config(app, &config)?.build()?;
    Ok(())
}
