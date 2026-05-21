mod session;

use std::sync::Arc;
use std::time::Duration;
use log::{info, error, debug};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::tuta::MailBackend;
use session::ImapSession;

pub async fn serve(port: u16, tuta: Arc<dyn MailBackend>, tls: TlsAcceptor) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
    info!("IMAP server listening on 127.0.0.1:{} (TLS)", port);

    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("IMAP connection from {}", addr);
        let tuta = tuta.clone();
        let tls = tls.clone();

        tokio::spawn(async move {
            match tls.accept(stream).await {
                Ok(tls_stream) => {
                    if let Err(e) = handle_connection(tls_stream, tuta).await {
                        error!("IMAP connection error: {}", e);
                    }
                }
                Err(e) => {
                    error!("IMAP TLS handshake failed: {}", e);
                }
            }
        });
    }
}

async fn handle_connection(
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    tuta: Arc<dyn MailBackend>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut session = ImapSession::new(tuta);

    writer.write_all(b"* OK TutaBridge IMAP4rev1 ready\r\n").await?;

    let mut line = String::new();
    loop {
        if session.is_idle() {
            let poll_interval = Duration::from_secs(30);
            line.clear();
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    let n = result?;
                    if n == 0 {
                        break;
                    }
                    let trimmed = line.trim_end();
                    debug!("IMAP C (idle): {}", trimmed);
                    if trimmed.eq_ignore_ascii_case("DONE") {
                        let responses = session.end_idle();
                        for resp in &responses {
                            debug!("IMAP S: {}", resp.trim_end());
                            writer.write_all(resp.as_bytes()).await?;
                        }
                    }
                }
                _ = tokio::time::sleep(poll_interval) => {
                    let updates = session.check_new_mail().await;
                    for resp in &updates {
                        debug!("IMAP S (idle): {}", resp.trim_end());
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
            }
            continue;
        }

        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim_end();
        debug!("IMAP C: {}", trimmed);

        let responses = session.handle_command(trimmed).await;
        for resp in &responses {
            debug!("IMAP S: {}", resp.trim_end());
            writer.write_all(resp.as_bytes()).await?;
        }

        if session.is_logout() {
            break;
        }
    }

    Ok(())
}
