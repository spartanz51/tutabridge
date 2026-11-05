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

    pub(super) fn read(&self) -> Result<Option<Vec<u8>>, String> {
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
        if !output.status.success() {
            // Never include stdout: a failed command may still have emitted
            // some of the decrypted credentials.
            return Err(format!(
                "pass show exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(Some(output.stdout))
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
