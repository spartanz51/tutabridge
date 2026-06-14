use base64::Engine;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::mail::parser::parse_rfc2822;
use crate::tuta::MailBackend;

/// Caps on what a single SMTP connection may buffer. The server advertises
/// `SIZE` in EHLO; these are what it actually enforces, so a misbehaving or
/// malicious local client cannot make the bridge buffer an unbounded message
/// (or a single unbounded line) into memory.
#[derive(Clone, Copy)]
struct SmtpLimits {
    /// Maximum total DATA payload (matches the advertised `SIZE`).
    max_message_bytes: usize,
    /// Maximum bytes in one protocol line before we give up on the connection.
    max_line_bytes: usize,
}

impl Default for SmtpLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 26_214_400, // 25 MiB, matches the advertised SIZE
            max_line_bytes: 1_048_576,     // 1 MiB: generous for headers/base64 lines
        }
    }
}

enum LineOutcome {
    Line,
    Eof,
    TooLong,
}

/// Read one `\n`-terminated line without ever buffering more than `max_bytes`.
/// Returns `TooLong` if a line exceeds the cap before terminating (the caller
/// should drop the connection: the stream is now stuck mid-line). Uses the
/// `AsyncBufRead` primitives so it never allocates beyond one line.
async fn read_line_capped<R>(
    reader: &mut R,
    max_bytes: usize,
    out: &mut String,
) -> std::io::Result<LineOutcome>
where
    R: AsyncBufRead + Unpin,
{
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF: a final line without a trailing newline is still a line.
            return Ok(if raw.is_empty() {
                LineOutcome::Eof
            } else {
                finish_line(&raw, max_bytes, out)
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                raw.extend_from_slice(&available[..=pos]);
                reader.consume(pos + 1);
                return Ok(finish_line(&raw, max_bytes, out));
            }
            None => {
                let len = available.len();
                raw.extend_from_slice(available);
                reader.consume(len);
                if raw.len() > max_bytes {
                    return Ok(LineOutcome::TooLong);
                }
            }
        }
    }
}

fn finish_line(raw: &[u8], max_bytes: usize, out: &mut String) -> LineOutcome {
    if raw.len() > max_bytes {
        return LineOutcome::TooLong;
    }
    out.clear();
    out.push_str(&String::from_utf8_lossy(raw));
    LineOutcome::Line
}

/// `true` if a `MAIL FROM` line declares a `SIZE=` larger than `max`. A client
/// that announces an over-limit message is rejected before it streams DATA.
fn size_param_exceeds(mail_line: &str, max: usize) -> bool {
    mail_line.split_whitespace().any(|tok| {
        tok.get(..5)
            .map(|p| p.eq_ignore_ascii_case("SIZE="))
            .unwrap_or(false)
            && tok[5..].parse::<usize>().map(|n| n > max).unwrap_or(false)
    })
}

#[derive(Debug)]
enum SmtpState {
    Init,
    Greeted,
    MailFrom(String),
    RcptTo {
        from: String,
        to: Vec<String>,
    },
    #[allow(dead_code)]
    Data {
        from: String,
        to: Vec<String>,
    },
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

    crate::net::accept_loop(
        listener,
        "SMTP",
        crate::net::MAX_CONNECTIONS,
        move |stream, _addr| {
            let tuta = tuta.clone();
            let tls = tls.clone();
            let pw_hash = password_hash.clone();
            async move {
                match tokio::time::timeout(crate::net::HANDSHAKE_TIMEOUT, tls.accept(stream)).await
                {
                    Ok(Ok(tls_stream)) => {
                        if let Err(e) =
                            handle_connection(tls_stream, tuta, pw_hash, SmtpLimits::default()).await
                        {
                            error!("SMTP connection error: {}", e);
                        }
                    }
                    Ok(Err(e)) => error!("SMTP TLS handshake failed: {}", e),
                    Err(_) => debug!("SMTP TLS handshake timed out"),
                }
            }
        },
    )
    .await;

    Ok(())
}

async fn handle_connection<S>(
    stream: S,
    tuta: Arc<dyn MailBackend>,
    password_hash: Option<String>,
    limits: SmtpLimits,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut state = SmtpState::Init;

    writer.write_all(b"220 TutaBridge SMTP ready\r\n").await?;

    let mut line = String::new();
    let mut data_buf = String::new();
    let mut in_data = false;
    let mut data_too_large = false;
    let mut auth_step = AuthStep::None;

    loop {
        match read_line_capped(&mut reader, limits.max_line_bytes, &mut line).await? {
            LineOutcome::Eof => break,
            LineOutcome::TooLong => {
                writer.write_all(b"500 5.5.2 line too long\r\n").await?;
                break;
            }
            LineOutcome::Line => {}
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
                if data_too_large {
                    data_too_large = false;
                    data_buf = String::new();
                    state = SmtpState::Greeted;
                    writer
                        .write_all(b"552 5.3.4 message size exceeds limit\r\n")
                        .await?;
                    continue;
                }
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
                        writer.write_all(b"451 Temporary failure\r\n").await?;
                    }
                }
                state = SmtpState::Greeted;
                data_buf.clear();
            } else if !data_too_large {
                let unstuffed = if line.starts_with("..") {
                    &line[1..]
                } else {
                    &line
                };
                if data_buf.len().saturating_add(unstuffed.len()) > limits.max_message_bytes {
                    // Over the cap: stop buffering and free what we held. Keep
                    // draining lines until the terminator, then reply 552.
                    data_too_large = true;
                    data_buf = String::new();
                } else {
                    data_buf.push_str(unstuffed);
                }
            }
            continue;
        }

        let cmd = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_uppercase();
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
                if size_param_exceeds(trimmed, limits.max_message_bytes) {
                    "552 5.3.4 message size exceeds limit\r\n".to_string()
                } else {
                    let from = extract_address(trimmed);
                    state = SmtpState::MailFrom(from);
                    "250 OK\r\n".to_string()
                }
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
            "DATA" => match &state {
                SmtpState::RcptTo { from, to } => {
                    state = SmtpState::Data {
                        from: from.clone(),
                        to: to.clone(),
                    };
                    in_data = true;
                    "354 Start mail input; end with <CRLF>.<CRLF>\r\n".to_string()
                }
                _ => "503 Bad sequence\r\n".to_string(),
            },
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
    line.split(':').nth(1).unwrap_or("").trim().to_string()
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

    // --- SIZE / line-cap enforcement ---

    use crate::mail::parser::ParsedMessage;
    use crate::tuta::FolderInfo;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    use tutasdk::entities::generated::tutanota::{Mail, MailDetails, MailSetEntry, TutanotaFile};
    use tutasdk::IdTupleGenerated;

    #[test]
    fn size_param_exceeds_detects_oversize() {
        let max = 26_214_400;
        assert!(size_param_exceeds("MAIL FROM:<a@b.com> SIZE=30000000", max));
        assert!(size_param_exceeds("MAIL FROM:<a@b.com> size=30000000", max)); // case-insensitive
        assert!(!size_param_exceeds("MAIL FROM:<a@b.com> SIZE=1000", max));
        assert!(!size_param_exceeds("MAIL FROM:<a@b.com>", max)); // no SIZE param
        assert!(!size_param_exceeds("MAIL FROM:<a@b.com> SIZE=notanumber", max));
    }

    #[tokio::test]
    async fn read_line_capped_splits_lines_then_eof() {
        let data = b"hello\r\nworld\r\n";
        let mut r = BufReader::new(&data[..]);
        let mut s = String::new();
        assert!(matches!(
            read_line_capped(&mut r, 1000, &mut s).await.unwrap(),
            LineOutcome::Line
        ));
        assert_eq!(s, "hello\r\n");
        assert!(matches!(
            read_line_capped(&mut r, 1000, &mut s).await.unwrap(),
            LineOutcome::Line
        ));
        assert_eq!(s, "world\r\n");
        assert!(matches!(
            read_line_capped(&mut r, 1000, &mut s).await.unwrap(),
            LineOutcome::Eof
        ));
    }

    #[tokio::test]
    async fn read_line_capped_rejects_overlong_line() {
        let data = b"xxxxxxxxxxxxxxxxxxxx\r\n"; // 20 chars before CRLF
        let mut r = BufReader::new(&data[..]);
        let mut s = String::new();
        assert!(matches!(
            read_line_capped(&mut r, 5, &mut s).await.unwrap(),
            LineOutcome::TooLong
        ));
    }

    #[derive(Default)]
    struct CountingBackend {
        sent: AtomicUsize,
    }
    impl CountingBackend {
        fn sent(&self) -> usize {
            self.sent.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl MailBackend for CountingBackend {
        async fn send_mail(&self, _m: &ParsedMessage) -> Result<(), String> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn load_mail_ids_for_folder(
            &self,
            _f: &FolderInfo,
            _l: usize,
        ) -> Result<Vec<Mail>, String> {
            unimplemented!()
        }
        async fn load_mail(&self, _l: &str, _e: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail(&self, _j: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_set_entry(
            &self,
            _j: &str,
        ) -> Result<Option<MailSetEntry>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_details_blob(
            &self,
            _j: &str,
        ) -> Result<Option<MailDetails>, String> {
            unimplemented!()
        }
        async fn load_mail_details(&self, _m: &Mail) -> Result<Option<MailDetails>, String> {
            unimplemented!()
        }
        async fn load_attachments(
            &self,
            _m: &Mail,
        ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, String> {
            unimplemented!()
        }
        async fn list_folders(&self) -> Result<Vec<FolderInfo>, String> {
            unimplemented!()
        }
        async fn set_unread_status(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _u: bool,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn trash_mails(&self, _ids: Vec<IdTupleGenerated>) -> Result<(), String> {
            unimplemented!()
        }
        async fn move_mails(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _t: &FolderInfo,
        ) -> Result<(), String> {
            unimplemented!()
        }
    }

    /// Read whatever the server has written so far (responses are small and
    /// arrive one per command, so a single read lines up with one reply).
    async fn drain(client: &mut tokio::io::DuplexStream) -> String {
        let mut buf = vec![0u8; 8192];
        let n = client.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    async fn run_to_data(client: &mut tokio::io::DuplexStream) {
        assert!(drain(client).await.starts_with("220"));
        client.write_all(b"EHLO test\r\n").await.unwrap();
        assert!(drain(client).await.starts_with("250"));
        client.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
        assert!(drain(client).await.starts_with("250"));
        client.write_all(b"RCPT TO:<c@d.com>\r\n").await.unwrap();
        assert!(drain(client).await.starts_with("250"));
        client.write_all(b"DATA\r\n").await.unwrap();
        assert!(drain(client).await.starts_with("354"));
    }

    #[tokio::test]
    async fn rejects_oversize_message_in_data() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let backend = Arc::new(CountingBackend::default());
        let b = backend.clone();
        let limits = SmtpLimits {
            max_message_bytes: 100,
            max_line_bytes: 65536,
        };
        let h = tokio::spawn(async move {
            let _ = handle_connection(server, b as Arc<dyn MailBackend>, None, limits).await;
        });

        run_to_data(&mut client).await;
        client.write_all(b"Subject: t\r\n\r\n").await.unwrap();
        // one line well over the 100-byte message cap
        client
            .write_all(format!("{}\r\n", "x".repeat(300)).as_bytes())
            .await
            .unwrap();
        client.write_all(b".\r\n").await.unwrap();

        let resp = drain(&mut client).await;
        assert!(resp.starts_with("552"), "expected 552, got {resp:?}");
        assert_eq!(backend.sent(), 0, "oversize message must not be sent");

        client.write_all(b"QUIT\r\n").await.unwrap();
        let _ = h.await;
    }

    #[tokio::test]
    async fn accepts_normal_message() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let backend = Arc::new(CountingBackend::default());
        let b = backend.clone();
        let h = tokio::spawn(async move {
            let _ =
                handle_connection(server, b as Arc<dyn MailBackend>, None, SmtpLimits::default())
                    .await;
        });

        run_to_data(&mut client).await;
        client
            .write_all(b"Subject: hi\r\n\r\nshort body\r\n")
            .await
            .unwrap();
        client.write_all(b".\r\n").await.unwrap();

        let resp = drain(&mut client).await;
        assert!(resp.starts_with("250"), "expected 250, got {resp:?}");
        assert_eq!(backend.sent(), 1, "normal message should be sent once");

        client.write_all(b"QUIT\r\n").await.unwrap();
        let _ = h.await;
    }

    #[tokio::test]
    async fn rejects_oversize_via_mail_size_param() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let backend = Arc::new(CountingBackend::default());
        let b = backend.clone();
        let limits = SmtpLimits {
            max_message_bytes: 100,
            max_line_bytes: 65536,
        };
        let h = tokio::spawn(async move {
            let _ = handle_connection(server, b as Arc<dyn MailBackend>, None, limits).await;
        });

        assert!(drain(&mut client).await.starts_with("220"));
        client.write_all(b"EHLO test\r\n").await.unwrap();
        assert!(drain(&mut client).await.starts_with("250"));
        client
            .write_all(b"MAIL FROM:<a@b.com> SIZE=99999999\r\n")
            .await
            .unwrap();
        let resp = drain(&mut client).await;
        assert!(resp.starts_with("552"), "expected 552, got {resp:?}");
        assert_eq!(backend.sent(), 0);

        client.write_all(b"QUIT\r\n").await.unwrap();
        let _ = h.await;
    }
}
