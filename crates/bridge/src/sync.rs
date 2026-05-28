use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::{watch, RwLock};
use tutasdk::entities::generated::tutanota::{Mail, MailDetails};

use crate::mail::mail_to_rfc2822;
use crate::store::{LocalStore, MailMetadata};
use crate::tuta::{FolderInfo, MailBackend};

const INTER_REQUEST_DELAY: Duration = Duration::from_millis(150);
const INTER_FOLDER_DELAY: Duration = Duration::from_millis(300);
/// Pause between background body-prefetch sweeps.
const PREFETCH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct StoredMail {
    pub mail: Mail,
    pub details: Option<MailDetails>,
    pub rfc2822: Option<String>,
    /// Stable IMAP UID within the folder, persisted across restarts.
    pub uid: u32,
}

pub struct MailStore {
    /// folder id → mails in that folder.
    folders: RwLock<HashMap<String, Vec<StoredMail>>>,
    /// The folder list (system + custom), for IMAP enumeration.
    folder_list: RwLock<Vec<FolderInfo>>,
    generation: watch::Sender<u64>,
    gen_counter: std::sync::atomic::AtomicU64,
}

impl MailStore {
    pub fn new() -> Arc<Self> {
        let (tx, _) = watch::channel(0u64);
        Arc::new(Self {
            folders: RwLock::new(HashMap::new()),
            folder_list: RwLock::new(Vec::new()),
            generation: tx,
            gen_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    pub async fn total_mail_count(&self) -> usize {
        self.folders.read().await.values().map(|v| v.len()).sum()
    }

    pub async fn folder_count(&self, folder_id: &str) -> usize {
        self.folders
            .read()
            .await
            .get(folder_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    pub async fn get_folder(&self, folder_id: &str) -> Vec<StoredMail> {
        self.folders
            .read()
            .await
            .get(folder_id)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn get_details(
        &self,
        folder_id: &str,
        element_id: &str,
    ) -> Option<(MailDetails, String)> {
        let folders = self.folders.read().await;
        let folder = folders.get(folder_id)?;
        folder.iter().find_map(|m| {
            let eid = m.mail._id.as_ref()?.element_id.to_string();
            if eid == element_id {
                Some((m.details.clone()?, m.rfc2822.clone()?))
            } else {
                None
            }
        })
    }

    /// The current folder list (system + custom).
    pub async fn list_folders(&self) -> Vec<FolderInfo> {
        self.folder_list.read().await.clone()
    }

    /// Look up a folder by its IMAP path (case-insensitive for INBOX).
    pub async fn folder_by_imap_path(&self, path: &str) -> Option<FolderInfo> {
        let list = self.folder_list.read().await;
        list.iter()
            .find(|f| f.imap_path == path || f.imap_path.eq_ignore_ascii_case(path))
            .cloned()
    }

    pub(crate) async fn set_folder_list(&self, folders: Vec<FolderInfo>) {
        *self.folder_list.write().await = folders;
        self.bump_generation();
    }

    pub(crate) async fn set_folder(&self, folder_id: &str, mails: Vec<StoredMail>) {
        self.folders
            .write()
            .await
            .insert(folder_id.to_string(), mails);
        self.bump_generation();
    }

    /// Refresh an existing mail's metadata in every folder that holds it
    /// (body/details preserved). No-op if the mail is not cached.
    pub async fn refresh_mail_in_place(&self, mail: &Mail) {
        let Some(eid) = mail
            ._id
            .as_ref()
            .map(|id| id.element_id.to_string())
        else {
            return;
        };
        let mut folders = self.folders.write().await;
        let mut changed = false;
        for mails in folders.values_mut() {
            if let Some(m) = mails.iter_mut().find(|m| {
                m.mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string())
                    .as_deref()
                    == Some(&eid)
            }) {
                m.mail = mail.clone();
                changed = true;
            }
        }
        drop(folders);
        if changed {
            self.bump_generation();
        }
    }

    /// Find a mail by element id across all folders. Returns the source
    /// folder id and a clone of the [`StoredMail`] entry — letting the
    /// event handler reuse the already-decrypted Mail/details when the
    /// same mail just hopped between two cached folders, no REST round-trip.
    pub async fn find_mail_anywhere(&self, element_id: &str) -> Option<(String, StoredMail)> {
        let folders = self.folders.read().await;
        for (fid, mails) in folders.iter() {
            for m in mails {
                if m.mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string())
                    .as_deref()
                    == Some(element_id)
                {
                    return Some((fid.clone(), m.clone()));
                }
            }
        }
        None
    }

    /// `true` if any folder still references this mail. Used after a
    /// per-folder removal to decide whether the cached `.eml` can go.
    pub async fn is_mail_anywhere(&self, element_id: &str) -> bool {
        let folders = self.folders.read().await;
        folders.values().any(|mails| {
            mails.iter().any(|m| {
                m.mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string())
                    .as_deref()
                    == Some(element_id)
            })
        })
    }

    /// Remove a single mail from one specific folder. Returns `true` if a
    /// row was actually removed (the mail might already be gone from this
    /// folder if we are reprocessing an event).
    pub async fn remove_mail_from_folder(&self, folder_id: &str, element_id: &str) -> bool {
        let mut folders = self.folders.write().await;
        let Some(mails) = folders.get_mut(folder_id) else {
            return false;
        };
        let before = mails.len();
        mails.retain(|m| {
            m.mail
                ._id
                .as_ref()
                .map(|id| id.element_id.to_string())
                .as_deref()
                != Some(element_id)
        });
        let changed = mails.len() != before;
        drop(folders);
        if changed {
            self.bump_generation();
        }
        changed
    }

    /// Insert or replace a mail in `folder_id`. Replace-by-element-id keeps
    /// the operation idempotent — re-applying the same event leaves the
    /// store unchanged.
    pub async fn upsert_mail_in_folder(&self, folder_id: &str, mail: StoredMail) {
        let Some(eid) = mail.mail._id.as_ref().map(|id| id.element_id.to_string()) else {
            return;
        };
        let mut folders = self.folders.write().await;
        let entries = folders.entry(folder_id.to_string()).or_default();
        if let Some(slot) = entries.iter_mut().find(|m| {
            m.mail
                ._id
                .as_ref()
                .map(|id| id.element_id.to_string())
                .as_deref()
                == Some(&eid)
        }) {
            *slot = mail;
        } else {
            entries.push(mail);
        }
        drop(folders);
        self.bump_generation();
    }

    /// Drop in-memory state for folders that are no longer on the server.
    /// Returns the ids that were removed so the caller can clean up the
    /// LocalStore + .eml files for them.
    pub async fn prune_unknown_folders(
        &self,
        known: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let mut removed = Vec::new();
        let mut folders = self.folders.write().await;
        folders.retain(|fid, _| {
            if known.contains(fid) {
                true
            } else {
                removed.push(fid.clone());
                false
            }
        });
        drop(folders);
        if !removed.is_empty() {
            self.bump_generation();
        }
        removed
    }

    /// Drop a mail from every folder it appears in (handles DELETE events).
    pub async fn remove_mail_everywhere(&self, element_id: &str) {
        let mut folders = self.folders.write().await;
        let mut changed = false;
        for mails in folders.values_mut() {
            let before = mails.len();
            mails.retain(|m| {
                m.mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string())
                    .as_deref()
                    != Some(element_id)
            });
            if mails.len() != before {
                changed = true;
            }
        }
        drop(folders);
        if changed {
            self.bump_generation();
        }
    }

    async fn update_mail_details(
        &self,
        folder_id: &str,
        element_id: &str,
        details: MailDetails,
        rfc2822: String,
    ) {
        let mut folders = self.folders.write().await;
        if let Some(folder) = folders.get_mut(folder_id) {
            if let Some(m) = folder.iter_mut().find(|m| {
                m.mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string())
                    .as_deref()
                    == Some(element_id)
            }) {
                m.details = Some(details);
                m.rfc2822 = Some(rfc2822);
            }
        }
        drop(folders);
        self.bump_generation();
    }

    fn bump_generation(&self) {
        let gen = self
            .gen_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.generation.send_replace(gen);
    }
}

pub async fn run_syncer(
    store: Arc<MailStore>,
    local_store: Arc<LocalStore>,
    backend: Arc<dyn MailBackend>,
    sync_limit: usize,
    shutdown: watch::Receiver<bool>,
) {
    info!(
        "Mail syncer started (limit={})",
        if sync_limit == 0 {
            "all".to_string()
        } else {
            sync_limit.to_string()
        }
    );

    // Fetch the folder list first; everything is keyed off it.
    let folders = match retry(|| backend.list_folders()).await {
        Ok(folders) => folders,
        Err(e) => {
            warn!("Could not load folder list: {e}");
            Vec::new()
        }
    };
    store.set_folder_list(folders.clone()).await;

    // Phase 0: load cached mails from the local store into memory.
    for folder in &folders {
        match load_cached_folder(&store, &local_store, folder).await {
            Ok(count) if count > 0 => {
                info!("Loaded {} cached mails for {}", count, folder.imap_path);
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to load cache for {}: {}", folder.imap_path, e),
        }
    }

    // Bootstrap: if we have no cached event-bus catch-up state, the on-disk
    // cache may be stale or empty. Run a one-shot full list sync of every
    // folder so the store reflects current server state; from then on the
    // event bus drives all updates (no periodic polling).
    let needs_bootstrap = match local_store.load_event_bus_state() {
        Ok(s) => s.is_empty(),
        Err(e) => {
            warn!("Could not read event_bus_state ({e}); assuming bootstrap needed");
            true
        }
    };
    if needs_bootstrap && !folders.is_empty() {
        info!("Bootstrap sync (no cached event-bus state)");
        for folder in &folders {
            if *shutdown.borrow() {
                return;
            }
            if let Err(e) = sync_folder(&store, &local_store, &*backend, folder, sync_limit).await {
                warn!("Bootstrap sync failed for {}: {}", folder.imap_path, e);
            }
            tokio::time::sleep(INTER_FOLDER_DELAY).await;
        }
    } else if !needs_bootstrap {
        debug!("Skipping bootstrap sync — event-bus catch-up will reconcile");
    }

    // From here on the syncer only owns the slow body prefetch. Folder /
    // new-mail refresh is driven by the event bus + `event_handler`.
    prefetch_loop(&store, &local_store, &*backend, shutdown.clone()).await;
    info!("Mail syncer shutting down");
}

/// Slow loop: progressively prefetch mail bodies in the background, on its own
/// cadence so it never delays `list_sync_loop`.
async fn prefetch_loop(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        for folder in store.list_folders().await {
            if *shutdown.borrow() {
                return;
            }
            prefetch_details(store, local_store, backend, &folder).await;
        }

        tokio::select! {
            _ = tokio::time::sleep(PREFETCH_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
    }
}

async fn load_cached_folder(
    store: &MailStore,
    local_store: &LocalStore,
    folder: &FolderInfo,
) -> Result<usize, String> {
    let metas = local_store
        .load_folder_metadata(&folder.id)
        .map_err(|e| format!("{e}"))?;

    if metas.is_empty() {
        return Ok(0);
    }

    let mut stored_mails = Vec::with_capacity(metas.len());
    for meta in &metas {
        let mail: Mail = serde_json::from_str(&meta.mail_json)
            .map_err(|e| format!("Bad cached mail {}: {e}", meta.element_id))?;

        let rfc2822 = if meta.has_details {
            match local_store.read_eml(&meta.element_id) {
                Ok(Some(eml)) => Some(eml),
                Ok(None) => Some(mail_to_rfc2822(&mail, None)),
                Err(e) => {
                    warn!("Failed to read cached eml {}: {e}", meta.element_id);
                    Some(mail_to_rfc2822(&mail, None))
                }
            }
        } else {
            Some(mail_to_rfc2822(&mail, None))
        };

        stored_mails.push(StoredMail {
            mail,
            details: None,
            rfc2822,
            uid: meta.uid as u32,
        });
    }

    let count = stored_mails.len();
    store.set_folder(&folder.id, stored_mails).await;
    Ok(count)
}

pub(crate) async fn sync_folder(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    folder: &FolderInfo,
    limit: usize,
) -> Result<(), String> {
    let new_mails = retry(|| backend.load_mail_ids_for_folder(folder, limit)).await?;

    let existing = store.get_folder(&folder.id).await;
    let existing_map: HashMap<String, StoredMail> = existing
        .into_iter()
        .filter_map(|m| {
            let eid = m.mail._id.as_ref()?.element_id.to_string();
            Some((eid, m))
        })
        .collect();

    // Allocate stable UIDs for mails we haven't seen before. `new_mails` is
    // newest-first; reverse the new ones so the oldest gets the lowest UID.
    let new_element_ids: Vec<String> = new_mails
        .iter()
        .rev()
        .filter_map(|m| m._id.as_ref().map(|id| id.element_id.to_string()))
        .filter(|eid| !existing_map.contains_key(eid))
        .collect();
    let new_uids = if new_element_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        let refs: Vec<&str> = new_element_ids.iter().map(|s| s.as_str()).collect();
        local_store
            .allocate_folder_uids(&folder.id, &refs)
            .unwrap_or_else(|e| {
                warn!("Failed to allocate UIDs for {}: {}", folder.imap_path, e);
                std::collections::HashMap::new()
            })
    };

    let mut updated = Vec::with_capacity(new_mails.len());
    let mut metas_to_upsert = Vec::with_capacity(new_mails.len());

    for mail in &new_mails {
        let elem_id = mail._id.as_ref().map(|id| id.element_id.to_string());
        let uid = elem_id
            .as_ref()
            .and_then(|id| existing_map.get(id).map(|m| m.uid).or_else(|| new_uids.get(id).copied()))
            .unwrap_or(0);

        if let Some(existing) = elem_id.as_ref().and_then(|id| existing_map.get(id)) {
            updated.push(StoredMail {
                mail: mail.clone(),
                details: existing.details.clone(),
                rfc2822: existing.rfc2822.clone(),
                uid,
            });
        } else {
            let rfc2822 = mail_to_rfc2822(mail, None);
            updated.push(StoredMail {
                mail: mail.clone(),
                details: None,
                rfc2822: Some(rfc2822),
                uid,
            });
        }

        metas_to_upsert.push(mail_to_metadata(mail, &folder.id, uid));
    }

    if let Err(e) = local_store.upsert_mail_metadata_batch(&metas_to_upsert) {
        warn!("Failed to persist metadata for {}: {}", folder.imap_path, e);
    }

    let current_ids: Vec<&str> = new_mails
        .iter()
        .filter_map(|m| m._id.as_ref().map(|id| id.element_id.as_str()))
        .collect();
    match local_store.delete_mails_not_in(&folder.id, &current_ids) {
        Ok(deleted) => {
            for eid in &deleted {
                if let Err(e) = local_store.delete_eml(eid) {
                    warn!("Failed to delete cached eml {}: {}", eid, e);
                }
            }
            if !deleted.is_empty() {
                debug!(
                    "Removed {} deleted mails from {} cache",
                    deleted.len(),
                    folder.imap_path
                );
            }
        }
        Err(e) => warn!(
            "Failed to clean up deleted mails for {}: {}",
            folder.imap_path, e
        ),
    }

    store.set_folder(&folder.id, updated).await;

    Ok(())
}

async fn prefetch_details(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    folder: &FolderInfo,
) {
    let mails = store.get_folder(&folder.id).await;
    let api_needed: Vec<Mail> = mails
        .into_iter()
        .filter(|m| m.details.is_none())
        .filter_map(|m| {
            let eid = m.mail._id.as_ref()?.element_id.to_string();
            if local_store.has_eml(&eid) {
                None
            } else {
                Some(m.mail)
            }
        })
        .collect();

    if api_needed.is_empty() {
        return;
    }

    debug!(
        "Pre-fetching {} mail details for {}",
        api_needed.len(),
        folder.imap_path
    );

    for mail in &api_needed {
        tokio::time::sleep(INTER_REQUEST_DELAY).await;

        let result = retry(|| backend.load_mail_details(mail)).await;
        match result {
            Ok(Some(details)) => {
                let rfc2822 = mail_to_rfc2822(mail, Some(&details));
                if let Some(id) = mail._id.as_ref() {
                    let eid = id.element_id.to_string();

                    if let Err(e) = local_store.write_eml(&eid, &rfc2822) {
                        warn!("Failed to cache eml {}: {}", eid, e);
                    }
                    if let Err(e) = local_store.mark_has_details(&eid) {
                        warn!("Failed to mark has_details {}: {}", eid, e);
                    }

                    store
                        .update_mail_details(&folder.id, &eid, details, rfc2822)
                        .await;
                }
            }
            Ok(None) => {
                debug!("No details for mail {:?}", mail.subject);
            }
            Err(e) => {
                warn!("Failed to prefetch details for {:?}: {}", mail.subject, e);
            }
        }
    }
}

async fn retry<F, Fut, T>(mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut delay = Duration::from_secs(1);
    for attempt in 0..=MAX_RETRIES {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES => {
                warn!("Attempt {} failed: {}, retrying in {:?}", attempt + 1, e, delay);
                tokio::time::sleep(delay).await;
                delay = backoff(delay);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

pub(crate) fn mail_to_metadata(mail: &Mail, folder_id: &str, uid: u32) -> MailMetadata {
    let (list_id, element_id) = mail
        ._id
        .as_ref()
        .map(|id| (id.list_id.to_string(), id.element_id.to_string()))
        .unwrap_or_default();

    let mail_json = serde_json::to_string(mail).unwrap_or_default();

    MailMetadata {
        list_id,
        element_id,
        folder_id: folder_id.to_string(),
        subject: mail.subject.clone(),
        sender_name: mail.sender.name.clone(),
        sender_address: mail.sender.address.clone(),
        received_date_ms: mail.receivedDate.as_millis() as i64,
        unread: mail.unread,
        has_details: false,
        uid: uid as i64,
        mail_json,
    }
}

fn backoff(current: Duration) -> Duration {
    let next = if current.is_zero() {
        Duration::from_secs(1)
    } else {
        current * 2
    };
    next.min(Duration::from_secs(120))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutasdk::date::DateTime;
    use tutasdk::entities::generated::tutanota::MailAddress;
    use tutasdk::{GeneratedId, IdTupleGenerated};

    fn id(s: &str) -> GeneratedId {
        GeneratedId(s.to_string())
    }

    /// Minimal `Mail` fixture for `MailStore` tests. Only the `_id` and a
    /// couple of metadata fields are read by the helpers under test; the rest
    /// is filled with defaults.
    fn make_mail(list: &str, element: &str, subject: &str, unread: bool) -> Mail {
        Mail {
            _id: Some(IdTupleGenerated::new(id(list), id(element))),
            _permissions: id("perm"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: subject.to_string(),
            receivedDate: DateTime::from_millis(1735130245000),
            state: 2,
            unread,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 1,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: "Sender".to_string(),
                address: "sender@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(id("conv_list"), id("conv_elem")),
            firstRecipient: None,
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        }
    }

    fn stored(mail: Mail, uid: u32) -> StoredMail {
        StoredMail {
            mail,
            details: None,
            rfc2822: None,
            uid,
        }
    }

    #[tokio::test]
    async fn refresh_mail_in_place_updates_metadata_in_every_folder() {
        let store = MailStore::new();
        // Same mail referenced in two folders (Tuta's model allows this via
        // MailSet membership). Both rows must be updated when the entity
        // changes — e.g. an "unread" toggle from the webmail.
        let m_a = make_mail("L1", "M1", "Hello", true);
        let m_b = m_a.clone();
        store.set_folder("folderA", vec![stored(m_a, 7)]).await;
        store.set_folder("folderB", vec![stored(m_b, 12)]).await;

        let mut updated = make_mail("L1", "M1", "Hello [updated]", false);
        // also tweak subject to verify the whole entity is swapped in.
        updated.subject = "Hello [updated]".into();
        store.refresh_mail_in_place(&updated).await;

        let a = store.get_folder("folderA").await;
        let b = store.get_folder("folderB").await;
        assert_eq!(a[0].mail.subject, "Hello [updated]");
        assert!(!a[0].mail.unread);
        assert_eq!(a[0].uid, 7, "UID is per-folder state, must survive a refresh");
        assert_eq!(b[0].mail.subject, "Hello [updated]");
        assert_eq!(b[0].uid, 12);
    }

    #[tokio::test]
    async fn refresh_mail_in_place_no_match_is_noop() {
        let store = MailStore::new();
        store
            .set_folder("folderA", vec![stored(make_mail("L1", "M1", "S", true), 1)])
            .await;
        let stranger = make_mail("L1", "OTHER", "X", false);
        store.refresh_mail_in_place(&stranger).await;
        let a = store.get_folder("folderA").await;
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].mail.subject, "S"); // unchanged
    }

    #[tokio::test]
    async fn remove_mail_everywhere_drops_from_all_folders() {
        let store = MailStore::new();
        store
            .set_folder(
                "folderA",
                vec![
                    stored(make_mail("L1", "keep", "k", true), 1),
                    stored(make_mail("L1", "gone", "g", true), 2),
                ],
            )
            .await;
        store
            .set_folder(
                "folderB",
                vec![stored(make_mail("L1", "gone", "g", true), 5)],
            )
            .await;
        store.remove_mail_everywhere("gone").await;

        let a = store.get_folder("folderA").await;
        let b = store.get_folder("folderB").await;
        assert_eq!(a.len(), 1);
        assert_eq!(
            a[0].mail._id.as_ref().unwrap().element_id.to_string(),
            "keep"
        );
        assert!(b.is_empty());
    }

    #[tokio::test]
    async fn remove_mail_everywhere_unknown_id_is_noop() {
        let store = MailStore::new();
        store
            .set_folder("folderA", vec![stored(make_mail("L1", "M1", "s", true), 1)])
            .await;
        store.remove_mail_everywhere("unknown").await;
        assert_eq!(store.get_folder("folderA").await.len(), 1);
    }

    #[tokio::test]
    async fn find_mail_anywhere_returns_first_match() {
        let store = MailStore::new();
        store
            .set_folder("A", vec![stored(make_mail("L1", "shared", "x", true), 7)])
            .await;
        store
            .set_folder("B", vec![stored(make_mail("L1", "shared", "x", true), 11)])
            .await;
        let found = store.find_mail_anywhere("shared").await.expect("must find");
        assert!(found.0 == "A" || found.0 == "B");
        assert_eq!(
            found.1.mail._id.as_ref().unwrap().element_id.to_string(),
            "shared"
        );
        assert!(store.find_mail_anywhere("missing").await.is_none());
    }

    #[tokio::test]
    async fn is_mail_anywhere_reflects_presence() {
        let store = MailStore::new();
        store
            .set_folder("A", vec![stored(make_mail("L1", "e1", "s", true), 1)])
            .await;
        assert!(store.is_mail_anywhere("e1").await);
        assert!(!store.is_mail_anywhere("e2").await);
    }

    #[tokio::test]
    async fn remove_mail_from_folder_only_touches_that_folder() {
        let store = MailStore::new();
        store
            .set_folder(
                "A",
                vec![
                    stored(make_mail("L1", "k", "keep", true), 1),
                    stored(make_mail("L1", "g", "gone", true), 2),
                ],
            )
            .await;
        store
            .set_folder("B", vec![stored(make_mail("L1", "g", "gone", true), 5)])
            .await;

        let removed = store.remove_mail_from_folder("A", "g").await;
        assert!(removed);
        assert_eq!(store.get_folder("A").await.len(), 1);
        // B still has the mail — `remove_mail_from_folder` is scoped.
        assert_eq!(store.get_folder("B").await.len(), 1);
        // Idempotent: re-removing the same mail from A is a no-op.
        assert!(!store.remove_mail_from_folder("A", "g").await);
    }

    #[tokio::test]
    async fn upsert_mail_in_folder_replaces_existing_by_element_id() {
        let store = MailStore::new();
        store
            .set_folder("A", vec![stored(make_mail("L1", "e1", "old", true), 4)])
            .await;
        store
            .upsert_mail_in_folder("A", stored(make_mail("L1", "e1", "new", false), 4))
            .await;
        let mails = store.get_folder("A").await;
        // Same element_id → replaced in place, no duplicate.
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].mail.subject, "new");
        assert!(!mails[0].mail.unread);
    }

    #[tokio::test]
    async fn upsert_mail_in_folder_appends_when_new() {
        let store = MailStore::new();
        store
            .set_folder("A", vec![stored(make_mail("L1", "e1", "one", true), 1)])
            .await;
        store
            .upsert_mail_in_folder("A", stored(make_mail("L1", "e2", "two", true), 2))
            .await;
        assert_eq!(store.get_folder("A").await.len(), 2);
    }


    #[tokio::test]
    async fn prune_unknown_folders_drops_disappeared_ones() {
        let store = MailStore::new();
        store
            .set_folder("keep", vec![stored(make_mail("L1", "M1", "k", true), 1)])
            .await;
        store
            .set_folder("gone", vec![stored(make_mail("L1", "M2", "g", true), 2)])
            .await;
        let known: std::collections::HashSet<String> =
            ["keep".to_string()].into_iter().collect();
        let removed = store.prune_unknown_folders(&known).await;
        assert_eq!(removed, vec!["gone".to_string()]);
        assert_eq!(store.get_folder("keep").await.len(), 1);
        assert!(store.get_folder("gone").await.is_empty());
    }
}
