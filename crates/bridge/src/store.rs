use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crypto_primitives::aes::InitializationVector;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use log::{debug, warn};
use rusqlite::{Connection, OptionalExtension};

/// Bumped when the on-disk schema changes, or when what an existing cache
/// holds can no longer be trusted. A mismatch drops the cached tables
/// (mails + sync_state + event_bus_state + mail_fts) and the markers that
/// said they were complete, and triggers a full re-sync; encrypted .eml files
/// are keyed by element id and survive the migration.
///
/// 5: up to 4, a mail moved to another folder kept the UID of the folder it
/// left, so a folder could hold two mails on one UID.
const SCHEMA_VERSION: &str = "5";

/// Set once every folder's mail list has been synced in full.
pub(crate) const FULL_METADATA_MARKER: &str = "full_metadata_synced_v1";
/// Set once the cached bodies have been indexed for full-text search.
pub(crate) const FTS_BACKFILL_MARKER: &str = "body_fts_indexed_v1";

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

/// Turn a free-text search term into a safe FTS5 MATCH expression. Each run of
/// alphanumeric characters becomes a prefix token (`word*`), and tokens are
/// ANDed together (FTS5's default). Because only alphanumerics survive, the
/// result can never contain FTS5 syntax, so it is injection-safe to interpolate
/// as a bound parameter. Returns an empty string when nothing is searchable.
fn fts_match_expr(term: &str) -> String {
    term.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("{w}*"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl LocalStore {
    /// Open the application's cache, protecting its dedicated data directory
    /// before SQLite can open the database or its WAL/shared-memory files.
    pub fn open_default(storage_key: GenericAesKey) -> Result<Self, StoreError> {
        Self::open_in_data_dir(&crate::config::data_dir(), storage_key)
    }

    // Only pass the application's dedicated directory here, never a shared
    // parent supplied to the general-purpose `open` API.
    fn open_in_data_dir(data_dir: &Path, storage_key: GenericAesKey) -> Result<Self, StoreError> {
        crate::fs_private::create_dir_all_private(data_dir)?;
        crate::fs_private::tighten_dir(data_dir)?;
        Self::open(
            &data_dir.join("store.db"),
            &data_dir.join("mails"),
            storage_key,
        )
    }

    pub fn open(
        db_path: &Path,
        mails_dir: &Path,
        storage_key: GenericAesKey,
    ) -> Result<Self, StoreError> {
        // Bodies and metadata are encrypted at rest, but file names, sizes
        // and mtimes still say how much mail there is and when it arrived,
        // so keep the cache private to the owner. The directory is tightened
        // on every open to heal installs created before this hardening.
        crate::fs_private::create_dir_all_private(mails_dir)?;
        crate::fs_private::tighten_dir(mails_dir)?;
        if let Some(parent) = db_path.parent() {
            crate::fs_private::create_dir_all_private(parent)?;
        }

        let conn = Connection::open(db_path)?;
        crate::fs_private::tighten(db_path);

        // Our copies of the key are wiped once SQLCipher has it.
        let hex_key = zeroize::Zeroizing::new(hex::encode(storage_key.as_bytes()));
        let key_pragma = zeroize::Zeroizing::new(format!("x'{}'", hex_key.as_str()));
        conn.pragma_update(None, "key", key_pragma.as_str())?;
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
                 DROP TABLE IF EXISTS event_bus_state;
                 DROP TABLE IF EXISTS mail_fts;",
            )?;
            // These two say the tables just dropped are complete. Left set,
            // a folder whose re-sync fails is never retried, and the bodies
            // kept on disk are never indexed again.
            conn.execute(
                "DELETE FROM store_meta WHERE key IN (?1, ?2)",
                [FULL_METADATA_MARKER, FTS_BACKFILL_MARKER],
            )?;
            // The UID counters went with `sync_state`, so every UID is about
            // to be handed out again. All of this runs before the schema
            // version is stamped below: if it fails, the next open comes back
            // here.
            renew_uid_validity(&conn, stored_uid_validity(&conn)?)?;
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
            CREATE TABLE IF NOT EXISTS message_ids (
                message_id  TEXT PRIMARY KEY,
                element_id  TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
                element_id UNINDEXED,
                body,
                tokenize = 'unicode61 remove_diacritics 2'
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

    /// The IMAP `UIDVALIDITY` of every mailbox. It changes whenever the UID
    /// counters start over (cache reset, schema change, new cache), which is
    /// how a client learns that the UIDs it remembers no longer hold. A cache
    /// that predates this value keeps 1, the value it was always served with.
    pub fn uid_validity(&self) -> Result<u32, StoreError> {
        stored_uid_validity(&self.conn.lock().unwrap())
    }

    /// Read a value from the generic `store_meta` key-value table.
    pub fn get_meta(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    /// Remember that a mail we hold carries this RFC `Message-ID`.
    ///
    /// Tuta keeps the id in `ConversationEntry.messageId`, bare, and that is
    /// what a reply has to pass as `previousMessageId`. The same string is in
    /// the mail's stored header block, so indexing it as mail arrives lets a
    /// later reply be threaded without asking the server anything.
    ///
    /// `message_id` is stored bare: callers strip the angle brackets, because
    /// a submission's `In-Reply-To` is parsed bare too.
    pub fn remember_message_id(
        &self,
        message_id: &str,
        element_id: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO message_ids(message_id, element_id) VALUES (?1, ?2)",
            [message_id, element_id],
        )?;
        Ok(())
    }

    /// Is this `Message-ID` one of a mail we hold? See `remember_message_id`.
    pub fn knows_message_id(&self, message_id: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM message_ids WHERE message_id = ?1",
            [message_id],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
    }

    /// Element ids that already have a `Message-ID` on record. The backfill
    /// uses this to skip mails it has already resolved, so an interrupted
    /// pass costs only the mails it had not reached.
    pub fn indexed_element_ids(&self) -> std::collections::HashSet<String> {
        let conn = self.conn.lock().unwrap();
        let mut out = std::collections::HashSet::new();
        if let Ok(mut stmt) = conn.prepare("SELECT element_id FROM message_ids") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// How many Message-IDs are indexed. Diagnostics only.
    pub fn message_id_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM message_ids", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// Write a value into the generic `store_meta` key-value table.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO store_meta(key, value) VALUES (?1, ?2)",
            [key, value],
        )?;
        Ok(())
    }

    pub fn reset(&self) -> Result<(), StoreError> {
        warn!("Resetting local store — all cached data will be deleted");
        let conn = self.conn.lock().unwrap();
        // A store being reset may not be readable; 1 is what it then served.
        let previous = stored_uid_validity(&conn).unwrap_or(1);
        // One transaction: the counters never start over under the old
        // UIDVALIDITY.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(&format!(
            "DELETE FROM mails;
             DELETE FROM sync_state;
             DELETE FROM store_meta;
             DELETE FROM mail_fts;
             DELETE FROM message_ids;
             INSERT INTO store_meta(key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');"
        ))?;
        renew_uid_validity(&tx, previous)?;
        tx.commit()?;
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
                uid = CASE WHEN excluded.uid > 0 THEN excluded.uid ELSE mails.uid END,
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
                    uid = CASE WHEN excluded.uid > 0 THEN excluded.uid ELSE mails.uid END,
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

    /// Return the next IMAP UID that would be assigned in `folder_id`.
    /// Defaults to `1` for folders that have never received a message.
    /// The counter only advances, so deleting or expunging the highest-UID
    /// message never makes `UIDNEXT` decrease (RFC 3501 §2.3.1.1).
    pub fn folder_next_uid(&self, folder_id: &str) -> Result<u32, StoreError> {
        let conn = self.conn.lock().unwrap();
        let next: i64 = conn
            .query_row(
                "SELECT next_uid FROM sync_state WHERE folder_id = ?1",
                [folder_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(1);
        Ok(clamp_uid_next(next))
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
                let mut stmt = conn.prepare_cached("DELETE FROM mails WHERE element_id = ?1")?;
                let mut ids =
                    conn.prepare_cached("DELETE FROM message_ids WHERE element_id = ?1")?;
                for eid in &deleted {
                    stmt.execute([eid])?;
                    ids.execute([eid])?;
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

    /// [`Self::mark_has_details`] for many mails, in one transaction.
    pub fn mark_has_details_batch(&self, element_ids: &[&str]) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE mails SET has_details = 1 WHERE element_id = ?1")?;
            for element_id in element_ids {
                stmt.execute([element_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn write_eml(&self, element_id: &str, rfc2822: &str) -> Result<(), StoreError> {
        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        let iv = InitializationVector::generate(&randomizer);
        let encrypted = self
            .storage_key
            .encrypt_data(rfc2822.as_bytes(), iv)
            .map_err(|e| StoreError::Crypto(format!("{e:?}")))?;

        let final_path = self.mails_dir.join(format!("{element_id}.eml.enc"));
        let tmp_path = self.mails_dir.join(format!("{element_id}.eml.enc.tmp"));
        crate::fs_private::write_private(&tmp_path, &encrypted)?;
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

    /// Index (or re-index) a message's plain-text body for full-text search.
    /// `body_text` should already be stripped of HTML markup. Stored inside the
    /// SQLCipher database, so it is encrypted at rest like everything else.
    pub fn index_body(&self, element_id: &str, body_text: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        // FTS5 has no UPSERT; clear any prior row for this id first.
        conn.execute("DELETE FROM mail_fts WHERE element_id = ?1", [element_id])?;
        conn.execute(
            "INSERT INTO mail_fts(element_id, body) VALUES (?1, ?2)",
            [element_id, body_text],
        )?;
        Ok(())
    }

    /// Drop a message from the full-text index (called when a mail is removed).
    pub fn unindex_body(&self, element_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mail_fts WHERE element_id = ?1", [element_id])?;
        Ok(())
    }

    /// Element ids whose indexed body matches a user search term. The term is a
    /// free-text query (one IMAP `BODY`/`TEXT` argument); it is turned into a
    /// safe FTS5 MATCH expression: each word becomes a prefix token so
    /// `factur` finds `facture`/`factures`, and multi-word terms must all be
    /// present. Returns an empty set when the term has no searchable tokens.
    pub fn search_body(&self, term: &str) -> Result<Vec<String>, StoreError> {
        let match_expr = fts_match_expr(term);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT element_id FROM mail_fts WHERE body MATCH ?1")?;
        let rows = stmt.query_map([&match_expr], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    /// Number of bodies currently in the full-text index (diagnostics / tests).
    pub fn fts_count(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail_fts", [], |row| row.get(0))?;
        Ok(count as usize)
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
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM mails", [], |row| row.get(0))?;
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
    pub fn set_event_bus_batch_id(&self, group_id: &str, batch_id: &str) -> Result<(), StoreError> {
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
            conn.execute(
                "DELETE FROM message_ids WHERE element_id IN \
                 (SELECT element_id FROM mails WHERE folder_id = ?1)",
                [folder_id],
            )?;
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
            // A reply to a deleted mail must not thread to an id the server
            // no longer holds.
            conn.execute(
                "DELETE FROM message_ids WHERE element_id = ?1",
                [element_id],
            )?;
        }
        self.delete_eml(element_id)?;
        self.unindex_body(element_id)?;
        Ok(())
    }
}

const UID_VALIDITY_KEY: &str = "uid_validity";

/// The stored `UIDVALIDITY`, or 1 when there is none. A failed read is an
/// error, not 1: it must not pass for a cache that predates the value.
fn stored_uid_validity(conn: &Connection) -> Result<u32, StoreError> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [UID_VALIDITY_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(1))
}

/// Store a `UIDVALIDITY` above `previous` (RFC 3501 §2.3.1.1): the current
/// time in seconds, the usual choice, or `previous + 1` if the clock is
/// behind it.
fn renew_uid_validity(conn: &Connection, previous: u32) -> Result<(), StoreError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    conn.execute(
        "INSERT OR REPLACE INTO store_meta(key, value) VALUES (?1, ?2)",
        rusqlite::params![
            UID_VALIDITY_KEY,
            next_uid_validity(previous, now).to_string()
        ],
    )?;
    Ok(())
}

fn next_uid_validity(previous: u32, now_secs: u64) -> u32 {
    let from_clock = u32::try_from(now_secs).unwrap_or(u32::MAX);
    from_clock.max(previous.saturating_add(1))
}

/// Clamp a raw SQLite `sync_state.next_uid` value into the valid RFC 3501
/// `nz-number` range (`1..=u32::MAX`).
fn clamp_uid_next(raw: i64) -> u32 {
    if raw < 1 {
        1
    } else if raw > i64::from(u32::MAX) {
        u32::MAX
    } else {
        raw as u32
    }
}

impl crate::tuta::MessageIdIndex for LocalStore {
    fn knows_message_id(&self, message_id: &str) -> bool {
        LocalStore::knows_message_id(self, message_id)
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
        let tmp_dir =
            std::env::temp_dir().join(format!("tutabridge_test_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("test.db");
        let mails_dir = tmp_dir.join("mails");
        LocalStore::open(&db_path, &mails_dir, key).unwrap()
    }

    #[test]
    fn message_ids_are_remembered_and_found() {
        let store = open_memory_store();
        assert_eq!(store.message_id_count(), 0);
        assert!(!store.knows_message_id("a1b2@mail.example.org"));

        store
            .remember_message_id("a1b2@mail.example.org", "elem1")
            .unwrap();
        assert!(store.knows_message_id("a1b2@mail.example.org"));
        assert_eq!(store.message_id_count(), 1);

        // Ids are matched exactly: a reply's In-Reply-To is parsed bare, so a
        // bracketed form is a different string and must not match.
        assert!(!store.knows_message_id("<a1b2@mail.example.org>"));
        assert!(!store.knows_message_id("other@example.org"));

        // Re-indexing the same mail is idempotent, not a duplicate row.
        store
            .remember_message_id("a1b2@mail.example.org", "elem1")
            .unwrap();
        assert_eq!(store.message_id_count(), 1);
    }

    #[test]
    fn removing_a_mail_forgets_its_message_id() {
        let store = open_memory_store();
        for (eid, folder) in [
            ("a", "inbox"),
            ("b", "inbox"),
            ("c", "doomed"),
            ("d", "kept"),
        ] {
            store.upsert_mail_metadata(&meta(eid, folder, 0)).unwrap();
            store
                .remember_message_id(&format!("{eid}@example.org"), eid)
                .unwrap();
        }
        store.delete_mail("a").unwrap();
        assert!(!store.knows_message_id("a@example.org"));
        store.delete_mails_not_in("inbox", &[]).unwrap();
        assert!(!store.knows_message_id("b@example.org"));
        store.delete_folder_mails("doomed").unwrap();
        assert!(!store.knows_message_id("c@example.org"));
        // Mail still held keeps its id until a reset drops everything.
        assert!(store.knows_message_id("d@example.org"));
        store.reset().unwrap();
        assert_eq!(store.message_id_count(), 0);
    }

    #[test]
    fn the_index_survives_reopening_the_store() {
        let key = test_key();
        let tmp_dir =
            std::env::temp_dir().join(format!("tutabridge_test_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("test.db");
        let mails_dir = tmp_dir.join("mails");

        {
            let store = LocalStore::open(&db_path, &mails_dir, key.clone()).unwrap();
            store
                .remember_message_id("kept@example.org", "elem1")
                .unwrap();
        }
        // The point of putting this on disk rather than in a cache: a reply
        // sent days after the mail arrived still threads.
        let store = LocalStore::open(&db_path, &mails_dir, key).unwrap();
        assert!(store.knows_message_id("kept@example.org"));
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

    #[cfg(unix)]
    mod cache_permissions {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct Fixture(PathBuf);

        impl Fixture {
            fn new() -> Self {
                let root = std::env::temp_dir()
                    .join(format!("tutabridge_permissions_{}", rand::random::<u64>()));
                std::fs::create_dir_all(&root).unwrap();
                chmod(&root, 0o755);
                Self(root)
            }

            fn data_dir(&self) -> PathBuf {
                self.0.join("tutabridge")
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        fn mode(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn opening_app_cache_heals_legacy_directory_without_losing_data() {
            let fixture = Fixture::new();
            let data_dir = fixture.data_dir();
            let db_path = data_dir.join("store.db");
            let mails_dir = data_dir.join("mails");
            std::fs::create_dir_all(&data_dir).unwrap();
            chmod(&data_dir, 0o755);
            let key = test_key();
            let store = LocalStore::open(&db_path, &mails_dir, key.clone()).unwrap();
            // The general-purpose API must not chmod an arbitrary DB parent.
            assert_eq!(mode(&data_dir), 0o755);
            store
                .upsert_mail_metadata(&meta("legacy", "inbox", 1234))
                .unwrap();
            store.set_meta("last_sync", "keep-me").unwrap();
            store
                .write_eml("legacy", "Subject: preserved\r\n\r\nbody")
                .unwrap();
            drop(store);

            let body_path = mails_dir.join("legacy.eml.enc");
            let encrypted_body = std::fs::read(&body_path).unwrap();
            chmod(&db_path, 0o644);
            chmod(&mails_dir, 0o755);
            chmod(&body_path, 0o644);

            let store = LocalStore::open_in_data_dir(&data_dir, key).unwrap();
            assert_eq!(mode(&data_dir), 0o700);
            assert_eq!(mode(&mails_dir), 0o700);
            assert_eq!(mode(&db_path), 0o600);
            assert_eq!(mode(&fixture.0), 0o755);
            assert!(store.verify_key());
            let loaded = store.load_folder_metadata("inbox").unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].element_id, "legacy");
            assert_eq!(store.get_meta("last_sync").as_deref(), Some("keep-me"));
            assert_eq!(
                store.read_eml("legacy").unwrap().as_deref(),
                Some("Subject: preserved\r\n\r\nbody")
            );
            assert_eq!(std::fs::read(body_path).unwrap(), encrypted_body);
        }

        #[test]
        fn new_app_cache_is_private_including_sqlite_sidecars() {
            let fixture = Fixture::new();
            let data_dir = fixture.data_dir();
            let store = LocalStore::open_in_data_dir(&data_dir, test_key()).unwrap();
            store.write_eml("new", "new mail").unwrap();
            assert_eq!(mode(&data_dir), 0o700);
            assert_eq!(mode(&data_dir.join("mails")), 0o700);
            for file in [
                "store.db",
                "store.db-wal",
                "store.db-shm",
                "mails/new.eml.enc",
            ] {
                assert_eq!(mode(&data_dir.join(file)), 0o600, "{file}");
            }
            assert_eq!(mode(&fixture.0), 0o755);
        }

        #[test]
        fn app_cache_reports_directory_preparation_failure() {
            let fixture = Fixture::new();
            let data_dir = fixture.data_dir();
            std::fs::write(&data_dir, "existing file").unwrap();
            assert!(matches!(
                LocalStore::open_in_data_dir(&data_dir, test_key()),
                Err(StoreError::Io(_))
            ));
            assert_eq!(std::fs::read_to_string(&data_dir).unwrap(), "existing file");
            assert_eq!(mode(&fixture.0), 0o755);
        }
    }

    #[test]
    fn fts_index_and_search() {
        let store = open_memory_store();
        store
            .index_body("a", "the quarterly invoice is attached")
            .unwrap();
        store.index_body("b", "lunch plans for friday").unwrap();
        assert_eq!(store.fts_count().unwrap(), 2);

        // Prefix match: "invoic" finds "invoice".
        assert_eq!(store.search_body("invoic").unwrap(), vec!["a".to_string()]);
        // Multi-word ANDs the tokens.
        assert_eq!(
            store.search_body("lunch friday").unwrap(),
            vec!["b".to_string()]
        );
        // No hit.
        assert!(store.search_body("zebra").unwrap().is_empty());
        // Empty / punctuation-only term yields no FTS query.
        assert!(store.search_body("   ").unwrap().is_empty());
    }

    #[test]
    fn fts_reindex_replaces_prior_body() {
        let store = open_memory_store();
        store.index_body("a", "first version about cats").unwrap();
        store.index_body("a", "second version about dogs").unwrap();
        assert_eq!(store.fts_count().unwrap(), 1);
        assert!(store.search_body("cats").unwrap().is_empty());
        assert_eq!(store.search_body("dogs").unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn fts_unindex_removes_body() {
        let store = open_memory_store();
        store.index_body("a", "removable content").unwrap();
        store.unindex_body("a").unwrap();
        assert!(store.search_body("removable").unwrap().is_empty());
        assert_eq!(store.fts_count().unwrap(), 0);
    }

    #[test]
    fn fts_match_expr_is_injection_safe() {
        // FTS syntax characters never survive into the expression.
        assert_eq!(fts_match_expr(r#"foo" OR bar"#), "foo* OR* bar*");
        assert_eq!(fts_match_expr("a-b.c"), "a* b* c*");
        assert_eq!(fts_match_expr("   "), "");
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
    fn folder_next_uid_starts_at_one_advances_and_survives_deletes() {
        let store = open_memory_store();
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 1);
        assert_eq!(store.folder_next_uid("sent").unwrap(), 1);

        let allocated = store.allocate_folder_uids("inbox", &["a", "b"]).unwrap();
        assert_eq!(allocated["b"], 2);
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 3);
        assert_eq!(store.folder_next_uid("sent").unwrap(), 1);

        // Removing every mail from the folder must not lower its next_uid.
        let mut m1 = meta("a", "inbox", 1);
        m1.uid = 1;
        let mut m2 = meta("b", "inbox", 2);
        m2.uid = 2;
        store.upsert_mail_metadata(&m1).unwrap();
        store.upsert_mail_metadata(&m2).unwrap();
        let deleted = store.delete_mails_not_in("inbox", &[]).unwrap();
        assert_eq!(deleted.len(), 2);
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 3);

        // A full cache reset clears sync_state back to 1.
        store.reset().unwrap();
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 1);
    }

    #[test]
    fn test_open_and_verify() {
        let store = open_memory_store();
        assert!(store.verify_key());
    }

    #[test]
    fn test_upsert_and_load_metadata() {
        let store = open_memory_store();
        store
            .upsert_mail_metadata(&meta("abc123", "inbox", 1700000000000))
            .unwrap();

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
        store
            .upsert_mail_metadata(&meta("b", "custom1", 2))
            .unwrap();

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
        assert_eq!(
            store.read_eml("test_mail").unwrap(),
            Some(rfc2822.to_string())
        );
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
        store
            .upsert_mail_metadata(&meta("abc", "inbox", 0))
            .unwrap();
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
        store
            .upsert_mail_metadata(&meta("del1", "inbox", 0))
            .unwrap();
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
        store
            .upsert_mail_metadata(&meta("det", "inbox", 0))
            .unwrap();
        assert!(!store.load_folder_metadata("inbox").unwrap()[0].has_details);

        store.mark_has_details("det").unwrap();
        assert!(store.load_folder_metadata("inbox").unwrap()[0].has_details);
    }

    #[test]
    fn a_moved_mail_takes_the_uid_of_its_new_folder() {
        let store = open_memory_store();
        let uid_of = |folder: &str, eid: &str| {
            store
                .load_folder_metadata(folder)
                .unwrap()
                .into_iter()
                .find(|m| m.element_id == eid)
                .map(|m| m.uid)
        };
        let mut in_a = meta("moved", "A", 1);
        in_a.uid = 50;
        store.upsert_mail_metadata(&in_a).unwrap();

        // Moved to B, where it is given UID 4. The row is keyed by element id,
        // so this is an update: it must not keep the 50 it had in A.
        let mut in_b = meta("moved", "B", 1);
        in_b.uid = 4;
        store.upsert_mail_metadata(&in_b).unwrap();
        assert_eq!(uid_of("B", "moved"), Some(4));
        assert_eq!(uid_of("A", "moved"), None);

        // Same through the batch a folder sync writes.
        let mut in_c = meta("moved", "C", 1);
        in_c.uid = 9;
        store.upsert_mail_metadata_batch(&[in_c]).unwrap();
        assert_eq!(uid_of("C", "moved"), Some(9));

        // 0 means the caller could not allocate a UID: the one on record stays.
        store.upsert_mail_metadata(&meta("moved", "C", 1)).unwrap();
        assert_eq!(uid_of("C", "moved"), Some(9));
        store
            .upsert_mail_metadata_batch(&[meta("moved", "C", 1)])
            .unwrap();
        assert_eq!(uid_of("C", "moved"), Some(9));
    }

    #[test]
    fn clamp_uid_next_keeps_the_counter_a_valid_uid() {
        assert_eq!(clamp_uid_next(i64::MIN), 1);
        assert_eq!(clamp_uid_next(0), 1);
        assert_eq!(clamp_uid_next(1), 1);
        assert_eq!(clamp_uid_next(112), 112);
        assert_eq!(clamp_uid_next(i64::from(u32::MAX)), u32::MAX);
        assert_eq!(clamp_uid_next(i64::from(u32::MAX) + 1), u32::MAX);
        assert_eq!(clamp_uid_next(i64::MAX), u32::MAX);

        // And it is applied to what the counter row holds.
        let store = open_memory_store();
        let set_counter = |raw: i64| {
            store
                .conn
                .lock()
                .unwrap()
                .execute(
                    "INSERT OR REPLACE INTO sync_state(folder_id, next_uid) VALUES ('f', ?1)",
                    [raw],
                )
                .unwrap();
        };
        set_counter(0);
        assert_eq!(store.folder_next_uid("f").unwrap(), 1);
        set_counter(i64::from(u32::MAX) + 7);
        assert_eq!(store.folder_next_uid("f").unwrap(), u32::MAX);
    }

    fn reopen(dir: &Path, key_byte: u8) -> LocalStore {
        let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[key_byte; 32]).unwrap());
        LocalStore::open(&dir.join("test.db"), &dir.join("mails"), key).unwrap()
    }

    fn fresh_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tutabridge_test_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn uid_validity_is_new_for_a_new_cache_and_kept_across_reopens() {
        let dir = fresh_dir();
        let first = reopen(&dir, 3).uid_validity().unwrap();
        // The current time: far above the 1 every older cache was served with.
        assert!(first > 1_700_000_000, "got {first}");
        assert_eq!(reopen(&dir, 3).uid_validity().unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uid_validity_stays_1_for_a_cache_that_predates_it() {
        let dir = fresh_dir();
        let store = reopen(&dir, 3);
        store
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM store_meta WHERE key = 'uid_validity'", [])
            .unwrap();
        drop(store);
        // Same schema, no stored value: nothing was renumbered, and clients
        // know this cache under 1.
        assert_eq!(reopen(&dir, 3).uid_validity().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uid_validity_grows_when_the_uid_counters_start_over() {
        let dir = fresh_dir();
        let store = reopen(&dir, 3);
        let first = store.uid_validity().unwrap();
        store.allocate_folder_uids("inbox", &["a", "b"]).unwrap();

        store.reset().unwrap();
        let after_reset = store.uid_validity().unwrap();
        assert!(after_reset > first, "{after_reset} <= {first}");
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 1);

        // A schema change drops the counters too.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE store_meta SET value = '0' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        drop(store);
        let after_migration = reopen(&dir, 3).uid_validity().unwrap();
        assert!(
            after_migration > after_reset,
            "{after_migration} <= {after_reset}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_uid_validity_is_the_clock_unless_it_would_not_grow() {
        assert_eq!(next_uid_validity(1, 1_800_000_000), 1_800_000_000);
        // Two renewals in the same second, or a clock set in the past.
        assert_eq!(
            next_uid_validity(1_800_000_000, 1_800_000_000),
            1_800_000_001
        );
        assert_eq!(next_uid_validity(1_800_000_000, 12), 1_800_000_001);
        assert_eq!(next_uid_validity(1, 0), 2);
        assert_eq!(next_uid_validity(1, u64::MAX), u32::MAX);
        assert_eq!(next_uid_validity(u32::MAX, 5), u32::MAX);
    }

    #[test]
    fn a_schema_change_drops_the_cache_and_the_markers_that_vouched_for_it() {
        let dir = fresh_dir();
        let store = reopen(&dir, 3);
        store.upsert_mail_metadata(&meta("m1", "inbox", 1)).unwrap();
        store.allocate_folder_uids("inbox", &["m1"]).unwrap();
        store.write_eml("m1", "Subject: s\r\n\r\nzanzibar").unwrap();
        store.index_body("m1", "zanzibar").unwrap();
        store.set_event_bus_batch_id("group", "batch0").unwrap();
        store.set_meta(FULL_METADATA_MARKER, "1").unwrap();
        store.set_meta(FTS_BACKFILL_MARKER, "1").unwrap();
        store.set_meta("schema_version", "4").unwrap();
        drop(store);

        let store = reopen(&dir, 3);
        assert!(store.load_folder_metadata("inbox").unwrap().is_empty());
        assert_eq!(store.folder_next_uid("inbox").unwrap(), 1);
        assert!(store.load_event_bus_state().unwrap().is_empty());
        assert!(store.search_body("zanzibar").unwrap().is_empty());
        // Left set, a start that follows a failed re-sync would skip the full
        // sync, and the bodies still on disk would never be indexed again.
        assert_eq!(store.get_meta(FULL_METADATA_MARKER), None);
        assert_eq!(store.get_meta(FTS_BACKFILL_MARKER), None);
        assert!(store.has_eml("m1"));

        // Once on the current schema, opening drops nothing.
        store.upsert_mail_metadata(&meta("m2", "inbox", 2)).unwrap();
        store.set_meta(FULL_METADATA_MARKER, "1").unwrap();
        drop(store);
        let store = reopen(&dir, 3);
        assert_eq!(store.load_folder_metadata("inbox").unwrap().len(), 1);
        assert_eq!(store.get_meta(FULL_METADATA_MARKER).as_deref(), Some("1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
