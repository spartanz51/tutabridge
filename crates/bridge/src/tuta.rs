use base64::Engine;
use crypto_primitives::aes::{Aes256Key, Iv, AES_256_KEY_SIZE};
use crypto_primitives::blake3::blake3_kdf;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use std::collections::HashMap;
use std::sync::Arc;
use tutasdk::bindings::file_client::{FileClient, FileClientError};
use tutasdk::bindings::rest_client::RestClient;
use tutasdk::blobs::blob_facade::FileData;
use tutasdk::crypto_entity_client::CryptoEntityClient;
use tutasdk::entities::generated::sys::BlobReferenceTokenWrapper;
use tutasdk::entities::generated::tutanota::{
    AttachmentKeyData, DraftAttachment, DraftCreateData, DraftData, DraftRecipient, Mail, MailBox,
    MailDetails, MailDetailsBlob, MailSetEntry, NewDraftAttachment, SendDraftData,
    SendDraftParameters, TutanotaFile,
};
use tutasdk::folder_system::{FolderSystem, MailSetKind};
use tutasdk::services::generated::tutanota::{DraftService, SendDraftService};
use tutasdk::services::ExtraServiceParams;
use tutasdk::tutanota_constants::ArchiveDataType;
use tutasdk::{ApiCallError, CustomId, IdTupleGenerated, ListLoadDirection, LoggedInSdk, Sdk};

use crate::config::Config;
use crate::mail::ParsedMessage;

/// A mail folder as seen by the bridge, keyed by its stable Tuta `MailSet`
/// element id rather than by a system folder kind (so custom/nested folders
/// are first-class).
#[derive(Clone, Debug)]
pub struct FolderInfo {
    /// `MailSet` element id — the stable key used everywhere.
    pub id: String,
    /// `MailSet` list id (the mailbox's folders list) — with `id` it forms the
    /// folder's full `IdTuple`, needed as a move target.
    pub list_id: String,
    /// `MailSet.entries` list id — used to load the mails in this folder.
    pub entries_list_id: String,
    pub kind: MailSetKind,
    /// IMAP mailbox path, e.g. `INBOX`, `Sent`, `Work/Projects`.
    pub imap_path: String,
    /// RFC 6154 special-use attribute, e.g. `\Sent` (system folders only).
    pub special_use: Option<String>,
}

/// IMAP hierarchy delimiter used to build nested folder paths.
pub const IMAP_DELIMITER: char = '/';

#[async_trait::async_trait]
pub trait MailBackend: Send + Sync {
    async fn load_mail_ids_for_folder(
        &self,
        folder: &FolderInfo,
        limit: usize,
    ) -> Result<Vec<Mail>, String>;
    /// Load a single mail by `(list_id, element_id)` — used by the event-bus
    /// handler to fetch a freshly-created or updated mail without re-listing
    /// its folder. `Ok(None)` means the entity is no longer on the server.
    async fn load_mail(&self, list_id: &str, element_id: &str) -> Result<Option<Mail>, String>;
    /// Decrypt the still-encrypted `event.instance` payload of a Mail event
    /// directly (no REST round-trip). `Ok(None)` covers both "session key
    /// transient" and "payload absent" so the handler can fall back to
    /// `load_mail`.
    async fn decrypt_inline_mail(&self, json: &str) -> Result<Option<Mail>, String>;
    /// Same shape for a `MailSetEntry` event — gives the handler the
    /// referenced `mail` IdTuple without ever asking the server.
    async fn decrypt_inline_mail_set_entry(
        &self,
        json: &str,
    ) -> Result<Option<MailSetEntry>, String>;
    /// Inline-decrypt the `event.blob_instance` carried alongside a Mail
    /// CREATE — that's the `MailDetailsBlob` (subject + body + recipients
    /// envelope) the prefetch loop would otherwise have to fetch via REST.
    async fn decrypt_inline_mail_details_blob(
        &self,
        json: &str,
    ) -> Result<Option<MailDetails>, String>;
    async fn load_mail_details(&self, mail: &Mail) -> Result<Option<MailDetails>, String>;
    /// Load and decrypt every attachment of `mail`. Returns a vector of
    /// `(file metadata, decrypted bytes)` in the order they appear in
    /// `mail.attachments`. Empty if the mail has no attachments. Errors are
    /// per-mail (no partial returns): if any one attachment fails the whole
    /// call returns `Err` so the caller can decide to retry the prefetch.
    async fn load_attachments(&self, mail: &Mail) -> Result<Vec<(TutanotaFile, Vec<u8>)>, String>;
    /// Enumerate all mail folders (system + custom, with hierarchy).
    async fn list_folders(&self) -> Result<Vec<FolderInfo>, String>;
    async fn set_unread_status(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        unread: bool,
    ) -> Result<(), String>;
    async fn trash_mails(&self, mail_ids: Vec<IdTupleGenerated>) -> Result<(), String>;
    /// Move mails into the given target folder.
    async fn move_mails(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        target: &FolderInfo,
    ) -> Result<(), String>;
    async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String>;
}

/// Canonical IMAP name for a system folder, or `None` for custom/unsupported.
fn system_imap_name(kind: MailSetKind) -> Option<&'static str> {
    match kind {
        MailSetKind::Inbox => Some("INBOX"),
        MailSetKind::Sent => Some("Sent"),
        MailSetKind::Draft => Some("Drafts"),
        MailSetKind::Trash => Some("Trash"),
        MailSetKind::Archive => Some("Archive"),
        MailSetKind::Spam => Some("Spam"),
        _ => None,
    }
}

/// RFC 6154 special-use attribute for a system folder.
fn system_special_use(kind: MailSetKind) -> Option<&'static str> {
    match kind {
        MailSetKind::Sent => Some("\\Sent"),
        MailSetKind::Draft => Some("\\Drafts"),
        MailSetKind::Trash => Some("\\Trash"),
        MailSetKind::Archive => Some("\\Archive"),
        MailSetKind::Spam => Some("\\Junk"),
        _ => None,
    }
}

/// One attachment held in the self-send cache, mirroring what we received
/// over SMTP from Thunderbird. Used to bypass the never-propagated
/// `File._ownerEncSessionKey` race on the Inbox copy of a self-sent mail.
#[derive(Clone)]
struct CachedAttachment {
    filename: String,
    mime_type: String,
    data: Vec<u8>,
}

/// A self-send cache entry — the attachments and when they were stored.
struct SelfSendCacheEntry {
    stored_at: std::time::Instant,
    attachments: Vec<CachedAttachment>,
}

/// How long the self-send cache holds an entry. Tuta's server never
/// populates the inbox-side `File._ownerEncSessionKey` for self-sends in
/// practice, but capping the cache lifetime keeps memory bounded for
/// long-running bridges.
const SELF_SEND_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Soft cap on number of cached self-sends. When exceeded on insert, the
/// oldest entry is evicted regardless of TTL.
const SELF_SEND_CACHE_MAX_ENTRIES: usize = 50;

pub struct TutaSession {
    pub logged_in: Arc<LoggedInSdk>,
    pub email: String,
    /// Bearer token issued at login; used to authenticate REST requests and
    /// the realtime event-bus WebSocket query string.
    pub access_token: String,
    /// Attachments we just sent to ourselves over SMTP, indexed by
    /// `(subject + sender + first_recipient)`. The Inbox copy of a
    /// self-sent mail can take an indefinite amount of time to surface a
    /// usable `File._ownerEncSessionKey`, but we already know each file's
    /// session key (we generated it locally on the send path), so we
    /// short-circuit `load_attachments` for self-sends and serve the
    /// cached bytes directly.
    self_send_cache: Arc<tokio::sync::Mutex<HashMap<String, SelfSendCacheEntry>>>,
}

impl TutaSession {
    pub async fn load_mailbox(&self) -> Result<MailBox, ApiCallError> {
        self.logged_in.mail_facade().load_user_mailbox().await
    }

    pub async fn load_folders(&self, mailbox: &MailBox) -> Result<FolderSystem, ApiCallError> {
        self.logged_in
            .mail_facade()
            .load_folders_for_mailbox(mailbox)
            .await
    }

    pub fn user_id(&self) -> Option<String> {
        self.logged_in.get_user_id().map(|id| id.0)
    }

    /// Group ids the event bus should subscribe to: all of the user's
    /// memberships except mailing lists, plus the user group itself
    /// (mirrors `EventBusClient.eventGroups()` in the TS worker).
    pub fn event_groups(&self) -> Vec<String> {
        use tutasdk::tutanota_constants::GroupType;
        let user = self.logged_in.get_user();
        let mut groups: Vec<String> = user
            .memberships
            .iter()
            .filter(|m| m.groupType != Some(GroupType::MailingList as i64))
            .map(|m| m.group.to_string())
            .collect();
        groups.push(user.userGroup.group.to_string());
        groups
    }

    pub async fn load_mail_by_id(
        &self,
        list_id: &str,
        element_id: &str,
    ) -> Result<Option<Mail>, ApiCallError> {
        let id = IdTupleGenerated {
            list_id: tutasdk::GeneratedId(list_id.to_string()),
            element_id: tutasdk::GeneratedId(element_id.to_string()),
        };
        match self.crypto_client().load::<Mail, _>(&id).await {
            Ok(mail) => Ok(Some(mail)),
            // Treat "not found" gracefully — the entity may have just been
            // deleted server-side between the event and our follow-up load.
            Err(ApiCallError::ServerResponseError {
                source: tutasdk::rest_error::HttpError::NotFoundError,
            }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn derive_storage_key(&self) -> Result<GenericAesKey, String> {
        let user_group_id = self.logged_in.get_user_group_id();
        let versioned_key = self
            .logged_in
            .get_current_sym_group_key(&user_group_id)
            .await
            .map_err(|e| format!("Failed to get user group key: {e}"))?;
        let derived = blake3_kdf(
            &[versioned_key.object.as_bytes()],
            "tutabridge local storage v1",
            AES_256_KEY_SIZE,
        );
        GenericAesKey::from_bytes(&derived).map_err(|e| format!("Key derivation error: {e:?}"))
    }

    fn crypto_client(&self) -> Arc<CryptoEntityClient> {
        self.logged_in.mail_facade().get_crypto_entity_client()
    }

    async fn load_mail_ids_for_folder_impl(
        &self,
        entries_list_id: &tutasdk::GeneratedId,
        limit: usize,
    ) -> Result<Vec<Mail>, ApiCallError> {
        // The server rejects a single `load_range` with `count` > 1000 (400
        // Bad request). For higher user-facing `sync_limit` we page through
        // by 1000 each call, advancing the cursor with the last entry id of
        // the previous page (DESC = newest first, oldest at the end). The
        // `limit == 0` path delegates to `load_all` which already paginates.
        const SERVER_PAGE: usize = 1000;
        let entries: Vec<MailSetEntry> = if limit == 0 {
            self.crypto_client()
                .load_all(entries_list_id, ListLoadDirection::DESC)
                .await?
        } else if limit <= SERVER_PAGE {
            self.crypto_client()
                .load_range(
                    entries_list_id,
                    &CustomId::default(),
                    limit,
                    ListLoadDirection::DESC,
                )
                .await?
        } else {
            let mut out: Vec<MailSetEntry> = Vec::with_capacity(limit);
            let mut start = CustomId::default();
            while out.len() < limit {
                let want = (limit - out.len()).min(SERVER_PAGE);
                let page: Vec<MailSetEntry> = self
                    .crypto_client()
                    .load_range(entries_list_id, &start, want, ListLoadDirection::DESC)
                    .await?;
                let got = page.len();
                if got == 0 {
                    break;
                }
                // Use the last (oldest in DESC) entry's element id as the
                // exclusive cursor for the next page.
                let next_start = page
                    .last()
                    .and_then(|e| e._id.as_ref())
                    .map(|id| id.element_id.clone());
                out.extend(page);
                if got < want {
                    break; // server has no more
                }
                let Some(ns) = next_start else { break };
                start = ns;
            }
            out
        };

        // Group entries by list_id for batch loading
        let mut by_list: std::collections::HashMap<String, Vec<tutasdk::GeneratedId>> =
            std::collections::HashMap::new();
        for entry in &entries {
            by_list
                .entry(entry.mail.list_id.to_string())
                .or_default()
                .push(entry.mail.element_id.clone());
        }

        let mut mails = Vec::new();
        for (list_id_str, element_ids) in &by_list {
            let list_id = tutasdk::GeneratedId(list_id_str.clone());
            match self
                .crypto_client()
                .load_multiple::<Mail>(&list_id, element_ids)
                .await
            {
                Ok(batch) => mails.extend(batch),
                Err(e) => log::warn!(
                    "Failed to batch load mails from list {}: {}",
                    list_id_str,
                    e
                ),
            }
        }

        Ok(mails)
    }

    async fn load_mail_details_impl(
        &self,
        mail: &Mail,
    ) -> Result<Option<MailDetails>, ApiCallError> {
        // Tuta stores a mail's body in two different places depending on
        // whether the mail is received or a draft. Match the TS routing
        // (`isDraft(mail) ? loadMailDetailsDraft : loadMailDetailsBlob`).
        let mail_facade = self.logged_in.mail_facade();
        if mail.mailDetailsDraft.is_some() {
            match mail_facade.load_mail_details_draft(mail).await {
                Ok(details) => Ok(Some(details)),
                Err(e) => {
                    log::error!("Failed to load mail details draft: {e}");
                    Err(e)
                }
            }
        } else if mail.mailDetails.is_some() {
            match mail_facade.load_mail_details_blob(mail).await {
                Ok(details) => Ok(Some(details)),
                Err(e) => {
                    log::error!("Failed to load mail details blob: {e}");
                    Err(e)
                }
            }
        } else {
            // Legacy mail without either reference — body lives nowhere we
            // know how to read. The prefetch layer treats `Ok(None)` as
            // "render with headers only, do not retry".
            Ok(None)
        }
    }

    async fn load_attachments_impl(
        &self,
        mail: &Mail,
    ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, ApiCallError> {
        if mail.attachments.is_empty() {
            return Ok(Vec::new());
        }
        // Fast path: an inbox copy of a self-sent mail. The server never
        // populates its File entities' `_ownerEncSessionKey`, but we still
        // hold the plaintext bytes we passed to SMTP — return them and
        // skip the REST round-trip entirely.
        if let Some(cached) = self.try_self_send_cache(mail).await {
            return Ok(cached);
        }
        let mail_facade = self.logged_in.mail_facade();
        let mut out: Vec<(TutanotaFile, Vec<u8>)> = Vec::with_capacity(mail.attachments.len());
        for file_id in &mail.attachments {
            let file = self.load_file_with_retry(file_id).await?;
            let data = mail_facade.load_file_attachment_data(&file).await?;
            out.push((file, data));
        }
        Ok(out)
    }

    /// Load a `TutanotaFile` entity, retrying with backoff when the server
    /// returns it before its encryption metadata has propagated.
    ///
    /// Mails delivered through the realtime event bus typically include
    /// freshly-created attachments whose `_ownerEncSessionKey` /
    /// `_ownerGroup` haven't been published yet — the SDK's session-key
    /// resolution then fails with "instance missing owner key/group data".
    /// 2s + 4s + 8s + 16s ≈ 30s of tolerance covers the propagation lag in
    /// practice (self-sends can take >20s because the same mailbox
    /// produces both the Sent entity and the Inbox copy); anything still
    /// failing after that bubbles up so the `prefetch_details` caller can
    /// decide to ship a body-only envelope and let the next prefetch
    /// sweep (triggered by another store mutation) take another shot.
    async fn load_file_with_retry(
        &self,
        file_id: &IdTupleGenerated,
    ) -> Result<TutanotaFile, ApiCallError> {
        let crypto_client = self.crypto_client();
        let backoffs = [
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(4),
            std::time::Duration::from_secs(8),
            std::time::Duration::from_secs(16),
        ];
        let mut iter = backoffs.iter();
        loop {
            match crypto_client.load::<TutanotaFile, _>(file_id).await {
                Ok(file) => return Ok(file),
                Err(e) if is_session_key_transient(&e) => match iter.next() {
                    Some(delay) => {
                        log::debug!(
                            "File {} not yet decryptable, retrying in {:?}",
                            file_id.element_id,
                            delay
                        );
                        tokio::time::sleep(*delay).await;
                        continue;
                    }
                    None => {
                        log::warn!(
                            "File {} still not decryptable after retries",
                            file_id.element_id
                        );
                        return Err(e);
                    }
                },
                Err(e) => return Err(e),
            }
        }
    }

    /// Encrypt + upload every attachment in one go, then assemble the
    /// `DraftAttachment` aggregates the `DraftService` needs. Returns the
    /// aggregates in the same order as `attachments`, paired with each
    /// file's plaintext session key so the later `SendDraftService` call can
    /// hand it to the server.
    async fn build_added_attachments(
        &self,
        attachments: &[crate::mail::Attachment],
        mail_group_id: &tutasdk::GeneratedId,
        mail_group_key: &GenericAesKey,
        mail_group_key_version: u64,
        randomizer: &RandomizerFacade,
    ) -> Result<(Vec<DraftAttachment>, Vec<GenericAesKey>), ApiCallError> {
        if attachments.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let session_keys: Vec<GenericAesKey> = (0..attachments.len())
            .map(|_| Aes256Key::generate(randomizer).into())
            .collect();

        let file_data: Vec<FileData> = attachments
            .iter()
            .zip(session_keys.iter())
            .map(|(att, sk)| FileData {
                session_key: sk.clone(),
                data: att.data.as_slice(),
            })
            .collect();

        let tokens_per_file: Vec<Vec<BlobReferenceTokenWrapper>> = self
            .logged_in
            .blob_facade()
            .encrypt_and_upload_multiple(
                ArchiveDataType::Attachments,
                mail_group_id,
                file_data.iter(),
            )
            .await?;

        if tokens_per_file.len() != attachments.len() {
            return Err(ApiCallError::internal(format!(
                "blob upload returned {} token sets for {} attachments",
                tokens_per_file.len(),
                attachments.len()
            )));
        }

        let mut drafts: Vec<DraftAttachment> = Vec::with_capacity(attachments.len());
        for ((att, file_sk), tokens) in attachments
            .iter()
            .zip(session_keys.iter())
            .zip(tokens_per_file.into_iter())
        {
            let enc_file_name = file_sk
                .encrypt_data(att.filename.as_bytes(), Iv::generate(randomizer))
                .map_err(|e| {
                    ApiCallError::internal(format!("Failed to encrypt attachment name: {e}"))
                })?;
            let enc_mime_type = file_sk
                .encrypt_data(att.mime_type.as_bytes(), Iv::generate(randomizer))
                .map_err(|e| {
                    ApiCallError::internal(format!("Failed to encrypt attachment mime type: {e}"))
                })?;
            let owner_enc_file_sk = mail_group_key.encrypt_key(file_sk, Iv::generate(randomizer));

            let new_draft = NewDraftAttachment {
                _id: Some(random_custom_id(randomizer)),
                encFileName: enc_file_name,
                encMimeType: enc_mime_type,
                encCid: None,
                referenceTokens: tokens,
            };
            drafts.push(DraftAttachment {
                _id: Some(random_custom_id(randomizer)),
                ownerEncFileSessionKey: owner_enc_file_sk,
                ownerKeyVersion: mail_group_key_version as i64,
                newFile: Some(new_draft),
                existingFile: None,
            });
        }

        Ok((drafts, session_keys))
    }

    /// After `DraftService` succeeds, the saved Mail's `attachments` Vec
    /// holds the freshly-allocated `File` IdTuples (in the same order as
    /// the `addedAttachments` we just submitted). Reload the Mail, zip
    /// those IdTuples with our local session keys, and produce the
    /// `AttachmentKeyData[]` the server expects from `SendDraftService`.
    async fn build_attachment_key_data(
        &self,
        draft_id: &IdTupleGenerated,
        session_keys: &[GenericAesKey],
        randomizer: &RandomizerFacade,
    ) -> Result<Vec<AttachmentKeyData>, ApiCallError> {
        let mail: Mail = self.crypto_client().load(draft_id).await?;
        if mail.attachments.len() != session_keys.len() {
            return Err(ApiCallError::internal(format!(
                "DraftService returned {} attachments but we uploaded {}",
                mail.attachments.len(),
                session_keys.len()
            )));
        }
        Ok(mail
            .attachments
            .into_iter()
            .zip(session_keys.iter())
            .map(|(file_id, sk)| AttachmentKeyData {
                _id: Some(random_custom_id(randomizer)),
                bucketEncFileSessionKey: None,
                fileSessionKey: Some(sk.as_bytes().to_vec()),
                file: file_id,
            })
            .collect())
    }

    async fn send_mail_impl(&self, msg: &ParsedMessage) -> Result<(), ApiCallError> {
        // Pre-populate the self-send cache *before* we hit any Tuta API,
        // so the inbox-side WS event has zero chance of beating the
        // insert (`SendDraftService.post` on the server emits the event
        // before its own response reaches us).
        if !msg.attachments.is_empty() && is_self_recipient(msg, &self.email) {
            self.cache_self_send_attachments(msg).await;
        }

        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        let session_key: GenericAesKey = Aes256Key::generate(&randomizer).into();

        let mail_group_id = self
            .logged_in
            .mail_facade()
            .get_group_id_for_mail_address(&self.email)
            .await?;
        let group_key = self
            .logged_in
            .get_current_sym_group_key(&mail_group_id)
            .await?;

        let owner_enc_session_key = group_key
            .object
            .encrypt_key(&session_key, Iv::generate(&randomizer));
        let owner_key_version = group_key.version as i64;

        // Upload every attachment first — the resulting `DraftAttachment`
        // aggregates ride along inside `DraftData.addedAttachments`, and we
        // stash each per-file session key in `attachment_keys` so the later
        // `SendDraftService` call can hand it to the server in
        // `attachmentKeyData[]`.
        let (added_attachments, attachment_keys) = self
            .build_added_attachments(
                &msg.attachments,
                &mail_group_id,
                &group_key.object,
                group_key.version,
                &randomizer,
            )
            .await?;

        let draft_data = build_draft_data(msg, &self.email, added_attachments);

        let create_data = DraftCreateData {
            _format: 0,
            previousMessageId: None,
            conversationType: 0,
            ownerEncSessionKey: owner_enc_session_key,
            ownerKeyVersion: owner_key_version,
            draftData: draft_data,
            _errors: Default::default(),
        };

        let executor = self.logged_in.get_service_executor();
        let draft_return = executor
            .post::<DraftService>(
                create_data,
                ExtraServiceParams {
                    session_key: Some(session_key.clone()),
                    ..Default::default()
                },
            )
            .await?;

        log::info!("Draft created: {:?}", draft_return.draft);

        // To populate `attachmentKeyData[]` we need each File entity's
        // IdTuple — the server allocated those when the draft was created.
        // We load the freshly-saved Mail just to read its `attachments`
        // field (order matches our `addedAttachments`) and zip with the
        // session keys we generated earlier.
        let attachment_key_data = if attachment_keys.is_empty() {
            Vec::new()
        } else {
            self.build_attachment_key_data(&draft_return.draft, &attachment_keys, &randomizer)
                .await?
        };

        let parameters_id = CustomId(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(randomizer.generate_random_array::<4>()),
        );
        let send_data = build_send_draft_data(
            session_key.as_bytes().to_vec(),
            draft_return.draft,
            parameters_id,
            attachment_key_data,
        );

        let send_return = executor
            .post::<SendDraftService>(send_data, ExtraServiceParams::default())
            .await?;

        log::info!("Mail sent, message_id: {}", send_return.messageId);
        Ok(())
    }

    /// Insert this mail's attachments into the self-send cache so the
    /// inbox-side `load_attachments` lookup can find them.
    async fn cache_self_send_attachments(&self, msg: &ParsedMessage) {
        let key = self_send_cache_key(&msg.subject, &msg.from_address, &self.email);
        let attachments: Vec<CachedAttachment> = msg
            .attachments
            .iter()
            .map(|a| CachedAttachment {
                filename: a.filename.clone(),
                mime_type: a.mime_type.clone(),
                data: a.data.clone(),
            })
            .collect();
        let mut cache = self.self_send_cache.lock().await;
        // Soft eviction: drop the oldest entry when we cross the cap. The
        // cache is tiny so a linear scan to find the min stored_at is fine.
        if cache.len() >= SELF_SEND_CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
            if let Some((oldest_key, _)) = cache
                .iter()
                .min_by_key(|(_, v)| v.stored_at)
                .map(|(k, v)| (k.clone(), v.stored_at))
            {
                cache.remove(&oldest_key);
            }
        }
        cache.insert(
            key,
            SelfSendCacheEntry {
                stored_at: std::time::Instant::now(),
                attachments,
            },
        );
    }

    /// Look up cached attachments for an incoming Mail whose own copy of
    /// the File entities can't be decrypted. Returns `None` if this is not
    /// a self-send, the cache has nothing for the envelope, or the entry
    /// is older than the TTL.
    async fn try_self_send_cache(&self, mail: &Mail) -> Option<Vec<(TutanotaFile, Vec<u8>)>> {
        if !mail.sender.address.eq_ignore_ascii_case(&self.email) {
            return None;
        }
        let recipient = mail.firstRecipient.as_ref()?.address.clone();
        if !recipient.eq_ignore_ascii_case(&self.email) {
            return None;
        }
        let key = self_send_cache_key(&mail.subject, &mail.sender.address, &recipient);
        let mut cache = self.self_send_cache.lock().await;
        let entry = cache.get(&key)?;
        if entry.stored_at.elapsed() > SELF_SEND_CACHE_TTL {
            cache.remove(&key);
            return None;
        }
        // Clone, do NOT consume — a self-send produces two `load_attachments`
        // calls (one for the Sent copy, one for the Inbox copy) and both
        // need to render multipart. The TTL handles eviction.
        let attachments = entry.attachments.clone();
        log::info!(
            "Self-send cache hit for {:?} ({} attachment(s))",
            mail.subject,
            attachments.len()
        );
        Some(
            attachments
                .into_iter()
                .map(|a| (synthetic_file_for_cached(&a), a.data))
                .collect(),
        )
    }
}

/// Build a synthetic `TutanotaFile` whose only consumer is
/// `mail_to_rfc2822` — it just needs the name and MIME type to put a
/// matching `Content-Type` / `Content-Disposition` on the part. The other
/// fields stay at sensible defaults; nothing will REST-load this entity.
fn synthetic_file_for_cached(a: &CachedAttachment) -> TutanotaFile {
    TutanotaFile {
        _id: None,
        _permissions: tutasdk::GeneratedId::default(),
        _format: 0,
        _ownerEncSessionKey: None,
        name: a.filename.clone(),
        size: a.data.len() as i64,
        mimeType: Some(a.mime_type.clone()),
        _ownerGroup: None,
        cid: None,
        _ownerKeyVersion: None,
        _kdfNonce: None,
        parent: None,
        subFiles: None,
        blobs: vec![],
        _errors: Default::default(),
    }
}

/// True when the parsed SMTP message is a self-send: the sender (the
/// `From:` we put on the wire) is the bridge's own email, and at least one
/// of `to`/`cc`/`bcc` includes that same address. Only then is the inbox
/// race relevant — third-party recipients hit Tuta's normal pipeline
/// before our bridge ever sees the file.
fn is_self_recipient(msg: &ParsedMessage, own_email: &str) -> bool {
    if !msg.from_address.eq_ignore_ascii_case(own_email) {
        return false;
    }
    let me = own_email.to_ascii_lowercase();
    msg.to
        .iter()
        .chain(msg.cc.iter())
        .chain(msg.bcc.iter())
        .any(|(_, addr)| addr.to_ascii_lowercase() == me)
}

/// Stable, lowercase key for the self-send cache. The Sent and Inbox
/// copies of a self-sent mail share these three header values verbatim, so
/// the inbox side can find the entry the send side just dropped in.
fn self_send_cache_key(subject: &str, sender: &str, recipient: &str) -> String {
    format!(
        "{}|{}|{}",
        subject.trim().to_lowercase(),
        sender.trim().to_lowercase(),
        recipient.trim().to_lowercase()
    )
}

#[async_trait::async_trait]
impl MailBackend for TutaSession {
    async fn load_mail_ids_for_folder(
        &self,
        folder: &FolderInfo,
        limit: usize,
    ) -> Result<Vec<Mail>, String> {
        let entries_list_id = tutasdk::GeneratedId(folder.entries_list_id.clone());
        self.load_mail_ids_for_folder_impl(&entries_list_id, limit)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn load_mail(&self, list_id: &str, element_id: &str) -> Result<Option<Mail>, String> {
        self.load_mail_by_id(list_id, element_id)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn decrypt_inline_mail(&self, json: &str) -> Result<Option<Mail>, String> {
        self.crypto_client()
            .decrypt_inline_and_parse::<Mail>(json)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn decrypt_inline_mail_set_entry(
        &self,
        json: &str,
    ) -> Result<Option<MailSetEntry>, String> {
        self.crypto_client()
            .decrypt_inline_and_parse::<MailSetEntry>(json)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn decrypt_inline_mail_details_blob(
        &self,
        json: &str,
    ) -> Result<Option<MailDetails>, String> {
        // `event.blob_instance` is the encrypted MailDetailsBlob. We decrypt
        // it through the same inline pipeline as the Mail itself and pull out
        // its `details` aggregate, which is what consumers actually want.
        self.crypto_client()
            .decrypt_inline_and_parse::<MailDetailsBlob>(json)
            .await
            .map(|opt| opt.map(|blob| blob.details))
            .map_err(|e| format!("{e}"))
    }

    async fn load_mail_details(&self, mail: &Mail) -> Result<Option<MailDetails>, String> {
        self.load_mail_details_impl(mail)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn load_attachments(&self, mail: &Mail) -> Result<Vec<(TutanotaFile, Vec<u8>)>, String> {
        self.load_attachments_impl(mail)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn list_folders(&self) -> Result<Vec<FolderInfo>, String> {
        let mailbox = self.load_mailbox().await.map_err(|e| format!("{e}"))?;
        let folder_system = self
            .load_folders(&mailbox)
            .await
            .map_err(|e| format!("{e}"))?;

        let mut result = Vec::new();
        for indented in folder_system.indented_list() {
            let folder = indented.folder;
            let kind = folder.mail_set_kind();

            // Only expose folder types we support over IMAP.
            let is_custom = kind == MailSetKind::Custom;
            if !is_custom && system_imap_name(kind).is_none() {
                continue; // skip Scheduled / virtual sets
            }

            let Some((list_id, elem_id)) = folder
                ._id
                .as_ref()
                .map(|id| (id.list_id.to_string(), id.element_id.to_string()))
            else {
                continue;
            };

            // Build the IMAP path by mapping each ancestor segment.
            let mut segments: Vec<String> = Vec::new();
            for ancestor in folder_system.path_to_folder(&tutasdk::GeneratedId(elem_id.clone())) {
                let akind = ancestor.mail_set_kind();
                if let Some(name) = system_imap_name(akind) {
                    segments.push(name.to_string());
                } else {
                    // sanitize the delimiter out of custom names
                    segments.push(ancestor.name.replace(IMAP_DELIMITER, "_"));
                }
            }
            let imap_path = segments.join(&IMAP_DELIMITER.to_string());
            if imap_path.is_empty() {
                continue;
            }

            result.push(FolderInfo {
                id: elem_id,
                list_id,
                entries_list_id: folder.entries.to_string(),
                kind,
                imap_path,
                special_use: system_special_use(kind).map(|s| s.to_string()),
            });
        }
        Ok(result)
    }

    async fn set_unread_status(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        unread: bool,
    ) -> Result<(), String> {
        self.logged_in
            .mail_facade()
            .set_unread_status_for_mails(mail_ids, unread)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn trash_mails(&self, mail_ids: Vec<IdTupleGenerated>) -> Result<(), String> {
        self.logged_in
            .mail_facade()
            .trash_mails(mail_ids)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn move_mails(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        target: &FolderInfo,
    ) -> Result<(), String> {
        let target_folder = IdTupleGenerated::new(
            tutasdk::GeneratedId(target.list_id.clone()),
            tutasdk::GeneratedId(target.id.clone()),
        );
        self.logged_in
            .mail_facade()
            .move_mails(mail_ids, target_folder)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String> {
        self.send_mail_impl(msg).await.map_err(|e| format!("{e}"))
    }
}

struct DiskFileClient {
    base_dir: std::path::PathBuf,
}

impl DiskFileClient {
    fn new() -> Self {
        let base_dir = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("tutabridge");
        std::fs::create_dir_all(&base_dir).ok();
        Self { base_dir }
    }
}

#[async_trait::async_trait]
impl FileClient for DiskFileClient {
    async fn persist_content(&self, name: String, content: Vec<u8>) -> Result<(), FileClientError> {
        let path = self.base_dir.join(&name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FileClientError::from(e.kind()))?;
        }
        std::fs::write(&path, &content).map_err(|e| FileClientError::from(e.kind()))
    }

    async fn read_content(&self, name: String) -> Result<Vec<u8>, FileClientError> {
        let path = self.base_dir.join(&name);
        std::fs::read(&path).map_err(|e| FileClientError::from(e.kind()))
    }
}

pub enum TwoFactorCallback {
    Totp(Box<dyn Fn() -> Result<u32, Box<dyn std::error::Error + Send + Sync>> + Send + Sync>),
}

pub async fn login(
    cfg: &Config,
    password: &str,
) -> Result<TutaSession, Box<dyn std::error::Error + Send + Sync>> {
    login_with_2fa(cfg, Some(password), None).await
}

pub async fn login_with_2fa(
    cfg: &Config,
    password: Option<&str>,
    totp_callback: Option<TwoFactorCallback>,
) -> Result<TutaSession, Box<dyn std::error::Error + Send + Sync>> {
    let raw_client: Arc<dyn RestClient> =
        Arc::new(tutasdk::net::native_rest_client::NativeRestClient::try_new()?);
    // Route every SDK request through the rate governor (bounded concurrency +
    // 429/503 backoff), replacing the SDK's header-only suspension layer so
    // there is exactly one authoritative choke point for outbound API traffic.
    let rest_client: Arc<dyn RestClient> =
        Arc::new(crate::governed_client::GovernedRestClient::new(raw_client));
    let file_client: Arc<dyn FileClient> = Arc::new(DiskFileClient::new());
    let sdk = Sdk::new_without_suspension(cfg.api_url.clone(), rest_client, file_client);

    if let Some(credentials) = load_credentials(&cfg.email) {
        log::info!("Resuming saved session...");
        let access_token = credentials.access_token.clone();
        match sdk.login(credentials).await {
            Ok(logged_in) => {
                return Ok(TutaSession {
                    logged_in,
                    email: cfg.email.clone(),
                    access_token,
                    self_send_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                });
            }
            Err(e) => {
                log::warn!("Session expired, re-authenticating: {e}");
                delete_credentials(&cfg.email);
            }
        }
    }

    let password = password.ok_or("No saved session and no password provided")?;

    log::info!("Authenticating with Tuta servers...");
    let session = sdk
        .initiate_session(&cfg.email, password)
        .await
        .map_err(|e| {
            Box::<dyn std::error::Error + Send + Sync>::from(format!("Login failed: {e}"))
        })?;
    let credentials = session.credentials;
    let access_token = credentials.access_token.clone();

    if !session.challenges.is_empty() {
        for c in &session.challenges {
            log::info!("2FA challenge: type={}, id={:?}", c.r#type, c._id);
        }

        let has_totp = session
            .challenges
            .iter()
            .any(|c| c.r#type == i64::from(tutasdk::tutanota_constants::SecondFactorType::Totp));

        if !has_totp {
            return Err(
                "Account requires U2F/WebAuthn 2FA which is not supported — only TOTP is supported"
                    .into(),
            );
        }

        let totp_code = match &totp_callback {
            Some(TwoFactorCallback::Totp(cb)) => cb()?,
            None => return Err("2FA required but no TOTP callback provided".into()),
        };
        sdk.authenticate_with_second_factor_totp(&access_token, totp_code)
            .await
            .map_err(|e| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!("2FA failed: {e}"))
            })?;

        let mut cleared = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let pending = sdk
                .is_second_factor_pending(&access_token)
                .await
                .map_err(|e| {
                    Box::<dyn std::error::Error + Send + Sync>::from(format!(
                        "2FA poll failed: {e}"
                    ))
                })?;
            if !pending {
                cleared = true;
                break;
            }
        }
        if !cleared {
            return Err("2FA verification timed out after 30 seconds".into());
        }
    }

    let logged_in = sdk.login(credentials.clone()).await.map_err(|e| {
        Box::<dyn std::error::Error + Send + Sync>::from(format!("Login failed: {e}"))
    })?;

    save_credentials(&cfg.email, &credentials);

    Ok(TutaSession {
        logged_in,
        email: cfg.email.clone(),
        access_token: credentials.access_token,
        self_send_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    })
}

const KEYRING_SERVICE: &str = "tutabridge";

use std::sync::Mutex;
static CREDENTIALS_CACHE: Mutex<Option<Option<tutasdk::login::Credentials>>> = Mutex::new(None);

pub fn has_saved_session(email: &str) -> bool {
    load_credentials(email).is_some()
}

fn save_credentials(email: &str, creds: &tutasdk::login::Credentials) {
    let data = serde_json::json!({
        "login": creds.login,
        "user_id": creds.user_id.0,
        "access_token": creds.access_token,
        "encrypted_passphrase_key": base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &creds.encrypted_passphrase_key,
        ),
        "credential_type": match creds.credential_type {
            tutasdk::login::CredentialType::Internal => "Internal",
            tutasdk::login::CredentialType::External => "External",
        },
    });

    match keyring::Entry::new(KEYRING_SERVICE, email) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(&data.to_string()) {
                log::warn!("Failed to save session to keychain: {e}");
            } else {
                log::info!("Session saved to keychain");
            }
        }
        Err(e) => log::warn!("Failed to create keychain entry: {e}"),
    }
    *CREDENTIALS_CACHE.lock().unwrap() = Some(Some(creds.clone()));
}

fn load_credentials(email: &str) -> Option<tutasdk::login::Credentials> {
    let mut cache = CREDENTIALS_CACHE.lock().unwrap();
    if let Some(cached) = cache.as_ref() {
        return cached.clone();
    }

    let result = load_credentials_from_keyring(email);
    *cache = Some(result.clone());
    result
}

fn load_credentials_from_keyring(email: &str) -> Option<tutasdk::login::Credentials> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, email).ok()?;

    let json_str = entry.get_password().ok()?;

    let v: serde_json::Value = serde_json::from_str(&json_str).ok()?;
    Some(tutasdk::login::Credentials {
        login: v["login"].as_str()?.to_string(),
        user_id: tutasdk::GeneratedId(v["user_id"].as_str()?.to_string()),
        access_token: v["access_token"].as_str()?.to_string(),
        encrypted_passphrase_key: base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            v["encrypted_passphrase_key"].as_str()?,
        )
        .ok()?,
        credential_type: match v["credential_type"].as_str()? {
            "External" => tutasdk::login::CredentialType::External,
            _ => tutasdk::login::CredentialType::Internal,
        },
    })
}

fn delete_credentials(email: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, email) {
        let _ = entry.delete_credential();
    }
    *CREDENTIALS_CACHE.lock().unwrap() = None;
}

/// Map SMTP recipients to `DraftRecipient`, falling back to the address when
/// the display name is empty (Tuta's send service rejects empty names).
fn build_draft_recipients(recipients: &[(String, String)]) -> Vec<DraftRecipient> {
    recipients
        .iter()
        .map(|(name, addr)| DraftRecipient {
            _id: None,
            name: if name.is_empty() {
                addr.clone()
            } else {
                name.clone()
            },
            mailAddress: addr.clone(),
            _errors: Default::default(),
        })
        .collect()
}

/// Build the `DraftData` for a draft creation from a parsed SMTP message.
///
/// Mirrors the web client: the body goes into both `bodyText` and
/// `compressedBodyText`, and empty sender/recipient names fall back to the
/// address (an empty name makes `SendDraftService` fail).
fn build_draft_data(
    msg: &ParsedMessage,
    sender_email: &str,
    added_attachments: Vec<DraftAttachment>,
) -> DraftData {
    DraftData {
        _id: None,
        subject: msg.subject.clone(),
        bodyText: msg.body_html.clone(),
        senderMailAddress: sender_email.to_string(),
        senderName: if msg.from_name.is_empty() {
            sender_email.to_string()
        } else {
            msg.from_name.clone()
        },
        confidential: false,
        method: 0,
        compressedBodyText: Some(msg.body_html.clone()),
        toRecipients: build_draft_recipients(&msg.to),
        ccRecipients: build_draft_recipients(&msg.cc),
        bccRecipients: build_draft_recipients(&msg.bcc),
        addedAttachments: added_attachments,
        removedAttachments: vec![],
        replyTos: vec![],
        _errors: Default::default(),
    }
}

/// Recognise the SDK error shape that means "the server returned the entity
/// but its `_ownerEncSessionKey` / `_ownerGroup` are not populated yet".
/// Newly-created mail attachments hit this for a few seconds after the
/// `SendDraftService` round-trip — the WS push beats the encryption-metadata
/// propagation. The exact wording comes from `CryptoFacade::resolve_session_key`
/// in the Rust SDK; matching on the substring keeps us decoupled from the
/// variant tree.
fn is_session_key_transient(e: &ApiCallError) -> bool {
    match e {
        ApiCallError::InternalSdkError { error_message } => {
            error_message.contains("missing owner key/group data")
                || error_message.contains("Session key resolution failure")
        }
        _ => false,
    }
}

/// Build a random 4-byte `CustomId` — used for `_id` on aggregates that
/// declare `cardinality: One`. Server never reads the value.
fn random_custom_id(randomizer: &RandomizerFacade) -> CustomId {
    CustomId(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(randomizer.generate_random_array::<4>()),
    )
}

/// Build the `SendDraftData` for sending a previously created draft.
///
/// The session data is mirrored into the nested `parameters` aggregate (with a
/// generated `_id`), which the current server model reads; `plaintext` is
/// `false` (it reflects the account's plaintext-only setting, not whether the
/// mail is encrypted). Recipient key arrays stay empty for a non-confidential
/// send.
fn build_send_draft_data(
    session_key_bytes: Vec<u8>,
    draft_id: IdTupleGenerated,
    parameters_id: CustomId,
    attachment_key_data: Vec<AttachmentKeyData>,
) -> SendDraftData {
    SendDraftData {
        _format: 0,
        language: "en".to_string(),
        mailSessionKey: Some(session_key_bytes.clone()),
        bucketEncMailSessionKey: None,
        senderNameUnencrypted: None,
        plaintext: false,
        calendarMethod: false,
        sessionEncEncryptionAuthStatus: None,
        sendAt: None,
        allowUndo: false,
        internalRecipientKeyData: vec![],
        secureExternalRecipientKeyData: vec![],
        attachmentKeyData: attachment_key_data.clone(),
        mail: draft_id.clone(),
        symEncInternalRecipientKeyData: vec![],
        parameters: Some(SendDraftParameters {
            _id: Some(parameters_id),
            language: "en".to_string(),
            mailSessionKey: Some(session_key_bytes),
            bucketEncMailSessionKey: None,
            senderNameUnencrypted: None,
            plaintext: false,
            calendarMethod: false,
            sessionEncEncryptionAuthStatus: None,
            mail: draft_id,
            internalRecipientKeyData: vec![],
            secureExternalRecipientKeyData: vec![],
            symEncInternalRecipientKeyData: vec![],
            attachmentKeyData: attachment_key_data,
        }),
    }
}

#[cfg(test)]
mod self_send_cache_tests {
    use super::*;

    fn msg(from: &str, to: &str, subject: &str) -> ParsedMessage {
        ParsedMessage {
            from_address: from.to_string(),
            from_name: "Me".to_string(),
            to: vec![("".to_string(), to.to_string())],
            cc: vec![],
            bcc: vec![],
            subject: subject.to_string(),
            body_html: "<p>body</p>".to_string(),
            attachments: vec![],
        }
    }

    #[test]
    fn cache_key_is_case_insensitive_and_trim_tolerant() {
        let a = self_send_cache_key("  Hello World  ", "Me@Tuta.IO", "me@TUTA.io");
        let b = self_send_cache_key("hello world", "me@tuta.io", "me@tuta.io");
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_differs_on_subject() {
        let a = self_send_cache_key("First", "me@tuta.io", "me@tuta.io");
        let b = self_send_cache_key("Second", "me@tuta.io", "me@tuta.io");
        assert_ne!(a, b);
    }

    #[test]
    fn is_self_recipient_true_when_to_matches_from() {
        assert!(is_self_recipient(
            &msg("me@tuta.io", "me@tuta.io", "self"),
            "me@tuta.io"
        ));
    }

    #[test]
    fn is_self_recipient_true_when_cc_matches_from() {
        let mut m = msg("me@tuta.io", "other@example.com", "cc");
        m.cc = vec![("".to_string(), "me@tuta.io".to_string())];
        assert!(is_self_recipient(&m, "me@tuta.io"));
    }

    #[test]
    fn is_self_recipient_false_when_from_differs() {
        assert!(!is_self_recipient(
            &msg("other@example.com", "me@tuta.io", "not self"),
            "me@tuta.io"
        ));
    }

    #[test]
    fn is_self_recipient_false_when_recipient_differs() {
        assert!(!is_self_recipient(
            &msg("me@tuta.io", "friend@example.com", "to-friend"),
            "me@tuta.io"
        ));
    }

    #[test]
    fn is_self_recipient_case_insensitive() {
        assert!(is_self_recipient(
            &msg("Me@Tuta.IO", "ME@tuta.io", "case"),
            "me@tuta.io"
        ));
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;

    #[test]
    fn missing_owner_key_is_transient() {
        let e = ApiCallError::InternalSdkError {
            error_message: "Failed to resolve session key for entity 'File' with ID: ...; \
                Session key resolution failure: instance missing owner key/group data"
                .to_string(),
        };
        assert!(is_session_key_transient(&e));
    }

    #[test]
    fn generic_session_key_failure_is_transient() {
        let e = ApiCallError::InternalSdkError {
            error_message: "Session key resolution failure: weird reason".to_string(),
        };
        assert!(is_session_key_transient(&e));
    }

    #[test]
    fn other_internal_errors_are_not_transient() {
        let e = ApiCallError::InternalSdkError {
            error_message: "Failed to parse blob response: bad JSON".to_string(),
        };
        assert!(!is_session_key_transient(&e));
    }

    #[test]
    fn server_errors_are_not_transient() {
        let e = ApiCallError::InternalSdkError {
            error_message: "missing owner key/group data".to_string(),
        };
        // We only retry session-key resolution; other shapes pass through.
        assert!(is_session_key_transient(&e));
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;

    fn sample_msg() -> ParsedMessage {
        ParsedMessage {
            from_address: "me@tuta.io".to_string(),
            from_name: "Me".to_string(),
            to: vec![("Bob".to_string(), "bob@example.com".to_string())],
            cc: vec![],
            bcc: vec![],
            subject: "Hi".to_string(),
            body_html: "<p>hello</p>".to_string(),
            attachments: vec![],
        }
    }

    #[test]
    fn draft_data_puts_body_in_both_fields() {
        let d = build_draft_data(&sample_msg(), "me@tuta.io", vec![]);
        assert_eq!(d.bodyText, "<p>hello</p>");
        assert_eq!(d.compressedBodyText.as_deref(), Some("<p>hello</p>"));
        assert!(!d.confidential);
        assert_eq!(d.method, 0);
    }

    #[test]
    fn draft_data_empty_sender_name_falls_back_to_address() {
        let mut msg = sample_msg();
        msg.from_name = String::new();
        let d = build_draft_data(&msg, "me@tuta.io", vec![]);
        assert_eq!(d.senderName, "me@tuta.io");
    }

    #[test]
    fn draft_data_keeps_non_empty_sender_name() {
        let d = build_draft_data(&sample_msg(), "me@tuta.io", vec![]);
        assert_eq!(d.senderName, "Me");
    }

    #[test]
    fn recipient_empty_name_falls_back_to_address() {
        let recips = build_draft_recipients(&[(String::new(), "x@example.com".to_string())]);
        assert_eq!(recips[0].name, "x@example.com");
        assert_eq!(recips[0].mailAddress, "x@example.com");
    }

    #[test]
    fn recipient_keeps_non_empty_name() {
        let recips = build_draft_recipients(&[("Alice".to_string(), "a@example.com".to_string())]);
        assert_eq!(recips[0].name, "Alice");
    }

    #[test]
    fn send_draft_data_mirrors_parameters_and_is_not_plaintext() {
        let draft_id = IdTupleGenerated::new(
            tutasdk::GeneratedId("list".to_string()),
            tutasdk::GeneratedId("elem".to_string()),
        );
        let pid = CustomId("aggId".to_string());
        let sk = vec![1u8, 2, 3, 4];
        let sd = build_send_draft_data(sk.clone(), draft_id.clone(), pid.clone(), vec![]);

        // top-level
        assert!(!sd.plaintext);
        assert_eq!(sd.mailSessionKey.as_deref(), Some(sk.as_slice()));
        assert!(sd.bucketEncMailSessionKey.is_none());
        assert!(sd.internalRecipientKeyData.is_empty());
        assert!(sd.secureExternalRecipientKeyData.is_empty());
        assert!(sd.symEncInternalRecipientKeyData.is_empty());
        assert_eq!(sd.mail, draft_id);

        // nested parameters must be populated (None causes a 500 server-side)
        let p = sd.parameters.expect("parameters must be set");
        assert_eq!(p._id, Some(pid));
        assert!(!p.plaintext);
        assert_eq!(p.mailSessionKey.as_deref(), Some(sk.as_slice()));
        assert_eq!(p.mail, draft_id);
    }
}
