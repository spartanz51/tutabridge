use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crypto_primitives::aes::Iv;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use log::{debug, warn};
use rusqlite::{Connection, OptionalExtension};

/// Bumped when the on-disk schema changes. A mismatch drops the cached tables
/// (mails + sync_state + event_bus_state) and triggers a full re-sync;
/// encrypted .eml files are keyed by element id and survive the migration.
const SCHEMA_VERSION: &str = "4";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Crypto error: {0}")]
    Crypto(String),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub struct MailMetadata {
    pub list_id: String,
    pub element_id: String,
    /// Stable folder id (Tuta `MailSet` element id).
    pub folder_id: String,
    pub subject: String,
    pub sender_name: String,
    pub sender_address: String,
    pub received_date_ms: i64,
    pub unread: bool,
    pub has_details: bool,
    /// Stable IMAP UID within the folder (0 = not yet assigned).
    pub uid: i64,
    pub mail_json: String,
}

pub struct LocalStore {
    conn: Mutex<Connection>,
    storage_key: GenericAesKey,
    mails_dir: PathBuf,
}

impl LocalStore {
    pub fn open(
        db_path: &Path,
        mails_dir: &Path,
        storage_key: GenericAesKey,
    ) -> Result<Self, StoreError> {
        std::fs::create_dir_all(mails_dir)?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)?;

        let hex_key = hex::encode(storage_key.as_bytes());
        conn.pragma_update(None, "key", format!("x'{hex_key}'"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Migrate: if the stored schema version differs, drop the cache tables.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS store_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if version.as_deref() != Some(SCHEMA_VERSION) {
            warn!(
                "Local store schema {:?} != {SCHEMA_VERSION}, dropping cache tables",
                version
            );
            conn.execute_batch(
                "DROP TABLE IF EXISTS mails;
                 DROP TABLE IF EXISTS sync_state;
                 DROP TABLE IF EXISTS event_bus_state;",
            )?;
        }

        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS mails (
                element_id      TEXT PRIMARY KEY,
                list_id         TEXT NOT NULL,
                folder_id       TEXT NOT NULL,
                subject         TEXT NOT NULL,
                sender_name     TEXT NOT NULL DEFAULT '',
                sender_address  TEXT NOT NULL DEFAULT '',
                received_date_ms INTEGER NOT NULL,
                unread          INTEGER NOT NULL DEFAULT 1,
                has_details     INTEGER NOT NULL DEFAULT 0,
                uid             INTEGER NOT NULL DEFAULT 0,
                mail_json       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mails_folder
                ON mails(folder_id, received_date_ms DESC);
            CREATE TABLE IF NOT EXISTS sync_state (
                folder_id     TEXT PRIMARY KEY,
                last_sync_ms  INTEGER NOT NULL DEFAULT 0,
                next_uid      INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS event_bus_state (
                group_id       TEXT PRIMARY KEY,
                last_batch_id  TEXT NOT NULL,
                updated_at_ms  INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR REPLACE INTO store_meta(key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');"
        ))?;

        debug!("LocalStore opened at {}", db_path.display());

        Ok(Self {
            conn: Mutex::new(conn),
            storage_key,
            mails_dir: mails_dir.to_path_buf(),
        })
    }

    pub fn verify_key(&self) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM store_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .is_ok()
    }

    pub fn reset(&self) -> Result<(), StoreError> {
        warn!("Resetting local store — all cached data will be deleted");
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(&format!(
            "DELETE FROM mails;
             DELETE FROM sync_state;
             DELETE FROM store_meta;
             INSERT INTO store_meta(key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');"
        ))?;
        drop(conn);

        if self.mails_dir.exists() {
            for entry in std::fs::read_dir(&self.mails_dir)? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) == Some("enc") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    pub fn load_folder_metadata(&self, folder_id: &str) -> Result<Vec<MailMetadata>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT element_id, list_id, folder_id, subject, sender_name, sender_address,
                    received_date_ms, unread, has_details, uid, mail_json
             FROM mails WHERE folder_id = ?1
             ORDER BY received_date_ms DESC",
        )?;
        let rows = stmt.query_map([folder_id], |row| {
            Ok(MailMetadata {
                element_id: row.get(0)?,
                list_id: row.get(1)?,
                folder_id: row.get(2)?,
                subject: row.get(3)?,
                sender_name: row.get(4)?,
                sender_address: row.get(5)?,
                received_date_ms: row.get(6)?,
                unread: row.get::<_, i64>(7)? != 0,
                has_details: row.get::<_, i64>(8)? != 0,
                uid: row.get(9)?,
                mail_json: row.get(10)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn upsert_mail_metadata(&self, meta: &MailMetadata) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mails (element_id, list_id, folder_id, subject, sender_name,
                                sender_address, received_date_ms, unread, has_details, uid, mail_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(element_id) DO UPDATE SET
                folder_id = excluded.folder_id,
                subject = excluded.subject,
                sender_name = excluded.sender_name,
                sender_address = excluded.sender_address,
                received_date_ms = excluded.received_date_ms,
                unread = excluded.unread,
                has_details = excluded.has_details,
                mail_json = excluded.mail_json",
            rusqlite::params![
                meta.element_id,
                meta.list_id,
                meta.folder_id,
                meta.subject,
                meta.sender_name,
                meta.sender_address,
                meta.received_date_ms,
                meta.unread as i64,
                meta.has_details as i64,
                meta.uid,
                meta.mail_json,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_mail_metadata_batch(&self, metas: &[MailMetadata]) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO mails (element_id, list_id, folder_id, subject, sender_name,
                                    sender_address, received_date_ms, unread, has_details, uid, mail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(element_id) DO UPDATE SET
                    folder_id = excluded.folder_id,
                    subject = excluded.subject,
                    sender_name = excluded.sender_name,
                    sender_address = excluded.sender_address,
                    received_date_ms = excluded.received_date_ms,
                    unread = excluded.unread,
                    has_details = CASE WHEN excluded.has_details = 1 THEN 1 ELSE mails.has_details END,
                    mail_json = excluded.mail_json",
            )?;
            for meta in metas {
                stmt.execute(rusqlite::params![
                    meta.element_id,
                    meta.list_id,
                    meta.folder_id,
                    meta.subject,
                    meta.sender_name,
                    meta.sender_address,
                    meta.received_date_ms,
                    meta.unread as i64,
                    meta.has_details as i64,
                    meta.uid,
                    meta.mail_json,
                ])?;
            }
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Assign stable, monotonic UIDs to the given (new) element ids in a folder.
    /// UIDs are never reused — the per-folder counter only advances — so an IMAP
    /// client's `(UIDVALIDITY, UID)` cache stays valid across bridge restarts.
    /// Ids should be supplied oldest-first so newer mail gets higher UIDs.
    pub fn allocate_folder_uids(
        &self,
        folder_id: &str,
        new_element_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, u32>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut next: i64 = conn
            .query_row(
                "SELECT next_uid FROM sync_state WHERE folder_id = ?1",
                [folder_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(1);

        let mut map = std::collections::HashMap::with_capacity(new_element_ids.len());
        for eid in new_element_ids {
            map.insert((*eid).to_string(), next as u32);
            next += 1;
        }

        conn.execute(
            "INSERT INTO sync_state(folder_id, next_uid) VALUES (?1, ?2)
             ON CONFLICT(folder_id) DO UPDATE SET next_uid = excluded.next_uid",
            rusqlite::params![folder_id, next],
        )?;
        Ok(map)
    }

    pub fn delete_mails_not_in(
        &self,
        folder_id: &str,
        element_ids: &[&str],
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().unwrap();

        let mut deleted = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT element_id FROM mails WHERE folder_id = ?1")?;
            let existing: Vec<String> = stmt
                .query_map([folder_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            let keep: std::collections::HashSet<&str> = element_ids.iter().copied().collect();

            for eid in existing {
                if !keep.contains(eid.as_str()) {
                    deleted.push(eid);
                }
            }
        }

        if !deleted.is_empty() {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            {
                let mut stmt =
                    conn.prepare_cached("DELETE FROM mails WHERE element_id = ?1")?;
                for eid in &deleted {
                    stmt.execute([eid])?;
                }
            }
            conn.execute_batch("COMMIT")?;
        }

        Ok(deleted)
    }

    pub fn mark_has_details(&self, element_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE mails SET has_details = 1 WHERE element_id = ?1",
            [element_id],
        )?;
        Ok(())
    }

    pub fn write_eml(&self, element_id: &str, rfc2822: &str) -> Result<(), StoreError> {
        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        let iv = Iv::generate(&randomizer);
        let encrypted = self
            .storage_key
            .encrypt_data(rfc2822.as_bytes(), iv)
            .map_err(|e| StoreError::Crypto(format!("{e:?}")))?;

        let final_path = self.mails_dir.join(format!("{element_id}.eml.enc"));
        let tmp_path = self.mails_dir.join(format!("{element_id}.eml.enc.tmp"));
        std::fs::write(&tmp_path, &encrypted)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn read_eml(&self, element_id: &str) -> Result<Option<String>, StoreError> {
        let path = self.mails_dir.join(format!("{element_id}.eml.enc"));
        if !path.exists() {
            return Ok(None);
        }
        let encrypted = std::fs::read(&path)?;
        let decrypted = self
            .storage_key
            .decrypt_data(&encrypted)
            .map_err(|e| StoreError::Crypto(format!("{e:?}")))?;
        String::from_utf8(decrypted)
            .map(Some)
            .map_err(|e| StoreError::Crypto(format!("Invalid UTF-8: {e}")))
    }

    pub fn has_eml(&self, element_id: &str) -> bool {
        self.mails_dir
            .join(format!("{element_id}.eml.enc"))
            .exists()
    }

    pub fn delete_eml(&self, element_id: &str) -> Result<(), StoreError> {
        let path = self.mails_dir.join(format!("{element_id}.eml.enc"));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn mail_count(&self, folder_id: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mails WHERE folder_id = ?1",
            [folder_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn total_count(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM mails", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Load the last processed event-batch id for every known group.
    pub fn load_event_bus_state(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT group_id, last_batch_id FROM event_bus_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (g, b) = row?;
            out.insert(g, b);
        }
        Ok(out)
    }

    /// Oldest `updated_at_ms` across all event-bus rows, or `None` if empty.
    /// Used at startup to detect a cache older than the server's batch
    /// replay window (~44 days) and force a full re-sync.
    pub fn event_bus_state_min_updated_at_ms(&self) -> Result<Option<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn.query_row(
            "SELECT MIN(updated_at_ms) FROM event_bus_state",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(v)
    }

    /// Wipe the per-group catch-up cursors. The next reconnect will not pass
    /// `groupsToLastEventBatchIds`, and the syncer will see an empty state
    /// and run the one-shot bootstrap.
    pub fn clear_event_bus_state(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM event_bus_state", [])?;
        Ok(())
    }

    /// Persist the last processed batch id for a group (event-bus catch-up
    /// resumes from this point on the next reconnect).
    pub fn set_event_bus_batch_id(
        &self,
        group_id: &str,
        batch_id: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO event_bus_state(group_id, last_batch_id, updated_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(group_id) DO UPDATE SET
                last_batch_id = excluded.last_batch_id,
                updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![group_id, batch_id, now_ms],
        )?;
        Ok(())
    }

    /// Refresh the per-mail fields (read/unread, subject, sender, JSON blob)
    /// for every row of this `element_id` — typically a mail lives in one
    /// folder, but Tuta's model allows multi-folder placement, so we update
    /// all matching rows. Folder placement itself is governed by
    /// `MailSetEntry` events.
    pub fn refresh_mail_fields(
        &self,
        element_id: &str,
        subject: &str,
        sender_name: &str,
        sender_address: &str,
        unread: bool,
        mail_json: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE mails SET
                subject = ?1,
                sender_name = ?2,
                sender_address = ?3,
                unread = ?4,
                mail_json = ?5
             WHERE element_id = ?6",
            rusqlite::params![
                subject,
                sender_name,
                sender_address,
                unread as i64,
                mail_json,
                element_id,
            ],
        )?;
        Ok(())
    }

    /// Drop every cached mail row that belongs to `folder_id` and return
    /// their element ids — the caller then deletes the matching .eml files.
    /// Used by the event handler when a `MailSet` event tells us a folder
    /// no longer exists on the server.
    pub fn delete_folder_mails(&self, folder_id: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT element_id FROM mails WHERE folder_id = ?1")?;
        let ids: Vec<String> = stmt
            .query_map([folder_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        if !ids.is_empty() {
            conn.execute("DELETE FROM mails WHERE folder_id = ?1", [folder_id])?;
        }
        Ok(ids)
    }

    /// Drop a mail entirely (metadata + .eml). Used by the event handler on a
    /// `DELETE` of the underlying mail entity.
    pub fn delete_mail(&self, element_id: &str) -> Result<(), StoreError> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM mails WHERE element_id = ?1", [element_id])?;
        }
        self.delete_eml(element_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto_primitives::aes::Aes256Key;

    fn test_key() -> GenericAesKey {
        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        GenericAesKey::Aes256(Aes256Key::generate(&randomizer))
    }

    fn open_memory_store() -> LocalStore {
        let key = test_key();
        let tmp_dir = std::env::temp_dir().join(format!("tutabridge_test_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("test.db");
        let mails_dir = tmp_dir.join("mails");
        LocalStore::open(&db_path, &mails_dir, key).unwrap()
    }

    fn meta(element_id: &str, folder_id: &str, received: i64) -> MailMetadata {
        MailMetadata {
            element_id: element_id.into(),
            list_id: "list1".into(),
            folder_id: folder_id.into(),
            subject: format!("Subject {element_id}"),
            sender_name: "Alice".into(),
            sender_address: "alice@example.com".into(),
            received_date_ms: received,
            unread: true,
            has_details: false,
            uid: 0,
            mail_json: "{}".into(),
        }
    }

    #[test]
    fn allocate_uids_are_monotonic_and_per_folder() {
        let store = open_memory_store();
        let m1 = store.allocate_folder_uids("inbox", &["a", "b"]).unwrap();
        assert_eq!(m1["a"], 1);
        assert_eq!(m1["b"], 2);
        // Continues, never reuses, even if "a"/"b" were deleted.
        let m2 = store.allocate_folder_uids("inbox", &["c"]).unwrap();
        assert_eq!(m2["c"], 3);
        // Each folder has its own counter.
        let m3 = store.allocate_folder_uids("custom", &["x"]).unwrap();
        assert_eq!(m3["x"], 1);
    }

    #[test]
    fn test_open_and_verify() {
        let store = open_memory_store();
        assert!(store.verify_key());
    }

    #[test]
    fn test_upsert_and_load_metadata() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("abc123", "inbox", 1700000000000)).unwrap();

        let loaded = store.load_folder_metadata("inbox").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].element_id, "abc123");
        assert_eq!(loaded[0].folder_id, "inbox");
        assert!(loaded[0].unread);
        assert!(!loaded[0].has_details);
    }

    #[test]
    fn test_metadata_is_per_folder() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("a", "inbox", 1)).unwrap();
        store.upsert_mail_metadata(&meta("b", "custom1", 2)).unwrap();

        assert_eq!(store.load_folder_metadata("inbox").unwrap().len(), 1);
        assert_eq!(store.load_folder_metadata("custom1").unwrap().len(), 1);
        assert_eq!(store.load_folder_metadata("missing").unwrap().len(), 0);
    }

    #[test]
    fn test_batch_upsert() {
        let store = open_memory_store();
        let metas: Vec<MailMetadata> = (0..100)
            .map(|i| meta(&format!("mail_{i}"), "inbox", 1700000000000 + i))
            .collect();
        store.upsert_mail_metadata_batch(&metas).unwrap();

        assert_eq!(store.load_folder_metadata("inbox").unwrap().len(), 100);
        assert_eq!(store.mail_count("inbox").unwrap(), 100);
        assert_eq!(store.total_count().unwrap(), 100);
    }

    #[test]
    fn test_delete_mails_not_in() {
        let store = open_memory_store();
        let metas: Vec<MailMetadata> = (0..5)
            .map(|i| meta(&format!("mail_{i}"), "inbox", 1700000000000 + i))
            .collect();
        store.upsert_mail_metadata_batch(&metas).unwrap();

        let keep = vec!["mail_0", "mail_2", "mail_4"];
        let deleted = store.delete_mails_not_in("inbox", &keep).unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(deleted.contains(&"mail_1".to_string()));
        assert!(deleted.contains(&"mail_3".to_string()));
        assert_eq!(store.mail_count("inbox").unwrap(), 3);
    }

    #[test]
    fn test_eml_write_read_roundtrip() {
        let store = open_memory_store();
        let rfc2822 = "From: test@example.com\r\nSubject: Hello\r\n\r\nBody text here";
        store.write_eml("test_mail", rfc2822).unwrap();
        assert_eq!(store.read_eml("test_mail").unwrap(), Some(rfc2822.to_string()));
    }

    #[test]
    fn test_eml_read_nonexistent() {
        let store = open_memory_store();
        assert_eq!(store.read_eml("nonexistent").unwrap(), None);
    }

    #[test]
    fn test_eml_delete() {
        let store = open_memory_store();
        store.write_eml("to_delete", "content").unwrap();
        assert!(store.read_eml("to_delete").unwrap().is_some());
        store.delete_eml("to_delete").unwrap();
        assert!(store.read_eml("to_delete").unwrap().is_none());
    }

    #[test]
    fn test_reset() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("abc", "inbox", 0)).unwrap();
        store.write_eml("abc", "content").unwrap();

        store.reset().unwrap();
        assert_eq!(store.total_count().unwrap(), 0);
        assert!(store.read_eml("abc").unwrap().is_none());
        assert!(store.verify_key());
    }

    #[test]
    fn event_bus_state_min_updated_at_and_clear() {
        let store = open_memory_store();
        assert!(store.event_bus_state_min_updated_at_ms().unwrap().is_none());

        store.set_event_bus_batch_id("g1", "b1").unwrap();
        // First write: min == that row's timestamp; just assert presence.
        let min1 = store.event_bus_state_min_updated_at_ms().unwrap();
        assert!(min1.is_some());
        // Sleep a millisecond so the next write has a strictly larger ts.
        std::thread::sleep(std::time::Duration::from_millis(2));
        store.set_event_bus_batch_id("g2", "b2").unwrap();
        let min2 = store.event_bus_state_min_updated_at_ms().unwrap();
        // Still the original (oldest) row.
        assert_eq!(min1, min2);

        store.clear_event_bus_state().unwrap();
        assert!(store.load_event_bus_state().unwrap().is_empty());
        assert!(store.event_bus_state_min_updated_at_ms().unwrap().is_none());
    }

    #[test]
    fn event_bus_state_roundtrip() {
        let store = open_memory_store();
        assert!(store.load_event_bus_state().unwrap().is_empty());

        store.set_event_bus_batch_id("group1", "batchA").unwrap();
        store.set_event_bus_batch_id("group2", "batchX").unwrap();

        let s = store.load_event_bus_state().unwrap();
        assert_eq!(s.get("group1"), Some(&"batchA".to_string()));
        assert_eq!(s.get("group2"), Some(&"batchX".to_string()));

        // Upsert overwrites.
        store.set_event_bus_batch_id("group1", "batchB").unwrap();
        let s = store.load_event_bus_state().unwrap();
        assert_eq!(s.get("group1"), Some(&"batchB".to_string()));
    }

    #[test]
    fn delete_folder_mails_returns_ids_and_drops_rows() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("a", "doomed", 1)).unwrap();
        store.upsert_mail_metadata(&meta("b", "doomed", 2)).unwrap();
        store.upsert_mail_metadata(&meta("c", "kept", 3)).unwrap();
        let ids = store.delete_folder_mails("doomed").unwrap();
        let mut ids = ids;
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(store.mail_count("doomed").unwrap(), 0);
        assert_eq!(store.mail_count("kept").unwrap(), 1);
        // No rows for an unknown folder, returns empty.
        assert!(store.delete_folder_mails("missing").unwrap().is_empty());
    }

    #[test]
    fn delete_mail_removes_metadata_and_eml() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("del1", "inbox", 0)).unwrap();
        store.write_eml("del1", "content").unwrap();
        assert_eq!(store.mail_count("inbox").unwrap(), 1);
        assert!(store.has_eml("del1"));

        store.delete_mail("del1").unwrap();
        assert_eq!(store.mail_count("inbox").unwrap(), 0);
        assert!(!store.has_eml("del1"));
    }

    #[test]
    fn test_mark_has_details() {
        let store = open_memory_store();
        store.upsert_mail_metadata(&meta("det", "inbox", 0)).unwrap();
        assert!(!store.load_folder_metadata("inbox").unwrap()[0].has_details);

        store.mark_has_details("det").unwrap();
        assert!(store.load_folder_metadata("inbox").unwrap()[0].has_details);
    }
}
