//! Each probe has its own process, cache, PATH and dummy password store.
//! No real keyring, GPG key or Tuta account is used.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use tutabridge_core::{config::Config, tuta};

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
[ "$1" = show ] && [ "$2" = -- ] && [ "$3" = Tuta/Bridge/nobody@example.invalid ] || exit 91
mode=$(/bin/cat "$PASS_TEST_ROOT/mode")
case "$mode" in
  fail)
    /bin/cat "$PASS_TEST_ROOT/session"
    printf '%s\n' 'gpg: decryption failed: No pinentry' >&2
    exit 2 ;;
  invalid) printf '%s\n' 'secret-token-must-not-be-logged'; exit 0 ;;
  utf8) printf '\377'; exit 0 ;;
  *) /bin/cat "$PASS_TEST_ROOT/session" ;;
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
}

fn calls(root: &Path) -> usize {
    std::fs::read_to_string(root.join("calls"))
        .unwrap_or_default()
        .lines()
        .count()
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
    if scenario == "absent" {
        assert!(!tuta::has_saved_session(&cfg).unwrap());
        assert_eq!(calls(&root), 0);
        return;
    }
    std::fs::write(root.join("store").join(ENTRY), "dummy encrypted entry").unwrap();
    match scenario.as_str() {
        "success" => {
            assert!(tuta::has_saved_session(&cfg).unwrap());
            assert!(tuta::has_saved_session(&cfg).unwrap());
            assert_eq!(calls(&root), 1, "successful reads are cached");
        }
        "retry" => {
            std::fs::write(root.join("mode"), "fail").unwrap();
            let error = tuta::has_saved_session(&cfg).unwrap_err().to_string();
            assert!(
                error.contains("gpg: decryption failed: No pinentry"),
                "{error}"
            );
            assert!(!error.contains("secret-token-must-not-be-logged"));
            std::fs::write(root.join("mode"), "ok").unwrap();
            assert!(tuta::has_saved_session(&cfg).unwrap());
            assert_eq!(calls(&root), 2, "errors must not poison the cache");
        }
        "missing-program" => {
            std::fs::rename(root.join("bin/pass"), root.join("bin/pass.disabled")).unwrap();
            assert!(tuta::has_saved_session(&cfg)
                .unwrap_err()
                .to_string()
                .contains("cannot run pass"));
            std::fs::rename(root.join("bin/pass.disabled"), root.join("bin/pass")).unwrap();
            assert!(tuta::has_saved_session(&cfg).unwrap());
        }
        "invalid" | "utf8" => {
            std::fs::write(root.join("mode"), &scenario).unwrap();
            let error = tuta::has_saved_session(&cfg).unwrap_err().to_string();
            assert!(error.contains("session"));
            assert!(!error.contains("secret-token-must-not-be-logged"));
            std::fs::write(root.join("mode"), "ok").unwrap();
            assert!(tuta::has_saved_session(&cfg).unwrap());
            assert_eq!(calls(&root), 2);
        }
        "login-error" => {
            std::fs::write(root.join("mode"), "fail").unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            // Even an explicitly supplied password must not overwrite a
            // session whose store could not be read. No SDK/network needed.
            let error = match runtime.block_on(tuta::login_with_2fa(&cfg, Some("unused"), None)) {
                Ok(_) => panic!("storage failure should prevent login"),
                Err(error) => error,
            };
            assert!(error.is::<tuta::SessionStoreError>());
            assert!(error.to_string().contains("No pinentry"));
            assert_eq!(calls(&root), 1);
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
fn login_preserves_the_storage_error_type() {
    run_probe("login-error");
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
