//! Run the real CLI with disposable config and a fake pass, never real secrets.
#![cfg(all(unix, not(target_os = "android")))]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("tutabridge-cli-{name}-{}", std::process::id()));
        fs::create_dir_all(root.join("bin")).unwrap();
        let base = if cfg!(target_os = "macos") {
            root.join("Library/Application Support")
        } else {
            root.join("config")
        };
        let config = base.join("tutabridge/config.toml");
        Self { root, config }
    }

    fn configure(&self, email: &str) {
        fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        fs::write(&self.config, format!(
            "email = {email:?}\nimap_port = 1143\nsmtp_port = 1025\npass_path = 'Docker/Session'\nbind_addr = '0.0.0.0'\n"
        )).unwrap();
    }

    fn fake_pass(&self, script: &str) {
        let program = self.root.join("bin/pass");
        fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let entry = self
            .root
            .join("pass/Docker/Session/test@example.invalid.gpg");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(entry, "encrypted-placeholder").unwrap();
    }

    fn run(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tutabridge"))
            .args(args)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("PASSWORD_STORE_DIR", self.root.join("pass"))
            .env("PATH", self.root.join("bin"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI waited instead of failing without a prompt");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_failure(output: &Output, diagnostic: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stderr.contains(diagnostic), "{stderr}");
    for prompt in ["Tuta email address:", "Password for", "TOTP code:"] {
        assert!(!stdout.contains(prompt), "{stdout}");
        assert!(!stderr.contains(prompt), "{stderr}");
    }
}

#[test]
fn service_without_an_account_fails_without_creating_config() {
    let fixture = Fixture::new("no-account");
    assert_failure(
        &fixture.run(&["run", "--non-interactive"], ""),
        "tutabridge init",
    );
    assert!(!fixture.config.exists());
}

#[test]
fn service_without_a_session_fails_before_creating_runtime_secrets() {
    let fixture = Fixture::new("no-session");
    fixture.configure("test@example.invalid");
    assert_failure(
        &fixture.run(&["run", "--non-interactive"], ""),
        "No saved session",
    );
    assert_eq!(
        fs::read_dir(fixture.config.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    assert!(!fs::read_to_string(&fixture.config)
        .unwrap()
        .contains("bridge_password"));
}

#[test]
fn service_reports_decryption_and_corruption_without_password_fallback() {
    for (name, script, error) in [
        (
            "gpg-error",
            "echo 'test key is locked' >&2; exit 9",
            "test key is locked",
        ),
        (
            "corrupt",
            "echo 'private-invalid-session'",
            "invalid session",
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.configure("test@example.invalid");
        fixture.fake_pass(script);
        let output = fixture.run(&["run", "--non-interactive"], "");
        assert_failure(&output, error);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-invalid-session"));
        assert_eq!(
            fs::read_to_string(
                fixture
                    .root
                    .join("pass/Docker/Session/test@example.invalid.gpg")
            )
            .unwrap(),
            "encrypted-placeholder"
        );
    }
}

#[test]
fn initialization_fills_only_the_missing_account_in_a_template() {
    let fixture = Fixture::new("template");
    fixture.configure("");
    fixture.fake_pass("echo 'test key is locked' >&2; exit 9");
    // A decryption failure stops init before any real login or password prompt.
    let output = fixture.run(&["init"], "test@example.invalid\n");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("test key is locked"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Password for"));
    let config = fs::read_to_string(&fixture.config).unwrap();
    for expected in ["test@example.invalid", "Docker/Session", "0.0.0.0"] {
        assert!(config.contains(expected), "{config}");
    }
    assert!(!config.contains("bridge_password"));
}

#[test]
fn unknown_command_never_enters_interactive_setup() {
    let fixture = Fixture::new("bad-command");
    assert_failure(&fixture.run(&["rn"], ""), "Invalid arguments");
    assert!(!fixture.config.exists());
}
