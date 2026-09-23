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
    config.validate_ports()?;
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

/// Holds the sender side of the channel the 2FA callback blocks on. `submit_totp`
/// pushes the code here while a login is waiting for it. Kept separate from
/// `BridgeState` so submitting the code never contends with the start lock.
pub struct TotpState(pub std::sync::Mutex<Option<std::sync::mpsc::Sender<u32>>>);

#[tauri::command]
pub async fn start_bridge(
    email: Option<String>,
    password: Option<String>,
    app: AppHandle,
    totp_state: State<'_, TotpState>,
    state: State<'_, BridgeState>,
) -> Result<(), String> {
    let mut cfg = match config::load_config() {
        Ok(Some(cfg)) if !cfg.email.is_empty() => cfg,
        // First run: no account yet. Bootstrap a default config from the email
        // the user just entered on the dashboard, instead of erroring out.
        _ => {
            let email = email.unwrap_or_default().trim().to_string();
            if email.is_empty() {
                return Err("Enter your Tuta email to get started".into());
            }
            let cfg = Config {
                email,
                ..Default::default()
            };
            config::save_config(&cfg).map_err(|e| format!("Failed to save config: {e}"))?;
            cfg
        }
    };

    config::ensure_bridge_password(&mut cfg)
        .map_err(|e| format!("Bridge password setup failed: {e}"))?;

    // Interactive 2FA, like the CLI: a single login. The callback fires only if
    // the account actually needs a code; it tells the UI to show the field
    // (`bridge://need-totp`) and blocks until `submit_totp` delivers the code.
    // One `initiate_session` either way, so it never trips Tuta's auth rate limit.
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    *totp_state.0.lock().unwrap() = Some(tx);
    let rx = std::sync::Mutex::new(rx);
    let app_for_cb = app.clone();
    let totp_cb = tuta::TwoFactorCallback::Totp(Box::new(move || {
        let _ = app_for_cb.emit("bridge://need-totp", ());
        rx.lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(120))
            .map_err(|_| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "Two-factor code was not entered in time",
                )
            })
    }));

    let result = {
        let mut handle = state.lock().await;
        handle.start(cfg, password, Some(totp_cb)).await
    };
    *totp_state.0.lock().unwrap() = None;
    result
}

/// Deliver the 2FA code to a login currently waiting on it (see `start_bridge`).
#[tauri::command]
pub async fn submit_totp(code: String, totp_state: State<'_, TotpState>) -> Result<(), String> {
    let parsed: u32 = code
        .trim()
        .parse()
        .map_err(|_| "Two-factor code must be digits".to_string())?;
    let tx = totp_state.0.lock().unwrap().clone();
    match tx {
        Some(tx) => tx
            .send(parsed)
            .map_err(|_| "No sign-in is waiting for a code".to_string()),
        None => Err("No sign-in is waiting for a code".to_string()),
    }
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

/// Build the MCP client config snippet (URL + bearer token) to paste into
/// Claude Desktop / Code. Read-only; reflects the saved port + bridge password.
#[tauri::command]
pub async fn get_mcp_client_config() -> Result<String, String> {
    let cfg = config::load_config()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let token = cfg.bridge_password.unwrap_or_default();
    let snippet = serde_json::json!({
        "mcpServers": {
            "tutabridge": {
                "type": "http",
                "url": format!("http://127.0.0.1:{}/mcp", cfg.mcp_port),
                "headers": { "Authorization": format!("Bearer {token}") }
            }
        }
    });
    serde_json::to_string_pretty(&snippet).map_err(|e| e.to_string())
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
