use std::sync::Arc;
use base64::Engine;
use log::{info, error, debug};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::mail::parser::parse_rfc2822;
use crate::tuta::MailBackend;

#[derive(Debug)]
enum SmtpState {
    Init,
    Greeted,
    MailFrom(String),
    RcptTo { from: String, to: Vec<String> },
    #[allow(dead_code)]
    Data { from: String, to: Vec<String> },
    Quit,
}

#[derive(Debug)]
enum AuthStep {
    None,
    WaitPlainData,
    WaitLoginUser,
    WaitLoginPass,
}

pub async fn serve(
    port: u16,
    tuta: Arc<dyn MailBackend>,
    tls: TlsAcceptor,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
    info!("SMTP server listening on 127.0.0.1:{} (TLS)", port);

    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("SMTP connection from {}", addr);
        let tuta = tuta.clone();
        let tls = tls.clone();
        let pw_hash = password_hash.clone();

        tokio::spawn(async move {
            match tls.accept(stream).await {
                Ok(tls_stream) => {
                    if let Err(e) = handle_connection(tls_stream, tuta, pw_hash).await {
                        error!("SMTP connection error: {}", e);
                    }
                }
                Err(e) => {
                    error!("SMTP TLS handshake failed: {}", e);
                }
            }
        });
    }
}

async fn handle_connection(
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    tuta: Arc<dyn MailBackend>,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut state = SmtpState::Init;

    writer.write_all(b"220 TutaBridge SMTP ready\r\n").await?;

    let mut line = String::new();
    let mut data_buf = String::new();
    let mut in_data = false;
    let mut auth_step = AuthStep::None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim_end();
        debug!("SMTP C: {}", trimmed);

        if !matches!(auth_step, AuthStep::None) {
            let response = match auth_step {
                AuthStep::WaitPlainData => {
                    auth_step = AuthStep::None;
                    verify_smtp_plain_data(trimmed, &password_hash)
                }
                AuthStep::WaitLoginUser => {
                    auth_step = AuthStep::WaitLoginPass;
                    "334 UGFzc3dvcmQ6\r\n".to_string()
                }
                AuthStep::WaitLoginPass => {
                    auth_step = AuthStep::None;
                    let password = base64::engine::general_purpose::STANDARD
                        .decode(trimmed.trim())
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                        .unwrap_or_default();
                    verify_smtp_password(&password, &password_hash)
                }
                AuthStep::None => unreachable!(),
            };
            debug!("SMTP S: {}", response.trim_end());
            writer.write_all(response.as_bytes()).await?;
            continue;
        }

        if in_data {
            if trimmed == "." {
                in_data = false;
                info!("SMTP: received message ({} bytes)", data_buf.len());

                let envelope_to: Vec<String> = match &state {
                    SmtpState::Data { to, .. } => to.clone(),
                    _ => vec![],
                };
                let mut parsed = parse_rfc2822(&data_buf);
                let header_addrs: std::collections::HashSet<String> = parsed
                    .to
                    .iter()
                    .chain(parsed.cc.iter())
                    .chain(parsed.bcc.iter())
                    .map(|(_, addr)| addr.to_lowercase())
                    .collect();
                for rcpt in &envelope_to {
                    if !header_addrs.contains(&rcpt.to_lowercase()) {
                        parsed.bcc.push((String::new(), rcpt.clone()));
                    }
                }
                match tuta.send_mail(&parsed).await {
                    Ok(()) => {
                        info!("SMTP: mail sent successfully via Tuta");
                        writer.write_all(b"250 OK message sent\r\n").await?;
                    }
                    Err(e) => {
                        error!("SMTP: failed to send via Tuta: {}", e);
                        writer
                            .write_all(b"451 Temporary failure\r\n")
                            .await?;
                    }
                }
                state = SmtpState::Greeted;
                data_buf.clear();
            } else {
                let unstuffed = if line.starts_with("..") {
                    &line[1..]
                } else {
                    &line
                };
                data_buf.push_str(unstuffed);
            }
            continue;
        }

        let cmd = trimmed.split_whitespace().next().unwrap_or("").to_uppercase();
        let response = match cmd.as_str() {
            "EHLO" | "HELO" => {
                state = SmtpState::Greeted;
                "250-TutaBridge\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 SIZE 26214400\r\n"
                    .to_string()
            }
            "AUTH" => {
                let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
                let auth_type = parts.get(1).unwrap_or(&"").to_uppercase();
                match auth_type.as_str() {
                    "PLAIN" => {
                        if let Some(data) = parts.get(2) {
                            verify_smtp_plain_data(data, &password_hash)
                        } else {
                            auth_step = AuthStep::WaitPlainData;
                            "334 \r\n".to_string()
                        }
                    }
                    "LOGIN" => {
                        auth_step = AuthStep::WaitLoginUser;
                        "334 VXNlcm5hbWU6\r\n".to_string()
                    }
                    _ => "504 Unrecognized auth type\r\n".to_string(),
                }
            }
            "MAIL" => {
                let from = extract_address(trimmed);
                state = SmtpState::MailFrom(from);
                "250 OK\r\n".to_string()
            }
            "RCPT" => {
                let to_addr = extract_address(trimmed);
                match &mut state {
                    SmtpState::MailFrom(from) => {
                        let from = from.clone();
                        state = SmtpState::RcptTo {
                            from,
                            to: vec![to_addr],
                        };
                    }
                    SmtpState::RcptTo { to, .. } => {
                        to.push(to_addr);
                    }
                    _ => {
                        writer.write_all(b"503 Bad sequence\r\n").await?;
                        continue;
                    }
                }
                "250 OK\r\n".to_string()
            }
            "DATA" => {
                match &state {
                    SmtpState::RcptTo { from, to } => {
                        state = SmtpState::Data {
                            from: from.clone(),
                            to: to.clone(),
                        };
                        in_data = true;
                        "354 Start mail input; end with <CRLF>.<CRLF>\r\n".to_string()
                    }
                    _ => "503 Bad sequence\r\n".to_string(),
                }
            }
            "RSET" => {
                state = SmtpState::Greeted;
                "250 OK\r\n".to_string()
            }
            "QUIT" => {
                state = SmtpState::Quit;
                "221 BYE\r\n".to_string()
            }
            "NOOP" => "250 OK\r\n".to_string(),
            _ => "502 Command not implemented\r\n".to_string(),
        };

        debug!("SMTP S: {}", response.trim_end());
        writer.write_all(response.as_bytes()).await?;

        if matches!(state, SmtpState::Quit) {
            break;
        }
    }

    Ok(())
}

fn extract_address(line: &str) -> String {
    if let Some(start) = line.find('<') {
        if let Some(end) = line.find('>') {
            if start < end {
                return line[start + 1..end].to_string();
            }
        }
    }
    line.split(':')
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn verify_smtp_plain_data(data: &str, expected: &Option<String>) -> String {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .unwrap_or_default();
    // AUTH PLAIN format: \0username\0password
    let parts: Vec<&[u8]> = decoded.splitn(3, |b| *b == 0).collect();
    let password = if parts.len() >= 3 {
        String::from_utf8_lossy(parts[2]).to_string()
    } else {
        String::new()
    };
    verify_smtp_password(&password, expected)
}

fn verify_smtp_password(password: &str, expected: &Option<String>) -> String {
    match expected {
        Some(expected_pw) if password == expected_pw => {
            "235 2.7.0 Authentication successful\r\n".to_string()
        }
        Some(_) => "535 5.7.8 Authentication failed\r\n".to_string(),
        None => "235 2.7.0 Authentication successful\r\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_address_angle_brackets() {
        assert_eq!(
            extract_address("MAIL FROM:<alice@example.com>"),
            "alice@example.com"
        );
    }

    #[test]
    fn test_extract_address_rcpt_to() {
        assert_eq!(
            extract_address("RCPT TO:<bob@example.com>"),
            "bob@example.com"
        );
    }

    #[test]
    fn test_extract_address_no_brackets() {
        assert_eq!(
            extract_address("MAIL FROM:alice@example.com"),
            "alice@example.com"
        );
    }

    #[test]
    fn test_extract_address_with_spaces() {
        assert_eq!(
            extract_address("MAIL FROM: <alice@example.com>"),
            "alice@example.com"
        );
    }

    #[test]
    fn test_extract_address_empty() {
        assert_eq!(extract_address("MAIL FROM:<>"), "");
    }

    #[test]
    fn test_extract_address_no_colon() {
        assert_eq!(extract_address("NOOP"), "");
    }

    #[test]
    fn test_extract_address_malformed_brackets() {
        let result = extract_address("MAIL FROM:>bad<");
        assert_eq!(result, ">bad<");
    }

    #[test]
    fn test_dot_unstuffing() {
        let line = "..This line started with a dot\r\n";
        let unstuffed = if line.starts_with("..") {
            &line[1..]
        } else {
            line
        };
        assert_eq!(unstuffed, ".This line started with a dot\r\n");
    }

    #[test]
    fn test_no_dot_unstuffing_for_normal_lines() {
        let line = "Normal line\r\n";
        let unstuffed = if line.starts_with("..") {
            &line[1..]
        } else {
            line
        };
        assert_eq!(unstuffed, "Normal line\r\n");
    }

    #[test]
    fn test_single_dot_not_unstuffed() {
        let line = ".other\r\n";
        let unstuffed = if line.starts_with("..") {
            &line[1..]
        } else {
            line
        };
        assert_eq!(unstuffed, ".other\r\n");
    }
}
