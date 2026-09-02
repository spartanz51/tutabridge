mod search;
mod session;
mod utf7;

use log::{debug, error, info};
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::store::LocalStore;
use crate::sync::MailStore;
use crate::tuta::MailBackend;
use session::ImapSession;

pub async fn serve(
    port: u16,
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    local_store: Arc<LocalStore>,
    tls: TlsAcceptor,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
    info!("IMAP server listening on 127.0.0.1:{} (TLS)", port);

    crate::net::accept_loop(
        listener,
        "IMAP",
        crate::net::MAX_CONNECTIONS,
        move |stream, _addr| {
            let store = store.clone();
            let backend = backend.clone();
            let local_store = local_store.clone();
            let tls = tls.clone();
            let pw_hash = password_hash.clone();
            async move {
                match tokio::time::timeout(crate::net::HANDSHAKE_TIMEOUT, tls.accept(stream)).await
                {
                    Ok(Ok(tls_stream)) => {
                        if let Err(e) =
                            handle_connection(tls_stream, store, backend, local_store, pw_hash)
                                .await
                        {
                            error!("IMAP connection error: {}", e);
                        }
                    }
                    Ok(Err(e)) => error!("IMAP TLS handshake failed: {}", e),
                    Err(_) => debug!("IMAP TLS handshake timed out"),
                }
            }
        },
    )
    .await;

    Ok(())
}

/// True for the io::Error rustls reports when a peer closes the TCP
/// connection without sending a TLS close_notify alert first. Under TLS 1.3's
/// AEAD framing this can't hide a truncated response the way it could
/// pre-1.3, and short-lived clients that open a connection, run one command
/// and exit (as most non-interactive IMAP CLI tools do) routinely skip the
/// clean shutdown. Treating it as an ordinary EOF keeps that common,
/// harmless case out of the error log.
fn is_close_notify_eof(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::UnexpectedEof
}

/// The client line as it may be logged: a `LOGIN` keeps its tag and verb but
/// loses its arguments, `AUTHENTICATE` loses any initial response, and the
/// continuation line that answers an `AUTHENTICATE` challenge is replaced
/// whole, since it is nothing but the base64 credentials. Every other line is
/// returned as is. The GUI shows the log on screen, so a mail client logging
/// in must not put the bridge password there.
fn redact_credentials(line: &str, awaiting_auth: bool) -> std::borrow::Cow<'_, str> {
    if awaiting_auth {
        return "<credentials>".into();
    }
    let mut words = line.split_whitespace();
    let (Some(tag), Some(verb)) = (words.next(), words.next()) else {
        return line.into();
    };
    if verb.eq_ignore_ascii_case("LOGIN") {
        return format!("{tag} {verb} <credentials>").into();
    }
    if verb.eq_ignore_ascii_case("AUTHENTICATE") {
        if let (Some(mechanism), Some(_initial_response)) = (words.next(), words.next()) {
            return format!("{tag} {verb} {mechanism} <credentials>").into();
        }
    }
    line.into()
}

async fn handle_connection(
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    local_store: Arc<LocalStore>,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut store_watch: watch::Receiver<u64> = store.subscribe();
    let mut session = ImapSession::new(store, backend, password_hash, Some(local_store));

    writer
        .write_all(b"* OK TutaBridge IMAP4rev1 ready\r\n")
        .await?;
    writer.flush().await?;

    run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await
}

/// The command loop shared by every IMAP connection, kept generic over the
/// stream so it can run against a `tokio::io::duplex` in tests without a real
/// TLS handshake — `handle_connection` supplies the concrete `TlsStream`.
async fn run_connection_loop<R, W>(
    reader: &mut R,
    writer: &mut W,
    session: &mut ImapSession,
    store_watch: &mut watch::Receiver<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut line = String::new();
    loop {
        if session.is_idle() {
            line.clear();
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    let n = match result {
                        Ok(n) => n,
                        Err(e) if is_close_notify_eof(&e) => {
                            debug!("IMAP client disconnected without a TLS close_notify while idle: {e}");
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    };
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
                        writer.flush().await?;
                    }
                }
                _ = store_watch.changed() => {
                    let updates = session.check_new_mail().await;
                    for resp in &updates {
                        debug!("IMAP S (store update): {}", resp.trim_end());
                        writer.write_all(resp.as_bytes()).await?;
                    }
                    writer.flush().await?;
                }
            }
            continue;
        }

        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) if is_close_notify_eof(&e) => {
                debug!("IMAP client disconnected without a TLS close_notify: {e}");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            break;
        }

        let trimmed = line.trim_end();
        debug!(
            "IMAP C: {}",
            redact_credentials(trimmed, session.is_awaiting_auth())
        );

        // APPEND carries a message literal the line-based session layer cannot
        // read, so it is handled here at the socket level.
        if !session.is_awaiting_auth() {
            if let Some(req) = session::parse_append(trimmed) {
                handle_append(reader, writer, session, req).await?;
                continue;
            }
        }

        let responses = if session.is_awaiting_auth() {
            session.handle_auth_response(trimmed)
        } else {
            session.handle_command(trimmed).await
        };
        for resp in &responses {
            debug!("IMAP S: {}", resp.trim_end());
            writer.write_all(resp.as_bytes()).await?;
        }
        writer.flush().await?;

        if session.is_logout() {
            break;
        }
    }

    Ok(())
}

/// Largest APPEND message literal we will read into memory.
const MAX_APPEND_BYTES: usize = 26_214_400;

/// Handle an `APPEND`. Tuta saves sent mail server-side, so an APPEND to the
/// Sent folder is accepted as a no-op: read and discard the literal, reply OK.
/// That lets a mail client's "save a copy to Sent" succeed without creating a
/// duplicate (the real copy arrives via the syncer). Other folders are not
/// supported yet and are rejected before the literal is sent, so the client
/// aborts the synchronizing literal and the stream stays in sync.
async fn handle_append<R, W>(
    reader: &mut R,
    writer: &mut W,
    session: &ImapSession,
    req: session::AppendRequest,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    if !session.append_targets_sent(&req.mailbox).await {
        let resp = format!(
            "{} NO [CANNOT] APPEND is only supported for the Sent folder; Tuta saves sent mail automatically\r\n",
            req.tag
        );
        writer.write_all(resp.as_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }
    if req.literal_size > MAX_APPEND_BYTES {
        let resp = format!("{} NO message too large\r\n", req.tag);
        writer.write_all(resp.as_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }

    // Synchronizing literal: tell the client to send the message, then read and
    // discard it plus the trailing CRLF (the real Sent copy comes from sync).
    writer.write_all(b"+ OK\r\n").await?;
    writer.flush().await?;
    let mut buf = vec![0u8; req.literal_size];
    reader.read_exact(&mut buf).await?;
    let mut tail = String::new();
    reader.read_line(&mut tail).await?;

    let resp = format!("{} OK APPEND completed\r\n", req.tag);
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_arguments_are_redacted() {
        assert_eq!(
            redact_credentials(r#"a1 LOGIN "me@tuta.com" "s3cret-pw""#, false),
            "a1 LOGIN <credentials>"
        );
        assert_eq!(
            redact_credentials("a1 login me@tuta.com s3cret-pw", false),
            "a1 login <credentials>"
        );
    }

    #[test]
    fn authenticate_challenge_response_is_redacted_whole() {
        // The bare `AUTHENTICATE PLAIN` carries nothing; the next line is the
        // base64 of \0user\0password and must not reach the log.
        assert_eq!(
            redact_credentials("a1 AUTHENTICATE PLAIN", false),
            "a1 AUTHENTICATE PLAIN"
        );
        assert_eq!(
            redact_credentials("AG1lQHR1dGEuY29tAHMzY3JldC1wdw==", true),
            "<credentials>"
        );
    }

    #[test]
    fn authenticate_initial_response_is_redacted() {
        // SASL-IR (RFC 4959): a client may send the credentials inline.
        assert_eq!(
            redact_credentials(
                "a1 AUTHENTICATE PLAIN AG1lQHR1dGEuY29tAHMzY3JldC1wdw==",
                false
            ),
            "a1 AUTHENTICATE PLAIN <credentials>"
        );
    }

    #[test]
    fn other_commands_are_logged_verbatim() {
        for line in ["a2 SELECT INBOX", "a3 FETCH 1:* (FLAGS)", "DONE", ""] {
            assert_eq!(redact_credentials(line, false), line);
        }
    }
    use crate::mail::parser::ParsedMessage;
    use crate::tuta::FolderInfo;
    use tokio::io::AsyncReadExt;
    use tutasdk::entities::generated::tutanota::{Mail, MailDetails, MailSetEntry, TutanotaFile};
    use tutasdk::folder_system::MailSetKind;
    use tutasdk::IdTupleGenerated;

    struct NoopBackend;
    #[async_trait::async_trait]
    impl MailBackend for NoopBackend {
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
        async fn send_mail(&self, _m: &ParsedMessage) -> Result<(), String> {
            unimplemented!()
        }
    }

    async fn session_with_sent() -> ImapSession {
        let store = MailStore::new();
        let sent = FolderInfo {
            id: "sent".into(),
            list_id: "folders".into(),
            entries_list_id: "se".into(),
            kind: MailSetKind::Sent,
            imap_path: "Sent".into(),
            special_use: Some("\\Sent".into()),
        };
        store.set_folder_list(vec![sent]).await;
        ImapSession::new(store, Arc::new(NoopBackend), None, None)
    }

    #[test]
    fn close_notify_eof_is_recognized() {
        let e = std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        );
        assert!(is_close_notify_eof(&e));
    }

    #[test]
    fn other_io_errors_are_not_treated_as_close_notify() {
        let e = std::io::Error::new(ErrorKind::ConnectionReset, "connection reset by peer");
        assert!(!is_close_notify_eof(&e));
    }

    /// Replays a fixed script of reads: each entry is either a chunk of bytes
    /// or an error to return, one per `poll_read` call. Lets a test drive the
    /// connection loop through a scenario a real socket can produce (like a
    /// close_notify-less disconnect) without a TLS handshake.
    struct ScriptedReader {
        script: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl tokio::io::AsyncRead for ScriptedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.script.pop_front() {
                Some(Ok(chunk)) => {
                    buf.put_slice(&chunk);
                    std::task::Poll::Ready(Ok(()))
                }
                Some(Err(e)) => std::task::Poll::Ready(Err(e)),
                None => std::task::Poll::Ready(Ok(())), // EOF: no more scripted reads
            }
        }
    }

    #[tokio::test]
    async fn connection_loop_ends_quietly_when_client_vanishes_after_login() {
        // Mirrors the scenario that used to log a false-alarm ERROR: a client
        // logs in and then just disappears (no LOGOUT, no TLS close_notify) —
        // e.g. a one-shot CLI tool that runs a single command and exits.
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(b"a1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
        script.push_back(Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        )));
        let reader = ScriptedReader { script };
        let mut reader = BufReader::new(reader);
        let mut writer = tokio::io::sink();
        let store = MailStore::new();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = ImapSession::new(store, Arc::new(NoopBackend), None, None);

        let result =
            run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

        assert!(
            result.is_ok(),
            "a close_notify-less disconnect must end the session quietly, not as an error: {result:?}"
        );
    }

    #[tokio::test]
    async fn append_to_sent_reads_literal_and_returns_ok() {
        let session = session_with_sent().await;
        let (mut client, server) = tokio::io::duplex(4096);
        let (sr, mut sw) = tokio::io::split(server);
        let mut reader = BufReader::new(sr);
        let req = session::AppendRequest {
            tag: "a1".into(),
            mailbox: "Sent".into(),
            literal_size: 5,
        };

        let server_fut = handle_append(&mut reader, &mut sw, &session, req);
        let client_fut = async {
            let mut buf = [0u8; 32];
            let n = client.read(&mut buf).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with('+'),
                "expected a continuation request"
            );
            client.write_all(b"hello\r\n").await.unwrap();
            let mut resp = vec![0u8; 128];
            let n = client.read(&mut resp).await.unwrap();
            String::from_utf8_lossy(&resp[..n]).into_owned()
        };
        let (res, resp) = tokio::join!(server_fut, client_fut);
        res.unwrap();
        assert!(resp.contains("a1 OK APPEND completed"), "got {resp:?}");
    }

    #[tokio::test]
    async fn append_to_non_sent_is_rejected_without_continuation() {
        let session = session_with_sent().await;
        let (mut client, server) = tokio::io::duplex(4096);
        let (sr, mut sw) = tokio::io::split(server);
        let mut reader = BufReader::new(sr);
        let req = session::AppendRequest {
            tag: "b2".into(),
            mailbox: "Drafts".into(),
            literal_size: 5,
        };

        let server_fut = handle_append(&mut reader, &mut sw, &session, req);
        let client_fut = async {
            let mut resp = vec![0u8; 128];
            let n = client.read(&mut resp).await.unwrap();
            String::from_utf8_lossy(&resp[..n]).into_owned()
        };
        let (res, resp) = tokio::join!(server_fut, client_fut);
        res.unwrap();
        assert!(resp.contains("b2 NO"), "got {resp:?}");
        assert!(
            !resp.contains('+'),
            "a rejected folder must not get a continuation"
        );
    }
}
