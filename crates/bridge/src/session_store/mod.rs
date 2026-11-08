//! Persistence and in-memory caching for one account's Tuta session.
//!
//! Clones share the cache. Construct a new store when the account, API endpoint,
//! or storage selection changes; there is deliberately no process-global cache.

mod codec;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod keyring;
mod pass;

use crate::config::Config;
use std::sync::{Arc, Mutex};
use tutasdk::login::Credentials;

/// Raw session bytes read from a backend, wiped when dropped.
pub(crate) type SecretBytes = zeroize::Zeroizing<Vec<u8>>;

const KEYRING_SUPPORTED: bool = cfg!(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
));

/// Storage failures never contain the session JSON or command stdout.
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("This platform has no supported keyring; configure a non-empty pass_path to use pass")]
    PassRequired,
    #[error("The keyring is unavailable: {message}")]
    KeyringUnavailable {
        kind: KeyringUnavailableKind,
        message: String,
    },
    #[error("Could not read the saved session from {backend}: {message}")]
    Read {
        backend: &'static str,
        message: String,
    },
    #[error("Could not save the session to {backend}: {message}")]
    Write {
        backend: &'static str,
        message: String,
    },
    #[error("Could not delete the saved session from {backend}: {message}")]
    Delete {
        backend: &'static str,
        message: String,
    },
    #[error("Saved entry in {backend} ({entry}) contains an invalid session. Sign in with your Tuta password to replace it after successful authentication; automatic resume leaves it unchanged")]
    InvalidSession {
        backend: &'static str,
        entry: String,
    },
    #[error("The saved session could not be read; refusing to change it")]
    UnreadableStore,
    #[error("The session is only kept in memory; the unreadable saved entry was left unchanged")]
    VolatileSession,
    #[error("A persisted session is required; the current session is only available in memory")]
    PersistenceRequired,
    #[error("Session storage is unavailable after an earlier worker failure")]
    Poisoned,
    #[error("Session storage worker failed")]
    Worker(#[source] tokio::task::JoinError),
}

/// These conditions permit an interactive login without persistence. Neither
/// proves that the existing entry is absent: a locked keyring has the same
/// protection against overwrite as an unavailable platform service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyringUnavailableKind {
    StorageAccess,
    Platform,
}

impl SessionStoreError {
    pub fn allows_volatile_login(&self) -> bool {
        matches!(self, Self::KeyringUnavailable { .. })
    }

    /// Bytes were read successfully, but do not represent usable credentials.
    /// This never includes access/decryption failures or ambiguous entries.
    pub fn allows_reauthentication(&self) -> bool {
        matches!(self, Self::InvalidSession { .. })
    }

    fn warning(&self) -> String {
        if self.allows_volatile_login() {
            format!(
                "{self}. You can sign in for this run, but the saved entry will be left \
                 untouched and you may need to sign in again after restarting. \
                 To retry persistent storage after unlocking the keychain, quit and \
                 reopen the application; restarting only the bridge keeps this session in memory."
            )
        } else {
            self.to_string()
        }
    }
}

/// A presentation-safe snapshot for the CLI/desktop. Inspecting an unavailable
/// keyring offers a password prompt without silently enabling volatile login.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SessionStatus {
    pub available: bool,
    pub warning: Option<String>,
}

/// A single account and storage context. The pass directory is captured when
/// this instance is constructed, so later environment changes cannot redirect
/// an existing instance to another password store.
#[derive(Clone)]
pub struct SessionStore {
    context: Context,
    state: Arc<Mutex<State>>,
}

impl SessionStore {
    pub fn new(config: &Config) -> Self {
        let context = Context::from_config(config);
        let backend = match &context.selection {
            Selection::Keyring => Backend::keyring(context.email.clone()),
            Selection::Pass { entry } => Backend::Pass(pass::PassStore::new(entry.clone())),
        };
        Self {
            context,
            state: Arc::new(Mutex::new(State {
                backend,
                cache: Cache::Unloaded,
                persistence: Persistence::Writable,
                warning: None,
            })),
        }
    }

    /// Pure comparison: unrelated settings do not invalidate a session, and
    /// normalization of `pass_path` matches the existing storage selection.
    pub fn matches_config(&self, config: &Config) -> bool {
        self.context == Context::from_config(config)
    }

    pub fn describe(&self) -> String {
        match &self.context.selection {
            Selection::Keyring => "the keychain".to_owned(),
            Selection::Pass { entry } => format!("pass ({entry})"),
        }
    }

    pub async fn load(&self) -> Result<Option<Credentials>, SessionStoreError> {
        self.run(State::load).await
    }

    /// Initialization and services cannot rely on credentials whose last save
    /// failed, even when a caller reuses a store from an interactive login.
    /// An explicit password can replace readable invalid data after successful
    /// authentication, but never opts into an unavailable keyring's volatile mode.
    pub async fn load_persisted(
        &self,
        password_supplied: bool,
    ) -> Result<Option<Credentials>, SessionStoreError> {
        self.run(move |state| {
            let credentials = if password_supplied {
                state.load_for_password(false)?
            } else {
                state.load()?
            };
            if state.persistence == Persistence::Volatile
                || (credentials.is_some() && state.warning.is_some())
            {
                return Err(SessionStoreError::PersistenceRequired);
            }
            Ok(credentials)
        })
        .await
    }

    /// Report availability and any persistence warning without opting into
    /// volatile login or authorizing replacement of an invalid entry.
    pub async fn inspect(&self) -> Result<SessionStatus, SessionStoreError> {
        self.run(State::inspect).await
    }

    /// Use only when a caller has explicitly supplied a password for an
    /// interactive login. Unavailable keyrings may then be left untouched
    /// while the new session lives in this instance's memory. Readable but
    /// invalid entries may be replaced after successful authentication.
    pub async fn load_interactive(&self) -> Result<Option<Credentials>, SessionStoreError> {
        self.run(|state| state.load_for_password(true)).await
    }

    /// Keep the session usable in this process even if persistence fails.
    /// The error still tells the caller that a restart will require login.
    /// After an unreadable entry, changes are refused until a successful read
    /// or an explicit password login authorizes replacement of invalid bytes
    /// (or a session in memory only for an unavailable keyring).
    pub async fn save(&self, credentials: &Credentials) -> Result<(), SessionStoreError> {
        let credentials = credentials.clone();
        self.run(move |state| state.save(credentials)).await
    }

    /// Container reauthentication commits encrypted pass bytes atomically.
    pub async fn save_preserving(
        &self,
        credentials: &Credentials,
    ) -> Result<(), SessionStoreError> {
        let credentials = credentials.clone();
        self.run(move |state| state.save_with_policy(credentials, true))
            .await
    }

    /// Tombstone the in-memory session even if deletion fails. A token rejected
    /// by Tuta must not be read back and retried in this process.
    pub async fn delete(&self) -> Result<(), SessionStoreError> {
        self.run(State::delete).await
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut State) -> Result<T, SessionStoreError> + Send + 'static,
    ) -> Result<T, SessionStoreError> {
        // Reject unsupported configurations before starting backend work or
        // caching a session. The login caller performs this read before any
        // Tuta network request, including when a password was supplied.
        self.context.selection.validate(KEYRING_SUPPORTED)?;
        let state = Arc::clone(&self.state);
        // Acquire the lock inside the blocking worker as well: another clone
        // may be waiting for GPG/pinentry while holding it. This serializes
        // cache changes with their I/O without blocking a Tokio async worker.
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().map_err(|_| SessionStoreError::Poisoned)?;
            operation(&mut state)
        })
        .await
        .map_err(SessionStoreError::Worker)?
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Context {
    email: String,
    api_url: String,
    selection: Selection,
}

impl Context {
    fn from_config(config: &Config) -> Self {
        Self {
            email: config.email.clone(),
            api_url: config.api_url.clone(),
            selection: Selection::from_config(config),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Selection {
    Keyring,
    Pass { entry: String },
}

impl Selection {
    fn from_config(config: &Config) -> Self {
        match config
            .pass_path
            .as_deref()
            .map(|prefix| prefix.trim_matches('/'))
        {
            Some(prefix) if !prefix.is_empty() => Self::Pass {
                entry: format!("{prefix}/{}", config.email),
            },
            _ => Self::Keyring,
        }
    }

    /// Platform capability is an input so both supported and unsupported
    /// targets can be tested on any host without a real keyring or Android.
    fn validate(&self, keyring_supported: bool) -> Result<(), SessionStoreError> {
        if matches!(self, Self::Keyring) && !keyring_supported {
            Err(SessionStoreError::PassRequired)
        } else {
            Ok(())
        }
    }
}

enum Cache {
    Unloaded,
    Loaded(Option<Credentials>),
}

struct State {
    backend: Backend,
    cache: Cache,
    persistence: Persistence,
    warning: Option<String>,
}

#[derive(PartialEq, Eq)]
enum Persistence {
    Writable,
    /// A failed read is never permission to overwrite or delete an entry.
    Unreadable,
    /// An explicit password login may replace bytes that were read but could
    /// not be decoded. Keep the entry intact until authentication and save.
    ReplaceInvalid,
    /// The caller explicitly chose an interactive login without persistence.
    Volatile,
}

impl State {
    fn load(&mut self) -> Result<Option<Credentials>, SessionStoreError> {
        if let Cache::Loaded(credentials) = &self.cache {
            return Ok(credentials.clone());
        }
        let raw = match self.backend.read() {
            Ok(raw) => raw,
            Err(error) => {
                self.persistence = Persistence::Unreadable;
                self.warning = Some(error.warning());
                return Err(error);
            }
        };
        let credentials = raw.as_deref().and_then(codec::decode);
        if raw.is_some() && credentials.is_none() {
            let error = SessionStoreError::InvalidSession {
                backend: self.backend.name(),
                entry: self.backend.entry(),
            };
            self.persistence = Persistence::Unreadable;
            self.warning = Some(error.warning());
            return Err(error);
        }
        // A successful read, including a confirmed absence, makes persistence
        // safe again. Failed reads remain uncached so unlocking or repairing
        // the backend permits a fresh attempt in this process.
        self.persistence = Persistence::Writable;
        self.warning = None;
        self.cache = Cache::Loaded(credentials.clone());
        Ok(credentials)
    }

    fn inspect(&mut self) -> Result<SessionStatus, SessionStoreError> {
        match self.load() {
            Ok(credentials) => Ok(SessionStatus {
                available: credentials.is_some(),
                warning: self.warning.clone(),
            }),
            Err(error) if error.allows_volatile_login() || error.allows_reauthentication() => {
                Ok(SessionStatus {
                    available: false,
                    warning: Some(error.warning()),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn load_for_password(
        &mut self,
        allow_volatile: bool,
    ) -> Result<Option<Credentials>, SessionStoreError> {
        match self.load() {
            Err(error) if error.allows_reauthentication() => {
                self.persistence = Persistence::ReplaceInvalid;
                // Do not cache absence: an abandoned/failed login must leave
                // the next strict resume reporting the original corruption.
                let warning = error.warning();
                log::warn!("{warning}");
                self.warning = Some(warning);
                Ok(None)
            }
            Err(error) if allow_volatile && error.allows_volatile_login() => {
                self.persistence = Persistence::Volatile;
                self.cache = Cache::Loaded(None);
                let warning = error.warning();
                log::warn!("{warning}");
                self.warning = Some(warning);
                Ok(None)
            }
            result => result,
        }
    }

    fn save(&mut self, credentials: Credentials) -> Result<(), SessionStoreError> {
        self.save_with_policy(credentials, false)
    }

    fn save_with_policy(
        &mut self,
        credentials: Credentials,
        preserve: bool,
    ) -> Result<(), SessionStoreError> {
        if self.persistence == Persistence::Unreadable {
            return Err(SessionStoreError::UnreadableStore);
        }
        let encoded = codec::encode(&credentials);
        self.cache = Cache::Loaded(Some(credentials));
        if self.persistence == Persistence::Volatile {
            return Err(SessionStoreError::VolatileSession);
        }
        self.persistence = Persistence::Writable;
        let write = if preserve {
            match &mut self.backend {
                Backend::Pass(store) => store.write_preserving(&encoded),
                other => other.write(&encoded),
            }
        } else {
            self.backend.write(&encoded)
        };
        let result = write.map_err(|message| SessionStoreError::Write {
            backend: self.backend.name(),
            message,
        });
        self.warning = result.as_ref().err().map(|error| format!(
            "{error}. The new session is only available for this run; you may need to sign in again after restarting."
        ));
        result
    }

    fn delete(&mut self) -> Result<(), SessionStoreError> {
        if matches!(
            self.persistence,
            Persistence::Unreadable | Persistence::ReplaceInvalid
        ) {
            return Err(SessionStoreError::UnreadableStore);
        }
        self.cache = Cache::Loaded(None);
        if self.persistence == Persistence::Volatile {
            return Err(SessionStoreError::VolatileSession);
        }
        let result = self
            .backend
            .delete()
            .map_err(|message| SessionStoreError::Delete {
                backend: self.backend.name(),
                message,
            });
        self.warning = result.as_ref().err().map(ToString::to_string);
        result
    }
}

enum Backend {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Keyring {
        email: String,
    },
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Unavailable,
    Pass(pass::PassStore),
    #[cfg(test)]
    Fake(tests::FakeBackend),
}

impl Backend {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn keyring(email: String) -> Self {
        Self::Keyring { email }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn keyring(_email: String) -> Self {
        Self::Unavailable
    }

    fn entry(&self) -> String {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Self::Keyring { email } => format!("service tutabridge, account {email}"),
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Self::Unavailable => "no supported keyring on this platform".to_owned(),
            Self::Pass(store) => store.entry().to_owned(),
            #[cfg(test)]
            Self::Fake(_) => "test-entry".to_owned(),
        }
    }
    fn name(&self) -> &'static str {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Self::Keyring { .. } => "the keychain",
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Self::Unavailable => "the unavailable keychain",
            Self::Pass(_) => "pass",
            #[cfg(test)]
            Self::Fake(_) => "test store",
        }
    }

    fn read(&mut self) -> Result<Option<SecretBytes>, SessionStoreError> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Self::Keyring { email } => keyring::read(email),
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Self::Unavailable => Err(SessionStoreError::PassRequired),
            Self::Pass(store) => store.read().map_err(|message| SessionStoreError::Read {
                backend: "pass",
                message,
            }),
            #[cfg(test)]
            Self::Fake(backend) => backend
                .read()
                .map(|raw| raw.map(|raw| zeroize::Zeroizing::new(raw.into_bytes()))),
        }
    }

    fn write(&mut self, secret: &str) -> Result<(), String> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Self::Keyring { email } => keyring::write(email, secret),
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Self::Unavailable => Err(SessionStoreError::PassRequired.to_string()),
            Self::Pass(store) => store.write(secret),
            #[cfg(test)]
            Self::Fake(backend) => backend.write(secret),
        }
    }

    fn delete(&mut self) -> Result<(), String> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Self::Keyring { email } => keyring::delete(email),
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Self::Unavailable => Err(SessionStoreError::PassRequired.to_string()),
            Self::Pass(store) => store.delete(),
            #[cfg(test)]
            Self::Fake(backend) => backend.delete(),
        }
    }
}

#[cfg(test)]
mod tests;
