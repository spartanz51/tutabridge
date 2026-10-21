mod search;
mod session;
mod utf7;

use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::mail::parser::parse_rfc2822;
use crate::store::LocalStore;
use crate::sync::MailStore;
use crate::tuta::MailBackend;
use session::ImapSession;

/// Numbers IMAP connections in acceptance order so log lines from concurrent
/// or overlapping sessions (auth, commands, the eventual disconnect) can be
/// told apart. Purely a log-correlation id — never persisted, never sent to
/// a client.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

pub async fn serve(
    port: u16,
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    local_store: Arc<LocalStore>,
    tls: TlsAcceptor,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = crate::net::bind_local("IMAP", port).await?;
    serve_listener(listener, store, backend, local_store, tls, password_hash).await
}

/// Serve IMAP on a listener the caller has already bound, so a caller that
/// needs to know the port is taken can find out before starting anything else.
pub async fn serve_listener(
    listener: TcpListener,
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    local_store: Arc<LocalStore>,
    tls: TlsAcceptor,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("IMAP server listening on {} (TLS)", listener.local_addr()?);

    crate::net::accept_loop(
        listener,
        "IMAP",
        crate::net::MAX_CONNECTIONS,
        move |stream, addr| {
            let store = store.clone();
            let backend = backend.clone();
            let local_store = local_store.clone();
            let tls = tls.clone();
            let pw_hash = password_hash.clone();
            async move {
                let session_id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
                match tokio::time::timeout(crate::net::HANDSHAKE_TIMEOUT, tls.accept(stream)).await
                {
                    Ok(Ok(tls_stream)) => {
                        info!("IMAP session {session_id}: opened ({addr})");
                        if let Err(e) = handle_connection(
                            tls_stream,
                            session_id,
                            store,
                            backend,
                            local_store,
                            pw_hash,
                        )
                        .await
                        {
                            error!("IMAP session {session_id}: connection error: {e}");
                        }
                    }
                    Ok(Err(e)) => {
                        error!("IMAP session {session_id}: TLS handshake failed ({addr}): {e}")
                    }
                    Err(_) => debug!("IMAP session {session_id}: TLS handshake timed out ({addr})"),
                }
            }
        },
    )
    .await;

    Ok(())
}

/// The client line as it may be logged: a `LOGIN` keeps its tag and verb but
/// loses its arguments, `AUTHENTICATE` loses any initial response, and the
/// continuation line that answers an `AUTHENTICATE` challenge is replaced
/// whole, since it is nothing but the base64 credentials. Every other line is
/// returned as is. The GUI logs at debug by default and the CLI can be asked
/// to, so a mail client logging in must not put the bridge password in the
/// log stream.
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
    session_id: u64,
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    local_store: Arc<LocalStore>,
    password_hash: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut store_watch: watch::Receiver<u64> = store.subscribe();
    let mut session = ImapSession::new(store, backend, password_hash, Some(local_store))
        .with_session_id(session_id);

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
    let session_id = session.session_id();
    let mut line = String::new();
    loop {
        if session.is_idle() {
            line.clear();
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    let n = match result {
                        Ok(n) => n,
                        Err(e) if crate::net::is_benign_disconnect(&e) => {
                            debug!("IMAP session {session_id}: ended, client went away without a clean shutdown while idle: {e}");
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    if n == 0 {
                        info!("IMAP session {session_id}: ended, client disconnected cleanly while idle");
                        break;
                    }
                    let trimmed = line.trim_end();
                    debug!(
                        "IMAP C (idle): {}",
                        redact_credentials(trimmed, session.is_awaiting_auth())
                    );
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
            Err(e) if crate::net::is_benign_disconnect(&e) => {
                debug!("IMAP session {session_id}: ended, client went away without a clean shutdown: {e}");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            info!("IMAP session {session_id}: ended, client disconnected cleanly");
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
            info!("IMAP session {session_id}: ended, client sent LOGOUT");
            break;
        }
    }

    Ok(())
}

/// Largest APPEND message literal we will read into memory.
const MAX_APPEND_BYTES: usize = 26_214_400;

/// Handle an `APPEND`.
///
/// Tuta saves sent mail server-side, so an APPEND to the Sent folder is a
/// no-op: read and discard the literal, reply OK. That lets a client's "save a
/// copy to Sent" succeed without creating a duplicate (the real copy arrives
/// via the syncer).
///
/// An APPEND to Drafts is saved as a real draft: the literal is parsed and
/// posted to Tuta's draft service, and the reply names the new draft's UID, so
/// the client can delete the copy it autosaved before instead of leaving one
/// behind on every save.
///
/// Every other folder is rejected before the literal is sent, so the client
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
    let target = session.append_target(&req.mailbox).await;
    if let session::AppendTarget::Unsupported = target {
        reply(
            writer,
            format!(
                "{} NO [CANNOT] APPEND is only supported for the Sent and Drafts folders\r\n",
                req.tag
            ),
        )
        .await?;
        return Ok(());
    }
    if req.literal_size > MAX_APPEND_BYTES {
        reply(writer, format!("{} NO message too large\r\n", req.tag)).await?;
        return Ok(());
    }

    // Synchronizing literal: tell the client to send the message, then read it
    // plus the trailing CRLF.
    writer.write_all(b"+ OK\r\n").await?;
    writer.flush().await?;
    let literal = read_literal(reader, req.literal_size).await?;

    if let session::AppendTarget::Drafts = target {
        let msg = parse_rfc2822(&literal);
        let resp = match session.create_draft(&msg).await {
            Ok(uid) => append_ok(&req.tag, uid),
            Err(e) => {
                warn!(
                    "IMAP APPEND to {}: saving the draft failed: {e}",
                    req.mailbox
                );
                format!(
                    "{} NO [SERVERBUG] the draft could not be saved\r\n",
                    req.tag
                )
            }
        };
        reply(writer, resp).await?;
        return Ok(());
    }

    // Sent: the server makes that copy itself.
    reply(writer, append_ok(&req.tag, None)).await?;
    Ok(())
}

/// The tagged reply to a successful `APPEND`. `APPENDUID` is only present when
/// the bridge knows the new draft's UID: RFC 4315 requires the code when UIDPLUS
/// is advertised, but a client that gets none is better off than one that is
/// told a UID that does not exist.
fn append_ok(tag: &str, uid: Option<u32>) -> String {
    match uid {
        Some(uid) => format!(
            "{tag} OK [APPENDUID {} {uid}] APPEND completed\r\n",
            session::UIDVALIDITY
        ),
        None => format!("{tag} OK APPEND completed\r\n"),
    }
}

/// Read an `APPEND` message literal and the CRLF that follows it. A MIME
/// message is not necessarily UTF-8, so invalid bytes are replaced rather than
/// rejected: the parser needs text, and a draft with one odd byte beats a
/// failed save.
async fn read_literal<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    size: usize,
) -> std::io::Result<String> {
    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf).await?;
    let mut tail = String::new();
    reader.read_line(&mut tail).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn reply<W: AsyncWriteExt + Unpin>(writer: &mut W, resp: String) -> std::io::Result<()> {
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mail::parser::ParsedMessage;
    use crate::tuta::FolderInfo;
    use tokio::io::AsyncReadExt;
    use tutabridge_tuta::folder_system::MailSetKind;
    use tutasdk::entities::generated::tutanota::{Mail, MailDetails, MailSetEntry, TutanotaFile};
    use tutasdk::IdTupleGenerated;

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

    #[derive(Default)]
    struct NoopBackend {
        created_drafts: std::sync::Mutex<Vec<ParsedMessage>>,
    }
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
        ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, tutabridge_tuta::mail::AttachmentError> {
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
        async fn create_draft(&self, msg: &ParsedMessage) -> Result<IdTupleGenerated, String> {
            self.created_drafts.lock().unwrap().push(msg.clone());
            Ok(IdTupleGenerated::new(
                tutasdk::GeneratedId("drafts".to_string()),
                tutasdk::GeneratedId("draft-1".to_string()),
            ))
        }
    }

    #[test]
    fn session_counter_yields_unique_increasing_ids() {
        let a = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let b = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        assert_ne!(
            a, b,
            "concurrent connections must not share a log-correlation id"
        );
        assert!(
            b > a,
            "ids must keep increasing so the newest connection is easy to spot in logs"
        );
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
        ImapSession::new(store, Arc::new(NoopBackend::default()), None, None)
    }

    async fn session_with_drafts() -> (ImapSession, Arc<NoopBackend>) {
        let store = MailStore::new();
        let drafts = FolderInfo {
            id: "drafts".into(),
            list_id: "folders".into(),
            entries_list_id: "drafts_entries".into(),
            kind: MailSetKind::Draft,
            imap_path: "Drafts".into(),
            special_use: Some("\\Drafts".into()),
        };
        store.set_folder_list(vec![drafts]).await;
        let backend = Arc::new(NoopBackend::default());
        (
            ImapSession::new(store, backend.clone(), None, None),
            backend,
        )
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
            std::io::ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        )));
        let reader = ScriptedReader { script };
        let mut reader = BufReader::new(reader);
        let mut writer = tokio::io::sink();
        let store = MailStore::new();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = ImapSession::new(store, Arc::new(NoopBackend::default()), None, None);

        let result =
            run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

        assert!(
            result.is_ok(),
            "a close_notify-less disconnect must end the session quietly, not as an error: {result:?}"
        );
    }

    #[tokio::test]
    async fn connection_loop_ends_quietly_when_client_resets_after_login() {
        // The other way a one-shot client goes away: it exits with the
        // server's reply still unread, so the kernel answers with a reset
        // rather than a FIN. Same outcome expected: no error.
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(b"a1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
        script.push_back(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "Connection reset by peer (os error 54)",
        )));
        let mut reader = BufReader::new(ScriptedReader { script });
        let mut writer = tokio::io::sink();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = ImapSession::new(
            MailStore::new(),
            Arc::new(NoopBackend::default()),
            None,
            None,
        );

        let result =
            run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

        assert!(
            result.is_ok(),
            "a reset must end the session quietly: {result:?}"
        );
    }

    #[tokio::test]
    async fn a_real_read_error_still_ends_the_connection_as_an_error() {
        // Only a peer going away is benign. Anything else the stream reports
        // must keep surfacing, or a broken TLS record would be logged as a
        // quiet disconnect.
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(b"a1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
        script.push_back(Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "received corrupt message",
        )));
        let mut reader = BufReader::new(ScriptedReader { script });
        let mut writer = tokio::io::sink();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = ImapSession::new(
            MailStore::new(),
            Arc::new(NoopBackend::default()),
            None,
            None,
        );

        let result =
            run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

        assert!(
            result.is_err(),
            "a corrupt stream must not pass for a disconnect"
        );
    }

    #[tokio::test]
    async fn while_idle_a_disconnect_ends_quietly_and_a_real_error_does_not() {
        // The IDLE branch has its own read site with the same two outcomes.
        for (kind, expect_ok) in [
            (std::io::ErrorKind::UnexpectedEof, true),
            (std::io::ErrorKind::ConnectionReset, true),
            (std::io::ErrorKind::InvalidData, false),
        ] {
            let mut script = std::collections::VecDeque::new();
            script.push_back(Ok(b"a1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
            script.push_back(Ok(b"a2 SELECT Sent\r\n".to_vec()));
            script.push_back(Ok(b"a3 IDLE\r\n".to_vec()));
            script.push_back(Err(std::io::Error::new(kind, "while idling")));
            let mut reader = BufReader::new(ScriptedReader { script });
            let mut writer = tokio::io::sink();
            let (_watch_tx, mut store_watch) = watch::channel(0u64);
            let mut session = session_with_sent().await;

            let result =
                run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

            assert!(
                session.is_idle(),
                "the script must have reached IDLE for this to test the idle read site"
            );
            assert_eq!(result.is_ok(), expect_ok, "{kind:?} while idle: {result:?}");
        }
    }

    #[tokio::test]
    async fn an_append_literal_cut_short_is_still_an_error() {
        // The one place a mid-stream EOF means a truncated message: the client
        // announced a 10-byte literal and vanished. This must not be filed
        // under "went away quietly".
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(b"a1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
        script.push_back(Ok(b"a2 APPEND Sent {10}\r\n".to_vec()));
        let mut reader = BufReader::new(ScriptedReader { script });
        let mut writer = tokio::io::sink();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = session_with_sent().await;

        let result =
            run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch).await;

        assert!(
            result.is_err(),
            "a truncated APPEND literal must surface: {result:?}"
        );
    }

    #[tokio::test]
    async fn the_command_log_never_carries_a_login_password() {
        // Drives the real connection loop and reads the real logger output:
        // this is what proves the redaction sits at the emission site and
        // not only in a helper. The capture is scoped to this test's thread;
        // the marker is the one string that must never show up in it.
        crate::net::log_capture::install();
        let marker = "PW-a1b2c3-main-loop";
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(
            format!("m1 LOGIN \"user\" \"{marker}\"\r\n").into_bytes()
        ));
        script.push_back(Ok(b"m2 LOGOUT\r\n".to_vec()));
        let mut reader = BufReader::new(ScriptedReader { script });
        let mut writer = tokio::io::sink();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = ImapSession::new(
            MailStore::new(),
            Arc::new(NoopBackend::default()),
            None,
            None,
        );

        run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch)
            .await
            .unwrap();

        assert!(
            crate::net::log_capture::lines_containing(marker).is_empty(),
            "the password reached the log"
        );
        assert!(
            !crate::net::log_capture::lines_containing("IMAP C: m1 LOGIN <credentials>").is_empty(),
            "the LOGIN line should be logged, redacted"
        );
        // The script was consumed as written, down to the LOGOUT.
        assert!(!crate::net::log_capture::lines_containing("IMAP C: m2 LOGOUT").is_empty());
    }

    #[tokio::test]
    async fn the_idle_command_log_never_carries_a_login_password() {
        // A client is not supposed to send anything but DONE while idling; one
        // that sends a LOGIN anyway must not have it echoed in clear.
        crate::net::log_capture::install();
        let marker = "PW-d4e5f6-idle-loop";
        let mut script = std::collections::VecDeque::new();
        script.push_back(Ok(b"i1 LOGIN \"user\" \"pass\"\r\n".to_vec()));
        script.push_back(Ok(b"i2 SELECT Sent\r\n".to_vec()));
        script.push_back(Ok(b"i3 IDLE\r\n".to_vec()));
        script.push_back(Ok(
            format!("i4 LOGIN \"user\" \"{marker}\"\r\n").into_bytes()
        ));
        script.push_back(Ok(b"DONE\r\n".to_vec()));
        script.push_back(Ok(b"i5 LOGOUT\r\n".to_vec()));
        let mut reader = BufReader::new(ScriptedReader { script });
        let mut writer = tokio::io::sink();
        let (_watch_tx, mut store_watch) = watch::channel(0u64);
        let mut session = session_with_sent().await;

        run_connection_loop(&mut reader, &mut writer, &mut session, &mut store_watch)
            .await
            .unwrap();

        assert!(
            crate::net::log_capture::lines_containing(marker).is_empty(),
            "the password reached the log through the idle echo"
        );
        assert!(
            !crate::net::log_capture::lines_containing("IMAP C (idle): i4 LOGIN <credentials>")
                .is_empty(),
            "the idle LOGIN line should be logged, redacted"
        );
        // The script was consumed as written: IDLE was entered (the LOGIN
        // above went through the idle echo), DONE left it, and LOGOUT ran.
        assert!(!crate::net::log_capture::lines_containing("IMAP C: i5 LOGOUT").is_empty());
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

    #[test]
    fn append_ok_offers_the_uid_only_when_it_is_known() {
        assert_eq!(
            append_ok("a1", Some(17)),
            "a1 OK [APPENDUID 1 17] APPEND completed\r\n"
        );
        assert_eq!(append_ok("a1", None), "a1 OK APPEND completed\r\n");
    }

    #[tokio::test]
    async fn append_to_non_sent_is_rejected_without_continuation() {
        let session = session_with_sent().await;
        let (mut client, server) = tokio::io::duplex(4096);
        let (sr, mut sw) = tokio::io::split(server);
        let mut reader = BufReader::new(sr);
        let req = session::AppendRequest {
            tag: "b2".into(),
            mailbox: "INBOX".into(),
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

    #[tokio::test]
    async fn append_to_drafts_saves_the_message_as_a_draft() {
        let (session, backend) = session_with_drafts().await;
        let (mut client, server) = tokio::io::duplex(4096);
        let (sr, mut sw) = tokio::io::split(server);
        let mut reader = BufReader::new(sr);
        let literal = b"From: Me <me@tuta.com>\r\nTo: You <you@example.com>\r\nSubject: a draft\r\n\r\nbody text\r\n"
            .to_vec();
        let req = session::AppendRequest {
            tag: "d1".into(),
            mailbox: "Drafts".into(),
            literal_size: literal.len(),
        };

        let server_fut = handle_append(&mut reader, &mut sw, &session, req);
        let client_fut = async {
            let mut buf = [0u8; 32];
            let n = client.read(&mut buf).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with('+'),
                "expected a continuation request"
            );
            client.write_all(&literal).await.unwrap();
            client.write_all(b"\r\n").await.unwrap();
            let mut resp = vec![0u8; 128];
            let n = client.read(&mut resp).await.unwrap();
            String::from_utf8_lossy(&resp[..n]).into_owned()
        };
        let (res, resp) = tokio::join!(server_fut, client_fut);
        res.unwrap();

        // The store never sees the draft in this test, so the reply carries no
        // APPENDUID; that is the whole point of it being optional.
        assert!(resp.contains("d1 OK APPEND completed"), "got {resp:?}");
        let created = backend.created_drafts.lock().unwrap();
        assert_eq!(created.len(), 1, "the literal must become one draft");
        assert_eq!(created[0].subject, "a draft");
        assert_eq!(
            created[0].to,
            vec![("You".to_string(), "you@example.com".to_string())]
        );
    }
}
