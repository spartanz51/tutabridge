//! Shared connection-accept loop for the IMAP and SMTP servers.
//!
//! Both servers used to inline `loop { listener.accept().await? }`, which had
//! two problems: a single transient `accept()` error (EMFILE, ECONNABORTED, …)
//! propagated out and killed the server for good, and there was no bound on the
//! number of concurrent connections. This loop fixes both and is transport
//! agnostic so it can be unit-tested without TLS.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Peer protocol failures are expected on public TLS ports. Keep internal
/// TLS/provider errors visible rather than lowering every handshake failure.
pub(crate) fn tls_failure_level(error: &std::io::Error) -> log::Level {
    use tokio_rustls::rustls::Error;
    if is_benign_disconnect(error) {
        return log::Level::Debug;
    }
    match error.get_ref().and_then(|e| e.downcast_ref::<Error>()) {
        Some(
            Error::InvalidMessage(_)
            | Error::InappropriateMessage { .. }
            | Error::InappropriateHandshakeMessage { .. },
        ) => log::Level::Debug,
        Some(Error::AlertReceived(_) | Error::PeerIncompatible(_)) => log::Level::Warn,
        _ => log::Level::Error,
    }
}

/// Max concurrent client connections per server. Bounds file descriptors and
/// memory if a client (or a port scanner) opens connections faster than they
/// close.
pub(crate) const MAX_CONNECTIONS: usize = 64;

/// How long a client has to complete the TLS handshake before being dropped.
/// Stops a connection that opens but never negotiates from parking a task and
/// a file descriptor forever.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Bind `service`'s listener on the configured address. The error names the
/// service and address, and says what to do when another application holds it: the
/// GUI shows it as the bridge status, the CLI prints it on exit.
pub(crate) async fn bind(service: &str, address: SocketAddr) -> Result<TcpListener, String> {
    TcpListener::bind(address)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "{service} address {address} is already in use by another application; \
                 choose a different {service} port"
            ),
            _ => format!("Cannot listen for {service} on {address}: {e}"),
        })
}

/// MCP is intentionally independent of the configurable IMAP/SMTP address.
pub(crate) async fn bind_local(service: &str, port: u16) -> Result<TcpListener, String> {
    bind(service, SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await
}

/// True for the read errors a peer produces by going away without a clean
/// TLS shutdown: a TCP EOF with no close_notify, which rustls reports as
/// `UnexpectedEof`, or a TCP reset, which the kernel sends when the peer
/// closes with data still unread (`ConnectionReset`). One-shot mail clients
/// exit both ways all the time.
///
/// rustls's own guidance (its manual, "Unexpected EOF") is that a protocol
/// whose messages carry their own length framing can treat this like an
/// ordinary EOF, since nothing is ever delimited by the close of the
/// connection and so nothing can be truncated unnoticed. IMAP (CRLF lines and
/// `{n}` literals) and SMTP (CRLF lines and a `.`-terminated DATA) both do.
/// The one place a mid-stream EOF does mean a truncated message, an IMAP
/// literal cut short, keeps propagating it as the error it is.
pub(crate) fn is_benign_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
    )
}

/// A `log::Log` that keeps every record from this crate so a test can assert
/// on what a code path logged, which is the only way to prove a log line was
/// redacted at its emission site rather than in a helper nobody calls.
///
/// The logger is process-global and can be installed once, so it is shared by
/// every test module. Each record remembers the thread that emitted it and a
/// test only ever reads its own: tests run in parallel, each on its own
/// thread, on a `current_thread` runtime, so nothing another test logs can
/// satisfy or spoil an assertion here.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::sync::{Mutex, Once};
    use std::thread::{self, ThreadId};

    static RECORDS: Mutex<Vec<(ThreadId, String)>> = Mutex::new(Vec::new());
    static INSTALL: Once = Once::new();

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            if record.target().starts_with("tutabridge_core") {
                crate::util::lock_recover(&RECORDS)
                    .push((thread::current().id(), record.args().to_string()));
            }
        }
        fn flush(&self) {}
    }

    static CAPTURE: Capture = Capture;

    /// Install the capturing logger (idempotent) at debug level.
    pub(crate) fn install() {
        INSTALL.call_once(|| {
            log::set_logger(&CAPTURE).expect("no other logger is installed in tests");
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    /// Every record the calling thread logged that contains `needle`.
    pub(crate) fn lines_containing(needle: &str) -> Vec<String> {
        let me = thread::current().id();
        crate::util::lock_recover(&RECORDS)
            .iter()
            .filter(|(t, l)| *t == me && l.contains(needle))
            .map(|(_, l)| l.clone())
            .collect()
    }
}

/// Accept connections forever, handing each to `handle` on its own task.
///
/// Robust by construction:
/// * a failed `accept()` is logged and retried after a short backoff instead of
///   returning, so a transient OS error cannot take the listener down;
/// * at most `max_conns` connections run at once — the loop waits for a free
///   slot before accepting the next, applying backpressure.
pub(crate) async fn accept_loop<F, Fut>(
    listener: TcpListener,
    label: &str,
    max_conns: usize,
    handle: F,
) where
    F: Fn(TcpStream, SocketAddr) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let sem = Arc::new(Semaphore::new(max_conns));
    loop {
        // Reserve a slot before accepting, so we never run more than
        // `max_conns` connections concurrently.
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed: never happens here
        };
        match listener.accept().await {
            Ok((stream, addr)) => {
                debug!("{label} connection from {addr}");
                let fut = handle(stream, addr);
                tokio::spawn(async move {
                    let _permit = permit; // released when the connection ends
                    fut.await;
                });
            }
            Err(e) => {
                drop(permit);
                error!("{label} accept failed (retrying): {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn tls_alerts_and_internal_errors_are_not_silenced() {
        use std::io::{Error, ErrorKind};
        use tokio_rustls::rustls::{self, AlertDescription, InvalidMessage};
        for (error, expected) in [
            (Error::from(ErrorKind::UnexpectedEof), log::Level::Debug),
            (Error::from(ErrorKind::ConnectionReset), log::Level::Debug),
            (
                Error::new(
                    ErrorKind::InvalidData,
                    rustls::Error::InvalidMessage(InvalidMessage::MessageTooShort),
                ),
                log::Level::Debug,
            ),
            (
                Error::new(
                    ErrorKind::InvalidData,
                    rustls::Error::AlertReceived(AlertDescription::UnknownCA),
                ),
                log::Level::Warn,
            ),
            (
                Error::new(
                    ErrorKind::InvalidData,
                    rustls::Error::General("provider failure".into()),
                ),
                log::Level::Error,
            ),
            (
                Error::new(ErrorKind::InvalidData, "unknown invalid data"),
                log::Level::Error,
            ),
        ] {
            assert_eq!(tls_failure_level(&error), expected);
        }
    }

    #[test]
    fn captured_records_are_scoped_to_the_thread_that_logged_them() {
        // The whole point of the capture: what another thread logs is
        // invisible here, and what this thread logs is not visible there.
        super::log_capture::install();
        let here = "CAPTURE-SCOPE-here";
        let there = "CAPTURE-SCOPE-there";
        log::debug!("{here}");
        let seen_there = std::thread::spawn(move || {
            log::debug!("{there}");
            (
                super::log_capture::lines_containing(there).len(),
                super::log_capture::lines_containing(here).len(),
            )
        })
        .join()
        .unwrap();
        assert_eq!(
            seen_there,
            (1, 0),
            "the other thread sees only its own record"
        );
        assert_eq!(super::log_capture::lines_containing(here).len(), 1);
        assert!(super::log_capture::lines_containing(there).is_empty());
    }

    #[tokio::test]
    async fn binding_a_taken_port_names_the_service_and_the_address() {
        // Hold a port the way a local mail catcher holds 1025.
        let squatter = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = squatter.local_addr().unwrap().port();

        let err = bind_local("SMTP", port).await.unwrap_err();

        assert_eq!(
            err,
            format!(
                "SMTP address 127.0.0.1:{port} is already in use by another application; \
                 choose a different SMTP port"
            )
        );
    }

    #[tokio::test]
    async fn binding_a_free_port_succeeds_on_loopback() {
        let listener = bind_local("MCP", 0).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn mail_listener_uses_the_requested_address() {
        for ip in [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED] {
            let requested = SocketAddr::from((ip, 0));
            let listener = bind("IMAP", requested).await.unwrap();
            assert_eq!(listener.local_addr().unwrap().ip(), ip);
            assert_ne!(listener.local_addr().unwrap().port(), 0);
        }
    }

    #[tokio::test]
    async fn mcp_loopback_binding_does_not_follow_mail_bind_address() {
        let config = crate::config::Config {
            bind_addr: Ipv4Addr::UNSPECIFIED.into(),
            imap_port: 0,
            mcp_port: 0,
            ..crate::config::Config::default()
        };
        let mail = bind("IMAP", config.imap_addr()).await.unwrap();
        let mcp = bind_local("MCP", config.mcp_port).await.unwrap();
        assert_eq!(mail.local_addr().unwrap().ip(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(mcp.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn a_peer_that_vanishes_is_a_benign_disconnect() {
        use std::io::{Error, ErrorKind};
        // rustls: TCP EOF with no close_notify.
        assert!(is_benign_disconnect(&Error::new(
            ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify"
        )));
        // The kernel: peer closed with data still unread.
        assert!(is_benign_disconnect(&Error::new(
            ErrorKind::ConnectionReset,
            "Connection reset by peer (os error 54)"
        )));
    }

    #[test]
    fn a_real_stream_error_is_not() {
        use std::io::{Error, ErrorKind};
        assert!(!is_benign_disconnect(&Error::new(
            ErrorKind::InvalidData,
            "received corrupt message"
        )));
        assert!(!is_benign_disconnect(&Error::from(ErrorKind::Other)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loop_keeps_accepting_across_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();

        tokio::spawn(async move {
            accept_loop(listener, "TEST", 64, move |_stream, _addr| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        });

        // Three sequential connections; the loop must handle all of them
        // (a non-robust `accept().await?` would have served at most one).
        for _ in 0..3 {
            let mut s = TcpStream::connect(addr).await.unwrap();
            let _ = s.shutdown().await;
        }

        for _ in 0..100 {
            if count.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "every connection must be accepted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loop_caps_concurrent_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            accept_loop(listener, "TEST", 2, move |_stream, _addr| {
                let tx = entered_tx.clone();
                async move {
                    let _ = tx.send(());
                    // Hold the slot open so concurrency stays pinned at the cap.
                    std::future::pending::<()>().await;
                }
            })
            .await;
        });

        // Keep three connections open simultaneously.
        let mut conns = Vec::new();
        for _ in 0..3 {
            conns.push(TcpStream::connect(addr).await.unwrap());
        }

        // Exactly two handlers may start (cap == 2).
        entered_rx.recv().await.unwrap();
        entered_rx.recv().await.unwrap();
        // The third must not start until a slot frees.
        let third = tokio::time::timeout(Duration::from_millis(300), entered_rx.recv()).await;
        assert!(
            third.is_err(),
            "a third connection started despite the cap of 2"
        );
    }
}
