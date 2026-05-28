use std::sync::Arc;
use log::{info, warn};
use tutabridge_core::{
    bridge as bridge_helpers, config, event_handler, imap, smtp, store::LocalStore, sync, tls,
    tuta,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install TLS crypto provider"))?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut cfg = match config::load_config().map_err(|e| anyhow::anyhow!("{e}"))? {
        Some(cfg) if !cfg.email.is_empty() => cfg,
        _ => {
            use std::io::{BufRead, Write};
            print!("Tuta email address: ");
            std::io::stdout().flush()?;
            let mut email = String::new();
            std::io::stdin().lock().read_line(&mut email)?;
            let email = email.trim().to_string();
            if email.is_empty() {
                anyhow::bail!("Email address is required");
            }
            let cfg = config::Config {
                email,
                ..Default::default()
            };
            config::save_config(&cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
            cfg
        }
    };

    let bridge_password = config::ensure_bridge_password(&mut cfg)
        .map_err(|e| anyhow::anyhow!("Bridge password setup failed: {e}"))?;
    info!("TutaBridge starting...");

    let tls_acceptor = tls::load_or_create_tls_acceptor()
        .map_err(|e| anyhow::anyhow!("TLS setup failed: {e}"))?;
    info!("TLS initialized");

    info!("IMAP will listen on 127.0.0.1:{}", cfg.imap_port);
    info!("SMTP will listen on 127.0.0.1:{}", cfg.smtp_port);

    let totp_cb = tuta::TwoFactorCallback::Totp(Box::new(|| {
        use std::io::{BufRead, Write};
        print!("TOTP code: ");
        std::io::stdout().flush()?;
        let mut code_str = String::new();
        std::io::stdin().lock().read_line(&mut code_str)?;
        let code: u32 = code_str
            .trim()
            .parse()
            .map_err(|_| "Invalid TOTP code — must be a number")?;
        Ok(code)
    }));

    // Try keyring session first, only prompt for password if needed
    let session = match tuta::login_with_2fa(&cfg, None, Some(totp_cb)).await {
        Ok(s) => s,
        Err(_) => {
            let password = rpassword::prompt_password(format!("Password for {}: ", cfg.email))?;
            let totp_cb2 = tuta::TwoFactorCallback::Totp(Box::new(|| {
                use std::io::{BufRead, Write};
                print!("TOTP code: ");
                std::io::stdout().flush()?;
                let mut code_str = String::new();
                std::io::stdin().lock().read_line(&mut code_str)?;
                let code: u32 = code_str
                    .trim()
                    .parse()
                    .map_err(|_| "Invalid TOTP code — must be a number")?;
                Ok(code)
            }));
            tuta::login_with_2fa(&cfg, Some(&password), Some(totp_cb2))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
    };
    info!("Logged in as {}", cfg.email);

    let storage_key = session.derive_storage_key().await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("Storage encryption key derived");

    let local_store = LocalStore::open(
        &config::store_db_path(),
        &config::store_mails_dir(),
        storage_key,
    ).map_err(|e| anyhow::anyhow!("{e}"))?;
    if !local_store.verify_key() {
        info!("Storage key changed — resetting local cache");
        let _ = local_store.reset();
    }
    let local_store = Arc::new(local_store);
    info!("Local store opened");

    // Build the realtime event bus and hydrate its catch-up cursor from
    // disk so reconnects resume from the last processed batch — the GUI's
    // `BridgeHandle` does the same dance; the CLI used to skip it entirely
    // and quietly degrade to "bootstrap-sync only at startup".
    let bus_access_token = session.access_token.clone();
    let bus_user_id = session
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("Missing user id from session"))?;
    let bus_client = Arc::new(tutasdk::event_bus::EventBusClient::new(
        cfg.api_url.clone(),
        bridge_helpers::sys_model_version(),
        bridge_helpers::tutanota_model_version(),
        tutasdk::CLIENT_VERSION.to_string(),
        bridge_helpers::CLIENT_NAME.to_string(),
    ));
    {
        // OutOfSync detection: if the oldest cursor is older than the
        // server's batch-replay window (~44 days), the server cannot
        // catch us up — wipe so the syncer falls back to a bootstrap.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let expire_ms = tutasdk::event_bus::ENTITY_EVENT_BATCH_EXPIRE.as_millis() as i64;
        if let Ok(Some(min_ms)) = local_store.event_bus_state_min_updated_at_ms() {
            if now_ms - min_ms > expire_ms {
                info!("Cached event-bus state is older than 44 days — wiping and forcing a full re-sync");
                if let Err(e) = local_store.clear_event_bus_state() {
                    warn!("Could not clear event_bus_state: {e}");
                }
            }
        }
        match local_store.load_event_bus_state() {
            Ok(s) if !s.is_empty() => {
                let ids_handle = bus_client.last_batch_ids();
                let mut m = ids_handle.lock().unwrap();
                let n = s.len();
                m.extend(s);
                info!("Event bus catch-up state loaded ({n} group(s))");
            }
            Ok(_) => info!("Event bus catch-up state is empty (first launch)"),
            Err(e) => warn!("Could not load event_bus_state: {e}"),
        }
    }
    let bus_ids_for_handler = bus_client.last_batch_ids();

    let backend: Arc<dyn tuta::MailBackend> = Arc::new(session);

    let store = sync::MailStore::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let imap_tls = tls_acceptor.clone();
    let smtp_tls = tls_acceptor;

    let pw = cfg.bridge_password.clone();

    // mpsc channel from event bus -> handler.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);

    let syncer_handle = tokio::spawn(sync::run_syncer(
        store.clone(),
        local_store.clone(),
        backend.clone(),
        cfg.sync_limit,
        shutdown_rx.clone(),
    ));
    let bus_handle = {
        let client = Arc::clone(&bus_client);
        let token = bus_access_token;
        let uid = bus_user_id;
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = client.run(token, uid, event_tx, shutdown).await {
                use tutasdk::event_bus::EventBusError;
                if !matches!(e, EventBusError::Stopped) {
                    warn!("Event bus exited: {e}");
                }
            }
        })
    };
    // Log every WsState transition at INFO so production logs reveal
    // reconnect storms without RUST_LOG=debug.
    {
        let mut ws_watch = bus_client.state();
        let mut shutdown_watch = shutdown_rx.clone();
        let mut last = *ws_watch.borrow();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = ws_watch.changed() => {
                        let now = *ws_watch.borrow();
                        if now != last {
                            info!("ws state: {:?} → {:?}", last, now);
                            last = now;
                        }
                    }
                    _ = shutdown_watch.changed() => return,
                }
            }
        });
    }
    let handler_handle = tokio::spawn(event_handler::run_event_handler(
        store.clone(),
        local_store.clone(),
        backend.clone(),
        cfg.sync_limit,
        bus_ids_for_handler,
        event_rx,
        shutdown_rx.clone(),
    ));
    let imap_handle = tokio::spawn(imap::serve(
        cfg.imap_port, store.clone(), backend.clone(), imap_tls, pw.clone(),
    ));
    let smtp_handle = tokio::spawn(smtp::serve(cfg.smtp_port, backend.clone(), smtp_tls, pw));

    info!("Bridge is running. Configure Thunderbird with:");
    info!("  IMAP server: 127.0.0.1:{} (SSL/TLS)", cfg.imap_port);
    info!("  SMTP server: 127.0.0.1:{} (SSL/TLS)", cfg.smtp_port);
    info!("  Username: {}", cfg.email);
    info!("  Password: {}", bridge_password);
    info!("  Accept the self-signed certificate when prompted");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down...");
            let _ = shutdown_tx.send(true);
            syncer_handle.abort();
            bus_handle.abort();
            handler_handle.abort();
            Ok(())
        }
        r = imap_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
        r = smtp_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
    }
}
