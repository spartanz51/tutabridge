use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, watch, RwLock};

use crate::config::{self, Config};
use crate::event_handler;
use crate::store::LocalStore;
use crate::sync::{self, MailStore};
use crate::tuta::{self, MailBackend, TwoFactorCallback};
use crate::{imap, smtp, tls};

/// Identifier the server uses for telemetry/rate-limit bucketing.
pub const CLIENT_NAME: &str = "tutabridge";

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

pub fn sys_model_version() -> u32 {
    static V: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| parse_model_version(SYS_TYPE_MODELS_JSON));
    *V
}

pub fn tutanota_model_version() -> u32 {
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
    /// Fires whenever something `stats()` would surface has changed —
    /// mail count, ws state, uptime tick on start/stop. Lets the UI replace
    /// its 1s poll with a push subscription.
    stats_dirty_tx: broadcast::Sender<()>,
    started_at: Option<std::time::Instant>,
    store: Option<Arc<MailStore>>,
    /// Logged-in backend + local cache, kept after `start` so a backup can
    /// reuse the live session instead of opening a second one.
    backend: Option<Arc<dyn MailBackend>>,
    local_store: Option<Arc<LocalStore>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Latest event-bus state, populated at `start` and observed by `stats`.
    ws_state_rx: Option<watch::Receiver<tutasdk::event_bus::WsState>>,
}

impl BridgeHandle {
    pub fn new() -> Self {
        let (log_tx, _) = broadcast::channel(256);
        let (stats_dirty_tx, _) = broadcast::channel(16);
        Self {
            status: Arc::new(RwLock::new(BridgeStatus::Stopped)),
            shutdown_tx: None,
            log_tx,
            stats_dirty_tx,
            started_at: None,
            store: None,
            backend: None,
            local_store: None,
            task: None,
            ws_state_rx: None,
        }
    }

    /// The logged-in backend + local cache, available while the bridge is
    /// running. Used by the GUI's backup command to reuse the live session.
    pub fn backend_and_store(&self) -> Option<(Arc<dyn MailBackend>, Arc<LocalStore>)> {
        match (&self.backend, &self.local_store) {
            (Some(b), Some(s)) => Some((b.clone(), s.clone())),
            _ => None,
        }
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<String> {
        self.log_tx.subscribe()
    }

    pub fn log_sender(&self) -> broadcast::Sender<String> {
        self.log_tx.clone()
    }

    /// Subscribe to "stats dirty" pulses. The receiver gets a `()` every
    /// time something that `stats()` would surface has changed; the UI calls
    /// `stats()` on each pulse instead of polling on a timer.
    pub fn subscribe_stats(&self) -> broadcast::Receiver<()> {
        self.stats_dirty_tx.subscribe()
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
        // Only a running bridge has an uptime. A teardown from inside the
        // bridge task leaves `started_at` set (only `stop()` clears it), and
        // the counter used to keep climbing next to a Stopped status.
        let running = *self.status.read().await == BridgeStatus::Running;
        BridgeStats {
            uptime_secs: self
                .started_at
                .filter(|_| running)
                .map(|t| t.elapsed().as_secs()),
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

        if let Err(msg) = config.validate_ports() {
            return Err(self.start_failed(msg).await);
        }

        let tls_acceptor = match tls::load_or_create_tls_acceptor() {
            Ok(a) => a,
            Err(e) => return Err(self.start_failed(format!("TLS setup failed: {e}")).await),
        };
        self.emit_log("TLS initialized");

        // Bind both listeners before logging in. A port another application
        // already holds then fails the start at once, with a message naming
        // it, instead of a full login (and a TOTP prompt) followed by a
        // teardown that logged nothing.
        let imap_listener = match crate::net::bind_local("IMAP", config.imap_port).await {
            Ok(listener) => listener,
            Err(msg) => return Err(self.start_failed(msg).await),
        };
        let smtp_listener = match crate::net::bind_local("SMTP", config.smtp_port).await {
            Ok(listener) => listener,
            Err(msg) => return Err(self.start_failed(msg).await),
        };
        // The MCP server is optional, but once the user has turned it on a
        // port another application holds must not leave it silently absent.
        let mcp_listener = if config.mcp_permission.is_enabled() {
            match crate::net::bind_local("MCP", config.mcp_port).await {
                Ok(listener) => Some(listener),
                Err(msg) => return Err(self.start_failed(msg).await),
            }
        } else {
            None
        };
        self.emit_log(&format!("IMAP listening on 127.0.0.1:{}", config.imap_port));
        self.emit_log(&format!("SMTP listening on 127.0.0.1:{}", config.smtp_port));

        self.emit_log(&format!("Authenticating as {}...", config.email));
        let session = match tuta::login_with_2fa(&config, password.as_deref(), totp_callback).await
        {
            Ok(s) => s,
            Err(e) => return Err(self.start_failed(format!("Login failed: {e}")).await),
        };
        self.emit_log(&format!("Logged in as {}", config.email));

        // Every failure from here on must go through `start_failed` too: an
        // early return that leaves the status at Starting wedges the GUI on
        // "Connecting…", with Stop doing nothing and Start refused as
        // "already running" until the app is restarted.
        let storage_key = match session.derive_storage_key().await {
            Ok(key) => key,
            Err(e) => {
                let msg = format!("Storage key derivation failed: {e}");
                return Err(self.start_failed(msg).await);
            }
        };
        self.emit_log("Storage encryption key derived");

        let local_store = match LocalStore::open(
            &config::store_db_path(),
            &config::store_mails_dir(),
            storage_key,
        ) {
            Ok(store) => store,
            Err(e) => {
                let msg = format!("Failed to open local store: {e}");
                return Err(self.start_failed(msg).await);
            }
        };
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
        let Some(bus_user_id) = session.user_id() else {
            return Err(self
                .start_failed("Missing user id from session".to_string())
                .await);
        };
        let bus_base_url = config.api_url.clone();

        let backend: Arc<dyn MailBackend> = Arc::new(session);
        let store = MailStore::new();
        self.store = Some(store.clone());
        self.backend = Some(backend.clone());
        self.local_store = Some(local_store.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let (shutdown_sync_tx, shutdown_sync_rx) = watch::channel(false);
        self.shutdown_tx = Some(tx);
        self.started_at = Some(std::time::Instant::now());

        let status = self.status.clone();
        let log_tx = self.log_tx.clone();
        let sync_limit = config.sync_limit;
        let pw = config.bridge_password.clone();
        let mcp_permission = config.mcp_permission;

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
                    let mut m = crate::util::lock_recover(&ids_handle);
                    m.extend(s);
                    self.emit_log(&format!(
                        "Event bus catch-up state loaded ({} group(s))",
                        m.len()
                    ));
                }
                Ok(_) => self.emit_log("Event bus catch-up state is empty (first launch)"),
                Err(e) => self.emit_log(&format!("Could not load event_bus_state: {e}")),
            }
        }
        let bus_ids_for_handler = bus_client.last_batch_ids();
        self.ws_state_rx = Some(bus_client.state());

        // Spawn a tiny watcher BEFORE the outer task swallows the
        // observables: it turns MailStore / WsState changes into
        // `stats_dirty_tx` pulses so the UI can replace its 1s poll with
        // an event subscription. Aborts when shutdown fires.
        {
            let stats_dirty = self.stats_dirty_tx.clone();
            let mut store_watch = store.subscribe();
            // Body-only updates ride a separate channel so they don't wake
            // idle IMAP sessions; the stats display still wants them.
            let mut details_watch = store.subscribe_details();
            let mut ws_watch = bus_client.state();
            let mut shutdown_watch = shutdown_sync_rx.clone();
            // Track the previous WS state so we only log on transitions,
            // not on every internal `publish()` (some calls re-set the same
            // state).
            let mut last_ws_state = *ws_watch.borrow();
            tokio::spawn(async move {
                // Initial pulse so a subscriber sees the freshly-started state.
                let _ = stats_dirty.send(());
                loop {
                    tokio::select! {
                        _ = store_watch.changed() => { let _ = stats_dirty.send(()); }
                        _ = details_watch.changed() => { let _ = stats_dirty.send(()); }
                        _ = ws_watch.changed() => {
                            let now = *ws_watch.borrow();
                            if now != last_ws_state {
                                log::info!("ws state: {:?} → {:?}", last_ws_state, now);
                                last_ws_state = now;
                            }
                            let _ = stats_dirty.send(());
                        }
                        _ = shutdown_watch.changed() => return,
                    }
                }
            });
        }

        let task = tokio::spawn(async move {
            let imap_tls = tls_acceptor.clone();
            let smtp_tls = tls_acceptor;

            // mpsc channel from event bus -> handler.
            let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
            // The syncer tells the event handler when its startup is done
            // (see `event_handler::run_event_handler`).
            let (startup_done_tx, startup_done_rx) = watch::channel(false);

            let syncer_handle = tokio::spawn(sync::run_syncer(
                store.clone(),
                local_store.clone(),
                backend.clone(),
                sync_limit,
                startup_done_tx,
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
                            tutasdk::event_bus::EventBusError::Stopped => {}
                            _ => log::warn!("Event bus exited: {e}"),
                        }
                    }
                })
            };
            let handler_handle = tokio::spawn(event_handler::run_event_handler(
                store.clone(),
                local_store.clone(),
                backend.clone(),
                bus_ids_for_handler,
                event_rx,
                startup_done_rx,
                shutdown_sync_rx.clone(),
            ));
            let mut imap_handle = tokio::spawn(imap::serve_listener(
                imap_listener,
                store.clone(),
                backend.clone(),
                local_store.clone(),
                imap_tls,
                pw.clone(),
            ));
            // Read-only MCP server, only when enabled (its listener is then
            // bound above). Kept out of the select!: it is optional, and
            // logs its own failure.
            let mcp_handle = mcp_listener.map(|listener| {
                tokio::spawn(crate::mcp::serve_listener(
                    listener,
                    store.clone(),
                    local_store,
                    backend.clone(),
                    pw.clone(),
                    mcp_permission,
                    shutdown_sync_rx.clone(),
                ))
            });
            let mut smtp_handle = tokio::spawn(smtp::serve_listener(
                smtp_listener,
                backend.clone(),
                smtp_tls,
                pw,
            ));

            let mut server_failure: Option<String> = None;
            tokio::select! {
                _ = rx => {
                    let _ = log_tx.send("Bridge shutting down...".to_string());
                }
                r = &mut imap_handle => server_failure = Some(server_exit_message("IMAP", r)),
                r = &mut smtp_handle => server_failure = Some(server_exit_message("SMTP", r)),
            }
            if let Some(msg) = &server_failure {
                log::error!("{msg}");
                let _ = log_tx.send(msg.clone());
            }
            let _ = shutdown_sync_tx.send(true);

            // Tear everything down and wait for it, so ports are released before
            // a subsequent start rebinds them. Skip awaiting a handle that already
            // resolved in the select above (re-polling it would panic).
            syncer_handle.abort();
            bus_handle.abort();
            handler_handle.abort();
            imap_handle.abort();
            if let Some(h) = &mcp_handle {
                h.abort();
            }
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
            // Its listener too, or a restart finds the MCP port still taken.
            if let Some(h) = mcp_handle {
                let _ = h.await;
            }
            *status.write().await = match server_failure {
                Some(msg) => BridgeStatus::Error(msg),
                None => BridgeStatus::Stopped,
            };
            let _ = log_tx.send("Bridge stopped".to_string());
        });

        self.task = Some(task);

        *self.status.write().await = BridgeStatus::Running;
        self.emit_log("Bridge is running");
        let _ = self.stats_dirty_tx.send(()); // status transition
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
        self.backend = None;
        self.local_store = None;
        // Final pulse so the UI re-reads `stats()` (now reporting Stopped /
        // zero uptime / zero mails) without waiting for a poll.
        let _ = self.stats_dirty_tx.send(());
    }

    /// Record a failed start: in the log stream, in the GUI panel and as the
    /// bridge status, so the user sees why it did not start.
    async fn start_failed(&self, msg: String) -> String {
        log::error!("{msg}");
        self.emit_log(&msg);
        *self.status.write().await = BridgeStatus::Error(msg.clone());
        msg
    }

    fn emit_log(&self, msg: &str) {
        let _ = self.log_tx.send(msg.to_string());
    }
}

/// What to tell the user when a server task ends on its own. Its future only
/// returns on an error, the accept loop never finishing by itself, or the task
/// panicked. The previous handling matched only the panic, so an error from the
/// server tore the bridge down with nothing logged.
fn server_exit_message(
    service: &str,
    exit: Result<Result<(), Box<dyn std::error::Error + Send + Sync>>, tokio::task::JoinError>,
) -> String {
    match exit {
        Ok(Ok(())) => format!("{service} server stopped unexpectedly"),
        Ok(Err(e)) => format!("{service} server failed: {e}"),
        Err(e) => format!("{service} server crashed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_that_returns_an_error_is_reported() {
        // The case that used to tear the bridge down silently: the task did
        // not panic, its future returned an error.
        let task = tokio::spawn(async {
            Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                "SMTP port 1025 is already in use by another application".into(),
            )
        });
        assert_eq!(
            server_exit_message("SMTP", task.await),
            "SMTP server failed: SMTP port 1025 is already in use by another application"
        );
    }

    #[tokio::test]
    async fn a_server_that_panics_is_reported() {
        let task = tokio::spawn(async {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let msg = server_exit_message("IMAP", task.await);
        assert!(msg.starts_with("IMAP server crashed: "), "{msg}");
    }

    #[tokio::test]
    async fn a_server_that_returns_ok_is_reported_as_unexpected() {
        let task = tokio::spawn(async { Ok::<(), Box<dyn std::error::Error + Send + Sync>>(()) });
        assert_eq!(
            server_exit_message("IMAP", task.await),
            "IMAP server stopped unexpectedly"
        );
    }

    #[tokio::test]
    async fn a_failed_start_leaves_an_error_status_not_starting() {
        // Starting would wedge the GUI: Start refused as "already running",
        // Stop a no-op. Colliding ports fail before TLS or any network call.
        let mut handle = BridgeHandle::new();
        let config = Config {
            imap_port: 1143,
            smtp_port: 1143,
            ..Config::default()
        };

        let err = handle.start(config, None, None).await.unwrap_err();

        assert_eq!(err, "IMAP and SMTP ports must differ (both are 1143)");
        assert_eq!(*handle.status.read().await, BridgeStatus::Error(err));
    }

    #[tokio::test]
    async fn uptime_is_only_reported_while_running() {
        // A teardown from inside the task sets the status but leaves
        // `started_at`, which only `stop()` clears.
        let mut handle = BridgeHandle::new();
        handle.started_at = Some(std::time::Instant::now());
        *handle.status.write().await = BridgeStatus::Running;
        assert!(handle.stats().await.uptime_secs.is_some());

        *handle.status.write().await = BridgeStatus::Error("SMTP server failed".into());
        assert_eq!(handle.stats().await.uptime_secs, None);

        *handle.status.write().await = BridgeStatus::Stopped;
        assert_eq!(handle.stats().await.uptime_secs, None);
    }

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
