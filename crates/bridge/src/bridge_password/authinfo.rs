//! Read the bridge password from an `authinfo` / `netrc` file, the one
//! Emacs (Gnus, auth-source) and other mail clients already keep their
//! logins in. Read-only: the bridge never writes to the file.
//!
//! A `.gpg` file is decrypted with `gpg`, through the user's `gpg-agent`,
//! the same way `pass` reads its entries.

use crate::config::Config;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Hosts the bridge's local IMAP/SMTP servers listen on.
const LOCAL_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// Protocol aliases Emacs `auth-source` writes in `protocol` fields for mail.
const MAIL_PROTOCOLS: [&str; 4] = ["imap", "imaps", "smtp", "smtps"];

/// One `machine … login … password …` entry. Unrecognized fields are skipped
/// along with their values so a value is never mistaken for a keyword.
#[derive(Debug, Default, PartialEq, Eq)]
struct Entry {
    /// `None` for a `default` entry, which matches any host.
    machine: Option<String>,
    login: Option<String>,
    password: Option<String>,
    port: Option<String>,
    protocol: Option<String>,
}

/// Resolve `cfg.authinfo_path` with a leading `~/` expanded; `Ok(None)` when unset.
pub(crate) fn configured_path(cfg: &Config) -> Result<Option<PathBuf>, String> {
    let Some(raw) = cfg.authinfo_path.as_deref().map(str::trim) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    expand_tilde(raw).map(Some)
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    let rest = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"));
    match rest {
        Some(tail) => dirs::home_dir()
            .map(|home| home.join(tail))
            .ok_or_else(|| "cannot expand '~' in authinfo_path: home directory is unknown".into()),
        None => Ok(PathBuf::from(raw)),
    }
}

/// Read the bridge password from `path` for `cfg.email` and the bridge's ports.
/// `Ok(None)` means the file was read/decrypted cleanly but had no matching entry.
pub(crate) fn read_bridge_password(cfg: &Config, path: &Path) -> Result<Option<String>, String> {
    let text = read_file(path)?;
    Ok(find_password(
        &text,
        &cfg.email,
        &[cfg.imap_port, cfg.smtp_port],
    ))
}

fn is_gpg_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gpg"))
}

fn read_file(path: &Path) -> Result<String, String> {
    let shown = path.display();
    if is_gpg_path(path) {
        let out = Command::new("gpg")
            .args(["--quiet", "--decrypt", "--"])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run gpg to read {shown}: {e}"))?;
        if !out.status.success() {
            // Never include stdout: a failed run may still have printed
            // part of the decrypted file.
            return Err(format!(
                "gpg could not decrypt {shown} ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8(out.stdout).map_err(|_| format!("{shown} is not UTF-8"))
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {shown}: {e}"))
    }
}

fn host_matches(machine: Option<&str>) -> bool {
    machine.is_none_or(|m| LOCAL_HOSTS.iter().any(|h| m.eq_ignore_ascii_case(h)))
}

fn port_matches(port: Option<&str>, ports: &[u16]) -> bool {
    port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| ports.contains(&n)))
}

fn protocol_matches(protocol: Option<&str>, ports: &[u16]) -> bool {
    protocol.is_none_or(|proto| {
        MAIL_PROTOCOLS
            .iter()
            .any(|known| proto.eq_ignore_ascii_case(known))
            || proto.parse::<u16>().is_ok_and(|n| ports.contains(&n))
    })
}

fn find_password(text: &str, email: &str, ports: &[u16]) -> Option<String> {
    parse(text).into_iter().find_map(|entry| {
        let login_ok = entry
            .login
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(email));
        let matches = host_matches(entry.machine.as_deref())
            && login_ok
            && port_matches(entry.port.as_deref(), ports)
            && protocol_matches(entry.protocol.as_deref(), ports);
        matches
            .then_some(entry.password)
            .flatten()
            .filter(|pw| !pw.is_empty())
    })
}

/// Split the file into entries. The format is netrc's: whitespace-separated
/// `keyword value` pairs, a new entry at each `machine` or `default`,
/// `"…"` quoting with backslash escapes, and `#` comments.
fn parse(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    let mut tokens = tokenize(text).into_iter();
    while let Some(keyword) = tokens.next() {
        match keyword.as_str() {
            "machine" | "host" => {
                entries.extend(current.take());
                current = Some(Entry {
                    machine: tokens.next(),
                    ..Entry::default()
                });
            }
            "default" => {
                entries.extend(current.take());
                current = Some(Entry::default());
            }
            "login" | "user" | "account" => {
                let value = tokens.next();
                if let Some(entry) = current.as_mut() {
                    if keyword != "account" || entry.login.is_none() {
                        entry.login = value;
                    }
                }
            }
            "password" => {
                let value = tokens.next();
                if let Some(entry) = current.as_mut() {
                    entry.password = value;
                }
            }
            "port" => {
                let value = tokens.next();
                if let Some(entry) = current.as_mut() {
                    entry.port = value;
                }
            }
            "protocol" => {
                let value = tokens.next();
                if let Some(entry) = current.as_mut() {
                    entry.protocol = value;
                }
            }
            // An unknown keyword's value is a token of its own: skip it so
            // it is never taken for a keyword.
            _ if current.is_some() => {
                tokens.next();
            }
            _ => {}
        }
    }
    entries.extend(current);
    entries
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for line in text.lines() {
        let mut chars = line.chars().peekable();
        loop {
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            let Some(&first) = chars.peek() else { break };
            if first == '#' {
                break;
            }
            let mut token = String::new();
            if first == '"' {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => token.extend(chars.next()),
                        c => token.push(c),
                    }
                }
            } else {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    token.push(c);
                }
            }
            tokens.push(token);
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL: &str = "me@tuta.io";
    const PORTS: [u16; 2] = [1143, 1025];

    #[test]
    fn the_gnus_entry_for_the_bridge_is_found() {
        let text = "machine imap.example.com login me@tuta.io password other\n\
                    machine 127.0.0.1 login me@tuta.io port 1143 password s3cret\n";
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn the_smtp_entry_counts_too() {
        let text = "machine localhost port 1025 login me@tuta.io password s3cret";
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn an_entry_without_a_port_matches() {
        let text = "machine 127.0.0.1 login me@tuta.io password s3cret";
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn protocol_does_not_clobber_port_and_mail_protocols_match() {
        for text in [
            "machine 127.0.0.1 port 1143 protocol imap login me@tuta.io password s3cret",
            "machine 127.0.0.1 protocol smtp port 1025 login me@tuta.io password s3cret",
            "machine 127.0.0.1 protocol imap login me@tuta.io password s3cret",
            "machine 127.0.0.1 protocol 1143 login me@tuta.io password s3cret",
        ] {
            assert_eq!(
                find_password(text, EMAIL, &PORTS).as_deref(),
                Some("s3cret"),
                "{text}"
            );
        }
        for rejected in [
            "machine 127.0.0.1 port 993 protocol imap login me@tuta.io password x",
            "machine 127.0.0.1 protocol http login me@tuta.io password x",
            "machine 127.0.0.1 protocol 993 login me@tuta.io password x",
        ] {
            assert_eq!(find_password(rejected, EMAIL, &PORTS), None, "{rejected}");
        }
    }

    #[test]
    fn another_port_host_login_or_empty_password_does_not_match() {
        for text in [
            "machine 127.0.0.1 port 993 login me@tuta.io password x",
            "machine mail.tuta.io login me@tuta.io password x",
            "machine 127.0.0.1 login you@tuta.io password x",
            "machine 127.0.0.1 password x",
            r#"machine 127.0.0.1 login me@tuta.io password """#,
        ] {
            assert_eq!(find_password(text, EMAIL, &PORTS), None, "{text}");
        }
    }

    #[test]
    fn quoted_values_and_escapes_are_read() {
        let text = r#"machine 127.0.0.1 login "me@tuta.io" password "a b\"c\\d""#;
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some(r#"a b"c\d"#)
        );
    }

    #[test]
    fn comments_and_multi_line_entries() {
        let text = "# bridge\nmachine 127.0.0.1\n  login me@tuta.io # me\n  password s3cret\n";
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn a_default_entry_matches_any_host() {
        let text =
            "machine other login me@tuta.io password no\ndefault login me@tuta.io password yes";
        assert_eq!(find_password(text, EMAIL, &PORTS).as_deref(), Some("yes"));
    }

    #[test]
    fn an_unknown_keyword_value_is_not_read_as_a_keyword() {
        let text = "machine 127.0.0.1 note password login me@tuta.io password s3cret";
        assert_eq!(
            find_password(text, EMAIL, &PORTS).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn gpg_extension_check_is_case_insensitive() {
        assert!(is_gpg_path(Path::new("/tmp/authinfo.gpg")));
        assert!(is_gpg_path(Path::new("/tmp/authinfo.GPG")));
        assert!(!is_gpg_path(Path::new("/tmp/authinfo")));
    }

    #[test]
    fn a_tilde_path_is_expanded_and_unset_or_blank_is_none() {
        let mut cfg = Config::default();
        assert_eq!(configured_path(&cfg).unwrap(), None);
        cfg.authinfo_path = Some("  ".into());
        assert_eq!(configured_path(&cfg).unwrap(), None);
        cfg.authinfo_path = Some("~/.authinfo.gpg".into());
        let path = configured_path(&cfg).unwrap().unwrap();
        assert!(path.ends_with(".authinfo.gpg") && !path.starts_with("~"));
    }

    #[test]
    fn a_plain_file_is_read_and_a_missing_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "tutabridge-authinfo-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("authinfo");
        std::fs::write(
            &file,
            "machine 127.0.0.1 login me@tuta.io password s3cret\n",
        )
        .unwrap();
        let cfg = Config {
            email: EMAIL.into(),
            ..Config::default()
        };
        assert_eq!(
            read_bridge_password(&cfg, &file).unwrap().as_deref(),
            Some("s3cret")
        );
        assert!(read_bridge_password(&cfg, &dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
