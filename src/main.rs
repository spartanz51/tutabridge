mod config;
mod tuta;
mod imap;
mod mail;
mod smtp;
mod tls;

use std::sync::Arc;
use log::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install TLS crypto provider"))?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = config::load_or_create_config().map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("TutaBridge starting...");

    let tls_acceptor = tls::load_or_create_tls_acceptor()
        .map_err(|e| anyhow::anyhow!("TLS setup failed: {e}"))?;
    info!("TLS initialized");

    info!("IMAP will listen on 127.0.0.1:{}", cfg.imap_port);
    info!("SMTP will listen on 127.0.0.1:{}", cfg.smtp_port);

    let session = tuta::login(&cfg).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let session: Arc<dyn tuta::MailBackend> = Arc::new(session);
    info!("Logged in as {}", cfg.email);

    let imap_session = session.clone();
    let smtp_session = session.clone();
    let imap_tls = tls_acceptor.clone();
    let smtp_tls = tls_acceptor;

    let imap_handle = tokio::spawn(imap::serve(cfg.imap_port, imap_session, imap_tls));
    let smtp_handle = tokio::spawn(smtp::serve(cfg.smtp_port, smtp_session, smtp_tls));

    info!("Bridge is running. Configure Thunderbird with:");
    info!("  IMAP server: 127.0.0.1:{} (SSL/TLS)", cfg.imap_port);
    info!("  SMTP server: 127.0.0.1:{} (SSL/TLS)", cfg.smtp_port);
    info!("  Username: {}", cfg.email);
    info!("  Password: (any password — bridge handles auth)");
    info!("  Accept the self-signed certificate when prompted");

    tokio::select! {
        r = imap_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
        r = smtp_handle => r.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| anyhow::anyhow!("{e}")),
    }
}
