use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, watch, RwLock};

use crate::config::{self, Config};
use crate::event_handler;
use crate::store::LocalStore;
use crate::sync::{self, MailStore};
use crate::tuta::{self, MailBackend, TwoFactorCallback};
use crate::{imap, smtp, tls};

/// Identifier the server uses for telemetry/rate-limit bucketing.
const CLIENT_NAME: &str = "tutabridge";

// Tuta `modelVersions=` for the event-bus URL. Read at compile-time from the
// vendored SDK's type-model JSONs so the values track the submodule bump
// automatically — no more hard-coded constants going stale silently.
const SYS_TYPE_MODELS_JSON: &str =
    include_str!("../../../tuta-repo/tuta-sdk/rust/sdk/src/type_models/sys.json");
const TUTANOTA_TYPE_MODELS_JSON: &str =
    include_str!("../../../tuta-repo/tuta-sdk/rust/sdk/src/type_models/tutanota.json");

fn parse_model_version(json: &str) -> u32 {
    // The SDK guarantees every entry of an app's type-model JSON carries the
    // same `version` field, so reading any one entry is enough.
    let v: serde_json::Value =
        serde_json::from_str(json).expect("type model JSON is malformed (build-time include)");
    v.as_object()
        .and_then(|m| m.values().next())
        .and_then(|first| first.get("version"))
        .and_then(|x| x.as_u64())
        .map(|x| x as u32)
        .expect("type model JSON has no version field")
}

fn sys_model_version() -> u32 {
    static V: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| parse_model_version(SYS_TYPE_MODELS_JSON));
    *V
}

fn tutanota_model_version() -> u32 {
    static V: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| parse_model_version(TUTANOTA_TYPE_MODELS_JSON));
    *V
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum BridgeStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

/// Live state of the event-bus WebSocket. Surfaces directly in the UI so the
/// user can see at a glance whether realtime push is healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum WsStatus {
    Stopped,
    Connecting,
    Connected,
    Reconnecting,
}

impl From<tutasdk::event_bus::WsState> for WsStatus {
    fn from(s: tutasdk::event_bus::WsState) -> Self {
        match s {
            tutasdk::event_bus::WsState::Stopped => Self::Stopped,
            tutasdk::event_bus::WsState::Connecting => Self::Connecting,
            tutasdk::event_bus::WsState::Connected => Self::Connected,
            tutasdk::event_bus::WsState::Reconnecting => Self::Reconnecting,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeStats {
    pub uptime_secs: Option<u64>,
    pub mails_synced: usize,
    pub ws_status: WsStatus,
}

pub struct BridgeHandle {
    status: Arc<RwLock<BridgeStatus>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    log_tx: broadcast::Sender<String>,
    started_at: Option<std::time::Instant>,
    store: Option<Arc<MailStore>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Latest event-bus state, populated at `start` and observed by `stats`.
    ws_state_rx: Option<watch::Receiver<tutasdk::event_bus::WsState>>,
}

impl BridgeHandle {
    pub fn new() -> Self {
        let (log_tx, _) = broadcast::channel(256);
        Self {
            status: Arc::new(RwLock::new(BridgeStatus::Stopped)),
            shutdown_tx: None,
            log_tx,
            started_at: None,
            store: None,
            task: None,
            ws_state_rx: None,
        }
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<String> {
        self.log_tx.subscribe()
    }

    pub fn log_sender(&self) -> broadcast::Sender<String> {
        self.log_tx.clone()
    }

    pub async fn status(&self) -> BridgeStatus {
        self.status.read().await.clone()
    }

    pub async fn stats(&self) -> BridgeStats {
        let count = match &self.store {
            Some(store) => store.total_mail_count().await,
            None => 0,
        };
        let ws_status = self
            .ws_state_rx
            .as_ref()
            .map(|rx| WsStatus::from(*rx.borrow()))
            .unwrap_or(WsStatus::Stopped);
        BridgeStats {
            uptime_secs: self.started_at.map(|t| t.elapsed().as_secs()),
            mails_synced: count,
            ws_status,
        }
    }

    pub async fn start(
        &mut self,
        config: Config,
        password: Option<String>,
        totp_callback: Option<TwoFactorCallback>,
    ) -> Result<(), String> {
        {
            let current = self.status.read().await;
            if *current == BridgeStatus::Running || *current == BridgeStatus::Starting {
                return Err("Bridge is already running".into());
            }
        }

        *self.status.write().await = BridgeStatus::Starting;
        self.emit_log("TutaBridge starting...");

        let tls_acceptor = match tls::load_or_create_tls_acceptor() {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("TLS setup failed: {e}");
                *self.status.write().await = BridgeStatus::Error(msg.clone());
                return Err(msg);
            }
        };
        self.emit_log("TLS initialized");

        self.emit_log(&format!("Authenticating as {}...", config.email));
        let session = match tuta::login_with_2fa(&config, password.as_deref(), totp_callback).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Login failed: {e}");
                *self.status.write().await = BridgeStatus::Error(msg.clone());
                return Err(msg);
            }
        };
        self.emit_log(&format!("Logged in as {}", config.email));

        let storage_key = session.derive_storage_key().await.map_err(|e| {
            let msg = format!("Storage key derivation failed: {e}");
            self.emit_log(&msg);
            msg
        })?;
        self.emit_log("Storage encryption key derived");

        let local_store = LocalStore::open(
            &config::store_db_path(),
            &config::store_mails_dir(),
            storage_key,
        )
        .map_err(|e| {
            let msg = format!("Failed to open local store: {e}");
            self.emit_log(&msg);
            msg
        })?;
        if !local_store.verify_key() {
            self.emit_log("Storage key changed — resetting local cache");
            let _ = local_store.reset();
        }
        let local_store = Arc::new(local_store);
        self.emit_log("Local store opened");

        // Seed the realtime event bus before we move `session` into the
        // backend Arc. We do not pass `event_groups()` to the bus: the URL's
        // `groupsToLastEventBatchIds=` is purely a per-group catch-up cursor
        // built from `last_batch_ids`, and the authenticated WebSocket already
        // implicitly subscribes to every group the user is a member of.
        let bus_access_token = session.access_token.clone();
        let bus_user_id = session
            .user_id()
            .ok_or_else(|| "Missing user id from session".to_string())?;
        let bus_base_url = config.api_url.clone();

        let backend: Arc<dyn MailBackend> = Arc::new(session);
        let store = MailStore::new();
        self.store = Some(store.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let (shutdown_sync_tx, shutdown_sync_rx) = watch::channel(false);
        self.shutdown_tx = Some(tx);
        self.started_at = Some(std::time::Instant::now());

        let status = self.status.clone();
        let log_tx = self.log_tx.clone();
        let imap_port = config.imap_port;
        let smtp_port = config.smtp_port;
        let sync_limit = config.sync_limit;
        let pw = config.bridge_password.clone();

        // Build the realtime event bus and hydrate its catch-up state from
        // disk so the next reconnect resumes from the last processed batch.
        let bus_client = Arc::new(tutasdk::event_bus::EventBusClient::new(
            bus_base_url,
            sys_model_version(),
            tutanota_model_version(),
            tutasdk::CLIENT_VERSION.to_string(),
            CLIENT_NAME.to_string(),
        ));
        {
            // OutOfSync detection: if the oldest cursor is older than the
            // server's batch-replay window (~44 days), the server cannot
            // catch us up — wipe the state so the syncer falls through to a
            // bootstrap full sync.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let expire_ms = tutasdk::event_bus::ENTITY_EVENT_BATCH_EXPIRE.as_millis() as i64;
            if let Ok(Some(min_ms)) = local_store.event_bus_state_min_updated_at_ms() {
                if now_ms - min_ms > expire_ms {
                    self.emit_log(
                        "Cached event-bus state is older than 44 days — wiping and forcing a full re-sync",
                    );
                    if let Err(e) = local_store.clear_event_bus_state() {
                        self.emit_log(&format!("Could not clear event_bus_state: {e}"));
                    }
                }
            }

            let ids_handle = bus_client.last_batch_ids();
            match local_store.load_event_bus_state() {
                Ok(s) if !s.is_empty() => {
                    let mut m = ids_handle.lock().unwrap();
                    m.extend(s);
                    self.emit_log(&format!(
                        "Event bus catch-up state loaded ({} group(s))",
                        m.len()
                    ));
                },
                Ok(_) => self.emit_log("Event bus catch-up state is empty (first launch)"),
                Err(e) => self.emit_log(&format!("Could not load event_bus_state: {e}")),
            }
        }
        let bus_ids_for_handler = bus_client.last_batch_ids();
        self.ws_state_rx = Some(bus_client.state());

        let task = tokio::spawn(async move {
            let imap_tls = tls_acceptor.clone();
            let smtp_tls = tls_acceptor;

            let _ = log_tx.send(format!("IMAP listening on 127.0.0.1:{imap_port}"));
            let _ = log_tx.send(format!("SMTP listening on 127.0.0.1:{smtp_port}"));

            // mpsc channel from event bus -> handler.
            let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);

            let syncer_handle = tokio::spawn(sync::run_syncer(
                store.clone(),
                local_store.clone(),
                backend.clone(),
                sync_limit,
                shutdown_sync_rx.clone(),
            ));
            let bus_handle = {
                let client = Arc::clone(&bus_client);
                let token = bus_access_token;
                let uid = bus_user_id;
                let shutdown = shutdown_sync_rx.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.run(token, uid, event_tx, shutdown).await {
                        match e {
                            tutasdk::event_bus::EventBusError::Stopped => {},
                            _ => log::warn!("Event bus exited: {e}"),
                        }
                    }
                })
            };
            let handler_handle = tokio::spawn(event_handler::run_event_handler(
                store.clone(),
                local_store,
                backend.clone(),
                sync_limit,
                bus_ids_for_handler,
                event_rx,
                shutdown_sync_rx.clone(),
            ));
            let mut imap_handle = tokio::spawn(imap::serve(
                imap_port,
                store.clone(),
                backend.clone(),
                imap_tls,
                pw.clone(),
            ));
            let mut smtp_handle = tokio::spawn(smtp::serve(smtp_port, backend.clone(), smtp_tls, pw));

            tokio::select! {
                _ = rx => {
                    let _ = log_tx.send("Bridge shutting down...".to_string());
                    let _ = shutdown_sync_tx.send(true);
                }
                r = &mut imap_handle => {
                    if let Err(e) = r {
                        let _ = log_tx.send(format!("IMAP server error: {e}"));
                    }
                }
                r = &mut smtp_handle => {
                    if let Err(e) = r {
                        let _ = log_tx.send(format!("SMTP server error: {e}"));
                    }
                }
            }

            // Tear everything down and wait for it, so ports are released before
            // a subsequent start rebinds them. Skip awaiting a handle that already
            // resolved in the select above (re-polling it would panic).
            syncer_handle.abort();
            bus_handle.abort();
            handler_handle.abort();
            imap_handle.abort();
            smtp_handle.abort();
            if !syncer_handle.is_finished() {
                let _ = syncer_handle.await;
            }
            if !bus_handle.is_finished() {
                let _ = bus_handle.await;
            }
            if !handler_handle.is_finished() {
                let _ = handler_handle.await;
            }
            if !imap_handle.is_finished() {
                let _ = imap_handle.await;
            }
            if !smtp_handle.is_finished() {
                let _ = smtp_handle.await;
            }
            *status.write().await = BridgeStatus::Stopped;
            let _ = log_tx.send("Bridge stopped".to_string());
        });

        self.task = Some(task);
        *self.status.write().await = BridgeStatus::Running;
        self.emit_log("Bridge is running");
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.started_at = None;
        self.ws_state_rx = None;
    }

    fn emit_log(&self, msg: &str) {
        let _ = self.log_tx.send(msg.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_version_extracts_first_entry_version() {
        let json = r#"{
            "0": {"name":"Foo","app":"sys","version":150,"id":0},
            "1": {"name":"Bar","app":"sys","version":150,"id":1}
        }"#;
        assert_eq!(parse_model_version(json), 150);
    }

    #[test]
    fn sys_and_tutanota_model_versions_are_positive() {
        // The included JSONs must always carry a positive version; if this
        // ever returns 0 something is very wrong with the vendored SDK.
        assert!(sys_model_version() > 0);
        assert!(tutanota_model_version() > 0);
    }
}
