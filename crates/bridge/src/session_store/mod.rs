//! Persistence and in-memory caching for one account's Tuta session.
//!
//! Clones share the cache. Construct a new store when the account, API endpoint,
//! or storage selection changes; there is deliberately no process-global cache.

mod codec;
mod keyring;
mod pass;

use crate::config::Config;
use std::sync::{Arc, Mutex};
use tutasdk::login::Credentials;

/// Storage failures never contain the session JSON or command stdout.
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
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
    #[error("{backend} returned an invalid session")]
    InvalidSession { backend: &'static str },
    #[error("Session storage is unavailable after an earlier worker failure")]
    Poisoned,
    #[error("Session storage worker failed")]
    Worker(#[source] tokio::task::JoinError),
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
            Selection::Keyring => Backend::Keyring {
                email: context.email.clone(),
            },
            Selection::Pass { entry } => Backend::Pass(pass::PassStore::new(entry.clone())),
        };
        Self {
            context,
            state: Arc::new(Mutex::new(State {
                backend,
                cache: Cache::Unloaded,
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

    /// Keep the session usable in this process even if persistence fails.
    /// The error still tells the caller that a restart will require login.
    pub async fn save(&self, credentials: &Credentials) -> Result<(), SessionStoreError> {
        let credentials = credentials.clone();
        self.run(move |state| state.save(credentials)).await
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
}

enum Cache {
    Unloaded,
    Loaded(Option<Credentials>),
}

struct State {
    backend: Backend,
    cache: Cache,
}

impl State {
    fn load(&mut self) -> Result<Option<Credentials>, SessionStoreError> {
        if let Cache::Loaded(credentials) = &self.cache {
            return Ok(credentials.clone());
        }
        let raw = match self.backend.read() {
            Ok(raw) => raw,
            // Preserve the legacy keyring behavior for machines without
            // Secret Service. A stricter interactive/volatile-session policy
            // can be reviewed independently of this extraction.
            Err(_) if self.backend.uses_legacy_fallback() => None,
            Err(message) => {
                return Err(SessionStoreError::Read {
                    backend: self.backend.name(),
                    message,
                });
            }
        };
        let credentials = raw.as_deref().and_then(codec::decode);
        if raw.is_some() && credentials.is_none() && !self.backend.uses_legacy_fallback() {
            return Err(SessionStoreError::InvalidSession {
                backend: self.backend.name(),
            });
        }
        // Failed pass reads/decodes leave this Unloaded so unlocking GPG or
        // repairing an entry permits a fresh read in the same process.
        self.cache = Cache::Loaded(credentials.clone());
        Ok(credentials)
    }

    fn save(&mut self, credentials: Credentials) -> Result<(), SessionStoreError> {
        let encoded = codec::encode(&credentials);
        self.cache = Cache::Loaded(Some(credentials));
        self.backend
            .write(&encoded)
            .map_err(|message| SessionStoreError::Write {
                backend: self.backend.name(),
                message,
            })
    }

    fn delete(&mut self) -> Result<(), SessionStoreError> {
        self.cache = Cache::Loaded(None);
        self.backend
            .delete()
            .map_err(|message| SessionStoreError::Delete {
                backend: self.backend.name(),
                message,
            })
    }
}

enum Backend {
    Keyring {
        email: String,
    },
    Pass(pass::PassStore),
    #[cfg(test)]
    Fake(tests::FakeBackend),
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Self::Keyring { .. } => "the keychain",
            Self::Pass(_) => "pass",
            #[cfg(test)]
            Self::Fake(_) => "test store",
        }
    }

    fn uses_legacy_fallback(&self) -> bool {
        match self {
            Self::Keyring { .. } => true,
            Self::Pass(_) => false,
            #[cfg(test)]
            Self::Fake(backend) => backend.legacy_fallback,
        }
    }

    fn read(&mut self) -> Result<Option<String>, String> {
        match self {
            Self::Keyring { email } => keyring::read(email),
            Self::Pass(store) => store.read(),
            #[cfg(test)]
            Self::Fake(backend) => backend.read(),
        }
    }

    fn write(&mut self, secret: &str) -> Result<(), String> {
        match self {
            Self::Keyring { email } => keyring::write(email, secret),
            Self::Pass(store) => store.write(secret),
            #[cfg(test)]
            Self::Fake(backend) => backend.write(secret),
        }
    }

    fn delete(&mut self) -> Result<(), String> {
        match self {
            Self::Keyring { email } => keyring::delete(email),
            Self::Pass(store) => store.delete(),
            #[cfg(test)]
            Self::Fake(backend) => backend.delete(),
        }
    }
}

#[cfg(test)]
mod tests;
