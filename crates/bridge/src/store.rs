use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crypto_primitives::aes::Iv;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use log::{debug, warn};
use rusqlite::Connection;
use tutasdk::folder_system::MailSetKind;

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
    pub folder_kind: i64,
    pub subject: String,
    pub sender_name: String,
    pub sender_address: String,
    pub received_date_ms: i64,
    pub unread: bool,
    pub has_details: bool,
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

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mails (
                element_id      TEXT PRIMARY KEY,
                list_id         TEXT NOT NULL,
                folder_kind     INTEGER NOT NULL,
                subject         TEXT NOT NULL,
                sender_name     TEXT NOT NULL DEFAULT '',
                sender_address  TEXT NOT NULL DEFAULT '',
                received_date_ms INTEGER NOT NULL,
                unread          INTEGER NOT NULL DEFAULT 1,
                has_details     INTEGER NOT NULL DEFAULT 0,
                mail_json       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mails_folder
                ON mails(folder_kind, received_date_ms DESC);
            CREATE TABLE IF NOT EXISTS sync_state (
                folder_kind   INTEGER PRIMARY KEY,
                last_sync_ms  INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS store_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT OR IGNORE INTO store_meta(key, value) VALUES ('schema_version', '1');",
        )?;

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
        conn.execute_batch(
            "DELETE FROM mails;
             DELETE FROM sync_state;
             DELETE FROM store_meta;
             INSERT INTO store_meta(key, value) VALUES ('schema_version', '1');",
        )?;
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

    pub fn load_folder_metadata(&self, kind: MailSetKind) -> Result<Vec<MailMetadata>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT element_id, list_id, folder_kind, subject, sender_name, sender_address,
                    received_date_ms, unread, has_details, mail_json
             FROM mails WHERE folder_kind = ?1
             ORDER BY received_date_ms DESC",
        )?;
        let rows = stmt.query_map([kind_to_i64(kind)], |row| {
            Ok(MailMetadata {
                element_id: row.get(0)?,
                list_id: row.get(1)?,
                folder_kind: row.get(2)?,
                subject: row.get(3)?,
                sender_name: row.get(4)?,
                sender_address: row.get(5)?,
                received_date_ms: row.get(6)?,
                unread: row.get::<_, i64>(7)? != 0,
                has_details: row.get::<_, i64>(8)? != 0,
                mail_json: row.get(9)?,
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
            "INSERT INTO mails (element_id, list_id, folder_kind, subject, sender_name,
                                sender_address, received_date_ms, unread, has_details, mail_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(element_id) DO UPDATE SET
                folder_kind = excluded.folder_kind,
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
                meta.folder_kind,
                meta.subject,
                meta.sender_name,
                meta.sender_address,
                meta.received_date_ms,
                meta.unread as i64,
                meta.has_details as i64,
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
                "INSERT INTO mails (element_id, list_id, folder_kind, subject, sender_name,
                                    sender_address, received_date_ms, unread, has_details, mail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(element_id) DO UPDATE SET
                    folder_kind = excluded.folder_kind,
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
                    meta.folder_kind,
                    meta.subject,
                    meta.sender_name,
                    meta.sender_address,
                    meta.received_date_ms,
                    meta.unread as i64,
                    meta.has_details as i64,
                    meta.mail_json,
                ])?;
            }
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn delete_mails_not_in(
        &self,
        kind: MailSetKind,
        element_ids: &[&str],
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().unwrap();

        let mut deleted = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT element_id FROM mails WHERE folder_kind = ?1",
            )?;
            let existing: Vec<String> = stmt
                .query_map([kind_to_i64(kind)], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            let keep: std::collections::HashSet<&str> =
                element_ids.iter().copied().collect();

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

    pub fn mail_count(&self, kind: MailSetKind) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mails WHERE folder_kind = ?1",
            [kind_to_i64(kind)],
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
}

fn kind_to_i64(kind: MailSetKind) -> i64 {
    kind as i64
}

pub fn kind_from_i64(v: i64) -> MailSetKind {
    match v {
        0 => MailSetKind::Inbox,
        1 => MailSetKind::Sent,
        2 => MailSetKind::Trash,
        3 => MailSetKind::Archive,
        4 => MailSetKind::Spam,
        5 => MailSetKind::Draft,
        _ => MailSetKind::Inbox,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto_primitives::aes::{Aes256Key, AES_256_KEY_SIZE};

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

    #[test]
    fn test_open_and_verify() {
        let store = open_memory_store();
        assert!(store.verify_key());
    }

    #[test]
    fn test_upsert_and_load_metadata() {
        let store = open_memory_store();
        let meta = MailMetadata {
            element_id: "abc123".into(),
            list_id: "list1".into(),
            folder_kind: kind_to_i64(MailSetKind::Inbox),
            subject: "Test email".into(),
            sender_name: "Alice".into(),
            sender_address: "alice@example.com".into(),
            received_date_ms: 1700000000000,
            unread: true,
            has_details: false,
            mail_json: "{}".into(),
        };
        store.upsert_mail_metadata(&meta).unwrap();

        let loaded = store.load_folder_metadata(MailSetKind::Inbox).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].element_id, "abc123");
        assert_eq!(loaded[0].subject, "Test email");
        assert!(loaded[0].unread);
        assert!(!loaded[0].has_details);
    }

    #[test]
    fn test_batch_upsert() {
        let store = open_memory_store();
        let metas: Vec<MailMetadata> = (0..100)
            .map(|i| MailMetadata {
                element_id: format!("mail_{i}"),
                list_id: "list1".into(),
                folder_kind: kind_to_i64(MailSetKind::Inbox),
                subject: format!("Subject {i}"),
                sender_name: "Test".into(),
                sender_address: "test@test.com".into(),
                received_date_ms: 1700000000000 + i,
                unread: i % 2 == 0,
                has_details: false,
                mail_json: "{}".into(),
            })
            .collect();
        store.upsert_mail_metadata_batch(&metas).unwrap();

        let loaded = store.load_folder_metadata(MailSetKind::Inbox).unwrap();
        assert_eq!(loaded.len(), 100);
        assert_eq!(store.mail_count(MailSetKind::Inbox).unwrap(), 100);
        assert_eq!(store.total_count().unwrap(), 100);
    }

    #[test]
    fn test_delete_mails_not_in() {
        let store = open_memory_store();
        let metas: Vec<MailMetadata> = (0..5)
            .map(|i| MailMetadata {
                element_id: format!("mail_{i}"),
                list_id: "list1".into(),
                folder_kind: kind_to_i64(MailSetKind::Inbox),
                subject: format!("Subject {i}"),
                sender_name: "Test".into(),
                sender_address: "test@test.com".into(),
                received_date_ms: 1700000000000 + i,
                unread: false,
                has_details: false,
                mail_json: "{}".into(),
            })
            .collect();
        store.upsert_mail_metadata_batch(&metas).unwrap();

        let keep = vec!["mail_0", "mail_2", "mail_4"];
        let deleted = store.delete_mails_not_in(MailSetKind::Inbox, &keep).unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(deleted.contains(&"mail_1".to_string()));
        assert!(deleted.contains(&"mail_3".to_string()));

        assert_eq!(store.mail_count(MailSetKind::Inbox).unwrap(), 3);
    }

    #[test]
    fn test_eml_write_read_roundtrip() {
        let store = open_memory_store();
        let rfc2822 = "From: test@example.com\r\nSubject: Hello\r\n\r\nBody text here";
        store.write_eml("test_mail", rfc2822).unwrap();

        let read_back = store.read_eml("test_mail").unwrap();
        assert_eq!(read_back, Some(rfc2822.to_string()));
    }

    #[test]
    fn test_eml_read_nonexistent() {
        let store = open_memory_store();
        let result = store.read_eml("nonexistent").unwrap();
        assert_eq!(result, None);
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
        let meta = MailMetadata {
            element_id: "abc".into(),
            list_id: "list1".into(),
            folder_kind: kind_to_i64(MailSetKind::Inbox),
            subject: "Test".into(),
            sender_name: "".into(),
            sender_address: "test@test.com".into(),
            received_date_ms: 0,
            unread: false,
            has_details: true,
            mail_json: "{}".into(),
        };
        store.upsert_mail_metadata(&meta).unwrap();
        store.write_eml("abc", "content").unwrap();

        store.reset().unwrap();
        assert_eq!(store.total_count().unwrap(), 0);
        assert!(store.read_eml("abc").unwrap().is_none());
        assert!(store.verify_key());
    }

    #[test]
    fn test_mark_has_details() {
        let store = open_memory_store();
        let meta = MailMetadata {
            element_id: "det".into(),
            list_id: "list1".into(),
            folder_kind: kind_to_i64(MailSetKind::Inbox),
            subject: "Test".into(),
            sender_name: "".into(),
            sender_address: "t@t.com".into(),
            received_date_ms: 0,
            unread: false,
            has_details: false,
            mail_json: "{}".into(),
        };
        store.upsert_mail_metadata(&meta).unwrap();

        let loaded = store.load_folder_metadata(MailSetKind::Inbox).unwrap();
        assert!(!loaded[0].has_details);

        store.mark_has_details("det").unwrap();

        let loaded = store.load_folder_metadata(MailSetKind::Inbox).unwrap();
        assert!(loaded[0].has_details);
    }
}
