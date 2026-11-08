//! What the bridge is doing right now, for the GUI: activity events and the
//! mail apps connected over IMAP. Content-free by construction: folder names
//! and counts, never subjects, addresses or bodies.

use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Something the bridge just did. `client` is the IMAP connection of the
/// mail app behind it, matching [`ClientInfo::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityEvent {
    /// A mail app read messages, served from the local cache.
    Synced {
        client: u64,
        folder: String,
        messages: usize,
    },
    /// A mail body that was not on this device yet came from Tuta.
    BodyDownloaded { client: u64 },
    /// A mail app changed the read state of mails, synced to Tuta.
    MarkedRead {
        client: u64,
        count: usize,
        read: bool,
    },
    /// A mail app moved mails to another folder.
    Moved {
        client: u64,
        count: usize,
        folder: String,
    },
    /// A mail app deleted mails (to Trash, or for good from Trash or Spam).
    Deleted { client: u64, count: usize },
    /// A mail arrived from Tuta.
    NewMail { folder: String },
    /// A mail was sent through SMTP.
    Sent,
}

/// A mail app signed in over IMAP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientInfo {
    pub id: u64,
    pub peer: String,
    /// What the app said about itself with the IMAP `ID` command, if it did.
    pub name: Option<String>,
    pub version: Option<String>,
    pub os: Option<String>,
    pub connected_at_ms: u64,
    /// The folder it has open.
    pub folder: Option<String>,
}

/// Shared by the bridge and its IMAP and SMTP servers; cheap to clone.
#[derive(Clone)]
pub struct Activity {
    inner: Arc<Inner>,
}

struct Inner {
    events: broadcast::Sender<ActivityEvent>,
    clients: Mutex<HashMap<u64, ClientInfo>>,
    /// Pulsed when the client list changes, so the GUI re-reads it.
    changed: broadcast::Sender<()>,
}

impl Activity {
    pub fn new(changed: broadcast::Sender<()>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                events,
                clients: Mutex::new(HashMap::new()),
                changed,
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ActivityEvent> {
        self.inner.events.subscribe()
    }

    pub fn emit(&self, event: ActivityEvent) {
        let _ = self.inner.events.send(event);
    }

    /// List a signed-in IMAP client until the returned guard is dropped.
    pub fn connect(&self, id: u64, peer: SocketAddr, identity: &ClientIdentity) -> ClientGuard {
        self.update(|clients| {
            clients.insert(
                id,
                ClientInfo {
                    id,
                    peer: peer.to_string(),
                    name: identity.name.clone(),
                    version: identity.version.clone(),
                    os: identity.os.clone(),
                    connected_at_ms: now_ms(),
                    folder: None,
                },
            );
        });
        ClientGuard {
            activity: self.clone(),
            id,
        }
    }

    pub fn identify(&self, id: u64, identity: &ClientIdentity) {
        self.update(|clients| {
            if let Some(client) = clients.get_mut(&id) {
                client.name = identity.name.clone();
                client.version = identity.version.clone();
                client.os = identity.os.clone();
            }
        });
    }

    pub fn set_folder(&self, id: u64, folder: Option<String>) {
        self.update(|clients| {
            if let Some(client) = clients.get_mut(&id) {
                client.folder = folder;
            }
        });
    }

    /// The signed-in clients, oldest connection first.
    pub fn clients(&self) -> Vec<ClientInfo> {
        let mut clients: Vec<_> = crate::util::lock_recover(&self.inner.clients)
            .values()
            .cloned()
            .collect();
        clients.sort_by_key(|c| c.id);
        clients
    }

    fn update(&self, f: impl FnOnce(&mut HashMap<u64, ClientInfo>)) {
        f(&mut crate::util::lock_recover(&self.inner.clients));
        let _ = self.inner.changed.send(());
    }
}

/// Removes its client from the list when the connection ends, whichever
/// way it ends.
pub struct ClientGuard {
    activity: Activity,
    id: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let id = self.id;
        self.activity.update(|clients| {
            clients.remove(&id);
        });
    }
}

/// What a mail app sent with the IMAP `ID` command (RFC 2971).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientIdentity {
    pub name: Option<String>,
    pub version: Option<String>,
    pub os: Option<String>,
}

impl ClientIdentity {
    /// From the `ID` parameter list, as `(field, value)` pairs.
    pub fn from_params(params: &[(String, String)]) -> Self {
        let field = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| display_safe(v))
                .filter(|v| !v.is_empty())
        };
        Self {
            name: field("name"),
            version: field("version"),
            os: field("os"),
        }
    }
}

/// A value fit to show in the GUI: printable characters, 64 at most.
fn display_safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity() -> (Activity, broadcast::Receiver<()>) {
        let (changed, rx) = broadcast::channel(16);
        (Activity::new(changed), rx)
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:50123".parse().unwrap()
    }

    #[test]
    fn a_client_is_listed_until_its_guard_drops() {
        let (activity, mut changed) = activity();
        let identity = ClientIdentity {
            name: Some("Thunderbird".into()),
            ..Default::default()
        };
        let guard = activity.connect(7, peer(), &identity);
        activity.set_folder(7, Some("INBOX".into()));
        let clients = activity.clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].name.as_deref(), Some("Thunderbird"));
        assert_eq!(clients[0].peer, "127.0.0.1:50123");
        assert_eq!(clients[0].folder.as_deref(), Some("INBOX"));
        drop(guard);
        assert!(activity.clients().is_empty());
        assert!(changed.try_recv().is_ok(), "the GUI is told to re-read");
    }

    #[test]
    fn identify_updates_a_listed_client_only() {
        let (activity, _changed) = activity();
        let identity = ClientIdentity {
            name: Some("Apple Mail".into()),
            version: Some("16.0".into()),
            os: Some("macOS".into()),
        };
        activity.identify(1, &identity);
        assert!(activity.clients().is_empty());
        let _guard = activity.connect(1, peer(), &ClientIdentity::default());
        activity.identify(1, &identity);
        assert_eq!(activity.clients()[0].version.as_deref(), Some("16.0"));
    }

    #[test]
    fn identity_keeps_known_fields_and_cleans_values() {
        let params = vec![
            ("NAME".to_string(), "Thunderbird\u{7}".to_string()),
            ("version".to_string(), "x".repeat(100)),
            ("vendor".to_string(), "Mozilla".to_string()),
        ];
        let identity = ClientIdentity::from_params(&params);
        assert_eq!(identity.name.as_deref(), Some("Thunderbird"));
        assert_eq!(identity.version.map(|v| v.len()), Some(64));
        assert_eq!(identity.os, None);
    }

    #[test]
    fn events_carry_no_more_than_counts_and_folders() {
        let json = serde_json::to_string(&ActivityEvent::Synced {
            client: 3,
            folder: "INBOX".into(),
            messages: 12,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"synced","client":3,"folder":"INBOX","messages":12}"#
        );
    }
}
