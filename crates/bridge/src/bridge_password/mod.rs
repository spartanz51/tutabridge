//! Resolution and lifecycle of the local IMAP/SMTP/MCP bridge password.
//!
//! Keeping this separate from `config::load_config` ensures reading config
//! metadata (e.g. update checks, session fingerprints, settings saves) never
//! invokes external decryption tools like `gpg` and never copies an external
//! secret into `config.toml`.

mod authinfo;

use crate::config::{save_config, Config};
use rand::Rng;
use std::path::PathBuf;

/// Where the active bridge password came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Config,
    Authinfo(PathBuf),
}

/// A resolved bridge password together with its origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPassword {
    pub value: String,
    pub source: PasswordSource,
}

/// Resolve the active bridge password without mutating `cfg` or touching disk
/// for writes.
///
/// When `cfg.authinfo_path` is set, a matching entry wins over
/// `cfg.bridge_password`. An unreadable or undecryptable authinfo file is
/// always an error — never a silent fallback to `config.toml`.
pub fn resolve(cfg: &Config) -> Result<Option<ResolvedPassword>, String> {
    if let Some(path) = authinfo::configured_path(cfg)
        .map_err(|e| format!("Could not read the bridge password: {e}"))?
    {
        match authinfo::read_bridge_password(cfg, &path) {
            Ok(Some(value)) => {
                return Ok(Some(ResolvedPassword {
                    value,
                    source: PasswordSource::Authinfo(path),
                }));
            }
            Ok(None) => {
                log::info!(
                    "No bridge entry for {} in {}; using the password in config.toml",
                    cfg.email,
                    path.display()
                );
            }
            Err(e) => return Err(format!("Could not read the bridge password: {e}")),
        }
    }

    Ok(cfg
        .bridge_password
        .as_deref()
        .filter(|pw| !pw.is_empty())
        .map(|pw| ResolvedPassword {
            value: pw.to_owned(),
            source: PasswordSource::Config,
        }))
}

/// Return the active bridge password and whether a new one was generated.
///
/// If an authinfo entry provides the password, `cfg.bridge_password` and
/// `config.toml` are left untouched. Only when no password exists anywhere is
/// a new one generated and saved to `config.toml`.
pub fn ensure(cfg: &mut Config) -> Result<(ResolvedPassword, bool), Box<dyn std::error::Error>> {
    if let Some(resolved) = resolve(cfg)? {
        return Ok((resolved, false));
    }
    let password = generate_bridge_password();
    cfg.bridge_password = Some(password.clone());
    save_config(cfg)?;
    Ok((
        ResolvedPassword {
            value: password,
            source: PasswordSource::Config,
        },
        true,
    ))
}

/// Generate and persist a new bridge password in `config.toml`.
///
/// Refused while an authinfo entry provides the password, because a new value
/// in `config.toml` would immediately be shadowed by the authinfo entry.
pub fn regenerate(cfg: &mut Config) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(ResolvedPassword {
        source: PasswordSource::Authinfo(path),
        ..
    }) = resolve(cfg)?
    {
        return Err(format!(
            "The bridge password comes from {}: change it there.",
            path.display()
        )
        .into());
    }
    let password = generate_bridge_password();
    cfg.bridge_password = Some(password.clone());
    save_config(cfg)?;
    Ok(password)
}

/// Async wrapper around [`resolve`] that runs file I/O and `gpg` on a blocking
/// thread so async runtime workers are never stalled.
pub async fn resolve_async(cfg: Config) -> Result<Option<ResolvedPassword>, String> {
    tokio::task::spawn_blocking(move || resolve(&cfg))
        .await
        .map_err(|e| format!("Bridge password worker failed: {e}"))?
}

/// Async wrapper around [`ensure`] for async callers (GUI, service).
pub async fn ensure_async(mut cfg: Config) -> Result<(Config, ResolvedPassword, bool), String> {
    tokio::task::spawn_blocking(move || {
        let (password, is_new) = ensure(&mut cfg).map_err(|e| e.to_string())?;
        Ok((cfg, password, is_new))
    })
    .await
    .map_err(|e| format!("Bridge password worker failed: {e}"))?
}

/// Async wrapper around [`regenerate`] for async callers (GUI).
pub async fn regenerate_async(mut cfg: Config) -> Result<(Config, String), String> {
    tokio::task::spawn_blocking(move || {
        let password = regenerate(&mut cfg).map_err(|e| e.to_string())?;
        Ok((cfg, password))
    })
    .await
    .map_err(|e| format!("Bridge password worker failed: {e}"))?
}

fn generate_bridge_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    let mut group = || -> String {
        (0..5)
            .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
            .collect()
    };
    format!("{}-{}-{}-{}", group(), group(), group(), group())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_authinfo(tag: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tutabridge-bpw-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("authinfo");
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    #[test]
    fn authinfo_entry_wins_without_mutating_config() {
        let (dir, file) = temp_authinfo(
            "wins",
            "machine 127.0.0.1 port 1143 login me@tuta.io password from-authinfo\n",
        );
        let mut cfg = Config {
            email: "me@tuta.io".into(),
            bridge_password: Some("from-file".into()),
            authinfo_path: Some(file.display().to_string()),
            ..Config::default()
        };

        let resolved = resolve(&cfg).unwrap().unwrap();
        assert_eq!(resolved.value, "from-authinfo");
        assert_eq!(resolved.source, PasswordSource::Authinfo(file));
        assert_eq!(cfg.bridge_password.as_deref(), Some("from-file"));

        let (ensured, is_new) = ensure(&mut cfg).unwrap();
        assert_eq!(ensured.value, "from-authinfo");
        assert!(!is_new);
        assert_eq!(cfg.bridge_password.as_deref(), Some("from-file"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_does_not_mark_authinfo_password_as_newly_generated() {
        let (dir, file) = temp_authinfo(
            "nofilepw",
            "machine 127.0.0.1 login me@tuta.io password from-authinfo\n",
        );
        let mut cfg = Config {
            email: "me@tuta.io".into(),
            bridge_password: None,
            authinfo_path: Some(file.display().to_string()),
            ..Config::default()
        };

        let (pw, is_new) = ensure(&mut cfg).unwrap();
        assert_eq!(pw.value, "from-authinfo");
        assert!(!is_new);
        assert_eq!(cfg.bridge_password, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_match_falls_back_to_config_password() {
        let (dir, file) = temp_authinfo(
            "fallback",
            "machine 127.0.0.1 port 993 login me@tuta.io password other\n",
        );
        let cfg = Config {
            email: "me@tuta.io".into(),
            bridge_password: Some("from-file".into()),
            authinfo_path: Some(file.display().to_string()),
            ..Config::default()
        };

        let resolved = resolve(&cfg).unwrap().unwrap();
        assert_eq!(resolved.value, "from-file");
        assert_eq!(resolved.source, PasswordSource::Config);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_authinfo_is_an_error_not_a_fallback() {
        let cfg = Config {
            email: "me@tuta.io".into(),
            bridge_password: Some("from-file".into()),
            authinfo_path: Some("/nonexistent/tutabridge-missing-authinfo".into()),
            ..Config::default()
        };
        assert!(resolve(&cfg).is_err());
    }

    #[test]
    fn regenerate_is_refused_while_authinfo_provides_the_password() {
        let (dir, file) = temp_authinfo(
            "regen",
            "machine 127.0.0.1 login me@tuta.io password from-authinfo\n",
        );
        let mut cfg = Config {
            email: "me@tuta.io".into(),
            bridge_password: Some("from-file".into()),
            authinfo_path: Some(file.display().to_string()),
            ..Config::default()
        };

        let err = regenerate(&mut cfg).unwrap_err().to_string();
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert_eq!(cfg.bridge_password.as_deref(), Some("from-file"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
