use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) struct PassStore {
    entry: String,
    directory: Option<PathBuf>,
}

impl PassStore {
    pub(super) fn new(entry: String) -> Self {
        Self {
            entry,
            directory: store_directory(std::env::var_os("PASSWORD_STORE_DIR"), dirs::home_dir()),
        }
    }

    pub(super) fn entry(&self) -> &str {
        &self.entry
    }

    fn directory(&self) -> Result<&Path, String> {
        self.directory
            .as_deref()
            .ok_or_else(|| "cannot locate the password store".to_owned())
    }

    fn entry_exists(&self) -> Result<bool, String> {
        self.directory()?
            .join(format!("{}.gpg", self.entry))
            .try_exists()
            .map_err(|error| format!("cannot inspect pass entry: {error}"))
    }

    fn command(&self) -> Result<Command, String> {
        let mut command = Command::new("pass");
        // The existence checks and subprocesses must use the same store, even
        // if the process environment changes after this instance was created.
        command.env("PASSWORD_STORE_DIR", self.directory()?);
        Ok(command)
    }

    pub(super) fn read(&self) -> Result<Option<super::SecretBytes>, String> {
        // A nonzero exit status cannot distinguish a missing entry from a GPG
        // failure. Only a confirmed missing file means there is no session.
        if !self.entry_exists()? {
            return Ok(None);
        }
        let output = self
            .command()?
            .args(["show", "--", &self.entry])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run pass: {error}"))?;
        // Own stdout before looking at the status: a failed command may still
        // have emitted some of the decrypted credentials, which must be wiped
        // and never included in the error.
        let std::process::Output {
            status,
            stdout,
            stderr,
        } = output;
        let stdout = zeroize::Zeroizing::new(stdout);
        if !status.success() {
            return Err(format!(
                "pass show exited with {status}: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        Ok(Some(stdout))
    }

    pub(super) fn write(&self, secret: &str) -> Result<(), String> {
        let mut child = self
            .command()?
            .args(["insert", "--multiline", "--force", "--", &self.entry])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run pass: {error}"))?;

        // Closing stdin ends `insert`; also reap the child if writing fails.
        let input_result = child
            .stdin
            .take()
            .ok_or_else(|| "pass has no stdin".to_owned())
            .and_then(|mut stdin| {
                stdin
                    .write_all(secret.as_bytes())
                    .map_err(|error| format!("cannot write to pass: {error}"))
            });
        if let Err(error) = input_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "pass insert exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    pub(super) fn write_preserving(&self, secret: &str) -> Result<(), String> {
        self.write_preserving_with(secret, |pending, value| {
            pending.write(value)?;
            // A recipient may have a public key but no usable private key.
            // Never replace a readable session with ciphertext we cannot read.
            let verified = pending
                .read()
                .map_err(|_| "Encrypted session could not be decrypted for verification")?;
            if verified.as_ref().map(|bytes| bytes.as_slice()) != Some(value.as_bytes()) {
                return Err("Encrypted session failed read-back verification".into());
            }
            Ok(())
        })
    }

    fn write_preserving_with(
        &self,
        secret: &str,
        write: impl FnOnce(&Self, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        // Caller holds the volume lock. Encrypt in an isolated sibling store:
        // pass must never auto-commit a temporary name in the real Git store.
        if !Path::new(&self.entry)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return Err("Invalid password-store entry path".into());
        }
        let directory = self.directory()?;
        let relative = Path::new(&self.entry).parent().unwrap();
        let parent = directory.join(relative);
        create_durable_directories(&parent).map_err(|e| e.to_string())?;
        // Interrupted encryption leaves only our narrowly named ciphertext
        // staging files. No recursive sweep of user-owned pass entries.
        for item in std::fs::read_dir(&parent).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if let Some(suffix) = name.strip_prefix(".tutabridge-pending-") {
                let hex = suffix.strip_suffix(".gpg").unwrap_or(suffix);
                if hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    let kind = item.file_type().map_err(|e| e.to_string())?;
                    let removed = if kind.is_file() {
                        std::fs::remove_file(item.path())
                    } else if kind.is_dir() {
                        std::fs::remove_dir_all(item.path())
                    } else {
                        Ok(())
                    };
                    if removed.is_err() {
                        log::warn!("Cannot remove an interrupted encrypted session; continuing with a new staging directory");
                    }
                }
            }
        }
        let staging = parent.join(format!(
            ".tutabridge-pending-{:016x}",
            rand::random::<u64>()
        ));
        let mut options = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            options.mode(0o700);
        }
        options.create(&staging).map_err(|e| e.to_string())?;
        let result = (|| {
            let mut recipient = parent.as_path();
            while !recipient.join(".gpg-id").is_file() && recipient != directory {
                recipient = recipient.parent().ok_or("Missing pass recipient")?;
            }
            std::fs::copy(recipient.join(".gpg-id"), staging.join(".gpg-id"))
                .map_err(|e| e.to_string())?;
            if recipient.join(".gpg-id.sig").exists() {
                std::fs::copy(recipient.join(".gpg-id.sig"), staging.join(".gpg-id.sig"))
                    .map_err(|e| e.to_string())?;
            }
            let pending = Self {
                entry: "session".into(),
                directory: Some(staging.clone()),
            };
            write(&pending, secret)?;
            let temporary = staging.join("session.gpg");
            if !std::fs::symlink_metadata(&temporary)
                .map_err(|e| e.to_string())?
                .is_file()
            {
                return Err("Invalid encrypted output".into());
            }
            let file = std::fs::File::open(&temporary).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            let target = directory.join(format!("{}.gpg", self.entry));
            std::fs::rename(&temporary, &target).map_err(|e| e.to_string())?;
            // Rename is the commit point. Report later durability/history issues
            // explicitly without pretending the old session is still present.
            #[cfg(unix)]
            if std::fs::File::open(&parent)
                .and_then(|f| f.sync_all())
                .is_err()
            {
                log::warn!("Session saved, but password-store directory sync failed");
            }
            self.commit_saved_entry(&target);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        #[cfg(unix)]
        let _ = std::fs::File::open(&parent).and_then(|f| f.sync_all());
        result
    }

    fn git_command(&self, parent: &Path) -> Result<Command, String> {
        // Match pass's Git discovery boundary and environment isolation. A
        // non-Git store inside a dotfiles repository must never commit there.
        let root = self
            .directory()?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let ceiling = root.parent().ok_or("Password store has no parent")?;
        let ceiling = std::env::join_paths([ceiling])
            .map_err(|_| "Cannot represent the password-store Git boundary")?;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(parent)
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null());
        Ok(command)
    }

    fn commit_saved_entry(&self, target: &Path) {
        let parent = target.parent().unwrap();
        let result = (|| -> Result<(), String> {
            if !self
                .git_command(parent)?
                .args(["rev-parse", "--is-inside-work-tree"])
                .output()
                .is_ok_and(|o| o.status.success())
            {
                return Ok(());
            }
            // Commit only this entry, preserving other staged user changes.
            let filename = target.file_name().unwrap();
            let signed = self
                .git_command(parent)?
                .args(["config", "--bool", "--get", "pass.signcommits"])
                .output()
                .is_ok_and(|o| o.stdout == b"true\n");
            let added = self
                .git_command(parent)?
                .args(["add", "--"])
                .arg(filename)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let mut commit = self.git_command(parent)?;
            commit.args(["commit", "--only", "-m", "Update TutaBridge session"]);
            if signed {
                commit.arg("-S");
            }
            commit
                .arg("--")
                .arg(filename)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if !added.is_ok_and(|s| s.success()) || !commit.status().is_ok_and(|s| s.success()) {
                return Err("Git update failed".into());
            }
            Ok(())
        })();
        if result.is_err() {
            log::warn!(
                "Session saved, but pass Git history update failed; inspect the password store"
            );
        }
    }

    pub(super) fn delete(&self) -> Result<(), String> {
        if !self.entry_exists()? {
            return Ok(());
        }
        let status = self
            .command()?
            .args(["rm", "--force", "--", &self.entry])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map_err(|error| format!("cannot run pass: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("pass rm exited with {status}"))
        }
    }
}

fn create_durable_directories(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.is_dir() && !current.as_os_str().is_empty() {
        missing.push(current);
        current = current
            .parent()
            .ok_or_else(|| std::io::Error::other("Missing store parent"))?;
    }
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(e) => return Err(e),
        }
        #[cfg(unix)]
        {
            std::fs::File::open(directory)?.sync_all()?;
            let parent = directory
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

fn store_directory(explicit: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    explicit
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|directory| directory.join(".password-store")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_store_directory_takes_precedence_over_home() {
        assert_eq!(
            store_directory(Some("custom store".into()), Some("home".into())),
            Some(PathBuf::from("custom store"))
        );
    }

    #[test]
    fn empty_or_unset_store_directory_falls_back_to_home() {
        for explicit in [None, Some(OsString::new())] {
            assert_eq!(
                store_directory(explicit, Some("home".into())),
                Some(PathBuf::from("home").join(".password-store"))
            );
        }
        assert_eq!(store_directory(None, None), None);
    }

    #[test]
    fn commands_use_the_captured_store_directory() {
        let store = PassStore {
            entry: "Tuta/Bridge/me@tuta.io".into(),
            directory: Some("captured directory".into()),
        };
        let command = store.command().unwrap();
        assert_eq!(command.get_program(), "pass");
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            vec![(
                std::ffi::OsStr::new("PASSWORD_STORE_DIR"),
                Some(std::ffi::OsStr::new("captured directory"))
            )]
        );
    }
}

#[cfg(test)]
mod preservation_tests {
    use super::*;
    #[test]
    fn partial_encryption_failure_preserves_old_bytes_and_success_replaces_them() {
        let root = std::env::temp_dir().join(format!("pass-atomic-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&root).unwrap();
        let store = PassStore {
            entry: "session".into(),
            directory: Some(root.clone()),
        };
        let target = root.join("session.gpg");
        std::fs::write(&target, b"original ciphertext").unwrap();
        std::fs::write(root.join(".gpg-id"), b"test recipient").unwrap();
        let fail = store.write_preserving_with("new secret", |pending, _| {
            let path = pending
                .directory()
                .unwrap()
                .join(format!("{}.gpg", pending.entry));
            std::fs::write(path, b"partial ciphertext").unwrap();
            Err("encryption failed".into())
        });
        assert!(fail.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original ciphertext");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        store
            .write_preserving_with("new secret", |pending, _| {
                std::fs::write(
                    pending
                        .directory()
                        .unwrap()
                        .join(format!("{}.gpg", pending.entry)),
                    b"complete ciphertext",
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"complete ciphertext");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unversioned_store_does_not_commit_in_parent_repository() {
        let root =
            std::env::temp_dir().join(format!("pass-git-boundary-{:016x}", rand::random::<u64>()));
        let directory = root.join("store");
        std::fs::create_dir_all(&directory).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = PassStore {
            entry: "nested/session".into(),
            directory: Some(directory.clone()),
        };
        std::fs::write(directory.join(".gpg-id"), "fixture").unwrap();
        store
            .write_preserving_with("fixture", |pending, _| {
                std::fs::write(pending.directory()?.join("session.gpg"), "ciphertext")
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        let command = store.git_command(&directory).unwrap();
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            assert_eq!(env[std::ffi::OsStr::new(name)], None);
        }
        assert_eq!(
            env[std::ffi::OsStr::new("GIT_CEILING_DIRECTORIES")],
            Some(root.canonicalize().unwrap().as_os_str())
        );
        assert!(!root.join("hook-ran").exists());
        assert!(!Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .unwrap()
            .status
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap()
            .stdout
            .is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated process with real pass/GPG and disposable GNUPGHOME"]
    fn real_pass_atomic_roundtrip_and_git_history() {
        let root = PathBuf::from(
            std::env::var_os("TUTABRIDGE_REAL_PASS_ROOT")
                .expect("explicit disposable root required"),
        );
        assert_eq!(root.parent(), Some(Path::new("/tmp")));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("tutabridge-real-pass-"));
        let directory = root.join("store");
        assert_eq!(
            std::env::var_os("PASSWORD_STORE_DIR").unwrap(),
            directory.as_os_str()
        );
        let store = PassStore {
            entry: "nested/nobody@example.invalid".into(),
            directory: Some(directory.clone()),
        };
        store.write_preserving("synthetic first session").unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().as_slice(),
            b"synthetic first session"
        );
        let target = directory.join(format!("{}.gpg", store.entry));
        let original = std::fs::read(&target).unwrap();
        let recipient = std::fs::read(directory.join(".gpg-id")).unwrap();
        std::fs::write(
            directory.join(".gpg-id"),
            b"nonexistent-recipient@example.invalid\n",
        )
        .unwrap();
        assert!(store
            .write_preserving("must not replace old session")
            .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), original);
        std::fs::write(directory.join(".gpg-id"), recipient).unwrap();
        let nested_recipient = target.parent().unwrap().join(".gpg-id");
        std::fs::copy(root.join("public-only-recipient"), &nested_recipient).unwrap();
        let failure = store
            .write_preserving("unreadable replacement")
            .unwrap_err();
        assert!(
            failure == "Encrypted session could not be decrypted for verification",
            "encryption must succeed before read-back rejects the public-only recipient"
        );
        assert_eq!(std::fs::read(&target).unwrap(), original);
        std::fs::remove_file(nested_recipient).unwrap();
        // Simulate SIGKILL leftovers and an unrelated staged user change.
        std::fs::write(
            target
                .parent()
                .unwrap()
                .join(".tutabridge-pending-0123456789abcdef.gpg"),
            b"partial encrypted bytes",
        )
        .unwrap();
        std::fs::write(directory.join("unrelated"), b"leave staged").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&directory)
            .args(["add", "unrelated"])
            .status()
            .unwrap()
            .success());
        store
            .write_preserving("synthetic replacement session")
            .unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().as_slice(),
            b"synthetic replacement session"
        );
        assert!(std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tutabridge-pending-")));
        let status = Command::new("git")
            .arg("-C")
            .arg(&directory)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(status.stdout).unwrap(), "A  unrelated\n");
        let history = Command::new("git")
            .arg("-C")
            .arg(&directory)
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .output()
            .unwrap();
        assert!(!String::from_utf8(history.stdout)
            .unwrap()
            .contains("pending"));
    }
}
