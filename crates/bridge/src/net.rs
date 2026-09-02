//! Shared connection-accept loop for the IMAP and SMTP servers.
//!
//! Both servers used to inline `loop { listener.accept().await? }`, which had
//! two problems: a single transient `accept()` error (EMFILE, ECONNABORTED, …)
//! propagated out and killed the server for good, and there was no bound on the
//! number of concurrent connections. This loop fixes both and is transport
//! agnostic so it can be unit-tested without TLS.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Max concurrent client connections per server. Bounds file descriptors and
/// memory if a client (or a port scanner) opens connections faster than they
/// close.
pub(crate) const MAX_CONNECTIONS: usize = 64;

/// How long a client has to complete the TLS handshake before being dropped.
/// Stops a connection that opens but never negotiates from parking a task and
/// a file descriptor forever.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

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
/// redacted at its emission site rather than in a helper nobody calls. The
/// logger is process-global and can be installed once, so it is shared by
/// every test module and filtered by content, since tests run in parallel.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::sync::{Mutex, Once};

    static RECORDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static INSTALL: Once = Once::new();

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            if record.target().starts_with("tutabridge_core") {
                RECORDS.lock().unwrap().push(record.args().to_string());
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

    /// Every captured record that contains `needle`.
    pub(crate) fn lines_containing(needle: &str) -> Vec<String> {
        RECORDS
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(needle))
            .cloned()
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
    #[test]
    fn a_peer_that_vanishes_is_a_benign_disconnect() {
        use std::io::{Error, ErrorKind};
        // rustls: TCP EOF with no close_notify.
        assert!(super::is_benign_disconnect(&Error::new(
            ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify"
        )));
        // The kernel: peer closed with data still unread.
        assert!(super::is_benign_disconnect(&Error::new(
            ErrorKind::ConnectionReset,
            "Connection reset by peer (os error 54)"
        )));
    }

    #[test]
    fn a_real_stream_error_is_not() {
        use std::io::{Error, ErrorKind};
        assert!(!super::is_benign_disconnect(&Error::new(
            ErrorKind::InvalidData,
            "received corrupt message"
        )));
        assert!(!super::is_benign_disconnect(&Error::from(ErrorKind::Other)));
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;

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
