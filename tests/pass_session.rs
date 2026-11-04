//! Each probe has its own process, cache, PATH and dummy password store.
//! No real keyring, GPG key or Tuta account is used.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use tutabridge_core::{
    config::Config,
    session_store::{SessionStore, SessionStoreError},
    tuta,
};

const SESSION: &str = r#"{"login":"nobody@example.invalid","user_id":"fakeuser0","access_token":"secret-token-must-not-be-logged","encrypted_passphrase_key":"AA==","credential_type":"Internal"}"#;
const ENTRY: &str = "Tuta/Bridge/nobody@example.invalid.gpg";

struct Fixture(PathBuf);

impl Fixture {
    fn new(scenario: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "tutabridge-pass-{scenario}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("store/Tuta/Bridge")).unwrap();
        std::fs::write(root.join("session"), SESSION).unwrap();
        std::fs::write(root.join("mode"), "ok").unwrap();
        std::fs::write(
            root.join("bin/pass"),
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$PASS_TEST_ROOT/calls"
[ "$PASSWORD_STORE_DIR" = "$PASS_TEST_ROOT/store" ] || exit 90
mode=$(/bin/cat "$PASS_TEST_ROOT/mode")
case "$1" in
  show)
    [ "$#" = 3 ] && [ "$2" = -- ] && [ "$3" = Tuta/Bridge/nobody@example.invalid ] || exit 91
    case "$mode" in
      fail)
        /bin/cat "$PASS_TEST_ROOT/session"
        printf '%s\n' 'gpg: decryption failed: No pinentry' >&2
        exit 2 ;;
      invalid) printf '%s\n' 'secret-token-must-not-be-logged'; exit 0 ;;
      utf8) printf '\377'; exit 0 ;;
      *) /bin/cat "$PASS_TEST_ROOT/session" ;;
    esac ;;
  insert)
    [ "$#" = 5 ] && [ "$2" = --multiline ] && [ "$3" = --force ] && [ "$4" = -- ] && [ "$5" = Tuta/Bridge/nobody@example.invalid ] || exit 92
    /bin/cat > "$PASS_TEST_ROOT/inserted-session"
    if [ "$mode" = fail ]; then
      /bin/cat "$PASS_TEST_ROOT/inserted-session"
      printf '%s\n' 'gpg: encryption failed: test failure' >&2
      exit 2
    fi
    printf '%s\n' 'replacement encrypted entry' > "$PASSWORD_STORE_DIR/$5.gpg" ;;
  rm)
    [ "$#" = 4 ] && [ "$2" = --force ] && [ "$3" = -- ] && [ "$4" = Tuta/Bridge/nobody@example.invalid ] || exit 93
    if [ "$mode" = fail ]; then
      /bin/cat "$PASS_TEST_ROOT/session"
      printf '%s\n' 'pass: deletion failed: test failure' >&2
      exit 2
    fi
    /bin/rm -- "$PASSWORD_STORE_DIR/$4.gpg" ;;
  *) exit 94 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("bin/pass"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self(root)
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("PASS_TEST_ROOT", &self.0)
            .env("PASSWORD_STORE_DIR", self.0.join("store"))
            // Only our fake executable is visible; no global environment edits.
            .env("PATH", self.0.join("bin"));
        cmd
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_probe(scenario: &str) {
    let fixture = Fixture::new(scenario);
    let output = fixture
        .command(std::env::current_exe().unwrap())
        .args(["--exact", "pass_probe", "--nocapture"])
        .env("PASS_TEST_SCENARIO", scenario)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{scenario}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for captured in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(captured).contains("secret-token-must-not-be-logged"));
    }
}

fn calls(root: &Path) -> usize {
    std::fs::read_to_string(root.join("calls"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn credentials() -> tutasdk::login::Credentials {
    tutasdk::login::Credentials {
        login: "nobody@example.invalid".into(),
        user_id: tutasdk::GeneratedId("fakeuser0".into()),
        access_token: "secret-token-must-not-be-logged".into(),
        encrypted_passphrase_key: vec![0],
        credential_type: tutasdk::login::CredentialType::Internal,
    }
}

#[test]
fn pass_probe() {
    let Ok(scenario) = std::env::var("PASS_TEST_SCENARIO") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("PASS_TEST_ROOT").unwrap());
    let cfg = Config {
        email: "nobody@example.invalid".into(),
        pass_path: Some("Tuta/Bridge".into()),
        ..Config::default()
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = SessionStore::new(&cfg);
    if scenario == "absent" || scenario == "delete-absent" {
        assert!(runtime.block_on(store.load()).unwrap().is_none());
        if scenario == "delete-absent" {
            runtime.block_on(store.delete()).unwrap();
        }
        assert_eq!(calls(&root), 0);
        return;
    }
    std::fs::write(root.join("store").join(ENTRY), "dummy encrypted entry").unwrap();
    match scenario.as_str() {
        "success" => {
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            assert_eq!(calls(&root), 1, "successful reads are cached");
        }
        "retry" => {
            std::fs::write(root.join("mode"), "fail").unwrap();
            let error = runtime
                .block_on(store.load())
                .err()
                .expect("read must fail")
                .to_string();
            assert!(
                error.contains("gpg: decryption failed: No pinentry"),
                "{error}"
            );
            assert!(!error.contains("secret-token-must-not-be-logged"));
            std::fs::write(root.join("mode"), "ok").unwrap();
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            assert_eq!(calls(&root), 2, "errors must not poison the cache");
        }
        "missing-program" => {
            std::fs::rename(root.join("bin/pass"), root.join("bin/pass.disabled")).unwrap();
            assert!(runtime
                .block_on(store.load())
                .err()
                .expect("missing executable must fail")
                .to_string()
                .contains("cannot run pass"));
            std::fs::rename(root.join("bin/pass.disabled"), root.join("bin/pass")).unwrap();
            assert!(runtime.block_on(store.load()).unwrap().is_some());
        }
        "invalid" | "utf8" => {
            std::fs::write(root.join("mode"), &scenario).unwrap();
            let error = runtime
                .block_on(store.load())
                .err()
                .expect("invalid session must fail")
                .to_string();
            assert!(error.contains("session"));
            assert!(!error.contains("secret-token-must-not-be-logged"));
            std::fs::write(root.join("mode"), "ok").unwrap();
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            assert_eq!(calls(&root), 2);
        }
        "replace-invalid" | "replace-utf8" => {
            std::fs::write(
                root.join("mode"),
                scenario.strip_prefix("replace-").unwrap(),
            )
            .unwrap();
            let status = runtime.block_on(store.inspect()).unwrap();
            assert!(!status.available);
            let warning = status.warning.unwrap();
            assert!(warning.contains("pass (Tuta/Bridge/nobody@example.invalid)"));
            assert!(warning.contains("Tuta password"));
            assert!(!warning.contains("secret-token-must-not-be-logged"));

            // Preparing a password login neither deletes nor overwrites the
            // entry. Abandoning it still leaves strict resume reporting it.
            assert!(runtime
                .block_on(store.load_interactive())
                .unwrap()
                .is_none());
            assert!(matches!(
                runtime.block_on(store.load()),
                Err(SessionStoreError::InvalidSession { .. })
            ));
            assert_eq!(calls(&root), 3);
            assert!(!root.join("inserted-session").exists());
            assert_eq!(
                std::fs::read_to_string(root.join("store").join(ENTRY)).unwrap(),
                "dummy encrypted entry"
            );

            assert!(runtime
                .block_on(store.load_interactive())
                .unwrap()
                .is_none());
            // Authentication has now succeeded: persist its new credentials.
            runtime.block_on(store.save(&credentials())).unwrap();
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            assert!(runtime.block_on(store.inspect()).unwrap().warning.is_none());
            let arguments = std::fs::read_to_string(root.join("calls")).unwrap();
            assert_eq!(calls(&root), 5);
            assert!(arguments
                .ends_with("insert --multiline --force -- Tuta/Bridge/nobody@example.invalid\n"));
            assert!(!arguments.contains("rm --"));
            assert_eq!(
                std::fs::read_to_string(root.join("store").join(ENTRY)).unwrap(),
                "replacement encrypted entry\n"
            );
        }
        "login-error" => {
            std::fs::write(root.join("mode"), "fail").unwrap();
            // Even an explicitly supplied password must not overwrite a
            // session whose store could not be read. No SDK/network needed.
            let error =
                match runtime.block_on(tuta::login_with_2fa(&cfg, &store, Some("unused"), None)) {
                    Ok(_) => panic!("storage failure should prevent login"),
                    Err(error) => error,
                };
            assert!(error.is::<SessionStoreError>());
            assert!(error.to_string().contains("No pinentry"));
            assert_eq!(calls(&root), 1);
        }
        "wrong-context" => {
            let other = Config {
                email: "other@example.invalid".into(),
                ..cfg.clone()
            };
            let error = match runtime.block_on(tuta::login_with_2fa(&other, &store, None, None)) {
                Ok(_) => panic!("another account must not use this session store"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("does not match"));
            assert_eq!(
                calls(&root),
                0,
                "reject the context before accessing secrets"
            );
        }
        "save" | "save-error" => {
            let credentials = credentials();
            if scenario == "save-error" {
                std::fs::write(root.join("mode"), "fail").unwrap();
                let error = runtime
                    .block_on(store.save(&credentials))
                    .expect_err("insert must fail");
                assert!(matches!(error, SessionStoreError::Write { .. }));
                let message = error.to_string();
                assert!(
                    message.contains("gpg: encryption failed: test failure"),
                    "{message}"
                );
                assert!(!message.contains("secret-token-must-not-be-logged"));
                assert_eq!(
                    std::fs::read_to_string(root.join("store").join(ENTRY)).unwrap(),
                    "dummy encrypted entry"
                );
            } else {
                runtime.block_on(store.save(&credentials)).unwrap();
                assert_eq!(
                    std::fs::read_to_string(root.join("store").join(ENTRY)).unwrap(),
                    "replacement encrypted entry\n"
                );
            }
            let stdin = std::fs::read_to_string(root.join("inserted-session")).unwrap();
            // All fixture values are simple strings with no commas. Compare
            // fields without imposing serde_json's object key order.
            let mut actual_fields: Vec<_> = stdin
                .strip_prefix('{')
                .unwrap()
                .strip_suffix('}')
                .unwrap()
                .split(',')
                .collect();
            let mut expected_fields: Vec<_> = SESSION
                .strip_prefix('{')
                .unwrap()
                .strip_suffix('}')
                .unwrap()
                .split(',')
                .collect();
            actual_fields.sort_unstable();
            expected_fields.sort_unstable();
            assert_eq!(actual_fields, expected_fields);
            assert_eq!(
                runtime
                    .block_on(store.load())
                    .unwrap()
                    .unwrap()
                    .access_token,
                credentials.access_token
            );
            assert_eq!(calls(&root), 1, "saved or volatile session must be cached");
            let arguments = std::fs::read_to_string(root.join("calls")).unwrap();
            assert_eq!(
                arguments,
                "insert --multiline --force -- Tuta/Bridge/nobody@example.invalid\n"
            );
            assert!(!arguments.contains("secret-token-must-not-be-logged"));
        }
        "delete" | "delete-error" => {
            assert!(runtime.block_on(store.load()).unwrap().is_some());
            if scenario == "delete-error" {
                std::fs::write(root.join("mode"), "fail").unwrap();
                let error = runtime
                    .block_on(store.delete())
                    .expect_err("delete must fail");
                assert!(matches!(error, SessionStoreError::Delete { .. }));
                let message = error.to_string();
                assert!(message.contains("pass rm exited with"), "{message}");
                assert!(!message.contains("secret-token-must-not-be-logged"));
                assert!(root.join("store").join(ENTRY).exists());
            } else {
                runtime.block_on(store.delete()).unwrap();
                assert!(!root.join("store").join(ENTRY).exists());
                runtime.block_on(store.delete()).unwrap();
            }
            assert!(runtime.block_on(store.load()).unwrap().is_none());
            assert_eq!(
                calls(&root),
                2,
                "invalidated session must not be read again"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("calls")).unwrap(),
                "show -- Tuta/Bridge/nobody@example.invalid\nrm --force -- Tuta/Bridge/nobody@example.invalid\n"
            );
        }
        other => panic!("unknown probe {other}"),
    }
}

#[test]
fn missing_entry_allows_a_fresh_login() {
    run_probe("absent");
}

#[test]
fn readable_session_is_cached() {
    run_probe("success");
}

#[test]
fn gpg_error_is_visible_without_plaintext_and_can_be_retried() {
    run_probe("retry");
}

#[test]
fn missing_pass_executable_is_an_error_and_can_be_retried() {
    run_probe("missing-program");
}

#[test]
fn malformed_session_is_an_error_and_can_be_retried() {
    run_probe("invalid");
    run_probe("utf8");
}

#[test]
fn corrupt_session_can_be_replaced_only_after_explicit_login() {
    run_probe("replace-invalid");
    run_probe("replace-utf8");
}

#[test]
fn login_preserves_the_storage_error_type() {
    run_probe("login-error");
}

#[test]
fn login_rejects_a_store_for_another_account_before_reading_it() {
    run_probe("wrong-context");
}

#[test]
fn save_sends_legacy_json_over_stdin_and_caches_the_session() {
    run_probe("save");
}

#[test]
fn failed_save_preserves_the_entry_and_reports_no_plaintext() {
    run_probe("save-error");
}

#[test]
fn delete_removes_the_entry_and_missing_entries_are_successful() {
    run_probe("delete");
    run_probe("delete-absent");
}

#[test]
fn failed_delete_preserves_the_entry_without_reusing_its_token() {
    run_probe("delete-error");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_reports_gpg_error_without_prompting_for_a_tuta_password() {
    let fixture = Fixture::new("cli-error");
    let config_dir = fixture.0.join("config");
    std::fs::create_dir_all(config_dir.join("tutabridge")).unwrap();
    std::fs::write(
        config_dir.join("tutabridge/config.toml"),
        "email = 'nobody@example.invalid'\nimap_port = 1143\nsmtp_port = 1025\nbridge_password = 'dummy'\npass_path = 'Tuta/Bridge'\n",
    ).unwrap();
    std::fs::write(fixture.0.join("store").join(ENTRY), "dummy encrypted entry").unwrap();
    std::fs::write(fixture.0.join("mode"), "fail").unwrap();
    let output = fixture
        .command(env!("CARGO_BIN_EXE_tutabridge"))
        .arg("backup")
        .arg(fixture.0.join("export"))
        .env("XDG_CONFIG_HOME", config_dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gpg: decryption failed: No pinentry"),
        "{stderr}"
    );
    assert!(!stderr.contains("Password for"), "{stderr}");
    assert!(!stderr.contains("secret-token-must-not-be-logged"));
    assert_eq!(
        calls(&fixture.0),
        1,
        "CLI must not re-read after a storage error"
    );
}
