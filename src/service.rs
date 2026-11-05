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
    Initialization = 20,
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
            Self::Initialization => "Initialization failed or cancelled; inspect diagnostics and saved session state",
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
            20 => Self::Initialization,
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
    address: Option<std::net::IpAddr>,
    password: Option<String>,
}
impl Status {
    fn waiting(state: State) -> Self {
        Self {
            state,
            ports: None,
            certificate: None,
            address: None,
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
            let mut failed = false;
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(connection) => {
                        failed = false;
                        connection
                    }
                    Err(e) => {
                        if !failed {
                            log::warn!("Status accept failed ({:?}); retrying", e.kind());
                        }
                        failed = true;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
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
                    "ports":snapshot.ports,"certificate":snapshot.certificate,"address":snapshot.address}).to_string()
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
        let address = status["address"]
            .as_str()
            .context("Service address unavailable")?
            .parse()?;
        probe(address, imap, smtp, pem).await
    })
    .await
    .context("Health check timed out")?
}

/// Pin the local leaf DER, independent of SAN/expiry; still verify possession
/// of its private key via the provider's TLS handshake signature algorithms.
#[derive(Debug)]
struct PinnedCertificate(tokio_rustls::rustls::pki_types::CertificateDer<'static>);
impl tokio_rustls::rustls::client::danger::ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        if end != &self.0 {
            return Err(tokio_rustls::rustls::Error::General(
                "Local certificate pin mismatch".into(),
            ));
        }
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        signature: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &tokio_rustls::rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        signature: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &tokio_rustls::rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn connect_health(address: std::net::IpAddr, port: u16) -> io::Result<tokio::net::TcpStream> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match address {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            // Some dual-stack listeners have IPv4 loopback but no ::1 route.
            // Only unspecified IPv6 permits fallback; explicit binds stay exact.
            if let Ok(Ok(tcp)) = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port)),
            )
            .await
            {
                return Ok(tcp);
            }
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await
        }
        ip => tokio::net::TcpStream::connect((ip, port)).await,
    }
}

async fn probe(address: std::net::IpAddr, imap: u16, smtp: u16, pem: &str) -> anyhow::Result<()> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio_rustls::{rustls, TlsConnector};
    let certificate = CertificateDer::from_pem_slice(pem.as_bytes())?;
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertificate(certificate)))
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
    let tcp = connect_health(address, imap)
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
    let tcp = connect_health(address, smtp)
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
        status.address = Some(cfg.bind_addr);
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
    // Container init rewrites config. A desktop keyring repair requires restarting --wait.
    let modified = std::fs::metadata(tutabridge_core::config::config_path())
        .ok()?
        .modified()
        .ok()?;
    bytes.extend(format!("{modified:?}").as_bytes());
    if let (Some(root), Some(path)) = (std::env::var_os("PASSWORD_STORE_DIR"), cfg.pass_path) {
        let entry = PathBuf::from(root)
            .join(path.trim_matches('/'))
            .join(format!("{}.gpg", cfg.email));
        if let Ok(encrypted) = std::fs::read(entry) {
            bytes.extend(encrypted);
        }
    }
    Some(bytes)
}

fn exited_without_reaping(pid: i32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } == pid)
}

async fn terminate_group(
    child: &mut tokio::process::Child,
    pid: i32,
) -> anyhow::Result<std::process::ExitStatus> {
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !exited_without_reaping(pid).unwrap_or(true) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Keep the unreaped leader as a PID/PGID reservation until our final signal.
    // No negative-PID kill is permitted after child.wait() (PGID reuse race).
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let status = child.wait().await?;
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
    reap_agents(None);
    Ok(status)
}

fn reap_agents(keep: Option<i32>) {
    // Reap adopted daemons without stealing the leader from Tokio or releasing
    // its PID reservation. This also runs while the bridge is serving.
    // Invariant: the supervisor has exactly one direct child tracked by Tokio
    // (keep). Every other waitable child is an adopted worker descendant. None
    // is passed only before spawn or after that direct child has been reaped.
    #[cfg(target_os = "linux")]
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            break;
        }
        let pid = unsafe { info.si_pid() };
        if pid == 0 || keep == Some(pid) {
            break;
        }
        if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } <= 0 {
            break;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = keep; // POSIX has no portable subreaper; orphan reaping is Linux-only.
}

pub fn prepare_report() {
    if let Some(fd) = report_fd() {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}
fn report_fd() -> Option<i32> {
    std::env::var("TUTABRIDGE_REPORT_FD")
        .ok()?
        .parse::<i32>()
        .ok()
        .filter(|fd| *fd > 2)
}
fn report(message: &str) {
    if let Some(fd) = report_fd() {
        // A single bounded datagram; no plaintext credentials or tool stdout.
        let bytes = message.as_bytes();
        unsafe {
            libc::send(
                fd,
                bytes.as_ptr().cast(),
                bytes.len().min(2048),
                libc::MSG_DONTWAIT,
            );
        }
    }
}

/// An interactive init worker has its own process group for bounded cleanup.
/// Give that group the terminal, otherwise a terminal read stops on SIGTTIN.
struct Foreground(Option<(File, libc::pid_t)>);
impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some((terminal, group)) = &self.0 {
            unsafe {
                let mut blocked = std::mem::zeroed();
                let mut old = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGTTOU);
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old);
                libc::tcsetpgrp(terminal.as_raw_fd(), *group);
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
) -> anyhow::Result<Option<(i32, String)>> {
    let prepared = (|| -> io::Result<_> {
        #[cfg(target_os = "linux")]
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let executable = std::env::current_exe()?;
        let (diagnostics, report_socket) = std::os::unix::net::UnixDatagram::pair()?;
        diagnostics.set_nonblocking(true)?;
        Ok((executable, diagnostics, report_socket))
    })();
    let (executable, diagnostics, report_socket) = match prepared {
        Ok(value) => value,
        Err(e) => {
            return Ok(Some((
                State::Failed as i32,
                format!("Cannot prepare worker ({:?}); retrying", e.kind()),
            )))
        }
    };
    let tty = if init {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/tty")
            .ok()
    } else {
        None
    };
    let terminal = tty.as_ref().map(AsRawFd::as_raw_fd);
    let _foreground = Foreground(if let Some(tty) = tty {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group < 0 || group != unsafe { libc::getpgrp() } {
            return Ok(Some((
                State::Initialization as i32,
                "Interactive init requires a foreground terminal".into(),
            )));
        }
        Some((tty, group))
    } else {
        None
    });
    let report_descriptor = report_socket.as_raw_fd();
    let descriptor = lock.0.as_raw_fd();
    let mut command = std::process::Command::new(executable);
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
    command.env("TUTABRIDGE_REPORT_FD", report_descriptor.to_string());
    unsafe {
        command.pre_exec(move || {
            if let Some(terminal) = terminal {
                let old = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                let result = libc::tcsetpgrp(terminal, libc::getpgrp());
                libc::signal(libc::SIGTTOU, old);
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0
                || libc::fcntl(report_descriptor, libc::F_SETFD, 0) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match tokio::process::Command::from(command).spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ok(Some((
                State::Failed as i32,
                format!("Cannot launch worker ({:?}); retrying", e.kind()),
            )))
        }
    };
    drop(report_socket);
    let pid = child.id().context("Worker PID unavailable")? as i32;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    let stopped = loop {
        tokio::select! {
            _=poll.tick()=>{ if exited_without_reaping(pid).unwrap_or(true) { break false; } },
            _=stop.wait_for(|stopping|*stopping)=>break true,
            _=interval.tick()=>{reap_agents(Some(pid)); status.send_replace(configure_status());},
        }
    };
    let result = terminate_group(&mut child, pid).await?;
    if stopped && !(init && result.success()) {
        return Ok(None);
    }
    let mut buffer = [0; 2048];
    let detail = diagnostics
        .recv(&mut buffer)
        .map(|n| String::from_utf8_lossy(&buffer[..n]).into_owned())
        .unwrap_or_default();
    Ok(Some((
        result.code().unwrap_or(if init {
            State::Initialization as i32
        } else {
            State::Failed as i32
        }),
        detail,
    )))
}

async fn acquire_for_init(
    init: bool,
    stop: &mut watch::Receiver<bool>,
) -> io::Result<Option<Lock>> {
    acquire_for_init_at(&data_dir().join(".lock"), init, stop).await
}

async fn acquire_for_init_at(
    path: &Path,
    init: bool,
    stop: &mut watch::Receiver<bool>,
) -> io::Result<Option<Lock>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut reported = false;
    loop {
        if *stop.borrow() {
            return Ok(None);
        }
        let result = Lock::at(path)?;
        if result.is_some() || !init || tokio::time::Instant::now() >= deadline || *stop.borrow() {
            return Ok(result);
        }
        if !reported {
            log::info!("Waiting briefly for volume lock before initialization");
            reported = true;
        }
        tokio::select! {
            _=stop.wait_for(|stopping| *stopping)=>return Ok(None),
            _=tokio::time::sleep(Duration::from_millis(50))=>{},
        }
    }
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
            if init {
                bail!("{}", State::Initialization.reason());
            }
            return Ok(());
        }
        reap_agents(None);
        let mut detail = String::new();
        let acquired = acquire_for_init(init, &mut stop).await;
        if init && *stop.borrow() {
            bail!("{}", State::Initialization.reason());
        }
        let result = match acquired {
            Ok(Some(lock)) => {
                let fingerprint = session_fingerprint();
                if blocked.as_ref().is_some_and(|(reason, old)| {
                    *reason == State::Version || (*reason == State::Rejected && old == &fingerprint)
                }) {
                    blocked.as_ref().unwrap().0
                } else {
                    sender.send_replace(configure_status());
                    match worker(&lock, init, &mut stop, &sender).await? {
                        None if init => bail!("{}", State::Initialization.reason()),
                        None => return Ok(()),
                        Some((0, _)) => return Ok(()),
                        Some((code, diagnostic)) => {
                            detail = diagnostic;
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
            Err(error) => {
                detail = format!("Cannot acquire volume lock ({:?})", error.kind());
                State::Storage
            }
        };
        sender.send_replace(Status::waiting(result));
        if !waiting {
            bail!(
                "{}{}{}",
                result.reason(),
                if detail.is_empty() { "" } else { ": " },
                detail
            );
        }
        if result != state || logged.elapsed() >= Duration::from_secs(300) {
            log::info!(
                "{}{}{}",
                result.reason(),
                if detail.is_empty() { "" } else { ": " },
                detail
            );
            logged = std::time::Instant::now();
        }
        state = result;
        let delay = if matches!(result, State::Failed | State::Network) {
            let delay = backoff;
            backoff = (backoff * 2).min(300);
            delay
        } else {
            backoff = 10;
            10
        };
        // All worker activity and guards have ended before this sleep.
        let deadline = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(deadline);
        let mut reap = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _=stop.wait_for(|stopping|*stopping)=>{sender.send_replace(Status::waiting(State::Stopping));return Ok(());},
                _=&mut deadline=>break,
                _=reap.tick()=>reap_agents(None),
            }
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
    let mut command = std::process::Command::new("bash");
    command
        .arg(assets.join("bootstrap.sh"))
        .arg(if init { "init" } else { "check" });
    let status = if init {
        command.status().map_err(|_| State::Storage)?
    } else {
        let output = command.stdout(Stdio::null()).output().map_err(|e| {
            report(&format!("Cannot launch storage check ({:?})", e.kind()));
            State::Storage
        })?;
        if !output.status.success() && output.status.code() != Some(10) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let known = [
                "Password store is not initialized",
                "Unexpected pass recipient",
                "The private GPG key for this password store is missing",
            ];
            let detail = known.into_iter().find(|message| stderr.contains(message));
            report(detail.unwrap_or("Storage check failed; verify permissions and GPG setup"));
        }
        output.status
    };
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
        let cert = rcgen::CertificateParams::new(vec!["nas.example.invalid".into()])
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
        probe("127.0.0.1".parse().unwrap(), imap, smtp, &pem)
            .await
            .unwrap();
        imap_task.await.unwrap();
        smtp_task.await.unwrap();
        // An IMAP-only service must not be healthy.
        let (imap, imap_task) = server(acceptor.clone(), true).await;
        assert!(probe("127.0.0.1".parse().unwrap(), imap, smtp, &pem)
            .await
            .is_err());
        imap_task.await.unwrap();
        // An SMTP-only service must not be healthy either.
        let (smtp, smtp_task) = server(acceptor, false).await;
        assert!(probe("127.0.0.1".parse().unwrap(), imap, smtp, &pem)
            .await
            .is_err());
        smtp_task.abort();
    }
    use tutabridge_core::{
        mail::ParsedMessage,
        tuta::{FolderInfo, MailBackend},
    };
    use tutasdk::{
        entities::generated::tutanota::{Mail, MailDetails, MailSetEntry, TutanotaFile},
        IdTupleGenerated,
    };
    struct NoopBackend;
    #[async_trait::async_trait]
    impl MailBackend for NoopBackend {
        async fn load_mail_ids_for_folder(
            &self,
            _f: &FolderInfo,
            _l: usize,
        ) -> Result<Vec<Mail>, String> {
            unimplemented!()
        }
        async fn load_mail(&self, _l: &str, _e: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail(&self, _j: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_set_entry(
            &self,
            _j: &str,
        ) -> Result<Option<MailSetEntry>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_details_blob(
            &self,
            _j: &str,
        ) -> Result<Option<MailDetails>, String> {
            unimplemented!()
        }
        async fn load_mail_details(&self, _m: &Mail) -> Result<Option<MailDetails>, String> {
            unimplemented!()
        }
        async fn load_attachments(
            &self,
            _m: &Mail,
        ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, tutabridge_tuta::mail::AttachmentError> {
            unimplemented!()
        }
        async fn list_folders(&self) -> Result<Vec<FolderInfo>, String> {
            unimplemented!()
        }
        async fn set_unread_status(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _u: bool,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn trash_mails(&self, _ids: Vec<IdTupleGenerated>) -> Result<(), String> {
            unimplemented!()
        }
        async fn delete_mails(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _folder: &FolderInfo,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn move_mails(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _t: &FolderInfo,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn send_mail(&self, _m: &ParsedMessage) -> Result<(), String> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn health_uses_real_servers_pinned_certificate_and_configured_address() {
        use tutabridge_core::{imap, smtp, store::LocalStore, sync::MailStore};
        let external = std::env::var_os("TUTABRIDGE_PUBLIC_HEALTH_FIXTURE").is_some();
        let address: std::net::IpAddr = if external { "127.0.0.2" } else { "::1" }.parse().unwrap();
        let root = std::env::temp_dir().join(format!(
            "tutabridge-health-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let local = Arc::new(
            LocalStore::open(
                &root.join("db"),
                &root.join("mails"),
                crypto_primitives::key::GenericAesKey::from_bytes(&[7; 32]).unwrap(),
            )
            .unwrap(),
        );
        let (acceptor, pem) = tls(); // SAN deliberately excludes localhost.
        let imap_listener = tokio::net::TcpListener::bind((address, 0)).await.unwrap();
        let smtp_listener = tokio::net::TcpListener::bind((address, 0)).await.unwrap();
        let imap_port = imap_listener.local_addr().unwrap().port();
        let smtp_port = smtp_listener.local_addr().unwrap().port();
        let backend = Arc::new(NoopBackend);
        let imap_task = tokio::spawn(imap::serve_listener(
            imap_listener,
            MailStore::new(),
            backend.clone(),
            local,
            acceptor.clone(),
            Some("unusable-test-hash".into()),
        ));
        let smtp_task = tokio::spawn(smtp::serve_listener(
            smtp_listener,
            backend,
            acceptor,
            Some("unusable-test-hash".into()),
        ));
        let wrong_pem = tls().1;
        assert!(probe(address, imap_port, smtp_port, &wrong_pem)
            .await
            .is_err());
        probe(address, imap_port, smtp_port, &pem).await.unwrap();
        if external {
            let (_, receiver) = watch::channel(Status {
                state: State::Starting,
                ports: Some((imap_port, smtp_port)),
                certificate: Some(pem.clone()),
                address: Some(address),
                password: None,
            });
            let _server = StatusServer::start(receiver).unwrap();
            println!("public-health-fixture-ready");
            use std::io::Write;
            std::io::stdout().flush().unwrap();
            // The smoke harness removes only the SMTP server to test one failed port.
            let marker = runtime_dir().join("stop-smtp");
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            smtp_task.abort();
            let _ = smtp_task.await;
            println!("public-health-fixture-smtp-stopped");
            std::io::stdout().flush().unwrap();
            std::future::pending::<()>().await;
        } else {
            smtp_task.abort();
            let _ = smtp_task.await;
        }
        assert!(probe(address, imap_port, smtp_port, &pem).await.is_err());
        imap_task.abort();
        let _ = imap_task.await;
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests;
