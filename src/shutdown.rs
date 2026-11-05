//! Controlled termination for services and authenticated interactive runs.
//!
//! Services install handlers before resuming a saved session. Interactive
//! callers install them only after terminal prompts have completed. The CLI
//! also bounds runtime teardown because cancelling a future cannot stop an
//! already running `spawn_blocking` keyring/pass worker.

use std::{future::Future, time::Duration};
use tokio::sync::watch;

const GRACE_PERIOD: Duration = Duration::from_secs(5);

pub async fn run<F, Fut>(operation: F) -> anyhow::Result<()>
where
    F: FnOnce(watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    controlled(operation, true).await
}

/// The subprocess supervisor performs its own bounded kill/reap sequence. Do
/// not drop it at the same deadline: that would release the lock before reaping.
pub async fn supervised<F, Fut>(operation: F) -> anyhow::Result<()>
where
    F: FnOnce(watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    controlled(operation, false).await
}

#[cfg(unix)]
fn optional_signal(
    kind: tokio::signal::unix::SignalKind,
) -> std::io::Result<Option<tokio::signal::unix::Signal>> {
    // nohup and callers may deliberately ignore HUP/QUIT. Preserve that
    // inherited disposition rather than silently restoring termination.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(kind.as_raw_value(), std::ptr::null(), &mut action) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if action.sa_sigaction == libc::SIG_IGN {
        return Ok(None);
    }
    tokio::signal::unix::signal(kind).map(Some)
}

async fn controlled<F, Fut>(operation: F, cancel: bool) -> anyhow::Result<()>
where
    F: FnOnce(watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    // Unix registration is immediate, before polling any network operation.
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(unix)]
    let mut hangup = if !cancel {
        optional_signal(tokio::signal::unix::SignalKind::hangup())?
    } else {
        None
    };
    #[cfg(unix)]
    let mut quit = if !cancel {
        optional_signal(tokio::signal::unix::SignalKind::quit())?
    } else {
        None
    };
    let signal = async {
        #[cfg(unix)]
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
            _ = async { match &mut hangup { Some(s) => {s.recv().await;}, None => std::future::pending().await } } => {},
            _ = async { match &mut quit { Some(s) => {s.recv().await;}, None => std::future::pending().await } } => {},
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        Ok::<(), std::io::Error>(())
    };
    let (sender, receiver) = watch::channel(false);
    let work = operation(receiver);
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => result,
        result = signal => {
            result?;
            log::info!("Shutting down...");
            let _ = sender.send(true);
            if !cancel { return work.await; }
            match tokio::time::timeout(GRACE_PERIOD, &mut work).await {
                Ok(result) => result,
                Err(_) => {
                    log::info!("Cancelling remaining startup or network operations");
                    Ok(())
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    #[test]
    fn signal_probe() {
        let Ok(mode) = std::env::var("TUTABRIDGE_SIGNAL_PROBE") else {
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(run(|mut stop| async move {
                if mode == "blocking" {
                    let (started, ready) = tokio::sync::oneshot::channel();
                    let worker = tokio::task::spawn_blocking(move || {
                        let _ = started.send(());
                        // Model a keyring/pass operation that cannot be
                        // cancelled when its awaiting future is dropped.
                        std::thread::sleep(Duration::from_secs(60));
                    });
                    ready.await?;
                    println!("signal-ready");
                    std::io::stdout().flush().unwrap();
                    worker.await?;
                    return Ok(());
                }
                println!("signal-ready");
                std::io::stdout().flush().unwrap();
                stop.wait_for(|stopping| *stopping).await.unwrap();
                println!("shutdown-observed");
                Ok(())
            }))
            .unwrap();
        runtime.shutdown_timeout(Duration::from_secs(1));
        println!("runtime-stopped");
    }

    fn terminate_probe(mode: &str) -> (String, Duration) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "shutdown::tests::signal_probe", "--nocapture"])
            .env("TUTABRIDGE_SIGNAL_PROBE", mode)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "child exited before registering signals"
            );
            if line.contains("signal-ready") {
                break;
            }
        }
        let started = std::time::Instant::now();
        assert!(Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("SIGTERM did not stop the operation");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{status}");
        let remaining: String = reader.lines().map(Result::unwrap).collect();
        assert!(remaining.contains("runtime-stopped"), "{remaining}");
        (remaining, started.elapsed())
    }

    #[test]
    fn sigterm_reaches_the_operation_and_exits_successfully() {
        let (remaining, _) = terminate_probe("cooperative");
        assert!(remaining.contains("shutdown-observed"), "{remaining}");
    }

    #[test]
    fn sigterm_bounds_shutdown_even_when_a_blocking_worker_cannot_be_cancelled() {
        let (_, elapsed) = terminate_probe("blocking");
        // Five seconds for cooperative cancellation, one for the runtime,
        // plus scheduler allowance. The fake worker itself waits a minute.
        assert!(
            elapsed < GRACE_PERIOD + Duration::from_secs(3),
            "{elapsed:?}"
        );
    }
}
