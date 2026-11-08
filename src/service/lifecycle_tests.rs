//! Process assertions run in an isolated test subprocess: subreaping must never
//! interfere with the test harness's other children or signal dispositions.
use super::health::connect_health;
use super::*;

fn temporary(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "tutabridge-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    std::fs::create_dir(&path).unwrap();
    path
}

#[tokio::test]
async fn acquire_for_init_retries_a_lock_observed_held_then_succeeds() {
    let root = temporary("retry-lock");
    let path = root.join("lock");
    let owner = Lock::at(&path).unwrap().unwrap();
    let (_sender, mut stop) = watch::channel(false);
    let acquisition = acquire_for_init_at(&path, true, &mut stop);
    tokio::pin!(acquisition);
    // The future actually ran while locked, and cannot finish on its first try.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut acquisition)
            .await
            .is_err()
    );
    drop(owner);
    let acquired = tokio::time::timeout(Duration::from_secs(1), &mut acquisition)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(Lock::at(&path).unwrap().is_none());
    drop(acquired);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn acquire_for_init_is_bounded_cancellable_and_run_does_not_wait() {
    let root = temporary("bounded-lock");
    let path = root.join("lock");
    let owner = Lock::at(&path).unwrap().unwrap();
    let (sender, mut stop) = watch::channel(false);
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        acquire_for_init_at(&path, false, &mut stop)
    )
    .await
    .unwrap()
    .unwrap()
    .is_none());
    let start = std::time::Instant::now();
    assert!(acquire_for_init_at(&path, true, &mut stop)
        .await
        .unwrap()
        .is_none());
    assert!(start.elapsed() >= Duration::from_secs(3) && start.elapsed() < Duration::from_secs(4));
    let mut pending = Box::pin(acquire_for_init_at(&path, true, &mut stop));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    sender.send_replace(true);
    assert!(tokio::time::timeout(Duration::from_millis(200), pending)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    drop(owner);
    // Cancellation must win even if the lock is now free.
    assert!(acquire_for_init_at(&path, true, &mut stop)
        .await
        .unwrap()
        .is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn inherited_ignored_hangup_and_quit_are_preserved() {
    isolated("signals");
}

#[cfg(target_os = "linux")]
#[test]
fn terminate_group_reaps_descendants_before_lock_release() {
    isolated("terminate");
}

#[cfg(target_os = "linux")]
#[test]
fn reap_agents_reaps_adopted_daemons_without_stealing_tokio_leader() {
    isolated("reap");
}

fn isolated(mode: &str) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "service::lifecycle_tests::isolated_process_fixture",
            "--nocapture",
        ])
        .env("TUTABRIDGE_PROCESS_FIXTURE", mode)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{mode}: {status}");
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{mode} timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn isolated_process_fixture() {
    let Ok(mode) = std::env::var("TUTABRIDGE_PROCESS_FIXTURE") else {
        return;
    };
    if mode == "signals" {
        for signal in [libc::SIGHUP, libc::SIGQUIT] {
            unsafe {
                libc::signal(signal, libc::SIG_IGN);
            }
        }
        crate::shutdown::supervised(|_stop| async {
            for signal in [libc::SIGHUP, libc::SIGQUIT] {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) },
                    0
                );
                assert_eq!(action.sa_sigaction, libc::SIG_IGN);
                assert_eq!(unsafe { libc::kill(libc::getpid(), signal) }, 0);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await
        .unwrap();
        return;
    }
    #[cfg(target_os = "linux")]
    process_case(&mode).await;
}

#[cfg(target_os = "linux")]
async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition timed out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
async fn process_case(mode: &str) {
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let root = temporary(mode);
    let lock_path = root.join("lock");
    let lock = Lock::at(&lock_path).unwrap().unwrap();
    std::fs::write(root.join("daemon.sh"), "echo $$ > daemon; exec sleep 60").unwrap();
    let mut command = std::process::Command::new("bash");
    command.current_dir(&root).process_group(0).args(["-c", if mode == "terminate" {
        // The leader exits first; an uncooperative descendant remains in its
        // reserved group, so waiting for the leader alone cannot pass the test.
        "bash -c 'trap \"\" TERM; echo $$ > daemon; exec sleep 60' & while [ ! -s daemon ]; do sleep .01; done; exit 0"
    } else {
        // Double-fork + setsid models gpg-agent: adopted by this subreaper,
        // outside the worker group, while the worker itself stays alive.
        "bash -c 'setsid bash daemon.sh &' ; while [ ! -s daemon ]; do sleep .01; done; echo ready > ready; exec sleep 60"
    }]);
    let mut child = tokio::process::Command::from(command).spawn().unwrap();
    let pid = child.id().unwrap() as i32;
    wait_until(|| root.join("daemon").exists()).await;
    let daemon: i32 = std::fs::read_to_string(root.join("daemon"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    if mode == "terminate" {
        wait_until(|| exited_without_reaping(pid).unwrap()).await;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "leader must still reserve the PGID"
        );
        terminate_group(&mut child, pid).await.unwrap();
        assert_eq!(
            unsafe { libc::kill(daemon, 0) },
            -1,
            "descendant survived or remains a zombie"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    } else {
        wait_until(|| root.join("ready").exists()).await;
        let status = std::fs::read_to_string(format!("/proc/{daemon}/status")).unwrap();
        assert!(
            status
                .lines()
                .any(|l| l == format!("PPid:\t{}", std::process::id())),
            "daemon was not adopted: {status}"
        );
        assert_eq!(unsafe { libc::kill(daemon, libc::SIGKILL) }, 0);
        wait_until(|| exited_without_reaping(daemon).unwrap()).await;
        reap_agents(Some(pid));
        assert_eq!(
            unsafe { libc::waitpid(daemon, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(
            !exited_without_reaping(pid).unwrap(),
            "live worker was stolen"
        );
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        wait_until(|| exited_without_reaping(pid).unwrap()).await;
        reap_agents(Some(pid));
        assert!(
            exited_without_reaping(pid).unwrap(),
            "zombie leader was stolen from Tokio"
        );
        terminate_group(&mut child, pid).await.unwrap();
    }
    assert!(
        Lock::at(&lock_path).unwrap().is_none(),
        "owner lost lock before cleanup"
    );
    drop(lock);
    assert!(Lock::at(&lock_path).unwrap().is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unspecified_ipv6_can_probe_ipv4_but_explicit_ipv6_does_not_fall_back() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(connect_health("::".parse().unwrap(), port).await.is_ok());
    assert!(connect_health("::1".parse().unwrap(), port).await.is_err());
}
