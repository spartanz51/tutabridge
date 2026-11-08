//! Integration tests for `tutabridge --version`.
//!
//! The version is printed before the configuration is read, so the command
//! never creates a config directory: every config location is pointed at an
//! empty directory, which has to stay empty.
use std::path::PathBuf;
use std::process::{Command, Output};

fn run(arg: &str, home: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tutabridge"))
        .arg(arg)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("APPDATA", home)
        .output()
        .expect("failed to run tutabridge binary")
}

fn empty_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tutabridge_version_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn prints_the_version_and_creates_nothing() {
    for (tag, arg) in [("long", "--version"), ("short", "-V"), ("word", "version")] {
        let home = empty_home(tag);
        let out = run(arg, &home);
        assert!(out.status.success(), "{arg} exited with {:?}", out.status);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("tutabridge {}\n", tutabridge_core::VERSION),
            "{arg}"
        );
        assert_eq!(
            std::fs::read_dir(&home).unwrap().count(),
            0,
            "{arg} wrote files"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}

#[test]
fn help_starts_with_the_version_and_lists_the_flag() {
    let home = empty_home("help");
    let out = run("--help", &home);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(&format!("tutabridge {}\n", tutabridge_core::VERSION)),
        "{text}"
    );
    assert!(text.contains("tutabridge --version"), "{text}");
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    std::fs::remove_dir_all(&home).unwrap();
}
