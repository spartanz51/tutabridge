//! Integration tests for `tutabridge password`.
//!
//! Linux-only: they steer the config directory through `XDG_CONFIG_HOME`,
//! which the `dirs` crate honours only there.
//!
//! Each config names a `pass_path`, and the process sees an empty password
//! store and a PATH without `pass`, so the secret store is deterministically
//! absent: the password stays in (or is read from) `config.toml`, whatever
//! keyring the machine running the tests happens to have.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Command, Output};

fn run_password(config_home: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tutabridge"))
        .arg("password")
        .env("XDG_CONFIG_HOME", config_home)
        .env("PASSWORD_STORE_DIR", config_home.join("store"))
        .env("PATH", config_home.join("bin"))
        .output()
        .expect("failed to run tutabridge binary")
}

fn temp_config_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tutabridge_pw_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tutabridge")).unwrap();
    std::fs::create_dir_all(dir.join("store")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

#[test]
fn prints_the_stored_password_to_stdout() {
    let home = temp_config_home("stored");
    std::fs::write(
        home.join("tutabridge").join("config.toml"),
        "email = \"test@tuta.com\"\n\
         imap_port = 1143\n\
         smtp_port = 1025\n\
         bridge_password = \"AbCdE-FgHjK-mNpQr-StUvW\"\n\
         pass_path = \"Tuta/Bridge\"\n",
    )
    .unwrap();

    let out = run_password(&home);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "AbCdE-FgHjK-mNpQr-StUvW\n"
    );
    // With no store to move it to, the file keeps the password.
    let kept = std::fs::read_to_string(home.join("tutabridge").join("config.toml")).unwrap();
    assert!(kept.contains("AbCdE-FgHjK-mNpQr-StUvW"), "{kept}");

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn fails_when_no_password_has_been_generated() {
    let home = temp_config_home("nopw");
    std::fs::write(
        home.join("tutabridge").join("config.toml"),
        "email = \"test@tuta.com\"\n\
         imap_port = 1143\n\
         smtp_port = 1025\n\
         pass_path = \"Tuta/Bridge\"\n",
    )
    .unwrap();

    let out = run_password(&home);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No bridge password"));

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn fails_when_no_config_exists() {
    let home = temp_config_home("nocfg");

    let out = run_password(&home);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());

    std::fs::remove_dir_all(&home).unwrap();
}
