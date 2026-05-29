//! Complete mailbox backup to a tree of plaintext `.eml` files.
//!
//! A backup must be *complete* — it exports every mail that exists on the
//! server, **not** just the `sync_limit`-capped subset the bridge keeps hot
//! in its local cache. Backing up only the synced subset would silently drop
//! mail and give a false sense of safety.
//!
//! Strategy: for each folder, enumerate **all** its mails from the server
//! (`limit == 0`), then for each mail use the encrypted local cache as a
//! fast path (decrypt the existing `.eml.enc`) and only fall back to a
//! rate-limited server fetch for mails that were never synced.
//!
//! Output format is plain RFC 2822 `.eml`, one file per mail, in a directory
//! tree mirroring the IMAP folder hierarchy:
//!
//! ```text
//! <output>/
//! ├── INBOX/
//! │   ├── 20260528-144935_OtjDuDU--3-9.eml
//! │   └── …
//! ├── Sent/
//! └── Café/Projets/…
//! ```
//!
//! `.eml` is the most portable choice: every mail client (Thunderbird, Apple
//! Mail, Outlook) opens it natively, it survives Windows filesystems (no
//! Maildir `:2,S` colons), and a single corrupt file never takes down the
//! whole archive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::mail::mail_to_rfc2822;
use crate::store::LocalStore;
use crate::tuta::{MailBackend, IMAP_DELIMITER};
use tutasdk::entities::generated::tutanota::TutanotaFile;

/// Politeness delay between two *server* fetches (mails not already cached).
/// Mirrors the syncer's prefetch throttle so a full backup of a large
/// mailbox doesn't trip Tuta's rate limiter. Cache hits are not delayed.
const INTER_FETCH_DELAY: Duration = Duration::from_millis(150);

/// Progress emitted once per mail so a CLI or GUI can render a bar.
#[derive(Debug, Clone)]
pub struct BackupProgress {
    /// IMAP path of the folder currently being exported (e.g. `INBOX`).
    pub folder: String,
    /// Mails completed in this folder so far (1-based, == total when done).
    pub done: usize,
    /// Total mails in this folder.
    pub total: usize,
}

/// Outcome of a backup run. Per-mail failures are collected in `errors`
/// rather than aborting the whole export — a backup should salvage as much
/// as it can.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackupStats {
    pub folders: usize,
    pub mails_written: usize,
    /// Mails served from the encrypted local cache (no network).
    pub from_cache: usize,
    /// Mails fetched + decrypted from the server during the backup.
    pub from_server: usize,
    /// Mails whose `.eml` was already on disk and so were skipped — this is
    /// what makes a re-run resume an interrupted backup and turns a periodic
    /// re-backup into an incremental one (only new mail is fetched).
    pub skipped: usize,
    pub bytes: u64,
    /// Non-fatal per-mail failures (`"<element_id>: <reason>"`).
    pub errors: Vec<String>,
}

/// Export every mail of every folder to `output` as `.eml` files.
///
/// `progress` is invoked once per mail. Fatal errors (cannot list folders,
/// cannot create the output directory) return `Err`; per-mail problems are
/// recorded in [`BackupStats::errors`] and do not stop the run.
pub async fn export_eml(
    backend: &dyn MailBackend,
    local_store: &LocalStore,
    output: &Path,
    mut progress: impl FnMut(&BackupProgress),
) -> Result<BackupStats, String> {
    std::fs::create_dir_all(output).map_err(|e| format!("Cannot create {}: {e}", output.display()))?;

    let folders = backend.list_folders().await?;
    let mut stats = BackupStats::default();

    for folder in &folders {
        let folder_dir = folder_output_dir(output, &folder.imap_path);
        std::fs::create_dir_all(&folder_dir)
            .map_err(|e| format!("Cannot create {}: {e}", folder_dir.display()))?;
        stats.folders += 1;

        // `limit == 0` => every mail on the server, paginated by the SDK.
        let mails = backend.load_mail_ids_for_folder(folder, 0).await?;
        let total = mails.len();

        for (i, mail) in mails.iter().enumerate() {
            let Some(id) = mail._id.as_ref() else {
                continue;
            };
            let eid = id.element_id.to_string();

            // The filename is deterministic (stable receivedDate + element
            // id), so if it's already on disk this mail was exported by an
            // earlier run. Skip it *before* any cache read or server fetch —
            // that's what makes a re-run resume an interrupted backup and an
            // incremental re-backup cheap. (Mail content is immutable, so an
            // existing file is never stale.)
            let fname = format!(
                "{}_{}.eml",
                date_stamp(mail.receivedDate.as_millis()),
                sanitize_segment(&eid)
            );
            let path = folder_dir.join(&fname);
            if path.exists() {
                stats.skipped += 1;
                progress(&BackupProgress {
                    folder: folder.imap_path.clone(),
                    done: i + 1,
                    total,
                });
                continue;
            }

            // Fast path: decrypt the cached `.eml.enc` if we have it.
            let (eml, from_cache) = match local_store.read_eml(&eid) {
                Ok(Some(cached)) => (cached, true),
                _ => {
                    // Slow path: pull body + attachments from the server.
                    let details = backend.load_mail_details(mail).await.ok().flatten();
                    let attachments_owned = match backend.load_attachments(mail).await {
                        Ok(a) => a,
                        Err(e) => {
                            stats.errors.push(format!("{eid}: attachments: {e}"));
                            Vec::new()
                        }
                    };
                    let refs: Vec<(&TutanotaFile, &[u8])> = attachments_owned
                        .iter()
                        .map(|(f, d)| (f, d.as_slice()))
                        .collect();
                    let eml = mail_to_rfc2822(mail, details.as_ref(), &refs);
                    tokio::time::sleep(INTER_FETCH_DELAY).await;
                    (eml, false)
                }
            };

            match std::fs::write(&path, eml.as_bytes()) {
                Ok(()) => {
                    stats.mails_written += 1;
                    stats.bytes += eml.len() as u64;
                    if from_cache {
                        stats.from_cache += 1;
                    } else {
                        stats.from_server += 1;
                    }
                }
                Err(e) => stats.errors.push(format!("{eid}: write: {e}")),
            }

            progress(&BackupProgress {
                folder: folder.imap_path.clone(),
                done: i + 1,
                total,
            });
        }
    }

    Ok(stats)
}

/// Map an IMAP folder path (`Café/Projets`) to a nested output directory,
/// sanitising each segment for the filesystem.
fn folder_output_dir(base: &Path, imap_path: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for segment in imap_path.split(IMAP_DELIMITER) {
        if segment.is_empty() {
            continue;
        }
        p.push(sanitize_segment(segment));
    }
    p
}

/// Replace characters that are illegal (or merely troublesome) in path
/// components across Windows / macOS / Linux. Windows is the strict one:
/// `< > : " / \ | ? *` and control chars are forbidden, and a trailing dot
/// or space is silently stripped by the OS.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Compact, sortable `YYYYMMDD-HHMMSS` stamp from an epoch-millis value, used
/// as a filename prefix so a folder listing sorts chronologically.
fn date_stamp(millis: u64) -> String {
    let secs = millis / 1000;
    let days = secs / 86400;
    let tod = secs % 86400;
    let (y, m, d) = crate::mail::rfc2822::days_to_ymd(days);
    let h = tod / 3600;
    let mi = (tod % 3600) / 60;
    let s = tod % 60;
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_windows_illegal_chars() {
        assert_eq!(sanitize_segment("a:b"), "a_b");
        assert_eq!(sanitize_segment("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_segment("re: <test>?"), "re_ _test__");
        assert_eq!(sanitize_segment("a|b*c\"d"), "a_b_c_d");
    }

    #[test]
    fn sanitize_strips_trailing_dot_and_space() {
        assert_eq!(sanitize_segment("name. "), "name");
        assert_eq!(sanitize_segment("  spaced  "), "spaced");
    }

    #[test]
    fn sanitize_keeps_unicode_and_safe_chars() {
        assert_eq!(sanitize_segment("Café"), "Café");
        assert_eq!(sanitize_segment("Achat Immo"), "Achat Immo");
        assert_eq!(sanitize_segment("OtjDuDU--3-9"), "OtjDuDU--3-9");
    }

    #[test]
    fn sanitize_empty_becomes_underscore() {
        assert_eq!(sanitize_segment(""), "_");
        assert_eq!(sanitize_segment("..."), "_");
    }

    #[test]
    fn folder_dir_nests_on_imap_delimiter() {
        let base = Path::new("/tmp/backup");
        let p = folder_output_dir(base, "Café/Projets");
        assert_eq!(p, Path::new("/tmp/backup/Café/Projets"));
    }

    #[test]
    fn folder_dir_sanitizes_each_segment() {
        let base = Path::new("/tmp/backup");
        // A custom folder literally named with a colon would break Windows.
        let p = folder_output_dir(base, "Work/A:B");
        assert_eq!(p, Path::new("/tmp/backup/Work/A_B"));
    }

    #[test]
    fn date_stamp_is_sortable_and_correct() {
        // 2024-12-25 12:37:25 UTC = 1735130245000 ms
        assert_eq!(date_stamp(1735130245000), "20241225-123725");
        // epoch
        assert_eq!(date_stamp(0), "19700101-000000");
    }

    // --- integration: export_eml over a mock backend + temp store ---

    use crate::mail::ParsedMessage;
    use crate::tuta::{FolderInfo, MailBackend};
    use base64::Engine as _;
    use crypto_primitives::aes::Aes256Key;
    use crypto_primitives::key::GenericAesKey;
    use crypto_primitives::randomizer_facade::RandomizerFacade;
    use std::collections::HashMap;
    use tutasdk::date::DateTime;
    use tutasdk::entities::generated::tutanota::{
        Body, Mail, MailAddress, MailDetails, MailSetEntry, Recipients,
    };
    use tutasdk::folder_system::MailSetKind;
    use tutasdk::{GeneratedId, IdTupleGenerated};

    fn gid(s: &str) -> GeneratedId {
        GeneratedId(s.to_string())
    }

    fn make_mail(element_id: &str, subject: &str) -> Mail {
        Mail {
            _id: Some(IdTupleGenerated::new(gid("list1"), gid(element_id))),
            _permissions: gid("perm"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: subject.to_string(),
            receivedDate: DateTime::from_millis(1735130245000),
            state: 2,
            unread: false,
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
                name: "Alice".to_string(),
                address: "alice@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(gid("cl"), gid("ce")),
            firstRecipient: Some(MailAddress {
                _id: None,
                name: "Bob".to_string(),
                address: "bob@example.com".to_string(),
                contact: None,
                _errors: Default::default(),
            }),
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        }
    }

    fn make_details(body: &str) -> MailDetails {
        MailDetails {
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
                text: Some(body.to_string()),
                compressedText: None,
                _errors: Default::default(),
            },
        }
    }

    fn folder(id: &str, entries: &str, path: &str) -> FolderInfo {
        FolderInfo {
            id: id.to_string(),
            list_id: "folders".to_string(),
            entries_list_id: entries.to_string(),
            kind: MailSetKind::Inbox,
            imap_path: path.to_string(),
            special_use: None,
        }
    }

    struct MockBackend {
        folders: Vec<FolderInfo>,
        mails: HashMap<String, Vec<Mail>>,
        server_loads: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl MailBackend for MockBackend {
        async fn list_folders(&self) -> Result<Vec<FolderInfo>, String> {
            Ok(self.folders.clone())
        }
        async fn load_mail_ids_for_folder(
            &self,
            folder: &FolderInfo,
            _limit: usize,
        ) -> Result<Vec<Mail>, String> {
            Ok(self.mails.get(&folder.entries_list_id).cloned().unwrap_or_default())
        }
        async fn load_mail_details(&self, _mail: &Mail) -> Result<Option<MailDetails>, String> {
            *self.server_loads.lock().unwrap() += 1;
            Ok(Some(make_details("<p>fetched from server</p>")))
        }
        async fn load_attachments(
            &self,
            _mail: &Mail,
        ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, String> {
            Ok(vec![])
        }
        async fn load_mail(&self, _l: &str, _e: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail(&self, _j: &str) -> Result<Option<Mail>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_set_entry(
            &self,
            _j: &str,
        ) -> Result<Option<MailSetEntry>, String> {
            unimplemented!()
        }
        async fn decrypt_inline_mail_details_blob(
            &self,
            _j: &str,
        ) -> Result<Option<MailDetails>, String> {
            unimplemented!()
        }
        async fn set_unread_status(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _u: bool,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn trash_mails(&self, _ids: Vec<IdTupleGenerated>) -> Result<(), String> {
            unimplemented!()
        }
        async fn move_mails(
            &self,
            _ids: Vec<IdTupleGenerated>,
            _t: &FolderInfo,
        ) -> Result<(), String> {
            unimplemented!()
        }
        async fn send_mail(&self, _m: &ParsedMessage) -> Result<(), String> {
            unimplemented!()
        }
    }

    fn temp_store() -> (LocalStore, std::path::PathBuf) {
        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        let key: GenericAesKey = GenericAesKey::Aes256(Aes256Key::generate(&randomizer));
        let tmp = std::env::temp_dir().join(format!("tutabridge_backup_test_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = LocalStore::open(&tmp.join("s.db"), &tmp.join("mails"), key).unwrap();
        (store, tmp)
    }

    #[tokio::test]
    async fn export_writes_eml_tree_with_cache_and_server_paths() {
        let (store, _tmp) = temp_store();

        // INBOX has two mails; one is pre-cached, one must be fetched.
        let m_cached = make_mail("Cached1--3-9", "Cached subject");
        let m_fetch = make_mail("Fetch1--3-9", "Fetched subject");
        // Sent has one mail, also fetched.
        let m_sent = make_mail("Sent1--3-9", "Sent subject");

        // Seed the cache for the cached one only.
        store
            .write_eml(
                "Cached1--3-9",
                &mail_to_rfc2822(&m_cached, Some(&make_details("<p>from cache</p>")), &[]),
            )
            .unwrap();

        let mut mails = HashMap::new();
        mails.insert("inbox_entries".to_string(), vec![m_cached, m_fetch]);
        mails.insert("sent_entries".to_string(), vec![m_sent]);

        let backend = MockBackend {
            folders: vec![
                folder("inbox", "inbox_entries", "INBOX"),
                folder("sent", "sent_entries", "Sent"),
            ],
            mails,
            server_loads: std::sync::Mutex::new(0),
        };

        let out = std::env::temp_dir().join(format!("tutabridge_backup_out_{}", rand::random::<u64>()));
        let mut progress_calls = 0;
        let stats = export_eml(&backend, &store, &out, |_p| progress_calls += 1)
            .await
            .unwrap();

        assert_eq!(stats.folders, 2);
        assert_eq!(stats.mails_written, 3);
        assert_eq!(stats.from_cache, 1, "the seeded mail should come from cache");
        assert_eq!(stats.from_server, 2, "the other two should be fetched");
        assert_eq!(progress_calls, 3);
        assert!(stats.errors.is_empty());
        // Only the two non-cached mails hit the backend.
        assert_eq!(*backend.server_loads.lock().unwrap(), 2);

        // Files landed in the right per-folder dirs with the date prefix.
        let inbox = out.join("INBOX");
        let sent = out.join("Sent");
        let inbox_files: Vec<_> = std::fs::read_dir(&inbox).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(inbox_files.len(), 2);
        assert_eq!(std::fs::read_dir(&sent).unwrap().count(), 1);

        // The cached mail's body must be the cached version, not a re-fetch.
        let cached_path = inbox.join("20241225-123725_Cached1--3-9.eml");
        let cached_eml = std::fs::read_to_string(&cached_path).unwrap();
        let from_cache_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>from cache</p>");
        assert!(cached_eml.contains(&from_cache_b64), "cached body should be served verbatim");

        // --- second run resumes: everything is already on disk ---
        let stats2 = export_eml(&backend, &store, &out, |_p| {})
            .await
            .unwrap();
        assert_eq!(stats2.skipped, 3, "a re-run must skip every already-exported mail");
        assert_eq!(stats2.mails_written, 0);
        assert_eq!(stats2.from_server, 0, "no server fetch on a resume");
        assert_eq!(
            *backend.server_loads.lock().unwrap(),
            2,
            "server_loads unchanged: the second run hit zero mails"
        );

        std::fs::remove_dir_all(&out).ok();
    }

    #[tokio::test]
    async fn resume_only_fetches_the_new_mail() {
        let (store, _tmp) = temp_store();
        let out =
            std::env::temp_dir().join(format!("tutabridge_backup_inc_{}", rand::random::<u64>()));

        // First backup: one mail fetched.
        let mut m1 = HashMap::new();
        m1.insert("inbox_entries".to_string(), vec![make_mail("Old1--3-9", "old")]);
        let backend1 = MockBackend {
            folders: vec![folder("inbox", "inbox_entries", "INBOX")],
            mails: m1,
            server_loads: std::sync::Mutex::new(0),
        };
        let s1 = export_eml(&backend1, &store, &out, |_p| {}).await.unwrap();
        assert_eq!(s1.from_server, 1);

        // A new mail shows up. A re-run against the same output dir must skip
        // the old one (already on disk) and only fetch the newcomer.
        let mut m2 = HashMap::new();
        m2.insert(
            "inbox_entries".to_string(),
            vec![make_mail("Old1--3-9", "old"), make_mail("New1--3-9", "new")],
        );
        let backend2 = MockBackend {
            folders: vec![folder("inbox", "inbox_entries", "INBOX")],
            mails: m2,
            server_loads: std::sync::Mutex::new(0),
        };
        let s2 = export_eml(&backend2, &store, &out, |_p| {}).await.unwrap();
        assert_eq!(s2.skipped, 1, "the old mail is already on disk");
        assert_eq!(s2.from_server, 1, "only the new mail is fetched");
        assert_eq!(
            *backend2.server_loads.lock().unwrap(),
            1,
            "the second backend only loaded the new mail's details"
        );

        std::fs::remove_dir_all(&out).ok();
    }
}
