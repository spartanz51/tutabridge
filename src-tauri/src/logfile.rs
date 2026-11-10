//! The desktop app's log file.
//!
//! A user who starts the app from Finder, the Start menu or a desktop launcher
//! never sees stderr, so every log line also goes to a file in the platform's
//! log folder. The tray's "Show logs" item reveals it, which still works when
//! the window itself cannot render.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_NAME: &str = "tutabridge.log";
/// Past this size the file is moved to `tutabridge.log.1` (replacing the
/// previous one) and a new one is started, so the logs never exceed twice this.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// `~/Library/Logs/TutaBridge` on macOS, where Console.app also looks; the
/// local data folder elsewhere (`~/.local/share/tutabridge/logs`,
/// `%LOCALAPPDATA%\tutabridge\logs`).
pub fn dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join("Library/Logs/TutaBridge"))
    } else {
        dirs::data_local_dir().map(|data| data.join("tutabridge").join("logs"))
    }
}

pub fn path() -> Option<PathBuf> {
    dir().map(|dir| dir.join(FILE_NAME))
}

/// Install the logger: every record goes to stderr, as before, and to the
/// log file when it can be opened.
pub fn init(default_filter: &str) {
    let file = path().and_then(|path| match RotatingFile::open(path) {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!("TutaBridge: no log file ({error}), logging to stderr only");
            None
        }
    });
    let logger = build(default_filter, file);
    log::set_max_level(logger.filter());
    if let Err(error) = log::set_boxed_logger(Box::new(logger)) {
        eprintln!("TutaBridge: logger already installed: {error}");
    }
}

fn build(default_filter: &str, file: Option<RotatingFile>) -> env_logger::Logger {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format_timestamp_secs()
        .target(env_logger::Target::Pipe(Box::new(Tee {
            file: file.map(Mutex::new),
        })))
        .build()
}

/// Writes each record to stderr and to the log file.
struct Tee {
    file: Option<Mutex<RotatingFile>>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                // A full disk must not take the app down with it.
                let _ = file.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}

struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut written = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if written >= MAX_BYTES {
            rotate(&path)?;
            written = 0;
        }
        Ok(Self {
            file: open_append(&path)?,
            path,
            written,
        })
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > MAX_BYTES && self.written > 0 {
            self.file.flush()?;
            rotate(&self.path)?;
            self.file = open_append(&self.path)?;
            self.written = 0;
        }
        // A record can exceed the cap on its own. Return a partial write so
        // write_all continues in the next file through the normal rotation.
        let n = self.file.write(&buf[..buf.len().min(MAX_BYTES as usize)])?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn rotate(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let mut previous = path.as_os_str().to_owned();
    previous.push(".1");
    fs::rename(path, previous)
}

/// Readable by the user only: the log names folders and the account address.
fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // OpenOptions::mode only applies when creating a file.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Show the log file in the system file manager, selected where the platform
/// allows it.
pub fn reveal() {
    let Some(path) = path() else {
        log::warn!("No log folder on this system");
        return;
    };
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
    } else if cfg!(target_os = "windows") {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(&path);
        std::process::Command::new("explorer").arg(select).spawn()
    } else {
        std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(&path))
            .spawn()
    };
    if let Err(error) = result {
        log::warn!("Could not show the log file {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_records_reach_the_file() {
        use log::Log;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let logger = build("info", Some(RotatingFile::open(path.clone()).unwrap()));
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("tutabridge")
                .args(format_args!("bridge started"))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .level(log::Level::Debug)
                .target("tutabridge")
                .args(format_args!("filtered out"))
                .build(),
        );
        logger.flush();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("INFO"), "{written}");
        assert!(written.contains("bridge started"), "{written}");
        assert!(!written.contains("filtered out"), "{written}");
        // The file is plain text: no terminal colour codes.
        assert!(!written.contains('\u{1b}'), "{written:?}");
    }

    #[test]
    fn a_full_file_moves_aside_and_a_new_one_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut file = RotatingFile::open(path.clone()).unwrap();
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..5 {
            file.write_all(&line).unwrap();
        }
        // The sixth megabyte would cross the cap: it lands in a fresh file.
        file.write_all(b"after\n").unwrap();
        file.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"after\n");
        let previous = dir.path().join(format!("{FILE_NAME}.1"));
        assert_eq!(fs::metadata(previous).unwrap().len(), MAX_BYTES);
    }

    #[test]
    fn an_oversized_file_is_rotated_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        let mut file = RotatingFile::open(path.clone()).unwrap();
        file.write_all(b"fresh\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fresh\n");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        RotatingFile::open(path.clone()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    #[test]
    fn a_large_write_stays_within_the_file_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut file = RotatingFile::open(path.clone()).unwrap();
        let mut record = vec![b'x'; MAX_BYTES as usize * 2];
        record.extend_from_slice(b"latest\n");
        file.write_all(&record).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 7);
        assert_eq!(fs::read(&path).unwrap(), b"latest\n");
        assert_eq!(
            fs::metadata(dir.path().join(format!("{FILE_NAME}.1")))
                .unwrap()
                .len(),
            MAX_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_logs_are_private_even_when_rotated_at_startup() {
        use std::os::unix::fs::PermissionsExt;
        for size in [1, MAX_BYTES as usize] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FILE_NAME);
            fs::write(&path, vec![b'x'; size]).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            RotatingFile::open(path.clone()).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            if size == MAX_BYTES as usize {
                let previous = dir.path().join(format!("{FILE_NAME}.1"));
                assert_eq!(
                    fs::metadata(previous).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
}
