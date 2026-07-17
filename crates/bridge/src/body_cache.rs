//! Bounded in-memory cache for rendered message bodies.
//!
//! The `MailStore` keeps **metadata only**; the heavy per-mail payload (the
//! rendered RFC 2822 string, attachments inlined as base64, plus the decrypted
//! `MailDetails`) lives here, capped by [`MAX_CACHE_BYTES`] with LRU eviction.
//! Eviction is safe because every body is also persisted encrypted on disk
//! (`LocalStore::write_eml`): a miss re-reads and re-decrypts the `.eml.enc`.
//!
//! Before this cache, every fetched body was retained in the store (and
//! re-cloned into each IMAP session's snapshot) forever: a client running a
//! full offline sync of a large mailbox drove the bridge to a multi-gigabyte
//! footprint. Now memory is bounded no matter how many bodies a client pulls.
//!
//! `MailDetails` is in-memory only (it is not persisted): after an eviction or
//! a restart a body re-read from disk has `details: None`. Consumers fall back
//! to metadata (e.g. ENVELOPE uses `firstRecipient`), which is the same
//! behavior mails outside the prefetch window always had.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use tutasdk::entities::generated::tutanota::MailDetails;

use crate::store::LocalStore;

/// Cap on the summed `rfc2822` bytes held in memory.
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// A cached body: the rendered message and, when it came fresh from the API,
/// its decrypted details (used for ENVELOPE recipient lists and attachment
/// retries).
pub struct BodyEntry {
    pub details: Option<MailDetails>,
    pub rfc2822: String,
}

struct CacheState {
    /// element id -> (entry, last-use tick)
    entries: HashMap<String, (Arc<BodyEntry>, u64)>,
    /// last-use tick -> element id (LRU order; lowest tick = coldest)
    order: BTreeMap<u64, String>,
    total_bytes: usize,
    tick: u64,
}

pub struct BodyCache {
    state: Mutex<CacheState>,
    max_bytes: usize,
    /// Set once at startup; `None` in unit tests (no disk fallback).
    local_store: RwLock<Option<Arc<LocalStore>>>,
}

impl BodyCache {
    pub fn new() -> Self {
        Self::with_capacity(MAX_CACHE_BYTES)
    }

    pub fn with_capacity(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                entries: HashMap::new(),
                order: BTreeMap::new(),
                total_bytes: 0,
                tick: 0,
            }),
            max_bytes,
            local_store: RwLock::new(None),
        }
    }

    pub fn set_local_store(&self, ls: Arc<LocalStore>) {
        *crate::util::rwlock_write_recover(&self.local_store) = Some(ls);
    }

    /// Insert (or replace) a body. Evicts cold entries until the cache fits.
    pub fn insert(&self, element_id: &str, entry: BodyEntry) {
        let size = entry.rfc2822.len();
        let mut st = crate::util::lock_recover(&self.state);
        st.tick += 1;
        let tick = st.tick;
        if let Some((old, old_tick)) = st
            .entries
            .insert(element_id.to_string(), (Arc::new(entry), tick))
        {
            st.order.remove(&old_tick);
            st.total_bytes -= old.rfc2822.len();
        }
        st.order.insert(tick, element_id.to_string());
        st.total_bytes += size;

        // Evict coldest-first until under budget. Never evict the entry we
        // just inserted, even if it alone exceeds the budget (a single body is
        // always allowed in memory so the FETCH that needs it can be served).
        while st.total_bytes > self.max_bytes && st.entries.len() > 1 {
            let Some((&cold_tick, _)) = st.order.iter().next() else {
                break;
            };
            if cold_tick == tick {
                break;
            }
            if let Some(eid) = st.order.remove(&cold_tick) {
                if let Some((old, _)) = st.entries.remove(&eid) {
                    st.total_bytes -= old.rfc2822.len();
                }
            }
        }
    }

    /// Update the rendered body of an existing entry in place (attachment
    /// retry rewrote the multipart). Details are preserved. No-op if the
    /// entry is not cached.
    pub fn update_rfc2822(&self, element_id: &str, rfc2822: String) {
        let st = crate::util::lock_recover(&self.state);
        if let Some((entry, _)) = st.entries.get(element_id) {
            let details = entry.details.clone();
            drop(st);
            self.insert(element_id, BodyEntry { details, rfc2822 });
        }
    }

    /// LRU-only lookup: refreshes recency, never touches the disk. Use for
    /// per-message decorations (sizes, envelopes, search views) where a miss
    /// must stay cheap.
    pub fn cached(&self, element_id: &str) -> Option<Arc<BodyEntry>> {
        let mut st = crate::util::lock_recover(&self.state);
        st.tick += 1;
        let tick = st.tick;
        let (entry, old_tick) = st.entries.get_mut(element_id)?;
        let entry = entry.clone();
        let prev = std::mem::replace(old_tick, tick);
        st.order.remove(&prev);
        st.order.insert(tick, element_id.to_string());
        Some(entry)
    }

    /// Lookup with disk fallback: on a miss, read and decrypt the persisted
    /// `.eml.enc` (on the blocking pool) and cache it. Use for real content
    /// requests (FETCH BODY[] / RFC822), which are naturally per-message.
    pub async fn get(&self, element_id: &str) -> Option<Arc<BodyEntry>> {
        if let Some(hit) = self.cached(element_id) {
            return Some(hit);
        }
        let ls = crate::util::rwlock_read_recover(&self.local_store)
            .as_ref()
            .cloned()?;
        let eid = element_id.to_string();
        let rfc = tokio::task::spawn_blocking(move || ls.read_eml(&eid))
            .await
            .ok()?
            .ok()
            .flatten()?;
        self.insert(
            element_id,
            BodyEntry {
                details: None,
                rfc2822: rfc,
            },
        );
        self.cached(element_id)
    }

    #[cfg(test)]
    fn total_bytes(&self) -> usize {
        crate::util::lock_recover(&self.state).total_bytes
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        crate::util::lock_recover(&self.state).entries.len()
    }
}

impl Default for BodyCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &str) -> BodyEntry {
        BodyEntry {
            details: None,
            rfc2822: body.to_string(),
        }
    }

    #[test]
    fn insert_and_cached_roundtrip() {
        let c = BodyCache::with_capacity(1024);
        assert!(c.cached("a").is_none());
        c.insert("a", entry("hello"));
        assert_eq!(c.cached("a").unwrap().rfc2822, "hello");
        assert_eq!(c.total_bytes(), 5);
    }

    #[test]
    fn replacing_an_entry_updates_accounting() {
        let c = BodyCache::with_capacity(1024);
        c.insert("a", entry("aaaa"));
        c.insert("a", entry("bb"));
        assert_eq!(c.total_bytes(), 2);
        assert_eq!(c.len(), 1);
        assert_eq!(c.cached("a").unwrap().rfc2822, "bb");
    }

    #[test]
    fn evicts_coldest_first_when_over_budget() {
        let c = BodyCache::with_capacity(10);
        c.insert("a", entry("aaaa")); // 4
        c.insert("b", entry("bbbb")); // 8
                                      // Touch `a` so `b` is the coldest.
        assert!(c.cached("a").is_some());
        c.insert("c", entry("cccc")); // 12 -> evict b
        assert!(c.cached("b").is_none(), "coldest entry must be evicted");
        assert!(c.cached("a").is_some());
        assert!(c.cached("c").is_some());
        assert!(c.total_bytes() <= 10);
    }

    #[test]
    fn single_oversized_entry_is_kept() {
        let c = BodyCache::with_capacity(4);
        c.insert("big", entry("0123456789"));
        assert!(
            c.cached("big").is_some(),
            "a single body larger than the budget must still be served"
        );
        // The next insert evicts it.
        c.insert("small", entry("xy"));
        assert!(c.cached("big").is_none());
        assert!(c.cached("small").is_some());
    }

    #[test]
    fn update_rfc2822_preserves_details_and_reaccounts() {
        let c = BodyCache::with_capacity(1024);
        c.insert("a", entry("original body"));
        c.update_rfc2822("a", "new".to_string());
        assert_eq!(c.cached("a").unwrap().rfc2822, "new");
        assert_eq!(c.total_bytes(), 3);
        // Unknown id is a no-op.
        c.update_rfc2822("ghost", "x".to_string());
        assert!(c.cached("ghost").is_none());
    }

    #[tokio::test]
    async fn get_without_local_store_is_none_on_miss() {
        let c = BodyCache::with_capacity(1024);
        assert!(c.get("nope").await.is_none());
        c.insert("a", entry("body"));
        assert_eq!(c.get("a").await.unwrap().rfc2822, "body");
    }
}
