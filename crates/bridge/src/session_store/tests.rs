use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Default)]
struct Observations {
    reads: AtomicUsize,
    writes: AtomicUsize,
    deletes: AtomicUsize,
    persisted: Mutex<Option<String>>,
}

pub(super) struct FakeBackend {
    observations: Arc<Observations>,
    read_results: VecDeque<Result<Option<String>, String>>,
    write_result: Result<(), String>,
    delete_result: Result<(), String>,
    pub(super) legacy_fallback: bool,
    read_gate: Option<ReadGate>,
}

struct ReadGate {
    started: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl FakeBackend {
    fn new(credentials: Option<&Credentials>) -> Self {
        Self {
            observations: Arc::new(Observations {
                persisted: Mutex::new(credentials.map(codec::encode)),
                ..Observations::default()
            }),
            read_results: VecDeque::new(),
            write_result: Ok(()),
            delete_result: Ok(()),
            legacy_fallback: false,
            read_gate: None,
        }
    }

    pub(super) fn read(&mut self) -> Result<Option<String>, String> {
        self.observations.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.read_gate.take() {
            let _ = gate.started.send(());
            // Bound a regression's failure: an accidentally blocking async
            // implementation must fail the test rather than hang the suite.
            let _ = gate.release.recv_timeout(Duration::from_secs(5));
        }
        self.read_results
            .pop_front()
            .unwrap_or_else(|| Ok(self.observations.persisted.lock().unwrap().clone()))
    }

    pub(super) fn write(&mut self, secret: &str) -> Result<(), String> {
        self.observations.writes.fetch_add(1, Ordering::SeqCst);
        self.write_result.clone()?;
        *self.observations.persisted.lock().unwrap() = Some(secret.to_owned());
        Ok(())
    }

    pub(super) fn delete(&mut self) -> Result<(), String> {
        self.observations.deletes.fetch_add(1, Ordering::SeqCst);
        self.delete_result.clone()?;
        *self.observations.persisted.lock().unwrap() = None;
        Ok(())
    }
}

fn config(email: &str, pass_path: Option<&str>) -> Config {
    Config {
        email: email.to_owned(),
        pass_path: pass_path.map(str::to_owned),
        ..Config::default()
    }
}

fn credentials(email: &str) -> Credentials {
    Credentials {
        login: email.to_owned(),
        user_id: tutasdk::GeneratedId("fakeuser0".to_owned()),
        access_token: format!("token-for-{email}"),
        encrypted_passphrase_key: vec![0, 1, 2, 254, 255],
        credential_type: tutasdk::login::CredentialType::Internal,
    }
}

fn store(config: &Config, backend: FakeBackend) -> SessionStore {
    SessionStore {
        context: Context::from_config(config),
        state: Arc::new(Mutex::new(State {
            backend: Backend::Fake(backend),
            cache: Cache::Unloaded,
        })),
    }
}

#[test]
fn backend_selection_preserves_legacy_path_normalization() {
    for path in [None, Some(""), Some("/"), Some("///")] {
        assert_eq!(
            Selection::from_config(&config("me@tuta.io", path)),
            Selection::Keyring
        );
    }
    for path in ["Tuta/Bridge", "/Tuta/Bridge/"] {
        for email in ["me@tuta.io", "other@tuta.io"] {
            assert_eq!(
                Selection::from_config(&config(email, Some(path))),
                Selection::Pass {
                    entry: format!("Tuta/Bridge/{email}")
                }
            );
        }
    }
}

#[test]
fn context_comparison_tracks_only_account_endpoint_and_normalized_backend() {
    let original = config("me@tuta.io", Some("Tuta/Bridge"));
    let session_store = store(&original, FakeBackend::new(None));
    let mut changed = original.clone();
    changed.pass_path = Some("/Tuta/Bridge/".to_owned());
    changed.imap_port += 1;
    changed.smtp_port += 1;
    changed.bridge_password = Some("different local password".to_owned());
    assert!(session_store.matches_config(&changed));

    changed.email = "other@tuta.io".to_owned();
    assert!(!session_store.matches_config(&changed));
    changed = original.clone();
    changed.api_url = "https://another.example.invalid".to_owned();
    assert!(!session_store.matches_config(&changed));
    changed = original.clone();
    changed.pass_path = Some("Other".to_owned());
    assert!(!session_store.matches_config(&changed));
    changed.pass_path = None;
    assert!(!session_store.matches_config(&changed));
}

#[tokio::test]
async fn repeated_and_cloned_loads_share_one_cache() {
    let expected = credentials("me@tuta.io");
    let backend = FakeBackend::new(Some(&expected));
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&expected.login, None), backend);
    let clone = session_store.clone();

    for instance in [&session_store, &clone, &session_store] {
        assert_eq!(
            instance.load().await.unwrap().unwrap().access_token,
            expected.access_token
        );
    }
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn independent_stores_do_not_share_credentials_or_missing_session_cache() {
    let alice = credentials("alice@tuta.io");
    let bob = credentials("bob@tuta.io");
    let alice_store = store(&config(&alice.login, None), FakeBackend::new(Some(&alice)));
    let bob_store = store(&config(&bob.login, None), FakeBackend::new(Some(&bob)));
    let empty_store = store(&config(&alice.login, None), FakeBackend::new(None));

    assert_eq!(
        alice_store.load().await.unwrap().unwrap().login,
        alice.login
    );
    assert!(empty_store.load().await.unwrap().is_none());
    assert_eq!(bob_store.load().await.unwrap().unwrap().login, bob.login);
    alice_store.delete().await.unwrap();
    assert_eq!(bob_store.load().await.unwrap().unwrap().login, bob.login);
}

#[tokio::test]
async fn a_confirmed_missing_session_is_cached() {
    let backend = FakeBackend::new(None);
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config("me@tuta.io", None), backend);
    assert!(session_store.load().await.unwrap().is_none());
    assert!(session_store.load().await.unwrap().is_none());
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_reads_can_be_retried_after_the_backend_is_unlocked() {
    let expected = credentials("me@tuta.io");
    let mut backend = FakeBackend::new(Some(&expected));
    backend.read_results.push_back(Err("locked".to_owned()));
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&expected.login, Some("Tuta")), backend);

    assert!(matches!(
        session_store.load().await,
        Err(SessionStoreError::Read { .. })
    ));
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        expected.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn malformed_sessions_are_not_cached_or_disclosed_in_errors() {
    let expected = credentials("me@tuta.io");
    let mut backend = FakeBackend::new(Some(&expected));
    backend
        .read_results
        .push_back(Ok(Some("sensitive broken session".to_owned())));
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&expected.login, Some("Tuta")), backend);

    let error = session_store.load().await.err().expect("invalid session");
    assert!(matches!(error, SessionStoreError::InvalidSession { .. }));
    assert!(!error.to_string().contains("sensitive"));
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        expected.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_keyring_read_errors_and_malformed_entries_remain_nonblocking() {
    for result in [
        Err("keyring unavailable".to_owned()),
        Ok(Some("invalid json".to_owned())),
    ] {
        let mut backend = FakeBackend::new(None);
        backend.legacy_fallback = true;
        backend.read_results.push_back(result);
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config("me@tuta.io", None), backend);

        assert!(session_store.load().await.unwrap().is_none());
        assert!(session_store.load().await.unwrap().is_none());
        assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
        assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn failed_saves_keep_the_new_session_in_memory_and_report_failure() {
    let old = credentials("me@tuta.io");
    let mut new = old.clone();
    new.access_token = "replacement-token".to_owned();
    let mut backend = FakeBackend::new(Some(&old));
    backend.write_result = Err("read-only store".to_owned());
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&old.login, None), backend);
    session_store.load().await.unwrap();

    assert!(matches!(
        session_store.save(&new).await,
        Err(SessionStoreError::Write { .. })
    ));
    assert_eq!(
        session_store
            .clone()
            .load()
            .await
            .unwrap()
            .unwrap()
            .access_token,
        new.access_token
    );
    let persisted = observations.persisted.lock().unwrap().clone().unwrap();
    assert_eq!(
        codec::decode(&persisted).unwrap().access_token,
        old.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn saving_persists_the_legacy_format_and_updates_the_cached_absence() {
    let expected = credentials("me@tuta.io");
    let backend = FakeBackend::new(None);
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&expected.login, Some("Tuta")), backend);
    assert!(session_store.load().await.unwrap().is_none());
    session_store.save(&expected).await.unwrap();

    assert_eq!(
        observations.persisted.lock().unwrap().as_deref(),
        Some(codec::encode(&expected).as_str())
    );
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        expected.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_rejected_session_stays_forgotten_when_persistent_deletion_fails() {
    let rejected = credentials("me@tuta.io");
    let mut backend = FakeBackend::new(Some(&rejected));
    backend.delete_result = Err("locked store".to_owned());
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&rejected.login, None), backend);
    assert!(session_store.load().await.unwrap().is_some());

    assert!(matches!(
        session_store.delete().await,
        Err(SessionStoreError::Delete { .. })
    ));
    assert!(session_store.clone().load().await.unwrap().is_none());
    assert!(observations.persisted.lock().unwrap().is_some());
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn backend_io_and_contended_cache_locks_do_not_block_the_async_worker() {
    let expected = credentials("me@tuta.io");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut backend = FakeBackend::new(Some(&expected));
    backend.read_gate = Some(ReadGate {
        started: started_tx,
        release: release_rx,
    });
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&expected.login, Some("Tuta")), backend);
    let first_store = session_store.clone();
    let first = tokio::spawn(async move { first_store.load().await });
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();

    let second = tokio::spawn(async move { session_store.load().await });
    tokio::task::yield_now().await;
    assert!(
        !first.is_finished(),
        "the async worker must run while backend I/O is blocked"
    );
    assert!(
        !second.is_finished(),
        "the second load must wait for the first cache fill"
    );
    release_tx.send(()).unwrap();

    assert_eq!(
        first.await.unwrap().unwrap().unwrap().access_token,
        expected.access_token
    );
    assert_eq!(
        second.await.unwrap().unwrap().unwrap().access_token,
        expected.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
}
