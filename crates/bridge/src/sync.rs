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
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
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

    // Run the fast list/folder sync and the slow body prefetch as independent
    // loops: a long prefetch pass must never block folder / new-mail refresh.
    tokio::join!(
        list_sync_loop(&store, &local_store, &*backend, sync_limit, shutdown.clone()),
        prefetch_loop(&store, &local_store, &*backend, shutdown.clone()),
    );
    info!("Mail syncer shutting down");
}

/// Fast loop: refresh the folder list and sync the mail-id list for every
/// folder. This is what surfaces new folders and new mail (~every
/// `SYNC_INTERVAL`), independent of the slow body prefetch.
async fn list_sync_loop(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    sync_limit: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut cycle_backoff = Duration::ZERO;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut had_error = false;

        let folders = match retry(|| backend.list_folders()).await {
            Ok(folders) => {
                store.set_folder_list(folders.clone()).await;
                folders
            }
            Err(e) => {
                warn!("Failed to refresh folder list: {e}");
                had_error = true;
                store.list_folders().await
            }
        };

        for folder in &folders {
            if *shutdown.borrow() {
                return;
            }
            if let Err(e) = sync_folder(store, local_store, backend, folder, sync_limit).await {
                warn!("Sync error for {}: {}", folder.imap_path, e);
                had_error = true;
            }
            tokio::time::sleep(INTER_FOLDER_DELAY).await;
        }

        cycle_backoff = if had_error {
            backoff(cycle_backoff)
        } else {
            Duration::ZERO
        };
        let wait = SYNC_INTERVAL + cycle_backoff;
        debug!("Next list sync in {:?}", wait);

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return,
        }
    }
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

async fn sync_folder(
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

fn mail_to_metadata(mail: &Mail, folder_id: &str, uid: u32) -> MailMetadata {
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
