use std::sync::Arc;
use std::time::Duration;

use log::{info, warn, debug};
use tokio::sync::{watch, RwLock};
use tutasdk::entities::generated::tutanota::{Mail, MailDetails};
use tutasdk::folder_system::MailSetKind;

use crate::mail::mail_to_rfc2822;
use crate::store::{LocalStore, MailMetadata};
use crate::tuta::MailBackend;

const FOLDERS: &[MailSetKind] = &[
    MailSetKind::Inbox,
    MailSetKind::Sent,
    MailSetKind::Draft,
    MailSetKind::Trash,
    MailSetKind::Archive,
    MailSetKind::Spam,
];

const INTER_REQUEST_DELAY: Duration = Duration::from_millis(150);
const INTER_FOLDER_DELAY: Duration = Duration::from_millis(300);
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
const MAX_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct StoredMail {
    pub mail: Mail,
    pub details: Option<MailDetails>,
    pub rfc2822: Option<String>,
}

pub struct MailStore {
    folders: RwLock<Vec<(MailSetKind, Vec<StoredMail>)>>,
    generation: watch::Sender<u64>,
    gen_counter: std::sync::atomic::AtomicU64,
}

impl MailStore {
    pub fn new() -> Arc<Self> {
        let (tx, _) = watch::channel(0u64);
        Arc::new(Self {
            folders: RwLock::new(Vec::new()),
            generation: tx,
            gen_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    pub async fn total_mail_count(&self) -> usize {
        self.folders.read().await.iter().map(|(_, v)| v.len()).sum()
    }

    pub async fn folder_count(&self, kind: MailSetKind) -> usize {
        self.folders
            .read()
            .await
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.len())
            .unwrap_or(0)
    }

    pub async fn get_folder(&self, kind: MailSetKind) -> Vec<StoredMail> {
        self.folders
            .read()
            .await
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    pub async fn get_details(&self, kind: MailSetKind, element_id: &str) -> Option<(MailDetails, String)> {
        let folders = self.folders.read().await;
        let (_, folder) = folders.iter().find(|(k, _)| *k == kind)?;
        folder.iter().find_map(|m| {
            let eid = m.mail._id.as_ref()?.element_id.to_string();
            if eid == element_id {
                let details = m.details.clone()?;
                let rfc = m.rfc2822.clone()?;
                Some((details, rfc))
            } else {
                None
            }
        })
    }

    pub(crate) async fn set_folder(&self, kind: MailSetKind, mails: Vec<StoredMail>) {
        let mut folders = self.folders.write().await;
        if let Some(entry) = folders.iter_mut().find(|(k, _)| *k == kind) {
            entry.1 = mails;
        } else {
            folders.push((kind, mails));
        }
        drop(folders);
        self.bump_generation();
    }

    async fn update_mail_details(
        &self,
        kind: MailSetKind,
        element_id: &str,
        details: MailDetails,
        rfc2822: String,
    ) {
        let mut folders = self.folders.write().await;
        if let Some((_, folder)) = folders.iter_mut().find(|(k, _)| *k == kind) {
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
        let gen = self.gen_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.generation.send_replace(gen);
    }
}

pub async fn run_syncer(
    store: Arc<MailStore>,
    local_store: Arc<LocalStore>,
    backend: Arc<dyn MailBackend>,
    sync_limit: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    info!("Mail syncer started (limit={})", if sync_limit == 0 { "all".to_string() } else { sync_limit.to_string() });

    // Phase 0: load cached mails from local store into memory
    for &kind in FOLDERS {
        match load_cached_folder(&store, &local_store, kind).await {
            Ok(count) if count > 0 => {
                info!("Loaded {} cached mails for {:?}", count, kind);
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to load cache for {:?}: {}", kind, e),
        }
    }

    let mut cycle_backoff = Duration::ZERO;

    loop {
        let mut had_error = false;

        // Phase 1: sync mail lists for ALL folders (fast, no body loading)
        for &kind in FOLDERS {
            if *shutdown.borrow() {
                info!("Mail syncer shutting down");
                return;
            }

            match sync_folder(&store, &local_store, &*backend, kind, sync_limit).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("Sync error for {:?}: {}", kind, e);
                    had_error = true;
                }
            }

            tokio::time::sleep(INTER_FOLDER_DELAY).await;
        }

        // Phase 2: prefetch mail details (slow, but all folders are already visible)
        for &kind in FOLDERS {
            if *shutdown.borrow() {
                return;
            }
            prefetch_details(&store, &local_store, &*backend, kind).await;
        }

        if had_error {
            cycle_backoff = backoff(cycle_backoff);
            warn!("Sync cycle had errors, backing off {:?}", cycle_backoff);
        } else {
            cycle_backoff = Duration::ZERO;
        }

        let wait = SYNC_INTERVAL + cycle_backoff;
        debug!("Next sync in {:?}", wait);

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                info!("Mail syncer shutting down");
                return;
            }
        }
    }
}

async fn load_cached_folder(
    store: &MailStore,
    local_store: &LocalStore,
    kind: MailSetKind,
) -> Result<usize, String> {
    let metas = local_store
        .load_folder_metadata(kind)
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
        });
    }

    let count = stored_mails.len();
    store.set_folder(kind, stored_mails).await;
    Ok(count)
}

async fn sync_folder(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    kind: MailSetKind,
    limit: usize,
) -> Result<(), String> {
    let new_mails = retry(|| backend.load_mail_ids_for_folder(kind, limit)).await?;

    let existing = store.get_folder(kind).await;
    let existing_map: std::collections::HashMap<String, StoredMail> = existing
        .into_iter()
        .filter_map(|m| {
            let eid = m.mail._id.as_ref()?.element_id.to_string();
            Some((eid, m))
        })
        .collect();

    let mut updated = Vec::with_capacity(new_mails.len());
    let mut metas_to_upsert = Vec::with_capacity(new_mails.len());

    for mail in &new_mails {
        let elem_id = mail._id.as_ref().map(|id| id.element_id.to_string());
        if let Some(existing) = elem_id.as_ref().and_then(|id| existing_map.get(id)) {
            updated.push(StoredMail {
                mail: mail.clone(),
                details: existing.details.clone(),
                rfc2822: existing.rfc2822.clone(),
            });
        } else {
            let rfc2822 = mail_to_rfc2822(mail, None);
            updated.push(StoredMail {
                mail: mail.clone(),
                details: None,
                rfc2822: Some(rfc2822),
            });
        }

        metas_to_upsert.push(mail_to_metadata(mail, kind));
    }

    // Persist metadata to local store
    if let Err(e) = local_store.upsert_mail_metadata_batch(&metas_to_upsert) {
        warn!("Failed to persist metadata for {:?}: {}", kind, e);
    }

    // Delete mails removed from server
    let current_ids: Vec<&str> = new_mails
        .iter()
        .filter_map(|m| m._id.as_ref().map(|id| id.element_id.as_str()))
        .collect();
    match local_store.delete_mails_not_in(kind, &current_ids) {
        Ok(deleted) => {
            for eid in &deleted {
                if let Err(e) = local_store.delete_eml(eid) {
                    warn!("Failed to delete cached eml {}: {}", eid, e);
                }
            }
            if !deleted.is_empty() {
                debug!("Removed {} deleted mails from {:?} cache", deleted.len(), kind);
            }
        }
        Err(e) => warn!("Failed to clean up deleted mails for {:?}: {}", kind, e),
    }

    store.set_folder(kind, updated).await;

    Ok(())
}

async fn prefetch_details(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    kind: MailSetKind,
) {
    let folder = store.get_folder(kind).await;
    let api_needed: Vec<Mail> = folder
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

    debug!("Pre-fetching {} mail details for {:?}", api_needed.len(), kind);

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
                        .update_mail_details(kind, &eid, details, rfc2822)
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

fn mail_to_metadata(mail: &Mail, kind: MailSetKind) -> MailMetadata {
    let (list_id, element_id) = mail
        ._id
        .as_ref()
        .map(|id| (id.list_id.to_string(), id.element_id.to_string()))
        .unwrap_or_default();

    let mail_json = serde_json::to_string(mail).unwrap_or_default();

    MailMetadata {
        list_id,
        element_id,
        folder_kind: kind as i64,
        subject: mail.subject.clone(),
        sender_name: mail.sender.name.clone(),
        sender_address: mail.sender.address.clone(),
        received_date_ms: mail.receivedDate.as_millis() as i64,
        unread: mail.unread,
        has_details: false,
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
