use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use tokio::sync::{watch, RwLock};
use tutasdk::entities::generated::tutanota::{Mail, MailDetails, TutanotaFile};

use crate::mail::{extract_body_text, mail_to_rfc2822, strip_html};
use crate::store::{LocalStore, MailMetadata};
use crate::tuta::{FolderInfo, MailBackend};

const INTER_REQUEST_DELAY: Duration = Duration::from_millis(150);
const INTER_FOLDER_DELAY: Duration = Duration::from_millis(300);
const MAX_RETRIES: u32 = 3;
/// How long a mail whose on-demand body fetch just failed is skipped before the
/// bridge will try the API again for it. Stops a client from re-triggering a
/// fetch for the same message on every redraw while the server is throttling.
const BODY_FETCH_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct StoredMail {
    pub mail: Mail,
    /// True when the real body is available locally (in the [`BodyCache`] or
    /// as a persisted `.eml.enc`). The body content itself is NOT stored here:
    /// keeping every rendered body (attachments inlined) in the metadata store
    /// grew the process by the size of the mailbox. Bodies live in the bounded
    /// [`crate::body_cache::BodyCache`], backed by the on-disk cache.
    pub body_loaded: bool,
    /// Stable IMAP UID within the folder, persisted across restarts.
    pub uid: u32,
    /// True while the mail's body has been rendered without its attachment
    /// parts because the server returned them with unresolved encryption
    /// metadata (a fresh-mail race). The `prefetch_loop` keeps retrying
    /// just the attachment load until it succeeds — at which point the
    /// `.eml` is rewritten as a proper `multipart/mixed`.
    pub attachments_pending: bool,
}

pub struct MailStore {
    /// folder id → mails in that folder.
    folders: RwLock<HashMap<String, Vec<StoredMail>>>,
    /// The folder list (system + custom), for IMAP enumeration.
    folder_list: RwLock<Vec<FolderInfo>>,
    /// Bumped on list-membership changes (mail added/removed, folder list).
    /// IMAP sessions and the prefetch loop wake on this one.
    generation: watch::Sender<u64>,
    /// Bumped on body-only updates (details/rfc2822 stored). Only the GUI
    /// stats watcher subscribes: waking every idle IMAP session for each of
    /// thousands of arriving bodies made each one deep-clone the folder per
    /// body (the CPU/memory storm).
    details_generation: watch::Sender<u64>,
    gen_counter: std::sync::atomic::AtomicU64,
    /// element_id -> instant until which an on-demand body fetch is skipped,
    /// armed after a fetch failure. Shared across IMAP connections.
    body_fetch_cooldown: Mutex<HashMap<String, Instant>>,
    /// Bounded LRU of rendered bodies (see [`crate::body_cache`]).
    bodies: crate::body_cache::BodyCache,
}

impl MailStore {
    pub fn new() -> Arc<Self> {
        let (tx, _) = watch::channel(0u64);
        let (details_tx, _) = watch::channel(0u64);
        Arc::new(Self {
            folders: RwLock::new(HashMap::new()),
            folder_list: RwLock::new(Vec::new()),
            generation: tx,
            details_generation: details_tx,
            gen_counter: std::sync::atomic::AtomicU64::new(0),
            body_fetch_cooldown: Mutex::new(HashMap::new()),
            bodies: crate::body_cache::BodyCache::new(),
        })
    }

    /// The bounded body cache. Wire the disk fallback once at startup with
    /// [`crate::body_cache::BodyCache::set_local_store`].
    pub fn bodies(&self) -> &crate::body_cache::BodyCache {
        &self.bodies
    }

    /// True if a recent on-demand body fetch for `element_id` failed and its
    /// cooldown has not yet elapsed. Expired entries are dropped lazily.
    pub(crate) fn body_fetch_on_cooldown(&self, element_id: &str) -> bool {
        let mut map = crate::util::lock_recover(&self.body_fetch_cooldown);
        match map.get(element_id) {
            Some(&until) if until > Instant::now() => true,
            Some(_) => {
                map.remove(element_id);
                false
            }
            None => false,
        }
    }

    /// Arm a cooldown after a failed on-demand body fetch so the bridge does not
    /// immediately re-hit a throttled or erroring server for the same mail.
    pub(crate) fn mark_body_fetch_failed(&self, element_id: &str) {
        let mut map = crate::util::lock_recover(&self.body_fetch_cooldown);
        let now = Instant::now();
        // Drop elapsed entries so the map stays bounded by the number of mails
        // currently cooling down, not every mail that has ever failed.
        map.retain(|_, until| *until > now);
        map.insert(element_id.to_string(), now + BODY_FETCH_COOLDOWN);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Watch body-only updates (a mail's details/rfc2822 landed). Cheap,
    /// high-frequency during prefetch; meant for stats display, not for
    /// anything that re-reads folder contents.
    pub fn subscribe_details(&self) -> watch::Receiver<u64> {
        self.details_generation.subscribe()
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

    /// A mail's rendered body (LRU hit, or re-read from the encrypted disk
    /// cache). `None` when the body was never fetched.
    pub async fn get_body(
        &self,
        element_id: &str,
    ) -> Option<std::sync::Arc<crate::body_cache::BodyEntry>> {
        self.bodies.get(element_id).await
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
        let Some(eid) = mail._id.as_ref().map(|id| id.element_id.to_string()) else {
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

    pub(crate) async fn update_complete_mail(
        &self,
        folder_id: &str,
        element_id: &str,
        details: Option<MailDetails>,
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
                m.body_loaded = true;
                m.attachments_pending = false;
            }
        }
        drop(folders);
        self.bodies.insert(
            element_id,
            crate::body_cache::BodyEntry { details, rfc2822 },
        );
        self.bump_details_generation();
    }

    fn bump_generation(&self) {
        let gen = self
            .gen_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.generation.send_replace(gen);
    }

    /// Signal a body-only update. Deliberately NOT the list generation: a body
    /// landing changes no EXISTS/EXPUNGE, so waking every idle IMAP session
    /// (which each deep-clone the folder to diff it) per arriving body is pure
    /// waste, at thousands of bodies per sync.
    fn bump_details_generation(&self) {
        let gen = self
            .gen_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.details_generation.send_replace(gen);
    }
}

pub async fn run_syncer(
    store: Arc<MailStore>,
    local_store: Arc<LocalStore>,
    backend: Arc<dyn MailBackend>,
    sync_limit: usize,
    startup_done: watch::Sender<bool>,
    shutdown: watch::Receiver<bool>,
) {
    info!(
        "Mail syncer started (body prefetch depth={})",
        if sync_limit == 0 {
            "all".to_string()
        } else {
            sync_limit.to_string()
        }
    );

    // Give the body cache its disk fallback: an evicted (or not-yet-loaded)
    // body is re-read from the encrypted `.eml.enc` on first use.
    store.bodies().set_local_store(local_store.clone());

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

    // We run a one-shot full **metadata** sync (the whole mail list of every
    // folder) when either:
    //   * there's no cached event-bus state (fresh install / cache wiped), or
    //   * we've never completed a full-metadata sync before — the marker
    //     below. This second case migrates existing installs that were synced
    //     under the old `sync_limit`-truncated model up to a complete mailbox.
    // After it runs once, the event bus keeps the list current; we don't
    // re-list every launch (the metadata is persisted + loaded in Phase 0).
    const FULL_METADATA_MARKER: &str = "full_metadata_synced_v1";
    let needs_bootstrap = match local_store.load_event_bus_state() {
        Ok(s) => s.is_empty(),
        Err(e) => {
            warn!("Could not read event_bus_state ({e}); assuming bootstrap needed");
            true
        }
    };
    let full_metadata_done = local_store.get_meta(FULL_METADATA_MARKER).is_some();
    let do_full_sync = (needs_bootstrap || !full_metadata_done) && !folders.is_empty();

    if do_full_sync {
        if needs_bootstrap {
            info!("Bootstrap sync (no cached event-bus state)");
        } else {
            info!("One-time full-metadata sync (completing the mailbox view)");
        }
        let mut all_ok = true;
        for folder in &folders {
            if *shutdown.borrow() {
                return;
            }
            if let Err(e) = sync_folder(&store, &local_store, &*backend, folder).await {
                warn!("Full-metadata sync failed for {}: {}", folder.imap_path, e);
                all_ok = false;
            }
            tokio::time::sleep(INTER_FOLDER_DELAY).await;
        }
        // Only set the marker if every folder synced, so a transient failure
        // gets retried on the next launch rather than silently leaving the
        // mailbox incomplete.
        if all_ok {
            if let Err(e) = local_store.set_meta(FULL_METADATA_MARKER, "1") {
                warn!("Could not persist full-metadata marker: {e}");
            }
        }
    } else {
        debug!("Skipping full-metadata sync — already complete, event bus reconciles");
    }

    // Both steps above replace whole folders in memory (and the full sync
    // prunes the disk cache to what the server listed). The event handler
    // holds its events until now, or a mail it applied in between would be
    // overwritten. Nothing below replaces a folder.
    let _ = startup_done.send(true);

    // One-time backfill of the full-text body index from bodies that were
    // already cached (downloaded before the index existed). Bodies fetched from
    // here on are indexed inline by `prefetch_details`, so this only ever runs
    // once per install.
    const FTS_BACKFILL_MARKER: &str = "body_fts_indexed_v1";
    if local_store.get_meta(FTS_BACKFILL_MARKER).is_none() && !folders.is_empty() {
        info!("Backfilling full-text body index from cached messages…");
        let mut indexed = 0usize;
        let mut all_ok = true;
        for folder in &folders {
            if *shutdown.borrow() {
                return;
            }
            // Same reasoning as `load_cached_folder`: reading and decrypting
            // every cached body is CPU-bound work that must not run on an async
            // worker, or it starves the network accept loops. Index one folder
            // per blocking task; the shutdown check stays on the async side
            // between folders.
            let ls = local_store.clone();
            let fid = folder.id.clone();
            let fpath = folder.imap_path.clone();
            let (folder_indexed, folder_ok) =
                tokio::task::spawn_blocking(move || -> (usize, bool) {
                    let mut indexed = 0usize;
                    let mut ok = true;
                    match ls.load_folder_metadata(&fid) {
                        Ok(metas) => {
                            for meta in metas.iter().filter(|m| m.has_details) {
                                match ls.read_eml(&meta.element_id) {
                                    Ok(Some(rfc)) => {
                                        let text = extract_body_text(&rfc);
                                        if let Err(e) = ls.index_body(&meta.element_id, &text) {
                                            warn!(
                                                "FTS backfill failed for {}: {e}",
                                                meta.element_id
                                            );
                                            ok = false;
                                        } else {
                                            indexed += 1;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        warn!(
                                            "FTS backfill read failed for {}: {e}",
                                            meta.element_id
                                        );
                                        ok = false;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("FTS backfill: no metadata for {fpath}: {e}");
                            ok = false;
                        }
                    }
                    (indexed, ok)
                })
                .await
                .unwrap_or_else(|e| {
                    warn!("FTS backfill task failed: {e}");
                    (0, false)
                });
            indexed += folder_indexed;
            if !folder_ok {
                all_ok = false;
            }
        }
        // Only mark done if nothing errored, so a partial run retries next boot.
        if all_ok {
            if let Err(e) = local_store.set_meta(FTS_BACKFILL_MARKER, "1") {
                warn!("Could not persist FTS backfill marker: {e}");
            }
        }
        info!("Full-text body index backfill complete ({indexed} messages)");
    }

    // From here on the syncer only owns the slow body prefetch (capped at
    // `sync_limit` — the body-prefetch depth). Folder / new-mail refresh is
    // driven by the event bus + `event_handler`.
    prefetch_loop(
        &store,
        &local_store,
        &*backend,
        sync_limit,
        shutdown.clone(),
    )
    .await;
    info!("Mail syncer shutting down");
}

/// Event-driven background prefetch. Wakes whenever the `MailStore`'s
/// generation counter changes (a new mail arrived, a sync finished, …) and
/// catches up any folder that still has bodies missing. When the store is
/// quiet — every Mail has its `.eml` cached, no folder mutations in flight —
/// the loop sleeps indefinitely on the watch channel; no timer-driven
/// polling.
async fn prefetch_loop(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    body_prefetch_limit: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut store_watch = store.subscribe();
    // Retry state survives store updates, but not a restart. Unknown or
    // transient failures back off to one hour; contract failures start there.
    let mut attachment_retry_history: HashMap<String, AttachmentRetry> = HashMap::new();
    loop {
        if *shutdown.borrow() {
            return;
        }
        for folder in store.list_folders().await {
            if *shutdown.borrow() {
                return;
            }
            prefetch_details(
                store,
                local_store,
                backend,
                &folder,
                body_prefetch_limit,
                &mut attachment_retry_history,
            )
            .await;
        }

        // If any attachment retry is still in flight (throttle is set) we
        // must wake up after the throttle window even if nothing else
        // touches the store — otherwise the retry loop relies on another
        // mail arriving to be re-triggered.
        let next_tick = next_attachment_retry(&attachment_retry_history, Instant::now());
        tokio::select! {
            _ = store_watch.changed() => {}
            _ = shutdown.changed() => return,
            _ = async {
                match next_tick {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
}

const ATTACHMENT_RETRY_MIN: Duration = Duration::from_secs(60);
const ATTACHMENT_RETRY_MAX: Duration = Duration::from_secs(3600);

pub(crate) struct AttachmentRetry {
    failures: u32,
    next_attempt: Instant,
}

impl AttachmentRetry {
    fn after_failure(previous: Option<&Self>, retryable: bool, now: Instant) -> Self {
        let failures = previous.map_or(1, |previous| previous.failures.saturating_add(1));
        let delay = if retryable {
            (ATTACHMENT_RETRY_MIN * (1 << failures.saturating_sub(1).min(6)))
                .min(ATTACHMENT_RETRY_MAX)
        } else {
            ATTACHMENT_RETRY_MAX
        };
        Self {
            failures,
            next_attempt: now + delay,
        }
    }
}

fn next_attachment_retry(
    history: &HashMap<String, AttachmentRetry>,
    now: Instant,
) -> Option<Duration> {
    history
        .values()
        .filter_map(|retry| retry.next_attempt.checked_duration_since(now))
        .filter(|delay| !delay.is_zero())
        .min()
}

pub(crate) async fn load_cached_folder(
    store: &MailStore,
    local_store: &Arc<LocalStore>,
    folder: &FolderInfo,
) -> Result<usize, String> {
    // Decoding a cached folder means deserializing every metadata row and, for
    // each mail, reading and decrypting its `.eml.enc` body (AES-CBC + an
    // HMAC-SHA256 verification). For a large folder that is seconds of pure CPU
    // with no `.await` in between. Run inline on a tokio worker it would keep
    // that worker (and the IO driver it owns) busy for the whole loop, which
    // stalls the IMAP/SMTP accept loops until the folder finishes loading. So
    // do the decode on the blocking pool and only touch the async store once
    // the `Vec` is built.
    let ls = local_store.clone();
    let folder_id = folder.id.clone();
    let stored_mails = tokio::task::spawn_blocking(move || -> Result<Vec<StoredMail>, String> {
        let metas = ls
            .load_folder_metadata(&folder_id)
            .map_err(|e| format!("{e}"))?;

        let mut stored_mails = Vec::with_capacity(metas.len());
        for meta in &metas {
            let mail: Mail = serde_json::from_str(&meta.mail_json)
                .map_err(|e| format!("Bad cached mail {}: {e}", meta.element_id))?;

            // The `.eml` file on disk is the source of truth for "body
            // available" and may exist even when the metadata row says
            // otherwise (a schema migration drops the `mails` table but keeps
            // the encrypted `.eml.enc` files, so `has_details` is reset to 0 on
            // first boot after the migration). Self-heal the row when we find
            // an orphaned body. The body content itself is NOT loaded here:
            // decrypting every cached body at boot pinned the whole mailbox in
            // memory; bodies are pulled into the bounded cache on first use.
            let body_loaded = ls.has_eml(&meta.element_id);
            if body_loaded && !meta.has_details {
                if let Err(e) = ls.mark_has_details(&meta.element_id) {
                    warn!("Failed to heal has_details for {}: {e}", meta.element_id);
                }
            }

            stored_mails.push(StoredMail {
                mail,
                body_loaded,
                uid: meta.uid as u32,
                attachments_pending: false,
            });
        }
        Ok(stored_mails)
    })
    .await
    .map_err(|e| format!("cache load task failed: {e}"))??;

    let count = stored_mails.len();
    store.set_folder(&folder.id, stored_mails).await;
    Ok(count)
}

/// Sync a folder's full mail-list **metadata** (subject / sender / date /
/// flags) — every message, not a `sync_limit`-capped slice. This is what
/// makes the whole mailbox visible over IMAP; message *bodies* are fetched
/// separately and lazily (see [`prefetch_details`] + on-demand FETCH).
pub(crate) async fn sync_folder(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    folder: &FolderInfo,
) -> Result<(), String> {
    // `0` = load every entry (the SDK paginates). Metadata is cheap relative
    // to bodies (no blob fetch), so we never truncate the list.
    let new_mails = retry(|| backend.load_mail_ids_for_folder(folder, 0)).await?;

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
            .and_then(|id| {
                existing_map
                    .get(id)
                    .map(|m| m.uid)
                    .or_else(|| new_uids.get(id).copied())
            })
            .unwrap_or(0);

        if let Some(existing) = elem_id.as_ref().and_then(|id| existing_map.get(id)) {
            updated.push(StoredMail {
                mail: mail.clone(),
                body_loaded: existing.body_loaded,
                uid,
                attachments_pending: existing.attachments_pending,
            });
        } else {
            // New mail, body not fetched yet — the IMAP layer fetches it on
            // demand the first time a client opens it.
            updated.push(StoredMail {
                mail: mail.clone(),
                body_loaded: false,
                uid,
                attachments_pending: false,
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
                if let Err(e) = local_store.unindex_body(eid) {
                    warn!("Failed to unindex body {}: {}", eid, e);
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

/// The mails `prefetch_details` covers: the newest `limit` of a folder, or all
/// of them when `limit` is 0.
///
/// Sorted here rather than trusting the folder's order: a folder is loaded
/// newest first, but mail that arrives later is appended by
/// `upsert_mail_in_folder`, so in a folder larger than the window new mail
/// would stay outside it until the bridge restarts, its body neither cached
/// nor indexed for search.
fn prefetch_window(mut mails: Vec<StoredMail>, limit: usize) -> Vec<StoredMail> {
    mails.sort_by_key(|m| std::cmp::Reverse(m.mail.receivedDate.as_millis()));
    if limit != 0 {
        mails.truncate(limit);
    }
    mails
}

pub(crate) async fn prefetch_details(
    store: &MailStore,
    local_store: &LocalStore,
    backend: &dyn MailBackend,
    folder: &FolderInfo,
    body_prefetch_limit: usize,
    attachment_retry_history: &mut HashMap<String, AttachmentRetry>,
) {
    let window = prefetch_window(store.get_folder(&folder.id).await, body_prefetch_limit);
    for stored in &window {
        let mail = &stored.mail;
        let Some(id) = &mail._id else { continue };
        let eid = id.element_id.to_string();
        if stored.body_loaded || local_store.has_eml(&eid) {
            attachment_retry_history.remove(&eid);
            continue;
        }
        if attachment_retry_history
            .get(&eid)
            .is_some_and(|last| Instant::now() < last.next_attempt)
        {
            continue;
        }
        tokio::time::sleep(INTER_REQUEST_DELAY).await;
        let details = match retry(|| backend.load_mail_details(mail)).await {
            Ok(details) => details,
            Err(_) => {
                warn!("Failed to prefetch message details");
                let retry = AttachmentRetry::after_failure(
                    attachment_retry_history.get(&eid),
                    true,
                    Instant::now(),
                );
                attachment_retry_history.insert(eid, retry);
                continue;
            }
        };
        let attachments = match backend.load_attachments(mail).await {
            Ok(attachments) => attachments,
            Err(error) => {
                let retry = AttachmentRetry::after_failure(
                    attachment_retry_history.get(&eid),
                    is_transient_attachment_error(&error),
                    Instant::now(),
                );
                attachment_retry_history.insert(eid, retry);
                warn!("Message remains unavailable: {error}");
                continue;
            }
        };
        let refs: Vec<(&TutanotaFile, &[u8])> =
            attachments.iter().map(|(f, d)| (f, d.as_slice())).collect();
        let rfc = mail_to_rfc2822(mail, details.as_ref(), &refs);
        // Only complete messages become final cache entries, including mails without a body reference.
        if let Err(error) = local_store.write_eml(&eid, &rfc) {
            warn!("Failed to persist complete message: {error}");
        } else if let Err(error) = local_store.mark_has_details(&eid) {
            warn!("Failed to mark complete message: {error}");
        }
        if let Some(details) = &details {
            let body_html = details
                .body
                .compressedText
                .as_deref()
                .or(details.body.text.as_deref())
                .unwrap_or("");
            let body_text = strip_html(body_html);
            if let Err(error) = local_store.index_body(&eid, &body_text) {
                warn!("Failed to index complete message: {error}");
            }
        }
        store
            .update_complete_mail(&folder.id, &eid, details, rfc)
            .await;
        attachment_retry_history.remove(&eid);
    }
}

fn is_transient_attachment_error(error: &tutabridge_tuta::mail::AttachmentError) -> bool {
    use tutabridge_tuta::mail::AttachmentError;
    use tutasdk::{rest_error::HttpError, ApiCallError};
    match error {
        AttachmentError::Contract(_) => false,
        AttachmentError::Indeterminate(_) => true,
        AttachmentError::Transport(ApiCallError::RestClient { .. }) => true,
        AttachmentError::Transport(ApiCallError::ServerResponseError { source }) => matches!(
            source,
            HttpError::ConnectionError
                | HttpError::RequestTimeoutError
                | HttpError::TooManyRequestsError { .. }
                | HttpError::InternalServerError
                | HttpError::BadGatewayError
                | HttpError::ServiceUnavailableError { .. }
        ),
        AttachmentError::Transport(ApiCallError::InternalSdkError { .. }) => true,
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
                warn!(
                    "Attempt {} failed: {}, retrying in {:?}",
                    attempt + 1,
                    e,
                    delay
                );
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
            body_loaded: false,
            uid,
            attachments_pending: false,
        }
    }

    #[tokio::test]
    async fn body_updates_wake_details_watchers_not_list_watchers() {
        use tutasdk::entities::generated::tutanota::{Body, Recipients};
        let details = MailDetails {
            _id: None,
            sentDate: DateTime::from_millis(0),
            authStatus: 0,
            replyTos: vec![],
            recipients: Recipients {
                _id: None,
                toRecipients: vec![],
                ccRecipients: vec![],
                bccRecipients: vec![],
            },
            headers: None,
            body: Body {
                _id: None,
                text: Some("hi".into()),
                compressedText: None,
                _errors: Default::default(),
            },
        };

        let store = MailStore::new();
        store
            .set_folder("f", vec![stored(make_mail("l", "e1", "s", false), 1)])
            .await;
        let mut list_watch = store.subscribe();
        let mut details_watch = store.subscribe_details();
        list_watch.borrow_and_update();
        details_watch.borrow_and_update();

        // A body landing must signal stats watchers without waking the list
        // watchers every idle IMAP session sleeps on (the clone storm).
        store
            .update_complete_mail("f", "e1", Some(details), "body".into())
            .await;
        assert!(
            !list_watch.has_changed().unwrap(),
            "body-only update must not wake list watchers"
        );
        assert!(
            details_watch.has_changed().unwrap(),
            "body-only update must wake details watchers"
        );

        // A membership change still wakes the list watchers.
        store
            .set_folder("f", vec![stored(make_mail("l", "e2", "s2", true), 2)])
            .await;
        assert!(list_watch.has_changed().unwrap());
    }

    #[test]
    fn attachment_retry_backoff_is_bounded_and_ignores_expired_deadlines() {
        let now = Instant::now();
        let mut previous = None;
        for expected in [60, 120, 240, 480, 960, 1920, 3600, 3600] {
            let retry = AttachmentRetry::after_failure(previous.as_ref(), true, now);
            assert_eq!(
                retry.next_attempt.duration_since(now),
                Duration::from_secs(expected)
            );
            previous = Some(retry);
        }
        let permanent = AttachmentRetry::after_failure(None, false, now);
        assert_eq!(
            permanent.next_attempt.duration_since(now),
            ATTACHMENT_RETRY_MAX
        );
        let mut history = HashMap::from([("failed".into(), permanent)]);
        assert_eq!(
            next_attachment_retry(&history, now),
            Some(ATTACHMENT_RETRY_MAX)
        );
        assert_eq!(
            next_attachment_retry(&history, now + ATTACHMENT_RETRY_MAX),
            None
        );
        history.insert(
            "recoverable".into(),
            AttachmentRetry::after_failure(None, true, now),
        );
        assert_eq!(
            next_attachment_retry(&history, now),
            Some(ATTACHMENT_RETRY_MIN)
        );
    }

    #[test]
    fn body_fetch_cooldown_blocks_then_expires() {
        let store = MailStore::new();
        assert!(!store.body_fetch_on_cooldown("mail-a"));
        store.mark_body_fetch_failed("mail-a");
        assert!(store.body_fetch_on_cooldown("mail-a"));
        // A different mail is unaffected.
        assert!(!store.body_fetch_on_cooldown("mail-b"));
        // An elapsed entry is treated as expired and dropped lazily.
        crate::util::lock_recover(&store.body_fetch_cooldown).insert(
            "mail-a".to_string(),
            Instant::now() - Duration::from_secs(1),
        );
        assert!(!store.body_fetch_on_cooldown("mail-a"));
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
        assert_eq!(
            a[0].uid, 7,
            "UID is per-folder state, must survive a refresh"
        );
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

    fn received_at(element: &str, received_ms: u64, uid: u32) -> StoredMail {
        let mut mail = make_mail("L1", element, element, true);
        mail.receivedDate = DateTime::from_millis(received_ms);
        stored(mail, uid)
    }

    fn element_ids(mails: &[StoredMail]) -> Vec<String> {
        mails
            .iter()
            .map(|m| m.mail._id.as_ref().unwrap().element_id.to_string())
            .collect()
    }

    #[tokio::test]
    async fn prefetch_window_covers_mail_that_arrives_after_the_load() {
        let store = MailStore::new();
        // Loaded newest first, as at startup.
        store
            .set_folder(
                "A",
                vec![
                    received_at("e3", 3_000, 3),
                    received_at("e2", 2_000, 2),
                    received_at("e1", 1_000, 1),
                ],
            )
            .await;
        // Newer than everything, and appended.
        store
            .upsert_mail_in_folder("A", received_at("e4", 4_000, 4))
            .await;

        let window = prefetch_window(store.get_folder("A").await, 2);
        assert_eq!(element_ids(&window), ["e4", "e3"]);
    }

    #[test]
    fn prefetch_window_without_a_limit_keeps_every_mail() {
        let folder = vec![
            received_at("e2", 2_000, 2),
            received_at("e1", 1_000, 1),
            received_at("e3", 3_000, 3),
        ];
        let window = prefetch_window(folder, 0);
        assert_eq!(element_ids(&window), ["e3", "e2", "e1"]);
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
        let known: std::collections::HashSet<String> = ["keep".to_string()].into_iter().collect();
        let removed = store.prune_unknown_folders(&known).await;
        assert_eq!(removed, vec!["gone".to_string()]);
        assert_eq!(store.get_folder("keep").await.len(), 1);
        assert!(store.get_folder("gone").await.is_empty());
    }
}
