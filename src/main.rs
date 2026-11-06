use anyhow::Context;
use log::{info, warn};
mod cli;
#[cfg(unix)]
mod service;
mod shutdown;
use cli::{Command, LoginMode};
use std::sync::Arc;
use tutabridge_core::{
    backup, bridge as bridge_helpers, config, event_handler, imap, mcp, smtp, store::LocalStore,
    sync, tls, tuta, VERSION,
};

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    if service::container() {
        unsafe {
            libc::umask(0o077);
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    // Bound desktop runtime teardown. The Unix service supervisor has already
    // terminated and reaped its worker group before returning here.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

async fn run() -> anyhow::Result<()> {
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install TLS crypto provider"))?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = cli::parse(&args).map_err(anyhow::Error::msg)?;
    #[cfg(unix)]
    if service::container() {
        match command {
            Command::Init => return shutdown::supervised(|stop| service::supervise(true,false,stop)).await,
            Command::Run(_) => return shutdown::supervised(|stop| service::supervise(false,false,stop)).await,
            Command::Password => {println!("{}",service::password().await?);return Ok(());},
            Command::Backup(_) => anyhow::bail!("Backup is not supported in the container service; stop it and use the desktop backup"),
            _ => {},
        }
    }
    match command {
        Command::Health => {
            #[cfg(unix)]
            {
                service::health().await?;
                println!("IMAP and SMTP ready");
                Ok(())
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("Service health requires Unix")
            }
        }
        Command::Wait => {
            #[cfg(unix)]
            {
                shutdown::supervised(|stop| service::supervise(false, true, stop)).await
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("Service wait requires Unix")
            }
        }
        Command::ServiceAttempt | Command::ContainerInit => {
            #[cfg(unix)]
            {
                let _lock = service::Lock::inherited()?;
                service::prepare_report();
                let init = matches!(command, Command::ContainerInit);
                let result = container_worker(init).await;
                if let Err(state) = result {
                    std::process::exit(state as i32);
                }
                Ok(())
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("Internal service worker requires Unix")
            }
        }
        Command::Version => {
            println!("tutabridge {VERSION}");
            Ok(())
        }
        Command::Backup(output) => run_backup(&output).await,
        Command::Password => {
            let pw = config::load_config()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .and_then(|cfg| cfg.bridge_password);
            let Some(pw) = pw else {
                eprintln!("No bridge password yet — run the bridge once to generate one.");
                std::process::exit(1);
            };
            println!("{pw}");
            Ok(())
        }
        Command::Help => {
            println!("tutabridge {VERSION}");
            println!();
            for (command, description) in [
                ("tutabridge", "run the IMAP/SMTP bridge (default)"),
                (
                    "tutabridge init",
                    "sign in and persist a session, then exit",
                ),
                (
                    "tutabridge run --non-interactive",
                    "resume a session without prompts",
                ),
                (
                    "tutabridge backup <dir>",
                    "export every mail to <dir> as .eml files",
                ),
                (
                    "tutabridge password",
                    "print the local bridge password (mail client setup)",
                ),
                (
                    "tutabridge run --wait",
                    "wait for initialization and serve without prompts (Unix)",
                ),
                ("tutabridge health", "check local IMAP and SMTP readiness"),
                ("tutabridge --version", "print the version"),
            ] {
                println!("{command:<34}{description}");
            }
            Ok(())
        }
        Command::Init => initialize().await,
        Command::Run(LoginMode::Interactive) => {
            // Keep the OS signal behavior while synchronous terminal prompts
            // run. Once authenticated, use the same controlled shutdown as
            // the service. Init and backup likewise retain normal OS signals.
            let cfg = load_account(LoginMode::Interactive)?;
            let session = login_session(&cfg, LoginMode::Interactive).await?;
            shutdown::run(|stop| serve_bridge(cfg, session, stop)).await
        }
        Command::Run(mode) => shutdown::run(|stop| run_bridge(mode, stop)).await,
    }
}

#[cfg(unix)]
async fn container_worker(init: bool) -> Result<(), service::State> {
    use service::State;
    service::bootstrap(init)?;
    if init {
        return initialize().await.map_err(|error| {
            log::error!("Initialization failed: {error}");
            State::Initialization
        });
    }
    let cfg = config::load_config()
        .map_err(|_| State::Configuration)?
        .ok_or(State::Configuration)?;
    if cfg.email.trim().is_empty() || cfg.validate_ports().is_err() {
        return Err(State::Configuration);
    }
    let store = tutabridge_core::session_store::SessionStore::new(&cfg);
    if store
        .load_persisted(false)
        .await
        .map_err(|_| State::Storage)?
        .is_none()
    {
        return Err(State::Session);
    }
    let session = tuta::login_with_persistence(
        &cfg,
        &store,
        None,
        None,
        tuta::PersistencePolicy::PreserveExistingRequired,
    )
    .await
    .map_err(|error| match error.downcast_ref::<tuta::ResumeError>() {
        Some(tuta::ResumeError::SessionInvalid) => State::Rejected,
        Some(tuta::ResumeError::ClientRejected) => State::Version,
        Some(tuta::ResumeError::Transient) => State::Network,
        None => {
            if error.is::<tutabridge_core::session_store::SessionStoreError>() {
                State::Storage
            } else {
                State::Network
            }
        }
    })?;
    shutdown::run(|stop| serve_bridge(cfg, session, stop))
        .await
        .map_err(|error| {
            log::error!("Service startup failed: {error}");
            State::Failed
        })
}

fn load_account(mode: LoginMode) -> anyhow::Result<config::Config> {
    let mut cfg = config::load_config()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .unwrap_or_default();
    if cfg.email.trim().is_empty() {
        if !mode.may_prompt() {
            anyhow::bail!("No account configured; run `tutabridge init` interactively first");
        }
        use std::io::{BufRead, Write};
        print!("Tuta email address: ");
        std::io::stdout().flush()?;
        let mut email = String::new();
        std::io::stdin().lock().read_line(&mut email)?;
        cfg.email = email.trim().to_string();
        if cfg.email.is_empty() {
            anyhow::bail!("Email address is required");
        }
        // Preserve the existing backend and listening settings in a Docker
        // template or a partly configured desktop installation.
        config::save_config(&cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    cfg.validate_ports().map_err(anyhow::Error::msg)?;
    Ok(cfg)
}

async fn initialize() -> anyhow::Result<()> {
    let mut cfg = load_account(LoginMode::Initialize)?;
    login_session(&cfg, LoginMode::Initialize).await?;
    config::ensure_bridge_password(&mut cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    tls::load_or_create_tls_acceptor().map_err(|e| anyhow::anyhow!("{e}"))?;
    #[cfg(unix)]
    if service::container() {
        config::save_config(&cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
        // The container's waiting service picks up the new session by itself.
        info!("Session saved. The running service starts IMAP and SMTP within seconds.");
        info!("Use `tutabridge password` to display the local mail-client password.");
        return Ok(());
    }
    info!("Session saved. Start the service with `tutabridge run --non-interactive`.");
    info!("Use `tutabridge password` to display the local mail-client password.");
    Ok(())
}

async fn run_bridge(
    mode: LoginMode,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let cfg = load_account(mode)?;
    // Fail on missing/unreadable credentials before creating local runtime
    // files. Service mode never asks for an account password or a TOTP code.
    let session = login_session(&cfg, mode).await?;
    serve_bridge(cfg, session, shutdown_rx).await
}

async fn serve_bridge(
    mut cfg: config::Config,
    session: tuta::TutaSession,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let bridge_password_is_new = cfg.bridge_password.is_none();
    let bridge_password = config::ensure_bridge_password(&mut cfg)
        .map_err(|e| anyhow::anyhow!("Bridge password setup failed: {e}"))?;
    info!("TutaBridge {VERSION} starting");

    let tls_acceptor =
        tls::load_or_create_tls_acceptor().map_err(|e| anyhow::anyhow!("TLS setup failed: {e}"))?;
    info!("TLS initialized");

    info!("IMAP will listen on {}", cfg.imap_addr());
    info!("SMTP will listen on {}", cfg.smtp_addr());
    info!("Logged in as {}", cfg.email);

    let storage_key = session
        .derive_storage_key()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("Storage encryption key derived");

    let local_store = LocalStore::open(
        &config::store_db_path(),
        &config::store_mails_dir(),
        storage_key,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !local_store.verify_key() {
        info!("Storage key changed — resetting local cache");
        let _ = local_store.reset();
    }
    let local_store = Arc::new(local_store);
    info!("Local store opened");

    // Build the realtime event bus and hydrate its catch-up cursor from
    // disk so reconnects resume from the last processed batch — the GUI's
    // `BridgeHandle` does the same dance; the CLI used to skip it entirely
    // and quietly degrade to "bootstrap-sync only at startup".
    let bus_access_token = session.access_token.clone();
    let bus_user_id = session
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("Missing user id from session"))?;
    let bus_client = Arc::new(tutabridge_tuta::event_bus::EventBusClient::new(
        cfg.api_url.clone(),
        bridge_helpers::sys_model_version(),
        bridge_helpers::tutanota_model_version(),
        tutasdk::CLIENT_VERSION.to_string(),
        bridge_helpers::CLIENT_NAME.to_string(),
    ));
    {
        // OutOfSync detection: if the oldest cursor is older than the
        // server's batch-replay window (~44 days), the server cannot
        // catch us up — wipe so the syncer falls back to a bootstrap.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let expire_ms = tutabridge_tuta::event_bus::ENTITY_EVENT_BATCH_EXPIRE.as_millis() as i64;
        if let Ok(Some(min_ms)) = local_store.event_bus_state_min_updated_at_ms() {
            if now_ms - min_ms > expire_ms {
                info!("Cached event-bus state is older than 44 days — wiping and forcing a full re-sync");
                if let Err(e) = local_store.clear_event_bus_state() {
                    warn!("Could not clear event_bus_state: {e}");
                }
            }
        }
        match local_store.load_event_bus_state() {
            Ok(s) if !s.is_empty() => {
                let ids_handle = bus_client.last_batch_ids();
                let mut m = tutabridge_core::util::lock_recover(&ids_handle);
                let n = s.len();
                m.extend(s);
                info!("Event bus catch-up state loaded ({n} group(s))");
            }
            Ok(_) => info!("Event bus catch-up state is empty (first launch)"),
            Err(e) => warn!("Could not load event_bus_state: {e}"),
        }
    }
    let bus_ids_for_handler = bus_client.last_batch_ids();

    let backend: Arc<dyn tuta::MailBackend> = Arc::new(session);

    let store = sync::MailStore::new();
    let mut background = tokio::task::JoinSet::new();

    let imap_tls = tls_acceptor.clone();
    let smtp_tls = tls_acceptor;

    let pw = cfg.bridge_password.clone();

    // mpsc channel from event bus -> handler.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    // The syncer tells the event handler when its startup is done (see
    // `event_handler::run_event_handler`).
    let (startup_done_tx, startup_done_rx) = tokio::sync::watch::channel(false);

    background.spawn(sync::run_syncer(
        store.clone(),
        local_store.clone(),
        backend.clone(),
        cfg.sync_limit,
        startup_done_tx,
        shutdown_rx.clone(),
    ));
    {
        let client = Arc::clone(&bus_client);
        let token = bus_access_token;
        let uid = bus_user_id;
        let shutdown = shutdown_rx.clone();
        background.spawn(async move {
            if let Err(e) = client.run(token, uid, event_tx, shutdown).await {
                use tutabridge_tuta::event_bus::EventBusError;
                if !matches!(e, EventBusError::Stopped) {
                    warn!("Event bus exited: {e}");
                }
            }
        });
    }
    // Log every WsState transition at INFO so production logs reveal
    // reconnect storms without RUST_LOG=debug.
    {
        let mut ws_watch = bus_client.state();
        let mut shutdown_watch = shutdown_rx.clone();
        let mut last = *ws_watch.borrow();
        background.spawn(async move {
            loop {
                tokio::select! {
                    _ = ws_watch.changed() => {
                        let now = *ws_watch.borrow();
                        if now != last {
                            info!("ws state: {:?} → {:?}", last, now);
                            last = now;
                        }
                    }
                    _ = shutdown_watch.changed() => return,
                }
            }
        });
    }
    background.spawn(event_handler::run_event_handler(
        store.clone(),
        local_store.clone(),
        backend.clone(),
        bus_ids_for_handler,
        event_rx,
        startup_done_rx,
        shutdown_rx.clone(),
    ));
    let imap_server = imap::serve(
        cfg.imap_addr(),
        store.clone(),
        backend.clone(),
        local_store.clone(),
        imap_tls,
        pw.clone(),
    );
    // Read-only MCP server — a no-op (returns immediately) when the permission
    // tier is Disabled, so it's always safe to spawn. Not awaited in the
    // select! below for exactly that reason: a disabled server returns Ok at
    // once and must not tear the bridge down.
    let mcp_server = mcp::serve(
        cfg.mcp_port,
        store.clone(),
        local_store.clone(),
        backend.clone(),
        pw.clone(),
        cfg.mcp_permission,
        shutdown_rx.clone(),
    );
    background.spawn(async move {
        let _ = mcp_server.await;
    });
    let smtp_server = smtp::serve(cfg.smtp_addr(), backend.clone(), smtp_tls, pw);

    info!("Bridge is running. Configure Thunderbird with:");
    info!("  IMAP listening: {} (SSL/TLS)", cfg.imap_addr());
    info!("  SMTP listening: {} (SSL/TLS)", cfg.smtp_addr());
    info!("  Username: {}", cfg.email);
    info!("  Password: ******** (run 'tutabridge password' to view)");
    info!("  Accept the self-signed certificate when prompted");
    if bridge_password_is_new {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            println!("Generated bridge password: {bridge_password}");
        }
    }

    let result = tokio::select! {
        _ = shutdown_rx.wait_for(|stop| *stop) => Ok(()),
        r = imap_server => r.map_err(|e| anyhow::anyhow!("{e}")),
        r = smtp_server => r.map_err(|e| anyhow::anyhow!("{e}")),
    };
    // JoinSet also aborts on drop if startup is cancelled by a signal. Normal
    // exit awaits cancellation so background tasks release their resources.
    background.shutdown().await;
    result
}

/// Interactive TOTP prompt used during a fresh (non-keyring) login.
fn make_totp_cb() -> tuta::TwoFactorCallback {
    tuta::TwoFactorCallback::Totp(Box::new(|| {
        use std::io::{BufRead, Write};
        print!("TOTP code: ");
        std::io::stdout().flush()?;
        let mut code_str = String::new();
        std::io::stdin().lock().read_line(&mut code_str)?;
        let code: u32 = code_str
            .trim()
            .parse()
            .map_err(|_| "Invalid TOTP code — must be a number")?;
        Ok(code)
    }))
}

/// Resume the saved keychain session if possible, otherwise prompt for the
/// Tuta password (and TOTP). Shared by the bridge-run path and `backup`.
async fn login_session(cfg: &config::Config, mode: LoginMode) -> anyhow::Result<tuta::TutaSession> {
    let store = tutabridge_core::session_store::SessionStore::new(cfg);
    let preserve = cfg!(unix) && std::env::var_os("TUTABRIDGE_CONTAINER").is_some_and(|v| v == "1");
    let policy = if preserve {
        tuta::PersistencePolicy::PreserveExistingRequired
    } else if mode.allows_volatile_login() {
        tuta::PersistencePolicy::AllowVolatile
    } else {
        tuta::PersistencePolicy::Required
    };
    if !mode.may_prompt() && store.load_persisted(false).await?.is_none() {
        anyhow::bail!("No saved session; run `tutabridge init` interactively first");
    }
    match tuta::login_with_persistence(cfg, &store, None, None, policy).await {
        Ok(s) => return Ok(s),
        Err(e) if !mode.may_prompt() => {
            return Err(anyhow::anyhow!(
                "{e}. If the session has expired, run `tutabridge init` interactively."
            ));
        }
        Err(e)
            if preserve
                && e.downcast_ref::<tuta::ResumeError>()
                    == Some(&tuta::ResumeError::SessionInvalid) =>
        {
            warn!("Saved session was rejected; it will only be replaced after successful authentication.");
        }
        Err(e)
            if e.downcast_ref::<tutabridge_core::session_store::SessionStoreError>()
                .is_some_and(|error| error.allows_reauthentication()) =>
        {
            warn!("{e}");
        }
        Err(e)
            if mode.allows_volatile_login()
                && e.downcast_ref::<tutabridge_core::session_store::SessionStoreError>()
                    .is_some_and(|error| error.allows_volatile_login()) =>
        {
            warn!("{e}. Sign in to continue for this run without saving the session.");
        }
        // Do not retry the store here or fall back to a Tuta password when
        // GPG/pinentry failed. Report the original diagnostic to the user.
        Err(e) if e.is::<tutabridge_core::session_store::SessionStoreError>() => {
            return Err(anyhow::anyhow!("{e}"));
        }
        // The saved session is still there, so it was kept on purpose: Tuta
        // refused the client or could not be reached, and a password would
        // change nothing. Only ask for one when there is no session to resume.
        Err(e) if store.load().await?.is_some() => return Err(anyhow::anyhow!("{e}")),
        Err(_) => {}
    }
    let password = rpassword::prompt_password(format!("Password for {}: ", cfg.email)).context(
        "Could not read the Tuta password; an interactive terminal is required \
         (Docker: `docker compose run --rm tutabridge init`)",
    )?;
    tuta::login_with_persistence(cfg, &store, Some(&password), Some(make_totp_cb()), policy)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// `tutabridge backup <dir>` — export every mail of every folder to a tree of
/// `.eml` files under `output`. This is a one-shot operation: it logs in,
/// opens the local cache read-only (no reset on key mismatch — a backup must
/// never destroy the cache), runs the export with a live progress line, and
/// exits.
async fn run_backup(output: &str) -> anyhow::Result<()> {
    let cfg = config::load_config()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .filter(|c| !c.email.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("No account configured — run `tutabridge` once to sign in first")
        })?;

    let session = login_session(&cfg, LoginMode::Interactive).await?;
    info!("Logged in as {}", cfg.email);

    let storage_key = session
        .derive_storage_key()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Open the cache without the reset-on-mismatch the bridge uses: a wrong
    // key just makes `read_eml` miss and fall through to a server fetch,
    // which is safe; wiping the cache during a backup would be destructive.
    let local_store = LocalStore::open(
        &config::store_db_path(),
        &config::store_mails_dir(),
        storage_key,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let output_path = std::path::Path::new(output);
    info!("Backing up all mail to {} ...", output_path.display());

    use std::io::Write;
    let mut last_folder = String::new();
    let stats = backup::export_eml(&session, &local_store, output_path, |p| {
        if p.folder != last_folder {
            if !last_folder.is_empty() {
                eprintln!();
            }
            last_folder = p.folder.clone();
        }
        eprint!("\r  {}: {}/{}     ", p.folder, p.done, p.total);
        let _ = std::io::stderr().flush();
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    if !last_folder.is_empty() {
        eprintln!();
    }

    info!(
        "Backup complete: {} mails written ({} from cache, {} fetched), {} skipped (already on disk) across {} folder(s), {:.1} MB",
        stats.mails_written,
        stats.from_cache,
        stats.from_server,
        stats.skipped,
        stats.folders,
        stats.bytes as f64 / 1_000_000.0,
    );
    if !stats.errors.is_empty() {
        warn!("{} mail(s) could not be exported:", stats.errors.len());
        for e in stats.errors.iter().take(20) {
            warn!("  {e}");
        }
        if stats.errors.len() > 20 {
            warn!("  … and {} more", stats.errors.len() - 20);
        }
    }
    Ok(())
}
