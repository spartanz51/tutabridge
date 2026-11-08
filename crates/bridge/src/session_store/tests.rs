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
    read_results: VecDeque<Result<Option<String>, SessionStoreError>>,
    write_result: Result<(), String>,
    delete_result: Result<(), String>,
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
                persisted: Mutex::new(credentials.map(|c| codec::encode(c).to_string())),
                ..Observations::default()
            }),
            read_results: VecDeque::new(),
            write_result: Ok(()),
            delete_result: Ok(()),
            read_gate: None,
        }
    }

    pub(super) fn read(&mut self) -> Result<Option<String>, SessionStoreError> {
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
            persistence: Persistence::Writable,
            warning: None,
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
fn targets_without_a_keyring_require_an_explicit_pass_entry() {
    for path in [None, Some(""), Some("/"), Some("///")] {
        let selection = Selection::from_config(&config("me@tuta.io", path));
        let error = selection
            .validate(false)
            .expect_err("pass must be configured");
        assert!(matches!(error, SessionStoreError::PassRequired));
        assert!(!error.allows_volatile_login());
        assert!(error.to_string().contains("pass_path"));
        assert!(
            selection.validate(true).is_ok(),
            "desktop keyring remains the default"
        );
    }
}

#[test]
fn configured_pass_is_available_regardless_of_native_keyring_support() {
    for path in ["Tuta/Bridge", "/Tuta/Bridge/"] {
        let selection = Selection::from_config(&config("me@tuta.io", Some(path)));
        for keyring_supported in [false, true] {
            assert!(selection.validate(keyring_supported).is_ok());
            assert_eq!(
                selection,
                Selection::Pass {
                    entry: "Tuta/Bridge/me@tuta.io".to_owned()
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
    backend.read_results.push_back(Err(SessionStoreError::Read {
        backend: "pass",
        message: "locked".to_owned(),
    }));
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
async fn inspecting_an_unavailable_keyring_does_not_allow_mutations_or_activate_volatile_login() {
    for kind in [
        KeyringUnavailableKind::StorageAccess,
        KeyringUnavailableKind::Platform,
    ] {
        let mut backend = FakeBackend::new(None);
        backend.read_results.push_back(Err(unavailable(kind)));
        backend.read_results.push_back(Err(unavailable(kind)));
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config("me@tuta.io", None), backend);

        let status = session_store.inspect().await.unwrap();
        assert!(!status.available);
        assert!(status
            .warning
            .unwrap()
            .contains("sign in again after restarting"));
        assert!(matches!(
            session_store.save(&credentials("me@tuta.io")).await,
            Err(SessionStoreError::UnreadableStore)
        ));
        assert!(matches!(
            session_store.delete().await,
            Err(SessionStoreError::UnreadableStore)
        ));
        assert!(matches!(
            session_store.load().await,
            Err(SessionStoreError::KeyringUnavailable { .. })
        ));
        assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
        assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
        assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    }
}

fn unavailable(kind: KeyringUnavailableKind) -> SessionStoreError {
    SessionStoreError::KeyringUnavailable {
        kind,
        message: "keyring could not be accessed".to_owned(),
    }
}

#[tokio::test]
async fn explicitly_interactive_login_keeps_sessions_in_memory_and_preserves_unreadable_entries() {
    let old = credentials("me@tuta.io");
    let mut new = old.clone();
    new.access_token = "new-token".to_owned();
    let mut backend = FakeBackend::new(Some(&old));
    backend
        .read_results
        .push_back(Err(unavailable(KeyringUnavailableKind::StorageAccess)));
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&old.login, None), backend);

    assert!(session_store.load_interactive().await.unwrap().is_none());
    assert!(matches!(
        session_store.save(&new).await,
        Err(SessionStoreError::VolatileSession)
    ));
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        new.access_token
    );
    let status = session_store.inspect().await.unwrap();
    assert!(status.available);
    assert!(status
        .warning
        .unwrap()
        .contains("sign in again after restarting"));
    assert!(matches!(
        session_store.delete().await,
        Err(SessionStoreError::VolatileSession)
    ));
    assert!(session_store.load().await.unwrap().is_none());
    assert!(session_store.inspect().await.unwrap().warning.is_some());
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(
        codec::decode(observations.persisted.lock().unwrap().as_deref().unwrap())
            .unwrap()
            .access_token,
        old.access_token
    );
}

#[tokio::test]
async fn a_successful_retry_restores_persistence_before_any_volatile_login() {
    for initially_present in [false, true] {
        let expected = credentials("me@tuta.io");
        let mut backend = FakeBackend::new(initially_present.then_some(&expected));
        backend
            .read_results
            .push_back(Err(unavailable(KeyringUnavailableKind::Platform)));
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config(&expected.login, None), backend);

        assert!(!session_store.inspect().await.unwrap().available);
        assert_eq!(
            session_store.load_interactive().await.unwrap().is_some(),
            initially_present
        );
        session_store.save(&expected).await.unwrap();
        assert!(session_store.inspect().await.unwrap().warning.is_none());
        assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
        assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn persisted_load_accepts_readable_entries_confirmed_absence_and_successful_saves() {
    for initially_present in [false, true] {
        let expected = credentials("me@tuta.io");
        let backend = FakeBackend::new(initially_present.then_some(&expected));
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config(&expected.login, Some("Tuta")), backend);
        let clone = session_store.clone();

        for instance in [&session_store, &clone] {
            let loaded = instance.load_persisted(false).await.unwrap();
            assert_eq!(loaded.is_some(), initially_present);
            if let Some(loaded) = loaded {
                assert_eq!(loaded.access_token, expected.access_token);
            }
        }
        assert_eq!(observations.reads.load(Ordering::SeqCst), 1);

        let mut replacement = expected;
        replacement.access_token = "persisted-replacement".to_owned();
        session_store.save(&replacement).await.unwrap();
        assert_eq!(
            session_store
                .load_persisted(false)
                .await
                .unwrap()
                .unwrap()
                .access_token,
            replacement.access_token
        );
        assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn persisted_load_refuses_the_interactive_keyring_fallback_cache() {
    let old = credentials("me@tuta.io");
    let mut replacement = old.clone();
    replacement.access_token = "volatile-replacement".to_owned();
    let mut backend = FakeBackend::new(Some(&old));
    backend
        .read_results
        .push_back(Err(unavailable(KeyringUnavailableKind::StorageAccess)));
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&old.login, None), backend);

    assert!(session_store.load_interactive().await.unwrap().is_none());
    for password_supplied in [false, true] {
        assert!(matches!(
            session_store.load_persisted(password_supplied).await,
            Err(SessionStoreError::PersistenceRequired)
        ));
    }
    assert!(matches!(
        session_store.save(&replacement).await,
        Err(SessionStoreError::VolatileSession)
    ));
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        replacement.access_token
    );
    for password_supplied in [false, true] {
        assert!(matches!(
            session_store
                .clone()
                .load_persisted(password_supplied)
                .await,
            Err(SessionStoreError::PersistenceRequired)
        ));
    }
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(
        codec::decode(observations.persisted.lock().unwrap().as_deref().unwrap())
            .unwrap()
            .access_token,
        old.access_token
    );
}

#[tokio::test]
async fn persisted_load_refuses_a_failed_save_instead_of_reusing_the_old_disk_entry() {
    let old = credentials("me@tuta.io");
    let mut replacement = old.clone();
    replacement.access_token = "unsaved-replacement".to_owned();
    let mut backend = FakeBackend::new(Some(&old));
    backend.write_result = Err("encryption failed".to_owned());
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&old.login, Some("Tuta")), backend);

    assert!(session_store.load_persisted(false).await.unwrap().is_some());
    assert!(matches!(
        session_store.save(&replacement).await,
        Err(SessionStoreError::Write { .. })
    ));
    for password_supplied in [false, true] {
        assert!(matches!(
            session_store.load_persisted(password_supplied).await,
            Err(SessionStoreError::PersistenceRequired)
        ));
    }
    assert_eq!(
        session_store.load().await.unwrap().unwrap().access_token,
        replacement.access_token
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        codec::decode(observations.persisted.lock().unwrap().as_deref().unwrap())
            .unwrap()
            .access_token,
        old.access_token
    );
}

#[tokio::test]
async fn persisted_login_can_replace_a_rejected_session_after_deletion_failed() {
    let rejected = credentials("me@tuta.io");
    let mut replacement = rejected.clone();
    replacement.access_token = "persisted-fresh-login".to_owned();
    let mut backend = FakeBackend::new(Some(&rejected));
    backend.delete_result = Err("deletion failed".to_owned());
    let observations = Arc::clone(&backend.observations);
    let session_store = store(&config(&rejected.login, Some("Tuta")), backend);

    assert!(session_store.load_persisted(false).await.unwrap().is_some());
    assert!(matches!(
        session_store.delete().await,
        Err(SessionStoreError::Delete { .. })
    ));
    // The rejected token is still on disk, but initialization must be able to
    // request a fresh login and save its replacement without resuming it.
    assert!(session_store.load_persisted(false).await.unwrap().is_none());
    assert!(session_store.load_persisted(true).await.unwrap().is_none());
    assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
    session_store.save(&replacement).await.unwrap();
    assert_eq!(
        session_store
            .load_persisted(false)
            .await
            .unwrap()
            .unwrap()
            .access_token,
        replacement.access_token
    );
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        codec::decode(observations.persisted.lock().unwrap().as_deref().unwrap())
            .unwrap()
            .access_token,
        replacement.access_token
    );
    assert!(session_store.inspect().await.unwrap().warning.is_none());
}

#[tokio::test]
async fn persisted_password_login_can_replace_corrupt_data_only_when_the_save_succeeds() {
    for fail_save in [false, true] {
        let mut backend = FakeBackend::new(None);
        if fail_save {
            backend.write_result = Err("encryption failed".into());
        }
        let observations = Arc::clone(&backend.observations);
        *observations.persisted.lock().unwrap() = Some("private corrupt data".into());
        let session_store = store(&config("me@tuta.io", Some("Tuta")), backend);

        assert!(matches!(
            session_store.load_persisted(false).await,
            Err(SessionStoreError::InvalidSession { .. })
        ));
        assert!(session_store.load_persisted(true).await.unwrap().is_none());
        assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
        assert!(matches!(
            session_store.delete().await,
            Err(SessionStoreError::UnreadableStore)
        ));
        assert_eq!(
            observations.persisted.lock().unwrap().as_deref(),
            Some("private corrupt data")
        );

        let authenticated = credentials("me@tuta.io");
        let saved = session_store.save(&authenticated).await;
        if fail_save {
            assert!(matches!(saved, Err(SessionStoreError::Write { .. })));
        } else {
            saved.unwrap();
        }
        for password_supplied in [false, true] {
            let resumed = session_store.load_persisted(password_supplied).await;
            if fail_save {
                assert!(matches!(
                    resumed,
                    Err(SessionStoreError::PersistenceRequired)
                ));
            } else {
                assert_eq!(
                    resumed.unwrap().unwrap().access_token,
                    authenticated.access_token
                );
            }
        }
        assert_eq!(
            session_store.load().await.unwrap().unwrap().access_token,
            authenticated.access_token
        );
        assert_eq!(
            session_store.inspect().await.unwrap().warning.is_some(),
            fail_save
        );
        let persisted = observations.persisted.lock().unwrap().clone().unwrap();
        if fail_save {
            assert_eq!(persisted, "private corrupt data");
        } else {
            assert_eq!(
                codec::decode(persisted).unwrap().access_token,
                authenticated.access_token
            );
        }
        assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
        assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
        assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn abandoned_persisted_password_login_leaves_corruption_strict_and_unchanged() {
    let backend = FakeBackend::new(None);
    let observations = Arc::clone(&backend.observations);
    *observations.persisted.lock().unwrap() = Some("private corrupt data".into());
    let session_store = store(&config("me@tuta.io", Some("Tuta")), backend);

    assert!(session_store.load_persisted(true).await.unwrap().is_none());
    // A rejected password or cancelled login never reaches save. The next
    // automatic start must still reject the original entry as corrupt.
    assert!(matches!(
        session_store.load_persisted(false).await,
        Err(SessionStoreError::InvalidSession { .. })
    ));
    assert!(matches!(
        session_store.save(&credentials("me@tuta.io")).await,
        Err(SessionStoreError::UnreadableStore)
    ));
    assert_eq!(
        observations.persisted.lock().unwrap().as_deref(),
        Some("private corrupt data")
    );
    assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
    assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn persisted_password_login_never_falls_back_on_keyring_or_pass_read_errors() {
    for password_supplied in [false, true] {
        for error in [
            unavailable(KeyringUnavailableKind::StorageAccess),
            unavailable(KeyringUnavailableKind::Platform),
            SessionStoreError::Read {
                backend: "pass",
                message: "GPG is locked".into(),
            },
            SessionStoreError::Read {
                backend: "the keychain",
                message: "multiple matching keyring entries".into(),
            },
        ] {
            let expected = error.to_string();
            let mut backend = FakeBackend::new(None);
            backend.read_results.push_back(Err(error));
            let observations = Arc::clone(&backend.observations);
            let session_store = store(&config("me@tuta.io", None), backend);

            let failure = session_store
                .load_persisted(password_supplied)
                .await
                .err()
                .expect("strict storage failure");
            assert_eq!(failure.to_string(), expected);
            assert!(matches!(
                session_store.save(&credentials("me@tuta.io")).await,
                Err(SessionStoreError::UnreadableStore)
            ));
            assert!(matches!(
                session_store.delete().await,
                Err(SessionStoreError::UnreadableStore)
            ));
            assert_eq!(observations.reads.load(Ordering::SeqCst), 1);
            assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
            assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn pass_failures_and_ambiguous_entries_remain_strict_even_with_an_interactive_password() {
    for result in [
        Err(SessionStoreError::Read {
            backend: "pass",
            message: "GPG is locked".to_owned(),
        }),
        Err(SessionStoreError::Read {
            backend: "the keychain",
            message: "multiple matching keyring entries".to_owned(),
        }),
    ] {
        let mut backend = FakeBackend::new(None);
        backend.read_results.push_back(result);
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config("me@tuta.io", None), backend);

        let error = session_store
            .load_interactive()
            .await
            .err()
            .expect("strict read failure");
        assert!(!error.allows_volatile_login());
        assert!(matches!(
            session_store.save(&credentials("me@tuta.io")).await,
            Err(SessionStoreError::UnreadableStore)
        ));
        assert!(matches!(
            session_store.delete().await,
            Err(SessionStoreError::UnreadableStore)
        ));
        assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
        assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn corrupt_entries_require_explicit_login_and_are_preserved_if_login_is_abandoned() {
    let backend = FakeBackend::new(None);
    let observations = Arc::clone(&backend.observations);
    *observations.persisted.lock().unwrap() = Some("private corrupt data".into());
    let session_store = store(&config("me@tuta.io", None), backend);

    let status = session_store.inspect().await.unwrap();
    assert!(!status.available);
    let warning = status.warning.unwrap();
    for expected in [
        "test store",
        "test-entry",
        "Tuta password",
        "successful authentication",
    ] {
        assert!(warning.contains(expected), "{warning}");
    }
    assert!(!warning.contains("private corrupt data"));
    assert!(matches!(
        session_store.save(&credentials("me@tuta.io")).await,
        Err(SessionStoreError::UnreadableStore)
    ));
    assert!(session_store.load_interactive().await.unwrap().is_none());
    assert!(matches!(
        session_store.delete().await,
        Err(SessionStoreError::UnreadableStore)
    ));
    // No save simulates authentication failure or cancellation. In particular,
    // automatic resume must not mistake the unreadable credentials for absence.
    assert!(matches!(
        session_store.load().await,
        Err(SessionStoreError::InvalidSession { .. })
    ));
    assert_eq!(
        observations.persisted.lock().unwrap().as_deref(),
        Some("private corrupt data")
    );
    assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_login_can_replace_a_corrupt_entry_without_deleting_it_first() {
    for fail_save in [false, true] {
        let mut backend = FakeBackend::new(None);
        if fail_save {
            backend.write_result = Err("encryption failed".into());
        }
        let observations = Arc::clone(&backend.observations);
        *observations.persisted.lock().unwrap() = Some("corrupt entry".into());
        let session_store = store(&config("me@tuta.io", Some("Tuta")), backend);

        assert!(session_store.load_interactive().await.unwrap().is_none());
        assert_eq!(
            observations.persisted.lock().unwrap().as_deref(),
            Some("corrupt entry")
        );
        let new = credentials("me@tuta.io");
        assert_eq!(session_store.save(&new).await.is_err(), fail_save);
        assert_eq!(
            session_store.load().await.unwrap().unwrap().access_token,
            new.access_token
        );
        assert_eq!(
            session_store.inspect().await.unwrap().warning.is_some(),
            fail_save
        );
        assert_eq!(observations.writes.load(Ordering::SeqCst), 1);
        assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
        let persisted = observations.persisted.lock().unwrap().clone().unwrap();
        if fail_save {
            assert_eq!(persisted, "corrupt entry");
        } else {
            assert_eq!(
                codec::decode(persisted).unwrap().access_token,
                new.access_token
            );
        }
    }
}

#[tokio::test]
async fn explicit_login_rereads_a_corrupt_entry_and_respects_repairs_or_new_read_errors() {
    for read_fails in [false, true] {
        let repaired = credentials("me@tuta.io");
        let mut backend = FakeBackend::new(Some(&repaired));
        backend
            .read_results
            .push_back(Ok(Some("corrupt entry".into())));
        if read_fails {
            backend.read_results.push_back(Err(SessionStoreError::Read {
                backend: "pass",
                message: "locked".into(),
            }));
        }
        let observations = Arc::clone(&backend.observations);
        let session_store = store(&config(&repaired.login, Some("Tuta")), backend);
        assert!(!session_store.inspect().await.unwrap().available);
        let result = session_store.load_interactive().await;
        if read_fails {
            assert!(matches!(result, Err(SessionStoreError::Read { .. })));
        } else {
            assert_eq!(result.unwrap().unwrap().access_token, repaired.access_token);
        }
        assert_eq!(observations.reads.load(Ordering::SeqCst), 2);
        assert_eq!(observations.writes.load(Ordering::SeqCst), 0);
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
    let status = session_store.inspect().await.unwrap();
    assert!(status.available);
    assert!(status
        .warning
        .unwrap()
        .contains("only available for this run"));
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
