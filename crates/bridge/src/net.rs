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
