use std::sync::Arc;
use log::info;
use tutabridge_core::{config, store::LocalStore, sync, tls, tuta, imap, smtp};

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

    let backend: Arc<dyn tuta::MailBackend> = Arc::new(session);

    let store = sync::MailStore::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let imap_tls = tls_acceptor.clone();
    let smtp_tls = tls_acceptor;

    let pw = cfg.bridge_password.clone();

    let syncer_handle = tokio::spawn(sync::run_syncer(
        store.clone(), local_store, backend.clone(), cfg.sync_limit, shutdown_rx,
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
            Ok(())
        }
        r = imap_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
        r = smtp_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
    }
}
