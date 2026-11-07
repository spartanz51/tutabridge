//! Filesystem helpers that keep secret-bearing files private to their owner.
//!
//! `config.toml` (bridge password), `key.pem` (local TLS private key) and
//! plaintext backup exports must not be readable by other local users.
//! `std::fs::write` honours the process umask (typically 022, i.e.
//! world-readable files), so these wrappers create files as 0600 and
//! directories as 0700 on Unix. On other platforms they fall back to the std
//! behaviour — Windows already scopes `%APPDATA%` to the user via ACLs.

use std::path::Path;

/// `std::fs::write`, but the file is created with mode 0600 on Unix. A
/// pre-existing file has its permissions tightened to 0600 as well, so
/// installs that predate this hardening are healed on their next write.
pub fn write_private(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies when the file is newly created.
        f.set_permissions(Permissions::from_mode(0o600))?;
        f.write_all(contents.as_ref())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// `std::fs::create_dir_all`, but directories it creates get mode 0700 on
/// Unix. Directories that already exist are left untouched.
pub fn create_dir_all_private(path: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Best-effort chmod 0600 of an existing secret-bearing file. No-op on
/// non-Unix platforms or when the file is absent. Called from load paths so
/// files written by versions that predate [`write_private`] get fixed up.
pub fn tighten(path: impl AsRef<Path>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Chmod 0700 of an existing, application-owned directory. Unlike [`tighten`],
/// errors are returned so callers do not open a cache whose directory could
/// not be protected. No-op on non-Unix platforms.
pub fn tighten_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_private_creates_0600() {
        let dir = std::env::temp_dir().join(format!("fs_private_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret.txt");

        write_private(&file, "s3cret").unwrap();
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "s3cret");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_private_tightens_existing_file() {
        let dir = std::env::temp_dir().join(format!("fs_private_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret.txt");

        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&file, "new").unwrap();
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_dir_all_private_creates_0700() {
        let base = std::env::temp_dir().join(format!("fs_private_test3_{}", std::process::id()));
        let nested = base.join("a").join("b");

        create_dir_all_private(&nested).unwrap();
        assert_eq!(mode_of(&nested), 0o700);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn tighten_dir_closes_an_existing_directory() {
        let dir = std::env::temp_dir().join(format!("fs_private_test4_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        tighten_dir(&dir).unwrap();
        assert_eq!(mode_of(&dir), 0o700);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tighten_dir_reports_errors() {
        let dir = std::env::temp_dir().join(format!(
            "fs_private_missing_{}_{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        assert_eq!(
            tighten_dir(&dir).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
