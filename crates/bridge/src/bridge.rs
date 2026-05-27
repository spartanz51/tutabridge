use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, watch, RwLock};

use crate::config::{self, Config};
use crate::store::LocalStore;
use crate::sync::{self, MailStore};
use crate::tuta::{self, MailBackend, TwoFactorCallback};
use crate::{imap, smtp, tls};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum BridgeStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeStats {
    pub uptime_secs: Option<u64>,
    pub mails_synced: usize,
}

pub struct BridgeHandle {
    status: Arc<RwLock<BridgeStatus>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    log_tx: broadcast::Sender<String>,
    started_at: Option<std::time::Instant>,
    store: Option<Arc<MailStore>>,
    task: Option<tokio::task::JoinHandle<()>>,
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
        BridgeStats {
            uptime_secs: self.started_at.map(|t| t.elapsed().as_secs()),
            mails_synced: count,
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

        let task = tokio::spawn(async move {
            let imap_tls = tls_acceptor.clone();
            let smtp_tls = tls_acceptor;

            let _ = log_tx.send(format!("IMAP listening on 127.0.0.1:{imap_port}"));
            let _ = log_tx.send(format!("SMTP listening on 127.0.0.1:{smtp_port}"));

            let syncer_handle = tokio::spawn(sync::run_syncer(
                store.clone(),
                local_store,
                backend.clone(),
                sync_limit,
                shutdown_sync_rx,
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
            imap_handle.abort();
            smtp_handle.abort();
            if !syncer_handle.is_finished() {
                let _ = syncer_handle.await;
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
    }

    fn emit_log(&self, msg: &str) {
        let _ = self.log_tx.send(msg.to_string());
    }
}
