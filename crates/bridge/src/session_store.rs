//! Session storage. Desktop: OS keychain via `keyring`.
//! Android/Termux: file encrypted through OpenKeychain with `okc-gpg`.
use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    NoEntry,
    Other(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NoEntry => write!(f, "no stored session"),
            StoreError::Other(s) => write!(f, "{s}"),
        }
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::StoreError;

    fn conv(e: keyring::Error) -> StoreError {
        match e {
            keyring::Error::NoEntry => StoreError::NoEntry,
            other => StoreError::Other(other.to_string()),
        }
    }
    pub fn set(service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        keyring::Entry::new(service, account)
            .map_err(conv)?
            .set_password(secret)
            .map_err(conv)
    }
    pub fn get(service: &str, account: &str) -> Result<String, StoreError> {
        keyring::Entry::new(service, account)
            .map_err(conv)?
            .get_password()
            .map_err(conv)
    }
    pub fn delete(service: &str, account: &str) -> Result<(), StoreError> {
        keyring::Entry::new(service, account)
            .map_err(conv)?
            .delete_credential()
            .map_err(conv)
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::StoreError;
    use std::io::{ErrorKind, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    fn other<E: std::fmt::Display>(e: E) -> StoreError {
        StoreError::Other(e.to_string())
    }

    fn session_path(account: &str) -> Result<PathBuf, StoreError> {
        let dir = match std::env::var_os("TUTABRIDGE_SESSION_DIR") {
            Some(d) => PathBuf::from(d),
            None => {
                let home = std::env::var_os("HOME").ok_or_else(|| other("HOME not set"))?;
                PathBuf::from(home).join(".config").join("tutabridge")
            }
        };
        std::fs::create_dir_all(&dir).map_err(other)?;
        Ok(dir.join(format!("session-{}.gpg", hex::encode(account.as_bytes()))))
    }

    fn key_id() -> Result<String, StoreError> {
        std::env::var("TUTABRIDGE_OKC_KEY")
            .map_err(|_| other("set TUTABRIDGE_OKC_KEY to your OpenKeychain key id"))
    }

    // Runs `timeout <secs> okc-gpg <args>`; the generous default leaves
    // time for the OpenKeychain prompt.
    fn run_okc(args: &[&str], input: &[u8]) -> Result<Vec<u8>, StoreError> {
        let secs = std::env::var("TUTABRIDGE_OKC_TIMEOUT").unwrap_or_else(|_| "900".into());
        let mut child = Command::new("timeout")
            .arg(&secs)
            .arg("okc-gpg")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| other(format!("could not run okc-gpg: {e}")))?;
        {
            let mut stdin = child.stdin.take().ok_or_else(|| other("no stdin"))?;
            stdin.write_all(input).map_err(other)?;
        }
        let out = child.wait_with_output().map_err(other)?;
        if !out.status.success() {
            return Err(other(format!(
                "okc-gpg failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if out.stdout.is_empty() {
            return Err(other("okc-gpg produced no output"));
        }
        Ok(out.stdout)
    }

    pub fn set(_service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        let path = session_path(account)?;
        let key = key_id()?;
        let enc = run_okc(&["-e", "-r", &key], secret.as_bytes())?;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(other)?;
        f.write_all(&enc).map_err(other)?;
        f.sync_all().map_err(other)?;
        std::fs::rename(&tmp, &path).map_err(other)
    }

    pub fn get(_service: &str, account: &str) -> Result<String, StoreError> {
        let path = session_path(account)?;
        let enc = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(StoreError::NoEntry),
            Err(e) => return Err(other(e)),
        };
        let plain = run_okc(&["-d"], &enc)?;
        String::from_utf8(plain).map_err(other)
    }

    pub fn delete(_service: &str, account: &str) -> Result<(), StoreError> {
        match std::fs::remove_file(session_path(account)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(StoreError::NoEntry),
            Err(e) => Err(other(e)),
        }
    }
}

pub use imp::{delete, get, set};
