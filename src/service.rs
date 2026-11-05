//! Unix service supervision. The owner holds the volume lock while a worker
//! initializes/resumes/serves. Workers use a separate process group so blocked
//! synchronous pass/GPG calls can be terminated before releasing that lock.
use anyhow::{bail, Context};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Configuration = 10,
    Session = 11,
    Storage = 12,
    Rejected = 13,
    Version = 14,
    Occupied = 15,
    Starting = 16,
    Network = 17,
    Failed = 18,
    Stopping = 19,
}
impl State {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Configuration => "Waiting for configuration; run tutabridge init",
            Self::Session => "Waiting for a saved session; run tutabridge init",
            Self::Storage => "Session storage unavailable; check volume permissions, GPG key and pass store",
            Self::Rejected => "Saved session rejected; run tutabridge init (existing entry preserved)",
            Self::Version => "Tuta rejects this version; update the image: docker compose pull && docker compose up -d",
            Self::Occupied => "Data volume occupied by another init or service",
            Self::Starting => "Starting IMAP and SMTP",
            Self::Network => "Tuta temporarily unavailable; retrying with backoff",
            Self::Failed => "Service failed; inspect startup diagnostics",
            Self::Stopping => "Stopping",
        }
    }
    fn from_code(code: i32) -> Self {
        match code {
            10 => Self::Configuration,
            11 => Self::Session,
            12 => Self::Storage,
            13 => Self::Rejected,
            14 => Self::Version,
            17 => Self::Network,
            _ => Self::Failed,
        }
    }
}

pub fn container() -> bool {
    std::env::var_os("TUTABRIDGE_CONTAINER").is_some_and(|v| v == "1")
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("TUTABRIDGE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if container() {
                PathBuf::from("/data")
            } else {
                tutabridge_core::config::config_path()
                    .parent()
                    .unwrap()
                    .to_path_buf()
            }
        })
}

pub struct Lock(File);
impl Lock {
    pub fn acquire() -> io::Result<Option<Self>> {
        Self::at(&data_dir().join(".lock"))
    }
    fn at(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("Lock is not a regular file"));
        }
        // The file is never removed: replacing its inode defeats flock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(Self(file)))
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
    pub fn inherited() -> anyhow::Result<Self> {
        // Internal workers must have a real open descriptor for THIS volume.
        let descriptor = std::env::var("TUTABRIDGE_LOCK_FD")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|fd| *fd > 2)
            .context("Missing inherited volume lock")?;
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } < 0 {
            bail!("Missing inherited volume lock");
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        let inherited = file.metadata()?;
        let actual = std::fs::symlink_metadata(data_dir().join(".lock"))?;
        if !actual.is_file() || inherited.dev() != actual.dev() || inherited.ino() != actual.ino() {
            bail!("Invalid inherited volume lock");
        }
        if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Volume lock is not exclusive");
        }
        // pass and any replacement gpg-agent must never inherit the lock.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("TUTABRIDGE_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("tutabridge-{}", unsafe { libc::geteuid() }))
        })
}

#[derive(Clone)]
struct Status {
    state: State,
    ports: Option<(u16, u16)>,
    certificate: Option<String>,
    password: Option<String>,
}
impl Status {
    fn waiting(state: State) -> Self {
        Self {
            state,
            ports: None,
            certificate: None,
            password: None,
        }
    }
}
struct StatusServer {
    _lock: Lock,
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for StatusServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
impl StatusServer {
    fn start(status: watch::Receiver<Status>) -> anyhow::Result<Self> {
        let dir = runtime_dir();
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } => {
                bail!("Unsafe runtime directory")
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir)?;
            }
            Err(e) => return Err(e.into()),
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let lock = Lock::at(&dir.join("owner.lock"))?
            .context("Another service owns this runtime directory")?;
        let path = dir.join("health.sock");
        // Only the runtime owner may remove an abandoned IPC socket.
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0; 1];
                if tokio::time::timeout(Duration::from_millis(200), stream.read_exact(&mut request))
                    .await
                    .is_err()
                {
                    continue;
                }
                let snapshot = status.borrow().clone();
                let reply = if request == [b'p'] {
                    // Private IPC (0700 directory). Only the lock owner reads
                    // configuration; health responses never contain this value.
                    serde_json::json!({"password":snapshot.password}).to_string()
                } else if request == [b'h'] {
                    serde_json::json!({"state":snapshot.state as i32,"reason":snapshot.state.reason(),
                    "ports":snapshot.ports,"certificate":snapshot.certificate}).to_string()
                } else {
                    continue;
                };
                // Small bounded response; no credentials, account or data paths.
                let _ = tokio::time::timeout(
                    Duration::from_millis(200),
                    stream.write_all(reply.as_bytes()),
                )
                .await;
            }
        });
        Ok(Self {
            _lock: lock,
            path,
            task,
        })
    }
}

pub async fn health() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(4), async {
        let mut socket = UnixStream::connect(runtime_dir().join("health.sock"))
            .await
            .context("Service status unavailable")?;
        socket.write_all(b"h").await?;
        let mut bytes = Vec::new();
        (&mut socket).take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            bail!("Invalid service status");
        }
        let status: serde_json::Value = serde_json::from_slice(&bytes)?;
        if status["state"].as_i64() != Some(State::Starting as i64) {
            bail!(
                "{}",
                status["reason"].as_str().unwrap_or("Service unavailable")
            );
        }
        let ports = status["ports"].as_array().context("Service is starting")?;
        if ports.len() != 2 {
            bail!("Invalid service ports");
        }
        let imap = u16::try_from(ports[0].as_u64().context("Invalid IMAP port")?)?;
        let smtp = u16::try_from(ports[1].as_u64().context("Invalid SMTP port")?)?;
        let pem = status["certificate"].as_str().context("TLS is starting")?;
        probe(imap, smtp, pem).await
    })
    .await
    .context("Health check timed out")?
}

async fn probe(imap: u16, smtp: u16, pem: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio_rustls::{rustls, TlsConnector};
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(pem)) {
        roots.add(cert?)?;
    }
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    async fn line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> anyhow::Result<String> {
        let mut value = String::new();
        let mut limited = r.take(4097);
        limited.read_line(&mut value).await?;
        if value.len() > 4096 || !value.ends_with('\n') {
            bail!("Invalid protocol response");
        }
        Ok(value)
    }
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", imap))
        .await
        .context("IMAP unavailable")?;
    let mut connection = BufReader::new(
        connector
            .connect("localhost".try_into()?, tcp)
            .await
            .context("IMAP TLS unavailable")?,
    );
    if !line(&mut connection).await?.starts_with("* OK") {
        bail!("IMAP greeting unavailable");
    }
    connection.get_mut().write_all(b"health NOOP\r\n").await?;
    if !line(&mut connection).await?.starts_with("health OK") {
        bail!("IMAP NOOP failed");
    }
    connection
        .get_mut()
        .write_all(b"healthend LOGOUT\r\n")
        .await?;
    let mut logged_out = false;
    for _ in 0..4 {
        if line(&mut connection).await?.starts_with("healthend OK") {
            logged_out = true;
            break;
        }
    }
    if !logged_out {
        bail!("IMAP LOGOUT failed");
    }
    let _ = connection.get_mut().shutdown().await;
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", smtp))
        .await
        .context("SMTP unavailable")?;
    let mut connection = BufReader::new(
        connector
            .connect("localhost".try_into()?, tcp)
            .await
            .context("SMTP TLS unavailable")?,
    );
    if !line(&mut connection).await?.starts_with("220 ") {
        bail!("SMTP greeting unavailable");
    }
    connection
        .get_mut()
        .write_all(b"EHLO localhost\r\n")
        .await?;
    let mut hello = false;
    for _ in 0..32 {
        let reply = line(&mut connection).await?;
        if reply.starts_with("250 ") {
            hello = true;
            break;
        }
        if !reply.starts_with("250-") {
            break;
        }
    }
    if !hello {
        bail!("SMTP EHLO failed");
    }
    connection.get_mut().write_all(b"QUIT\r\n").await?;
    if !line(&mut connection).await?.starts_with("221 ") {
        bail!("SMTP QUIT failed");
    }
    let _ = connection.get_mut().shutdown().await;
    Ok(())
}

fn configure_status() -> Status {
    // Called only while holding the data lock. Never load a session here.
    let mut status = Status::waiting(State::Starting);
    if let Ok(Some(cfg)) = tutabridge_core::config::load_config() {
        status.ports = Some((cfg.imap_port, cfg.smtp_port));
        status.password = cfg.bridge_password;
        status.certificate = std::fs::read_to_string(
            tutabridge_core::config::config_path().with_file_name("cert.pem"),
        )
        .ok();
    }
    status
}

fn session_fingerprint() -> Option<Vec<u8>> {
    // Local ciphertext fingerprint; no decryption and never logged. Includes
    // config to see an explicit account/backend repair by another init.
    let config = std::fs::read(tutabridge_core::config::config_path()).ok()?;
    let cfg = tutabridge_core::config::load_config().ok()??;
    let mut bytes = config;
    // Successful init rewrites config, including for the desktop keyring.
    let modified = std::fs::metadata(tutabridge_core::config::config_path())
        .ok()?
        .modified()
        .ok()?;
    bytes.extend(format!("{modified:?}").as_bytes());
    if let (Some(root), Some(path)) = (std::env::var_os("PASSWORD_STORE_DIR"), cfg.pass_path) {
        let entry = PathBuf::from(root)
            .join(path)
            .join(format!("{}.gpg", cfg.email));
        if let Ok(encrypted) = std::fs::read(entry) {
            bytes.extend(encrypted);
        }
    }
    Some(bytes)
}

async fn terminate_group(child: &mut tokio::process::Child, pid: i32) -> anyhow::Result<()> {
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        child.wait().await?;
    }
    // Reap orphaned foreground workers before releasing the volume lock. The
    // independent GPG agent has another session and never inherits that lock.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    #[cfg(target_os = "linux")]
    loop {
        let result = unsafe { libc::waitpid(-pid, std::ptr::null_mut(), 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() != Some(libc::ECHILD) {
                return Err(error.into());
            }
            break;
        }
    }
    Ok(())
}

/// An interactive init worker has its own process group for bounded cleanup.
/// Give that group the terminal, otherwise a terminal read stops on SIGTTIN.
struct Foreground(Option<libc::pid_t>);
impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            unsafe {
                let mut blocked = std::mem::zeroed();
                let mut old = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGTTOU);
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old);
                libc::tcsetpgrp(libc::STDIN_FILENO, group);
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            }
        }
    }
}

async fn worker(
    lock: &Lock,
    init: bool,
    stop: &mut watch::Receiver<bool>,
    status: &watch::Sender<Status>,
) -> anyhow::Result<Option<i32>> {
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let terminal = init && unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let _foreground = Foreground(if terminal {
        let group = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if group < 0 || group != unsafe { libc::getpgrp() } {
            bail!("Interactive init requires a foreground terminal");
        }
        Some(group)
    } else {
        None
    });
    let descriptor = lock.0.as_raw_fd();
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg(if init {
        "__container-init"
    } else {
        "__service-attempt"
    });
    command.stdin(if init {
        Stdio::inherit()
    } else {
        Stdio::null()
    });
    command.process_group(0);
    command.env("TUTABRIDGE_LOCK_FD", descriptor.to_string());
    unsafe {
        command.pre_exec(move || {
            if terminal {
                let old = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                let result = libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                libc::signal(libc::SIGTTOU, old);
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = tokio::process::Command::from(command).spawn()?;
    let pid = child.id().context("Worker PID unavailable")? as i32;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let result = loop {
        tokio::select! {
            result=child.wait()=>break result.map(|exit|Some(exit.code().unwrap_or(State::Failed as i32))),
            _=stop.wait_for(|stopping|*stopping)=>break Ok(None),
            _=interval.tick()=>{status.send_replace(configure_status());},
        }
    };
    terminate_group(&mut child, pid).await?;
    result.map_err(Into::into)
}

pub async fn supervise(
    init: bool,
    waiting: bool,
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (sender, receiver) = watch::channel(Status::waiting(State::Starting));
    let _server = if !init {
        Some(StatusServer::start(receiver)?)
    } else {
        None
    };
    let mut blocked = None;
    let mut state = State::Starting;
    let mut logged = std::time::Instant::now() - Duration::from_secs(301);
    let mut backoff = 10;
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let result = match Lock::acquire() {
            Ok(Some(lock)) => {
                let fingerprint = session_fingerprint();
                if blocked.as_ref().is_some_and(|(reason, old)| {
                    *reason == State::Version || (*reason == State::Rejected && old == &fingerprint)
                }) {
                    blocked.as_ref().unwrap().0
                } else {
                    sender.send_replace(configure_status());
                    match worker(&lock, init, &mut stop, &sender).await? {
                        None => return Ok(()),
                        Some(0) => return Ok(()),
                        Some(code) => {
                            let result = State::from_code(code);
                            if matches!(result, State::Rejected | State::Version) {
                                blocked = Some((result, fingerprint));
                            }
                            result
                        }
                    }
                }
            }
            Ok(None) => State::Occupied,
            Err(_) => State::Storage,
        };
        sender.send_replace(Status::waiting(result));
        if !waiting {
            bail!("{}", result.reason());
        }
        if result != state || logged.elapsed() >= Duration::from_secs(300) {
            log::info!("{}", result.reason());
            logged = std::time::Instant::now();
        }
        state = result;
        let delay = if result == State::Failed {
            300
        } else if result == State::Network {
            let delay = backoff;
            backoff = (backoff * 2).min(300);
            delay
        } else {
            backoff = 10;
            10
        };
        // All worker activity and guards have ended before this sleep.
        tokio::select! {
            _=stop.wait_for(|stopping|*stopping)=>{sender.send_replace(Status::waiting(State::Stopping));return Ok(());},
            _=tokio::time::sleep(Duration::from_secs(delay))=>{},
        }
    }
}

pub fn bootstrap(init: bool) -> Result<(), State> {
    if !container() {
        return Ok(());
    }
    let assets = std::env::var_os("TUTABRIDGE_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/lib/tutabridge"));
    let status = std::process::Command::new("bash")
        .arg(assets.join("bootstrap.sh"))
        .arg(if init { "init" } else { "check" })
        .stdout(if init {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stderr(if init {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .status()
        .map_err(|_| State::Storage)?;
    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(10) => State::Configuration,
            _ => State::Storage,
        })
    }
}

/// Password is read by the volume owner, including when a bridge is running.
pub async fn password() -> anyhow::Result<String> {
    if let Some(_lock) = Lock::acquire()? {
        return tutabridge_core::config::load_config()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .and_then(|cfg| cfg.bridge_password)
            .context("No bridge password yet; run tutabridge init");
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut socket = UnixStream::connect(runtime_dir().join("health.sock"))
            .await
            .context("Volume busy; password is available only in the running service container")?;
        socket.write_all(b"p").await?;
        let mut bytes = Vec::new();
        socket.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            bail!("Invalid service response");
        }
        let response: serde_json::Value = serde_json::from_slice(&bytes)?;
        response["password"]
            .as_str()
            .map(str::to_owned)
            .context("Password unavailable during initialization")
    })
    .await
    .context("Password request timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn lock_is_exclusive_stable_and_never_unlinked() {
        let path = std::env::temp_dir().join(format!(
            "tutabridge-lock-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let first = Lock::at(&path).unwrap().unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert!(Lock::at(&path).unwrap().is_none());
        assert_ne!(
            unsafe { libc::fcntl(first.0.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        drop(first);
        // Other parallel tests fork subprocesses. CLOEXEC closes the copied
        // descriptor at exec, which can briefly outlive our close here.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let second = loop {
            if let Some(lock) = Lock::at(&path).unwrap() {
                break lock;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "lock survived descriptor closure"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(inode, second.0.metadata().unwrap().ino());
        drop(second);
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(Lock::at(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    fn tls() -> (tokio_rustls::TlsAcceptor, String) {
        use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        (
            tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            cert.pem(),
        )
    }

    async fn server(
        acceptor: tokio_rustls::TlsAcceptor,
        imap: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(acceptor.accept(tcp).await.unwrap());
            stream
                .get_mut()
                .write_all(if imap {
                    b"* OK ready\r\n"
                } else {
                    b"220 localhost\r\n"
                })
                .await
                .unwrap();
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                if imap {
                    "health NOOP\r\n"
                } else {
                    "EHLO localhost\r\n"
                }
            );
            stream
                .get_mut()
                .write_all(if imap {
                    b"health OK NOOP\r\n"
                } else {
                    b"250-localhost\r\n250 AUTH PLAIN LOGIN\r\n"
                })
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                if imap {
                    "healthend LOGOUT\r\n"
                } else {
                    "QUIT\r\n"
                }
            );
            stream
                .get_mut()
                .write_all(if imap {
                    b"* BYE closing\r\nhealthend OK LOGOUT\r\n"
                } else {
                    b"221 Bye\r\n"
                })
                .await
                .unwrap();
            stream.get_mut().shutdown().await.unwrap();
        });
        (port, task)
    }

    #[tokio::test]
    async fn health_requires_both_tls_protocols_and_never_authenticates() {
        let (acceptor, pem) = tls();
        let (imap, imap_task) = server(acceptor.clone(), true).await;
        let (smtp, smtp_task) = server(acceptor.clone(), false).await;
        probe(imap, smtp, &pem).await.unwrap();
        imap_task.await.unwrap();
        smtp_task.await.unwrap();
        // An IMAP-only service must not be healthy.
        let (imap, imap_task) = server(acceptor.clone(), true).await;
        assert!(probe(imap, smtp, &pem).await.is_err());
        imap_task.await.unwrap();
        // An SMTP-only service must not be healthy either.
        let (smtp, smtp_task) = server(acceptor, false).await;
        assert!(probe(imap, smtp, &pem).await.is_err());
        smtp_task.abort();
    }
}
