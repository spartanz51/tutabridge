//! Integration tests for `tutabridge password`.
//!
//! Linux-only: they steer the config directory through `XDG_CONFIG_HOME`,
//! which the `dirs` crate honours only there.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Command, Output};

fn run_password(config_home: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tutabridge"))
        .arg("password")
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .expect("failed to run tutabridge binary")
}

fn temp_config_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tutabridge_pw_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tutabridge")).unwrap();
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
         bridge_password = \"AbCdE-FgHjK-mNpQr-StUvW\"\n",
    )
    .unwrap();

    let out = run_password(&home);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "AbCdE-FgHjK-mNpQr-StUvW\n"
    );

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn fails_when_no_password_has_been_generated() {
    let home = temp_config_home("nopw");
    std::fs::write(
        home.join("tutabridge").join("config.toml"),
        "email = \"test@tuta.com\"\n\
         imap_port = 1143\n\
         smtp_port = 1025\n",
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
/// `tutabridge password` with only the fake `gpg` below on PATH.
fn run_password_with_fake_gpg(config_home: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tutabridge"))
        .arg("password")
        .env("XDG_CONFIG_HOME", config_home)
        .env("PATH", config_home.join("bin"))
        .output()
        .expect("failed to run tutabridge binary")
}

/// A fake `gpg` that "decrypts" by printing the file, or fails like a
/// locked agent when the file holds `LOCKED`.
fn install_fake_gpg(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(home.join("bin")).unwrap();
    let gpg = home.join("bin").join("gpg");
    std::fs::write(
        &gpg,
        "#!/bin/sh\n\
         [ \"$1 $2 $3\" = \"--quiet --decrypt --\" ] || exit 91\n\
         if /bin/grep -q LOCKED \"$4\"; then\n\
         \x20 echo 'gpg: decryption failed: No secret key' >&2; exit 2\n\
         fi\n\
         /bin/cat \"$4\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_authinfo_config(home: &std::path::Path, authinfo: &str) {
    std::fs::write(home.join("authinfo.gpg"), authinfo).unwrap();
    std::fs::write(
        home.join("tutabridge").join("config.toml"),
        format!(
            "email = \"test@tuta.com\"\n\
             imap_port = 1143\n\
             smtp_port = 1025\n\
             bridge_password = \"from-config-file\"\n\
             authinfo_path = \"{}\"\n",
            home.join("authinfo.gpg").display()
        ),
    )
    .unwrap();
}

#[test]
fn the_authinfo_entry_wins_and_the_config_is_left_alone() {
    let home = temp_config_home("authinfo");
    install_fake_gpg(&home);
    write_authinfo_config(
        &home,
        "machine imap.example.com login test@tuta.com password nope\n\
         machine 127.0.0.1 port 1143 login test@tuta.com password from-authinfo\n",
    );
    let before = std::fs::read_to_string(home.join("tutabridge").join("config.toml")).unwrap();

    let out = run_password_with_fake_gpg(&home);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-authinfo\n");
    let after = std::fs::read_to_string(home.join("tutabridge").join("config.toml")).unwrap();
    assert_eq!(before, after);

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn without_a_matching_authinfo_entry_the_usual_store_is_used() {
    let home = temp_config_home("authinfo_nomatch");
    install_fake_gpg(&home);
    write_authinfo_config(
        &home,
        "machine 127.0.0.1 port 993 login test@tuta.com password x\n",
    );

    let out = run_password_with_fake_gpg(&home);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-config-file\n");

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn an_authinfo_gpg_cannot_decrypt_is_an_error_not_a_fallback() {
    let home = temp_config_home("authinfo_locked");
    install_fake_gpg(&home);
    write_authinfo_config(&home, "LOCKED\n");

    let out = run_password_with_fake_gpg(&home);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No secret key"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&home).unwrap();
}
