use base64::Engine;
use crypto_primitives::aes::{Aes256Key, InitializationVector, AES_256_KEY_SIZE};
use crypto_primitives::blake3::blake3_kdf;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use std::collections::HashMap;
use std::sync::Arc;
use tutabridge_tuta::folder_system::{FolderSystem, MailSetExt, MailSetKind};
use tutabridge_tuta::mail::MailExtensions;
use tutasdk::bindings::file_client::{FileClient, FileClientError};
use tutasdk::bindings::rest_client::RestClient;
use tutasdk::blobs::blob_facade::FileData;
use tutasdk::crypto_entity_client::CryptoEntityClient;
use tutasdk::entities::generated::sys::BlobReferenceTokenWrapper;
use tutasdk::entities::generated::tutanota::{
    AttachmentKeyData, ConversationEntry, DraftAttachment, DraftCreateData, DraftData,
    DraftRecipient, Mail, MailBox, MailDetails, MailDetailsBlob, MailSetEntry, NewDraftAttachment,
    SendDraftData, SendDraftParameters, TutanotaFile,
};
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

/// Client name Tuta shows for the bridge's sessions in the account's session list.
pub const CLIENT_IDENTIFIER: &str = "TutaBridge";

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
    async fn load_attachments(
        &self,
        mail: &Mail,
    ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, tutabridge_tuta::mail::AttachmentError>;
    /// Enumerate all mail folders (system + custom, with hierarchy).
    async fn list_folders(&self) -> Result<Vec<FolderInfo>, String>;
    async fn set_unread_status(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        unread: bool,
    ) -> Result<(), String>;
    async fn trash_mails(&self, mail_ids: Vec<IdTupleGenerated>) -> Result<(), String>;
    /// Delete mails permanently, as Tuta's "Delete permanently" does in Trash
    /// and Spam. Only mails that are in `folder` are deleted.
    async fn delete_mails(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        folder: &FolderInfo,
    ) -> Result<(), String>;
    /// Move mails into the given target folder.
    async fn move_mails(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        target: &FolderInfo,
    ) -> Result<(), String>;
    async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String>;
    /// Save a message as a draft on the server and return its id. A draft is
    /// the only mail Tuta can create that is not sent, which is what makes an
    /// IMAP `APPEND` to Drafts possible.
    async fn create_draft(&self, _msg: &ParsedMessage) -> Result<IdTupleGenerated, String> {
        Err("drafts are not supported by this backend".into())
    }
    /// Element id of a mail recently sent through this bridge, by the
    /// Message-ID its client submitted. Lets IMAP answer the client's "save a
    /// copy to Sent" with the UID of Tuta's own Sent copy.
    async fn sent_mail_element_id(&self, _submitted_message_id: &str) -> Option<String> {
        None
    }
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

/// Tuta `ConversationType` values (see the web client's `ConversationType`
/// enum): NEW=0, REPLY=1, FORWARD=2, UNKNOWN=3. UNKNOWN is unusable here:
/// the send 500s for UNKNOWN with a `previousMessageId` the server can't
/// resolve (established by probing), so unresolvable references send as
/// NEW instead.
const CONVERSATION_TYPE_NEW: i64 = 0;
const CONVERSATION_TYPE_REPLY: i64 = 1;

/// How long a submitted-Message-ID → server-assigned-id mapping is kept.
/// git send-email submits a whole series within seconds; an hour covers
/// pauses and manual resends without unbounded growth.
const SENT_ID_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Soft cap on remembered sent ids — a 200-patch series fits, a
/// long-running bridge stays bounded.
const SENT_ID_CACHE_MAX_ENTRIES: usize = 200;

/// One remembered send: when it happened, the Message-ID the server
/// assigned to the delivered mail (bare, no angle brackets), and the element
/// id of the sent mail, which keeps its draft's id when it moves to Sent.
struct SentIdEntry {
    stored_at: std::time::Instant,
    tuta_message_id: String,
    element_id: String,
}

/// A draft the server saved, plus the keys a later send of it needs.
struct CreatedDraft {
    id: IdTupleGenerated,
    session_key: GenericAesKey,
    attachment_keys: Vec<GenericAesKey>,
}

/// Pass a resolved In-Reply-To through. An unresolved one goes out as a new,
/// unthreaded message, unless `refuse` is set (`strict_threading`, on send).
fn refuse_if_unthreaded(
    in_reply_to: &str,
    resolved: (Option<String>, i64),
    refuse: bool,
) -> Result<(Option<String>, i64), ApiCallError> {
    if resolved.0.is_none() && refuse {
        return Err(ApiCallError::InternalSdkError {
            error_message: format!(
                "In-Reply-To <{in_reply_to}> could not be resolved to a Tuta \
                 conversation, so this reply would be delivered \
                 detached from its thread. Refusing because \
                 strict_threading is set in the config. The referenced \
                 message has to be one this bridge sent recently, \
                 one it served over IMAP, or one whose Message-ID \
                 the index holds."
            ),
        });
    }
    Ok(resolved)
}

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
    /// Submitted `Message-ID` → server-assigned Message-ID, for mails sent
    /// through this bridge. The server replaces the submitted id on
    /// delivery, so a later submission whose `In-Reply-To` references an id
    /// we sent (patch 2..N of a git send-email series) must be threaded
    /// against the id the server actually delivered.
    sent_id_cache: Arc<tokio::sync::Mutex<HashMap<String, SentIdEntry>>>,
    /// Message-IDs of mail this account holds, for threading replies to mail
    /// the bridge did not send. Attached by the bridge after the local store
    /// is open; `None` for sessions without one (the `backup` path), which
    /// simply falls back to the unindexed behaviour.
    message_ids: std::sync::OnceLock<Arc<dyn MessageIdIndex>>,
    /// Refuse a reply whose `In-Reply-To` cannot be resolved instead of
    /// sending it as an unthreaded NEW message (`strict_threading` in the
    /// config). Tuta carries no `In-Reply-To`/`References` of its own: the
    /// server derives them from `previousMessageId`, so an unresolvable
    /// reference does not merely lose a header, it detaches the message
    /// from its thread, which the sender only notices in the archive.
    strict_threading: std::sync::atomic::AtomicBool,
}

/// Lookup of Message-IDs the account holds, implemented by the local store.
///
/// Deliberately one method: the session talks to Tuta and the store owns the
/// disk, and this is the only thing the send path needs from the other side
/// of that line. Keeping it this narrow keeps the backend mockable.
pub trait MessageIdIndex: Send + Sync {
    fn knows_message_id(&self, message_id: &str) -> bool;
}

impl TutaSession {
    /// Give the send path a way to ask whether a Message-ID belongs to mail
    /// this account holds. Called once, after the local store is open.
    pub fn attach_message_id_index(&self, index: Arc<dyn MessageIdIndex>) {
        let _ = self.message_ids.set(index);
    }

    /// Apply the config's `strict_threading` (see the field).
    pub fn set_strict_threading(&self, strict: bool) {
        self.strict_threading
            .store(strict, std::sync::atomic::Ordering::Relaxed);
    }

    fn strict_threading(&self) -> bool {
        self.strict_threading
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn load_mailbox(&self) -> Result<MailBox, ApiCallError> {
        self.logged_in.mail_facade().load_user_mailbox().await
    }

    pub async fn load_folders(&self, mailbox: &MailBox) -> Result<FolderSystem, ApiCallError> {
        MailExtensions::new(self.logged_in.clone())
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
        let derived = zeroize::Zeroizing::new(blake3_kdf(
            &[versioned_key.object.as_bytes()],
            "tutabridge local storage v1",
            AES_256_KEY_SIZE,
        ));
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
        let mail_facade = MailExtensions::new(self.logged_in.clone());
        if mail.mailDetailsDraft.is_some() {
            match mail_facade.load_mail_details_draft(mail).await {
                Ok(details) => Ok(Some(details)),
                // A draft that was just sent or deleted loses its draft
                // details; callers re-read the mail, so this is not a failure.
                Err(
                    e @ ApiCallError::ServerResponseError {
                        source: tutasdk::rest_error::HttpError::NotFoundError,
                    },
                ) => {
                    log::debug!("Draft details gone (sent or deleted): {e}");
                    Err(e)
                }
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
    ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, tutabridge_tuta::mail::AttachmentError> {
        if mail.attachments.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(cached) = self.try_self_send_cache(mail).await {
            if cached.len() == mail.attachments.len() {
                return Ok(cached);
            }
        }
        MailExtensions::new(self.logged_in.clone())
            .load_attachments(mail)
            .await
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
                .encrypt_data(
                    att.filename.as_bytes(),
                    InitializationVector::generate(randomizer),
                )
                .map_err(|e| {
                    ApiCallError::internal(format!("Failed to encrypt attachment name: {e}"))
                })?;
            let enc_mime_type = file_sk
                .encrypt_data(
                    att.mime_type.as_bytes(),
                    InitializationVector::generate(randomizer),
                )
                .map_err(|e| {
                    ApiCallError::internal(format!("Failed to encrypt attachment mime type: {e}"))
                })?;
            let owner_enc_file_sk =
                mail_group_key.encrypt_key(file_sk, InitializationVector::generate(randomizer));

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

    /// Pick the address this submission goes out as, and the mail group
    /// that address belongs to.
    ///
    /// Returns the account's own address whenever `requested` is empty, is
    /// the account address already, or is one this account may not send
    /// as. The group id comes back alongside deliberately: it is the same
    /// call that authorises the address, so a caller cannot end up holding
    /// a group for one address and a sender for another.
    async fn resolve_sender(
        &self,
        requested: &str,
    ) -> Result<(String, tutasdk::GeneratedId), ApiCallError> {
        let facade = self.logged_in.mail_facade();

        let wants_other = !requested.is_empty() && !requested.eq_ignore_ascii_case(&self.email);

        if wants_other {
            match facade.get_group_id_for_mail_address(requested).await {
                Ok(group) => return Ok((requested.to_string(), group)),
                // Not an alias this account may send as. Every other error
                // is a real failure and must not be swallowed -- falling
                // back on a transport error would turn "the network is
                // down" into a silently mis-addressed mail.
                Err(ApiCallError::ServerResponseError {
                    source: tutasdk::rest_error::HttpError::NotFoundError,
                }) => {
                    log::warn!(
                        "sending as {} instead of {}: not an enabled alias on this account",
                        self.email,
                        requested
                    );
                }
                Err(e) => return Err(e),
            }
        }

        let group = facade.get_group_id_for_mail_address(&self.email).await?;
        Ok((self.email.clone(), group))
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
        let CreatedDraft {
            id: draft,
            session_key,
            attachment_keys,
        } = self
            .create_draft_impl(msg, &randomizer, self.strict_threading())
            .await?;

        // To populate `attachmentKeyData[]` we need each File entity's
        // IdTuple — the server allocated those when the draft was created.
        // We load the freshly-saved Mail just to read its `attachments`
        // field (order matches our `addedAttachments`) and zip with the
        // session keys we generated earlier.
        let attachment_key_data = if attachment_keys.is_empty() {
            Vec::new()
        } else {
            self.build_attachment_key_data(&draft, &attachment_keys, &randomizer)
                .await?
        };

        let parameters_id = CustomId(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(randomizer.generate_random_array::<4>()),
        );
        let draft_element_id = draft.element_id.to_string();
        let send_data = build_send_draft_data(
            session_key.as_bytes().to_vec(),
            draft,
            parameters_id,
            attachment_key_data,
            msg.is_plaintext,
        );

        let send_return = self
            .logged_in
            .get_service_executor()
            .post::<SendDraftService>(send_data, ExtraServiceParams::default())
            .await?;

        log::info!("Mail sent, message_id: {}", send_return.messageId);

        // Remember submitted-id → delivered-id so the next patch in a
        // series can thread against what the server actually sent.
        if let Some(submitted_id) = &msg.message_id {
            self.remember_sent_message_id(submitted_id, &send_return.messageId, &draft_element_id)
                .await;
        }
        Ok(())
    }

    /// Create a draft on the server: what a send submits, or what an IMAP
    /// `APPEND` to Drafts saves. Returns the keys a later `SendDraftService`
    /// call needs; a draft that is never sent does not use them.
    async fn create_draft_impl(
        &self,
        msg: &ParsedMessage,
        randomizer: &RandomizerFacade,
        refuse_unthreaded: bool,
    ) -> Result<CreatedDraft, ApiCallError> {
        // Thread replies: a submission carrying In-Reply-To (git send-email
        // series, MUA replies) gets its reference resolved to an id the
        // server can act on. Unresolvable → (None, NEW), today's behavior,
        // unless the caller refuses that (`strict_threading`, on send only).
        // Resolved first, so a refused send uploads no attachments.
        let (previous_message_id, conversation_type) = match &msg.in_reply_to {
            Some(irt) => refuse_if_unthreaded(
                irt,
                self.resolve_previous_message(irt).await,
                refuse_unthreaded,
            )?,
            None => (None, CONVERSATION_TYPE_NEW),
        };

        let session_key: GenericAesKey = Aes256Key::generate(randomizer).into();

        // Send as the address the submission asked for, when the account
        // is entitled to it. `get_group_id_for_mail_address` is not just a
        // lookup -- it walks the group's *enabled* aliases and returns
        // `NotFoundError` for anything else (`mail_facade.rs`), so the
        // account itself is the allowlist and there is no second one here
        // to drift out of step with it.
        //
        // Falling back rather than failing, because a submission whose
        // `From:` this account cannot use is the overwhelmingly common
        // case (every MUA that fills it in from something other than the
        // bridge address) and refusing it would break sending for anyone
        // who never asked for this. The warning is what makes the
        // difference visible: silently substituting the sender is exactly
        // the behaviour that made this hard to diagnose from the outside,
        // where the only symptom is the wrong name in a list archive.
        let (sender_email, mail_group_id) = self.resolve_sender(&msg.from_address).await?;
        let group_key = self
            .logged_in
            .get_current_sym_group_key(&mail_group_id)
            .await?;

        let owner_enc_session_key = group_key
            .object
            .encrypt_key(&session_key, InitializationVector::generate(randomizer));
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
                randomizer,
            )
            .await?;

        // Tuta cannot carry a Reply-To on an outgoing mail (see
        // `build_draft_data`), so say so rather than dropping it silently: the
        // submitter asked for replies to go somewhere and they will not.
        // Skip blank addresses so a malformed header (`Reply-To: <>`) does not
        // produce a warning about nothing.
        let asked: Vec<&str> = msg
            .reply_to
            .iter()
            .map(|(_, addr)| addr.trim())
            .filter(|addr| !addr.is_empty())
            .collect();
        if !asked.is_empty() {
            log::warn!(
                "dropping Reply-To [{}]: Tuta does not support it on outgoing mail, replies will go to {}",
                asked.join(", "),
                sender_email
            );
        }

        let draft_data = build_draft_data(msg, &sender_email, added_attachments);

        let create_data = DraftCreateData {
            _format: 0,
            previousMessageId: previous_message_id,
            conversationType: conversation_type,
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
        Ok(CreatedDraft {
            id: draft_return.draft,
            session_key,
            attachment_keys,
        })
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

    /// Remember which Message-ID the server assigned to a mail we just
    /// sent, keyed by the id the submitter used. Same soft-eviction shape
    /// as the self-send cache.
    async fn remember_sent_message_id(&self, submitted_id: &str, tuta_id: &str, element_id: &str) {
        let mut cache = self.sent_id_cache.lock().await;
        if cache.len() >= SENT_ID_CACHE_MAX_ENTRIES && !cache.contains_key(submitted_id) {
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, v)| v.stored_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            }
        }
        cache.insert(
            submitted_id.to_string(),
            SentIdEntry {
                stored_at: std::time::Instant::now(),
                tuta_message_id: tuta_id.to_string(),
                element_id: element_id.to_string(),
            },
        );
    }

    async fn lookup_sent_message_id(&self, submitted_id: &str) -> Option<String> {
        let cache = self.sent_id_cache.lock().await;
        cache
            .get(submitted_id)
            .filter(|e| e.stored_at.elapsed() <= SENT_ID_CACHE_TTL)
            .map(|e| e.tuta_message_id.clone())
    }

    /// The server-known Message-ID of a mail the bridge served over IMAP,
    /// given the `<list.elem@tutabridge.local>` id it minted for it: load
    /// the Mail, then its ConversationEntry — `messageId` there is what
    /// the official client passes as `previousMessageId` when replying.
    async fn load_conversation_entry_message_id(
        &self,
        list_id: &str,
        element_id: &str,
    ) -> Result<Option<String>, ApiCallError> {
        let Some(mail) = self.load_mail_by_id(list_id, element_id).await? else {
            return Ok(None);
        };
        match self
            .crypto_client()
            .load::<ConversationEntry, _>(&mail.conversationEntry)
            .await
        {
            Ok(ce) => Ok(Some(ce.messageId)),
            Err(ApiCallError::ServerResponseError {
                source: tutasdk::rest_error::HttpError::NotFoundError,
            }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Resolve a submission's `In-Reply-To` into `(previousMessageId,
    /// conversationType)` for draft creation. Every failure degrades to
    /// `(None, NEW)` — the pre-threading behavior — so a send can never
    /// fail because of this.
    async fn resolve_previous_message(&self, in_reply_to: &str) -> (Option<String>, i64) {
        // A mail this bridge sent: the submitter's id was replaced on
        // delivery, thread against the server-assigned one.
        if let Some(tuta_id) = self.lookup_sent_message_id(in_reply_to).await {
            return (Some(tuta_id), CONVERSATION_TYPE_REPLY);
        }
        // A mail this bridge served over IMAP under its own minted id:
        // resolve the real id via the conversation entry.
        if let Some((list_id, element_id)) = parse_bridge_local_id(in_reply_to) {
            return match self
                .load_conversation_entry_message_id(&list_id, &element_id)
                .await
            {
                Ok(Some(mid)) => (Some(mid), CONVERSATION_TYPE_REPLY),
                // Resolution failed — don't leak the synthetic local id
                // upstream, it references nothing outside this bridge.
                _ => (None, CONVERSATION_TYPE_NEW),
            };
        }
        // Mail this account holds: its Message-ID is exactly what the server
        // stores in ConversationEntry.messageId, so it can be passed straight
        // through. This is what threads a reply to someone else's mail, which
        // neither branch above can do.
        match self.message_ids.get() {
            Some(index) if index.knows_message_id(in_reply_to) => {
                return (Some(in_reply_to.to_string()), CONVERSATION_TYPE_REPLY);
            }
            Some(_) => {}
            // Nothing attached the index. Every foreign reply then falls
            // through to the unthreaded branch below, which looks exactly
            // like an id the mailbox does not hold — so say which it is,
            // or the wiring bug is invisible from the outside.
            None => log::warn!(
                "No Message-ID index is attached to this session; \
                 replies to mail this bridge did not send cannot be threaded"
            ),
        }

        // A foreign id the server may not know, and no conversation type
        // can safely carry it (see `CONVERSATION_TYPE_NEW` above), so keep
        // today's unthreaded behavior rather than risk the send.
        (None, CONVERSATION_TYPE_NEW)
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
        log::debug!(
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

/// Split a Message-ID the bridge minted for IMAP —
/// `<list>.<element>@tutabridge.local` (see `mail_to_rfc2822`) — back into
/// its id tuple. Returns `None` for any other id. Tuta generated ids never
/// contain `.`, so the first dot is the separator.
fn parse_bridge_local_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_suffix("@tutabridge.local")?;
    let (list, elem) = rest.split_once('.')?;
    if list.is_empty() || elem.is_empty() {
        return None;
    }
    Some((list.to_string(), elem.to_string()))
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
        MailExtensions::new(self.logged_in.clone())
            .decrypt_inline_and_parse::<Mail>(json)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn decrypt_inline_mail_set_entry(
        &self,
        json: &str,
    ) -> Result<Option<MailSetEntry>, String> {
        MailExtensions::new(self.logged_in.clone())
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
        MailExtensions::new(self.logged_in.clone())
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

    async fn load_attachments(
        &self,
        mail: &Mail,
    ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, tutabridge_tuta::mail::AttachmentError> {
        self.load_attachments_impl(mail).await
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
            let kind = folder.kind();

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
                let akind = ancestor.kind();
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

    async fn delete_mails(
        &self,
        mail_ids: Vec<IdTupleGenerated>,
        folder: &FolderInfo,
    ) -> Result<(), String> {
        let folder = IdTupleGenerated::new(
            tutasdk::GeneratedId(folder.list_id.clone()),
            tutasdk::GeneratedId(folder.id.clone()),
        );
        MailExtensions::new(self.logged_in.clone())
            .delete_mails(mail_ids, folder)
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
        MailExtensions::new(self.logged_in.clone())
            .move_mails(mail_ids, target_folder)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String> {
        self.send_mail_impl(msg).await.map_err(|e| format!("{e}"))
    }

    async fn create_draft(&self, msg: &ParsedMessage) -> Result<IdTupleGenerated, String> {
        let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
        // A saved draft is not sent, so strict threading does not apply.
        self.create_draft_impl(msg, &randomizer, false)
            .await
            .map(|draft| draft.id)
            .map_err(|e| format!("{e}"))
    }

    async fn sent_mail_element_id(&self, submitted_message_id: &str) -> Option<String> {
        let cache = self.sent_id_cache.lock().await;
        cache
            .get(submitted_message_id)
            .filter(|e| e.stored_at.elapsed() <= SENT_ID_CACHE_TTL)
            .map(|e| e.element_id.clone())
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
    let store = crate::session_store::SessionStore::new(cfg);
    login_with_2fa(cfg, &store, Some(password), None).await
}

pub async fn login_with_2fa(
    cfg: &Config,
    store: &crate::session_store::SessionStore,
    password: Option<&str>,
    totp_callback: Option<TwoFactorCallback>,
) -> Result<TutaSession, Box<dyn std::error::Error + Send + Sync>> {
    login_with_persistence(
        cfg,
        store,
        password,
        totp_callback,
        PersistencePolicy::AllowVolatile,
    )
    .await
}

/// Initialization must fail when fresh credentials cannot be saved. Desktop
/// logins may keep working in memory, with a visible persistence warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistencePolicy {
    AllowVolatile,
    Required,
    /// Container resume/init keeps rejected bytes until a successful new login.
    PreserveExistingRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    #[error("Saved session rejected; explicit initialization is required")]
    SessionInvalid,
    #[error("Tuta rejects this version; update the image")]
    ClientRejected,
    #[error("Tuta temporarily unavailable; saved session retained")]
    Transient,
}

fn accept_session_save(
    policy: PersistencePolicy,
    result: Result<(), crate::session_store::SessionStoreError>,
) -> Result<(), crate::session_store::SessionStoreError> {
    match (policy, result) {
        (PersistencePolicy::Required | PersistencePolicy::PreserveExistingRequired, result) => {
            result
        }
        (_, Err(error)) => {
            log::warn!("Session is available only in memory: {error}");
            Ok(())
        }
        (_, Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod persistence_policy_tests {
    use super::*;

    #[test]
    fn initialization_reports_a_save_failure_but_desktop_can_keep_the_session() {
        let failure = || crate::session_store::SessionStoreError::Write {
            backend: "pass",
            message: "encryption failed".into(),
        };
        assert!(accept_session_save(PersistencePolicy::Required, Err(failure())).is_err());
        assert!(
            accept_session_save(PersistencePolicy::PreserveExistingRequired, Err(failure()))
                .is_err()
        );
        assert!(accept_session_save(PersistencePolicy::AllowVolatile, Err(failure())).is_ok());
        for policy in [
            PersistencePolicy::Required,
            PersistencePolicy::AllowVolatile,
        ] {
            assert!(accept_session_save(policy, Ok(())).is_ok());
        }
    }
}

/// Required persistence also rejects a cached session whose earlier save
/// failed, so reusing an interactive store cannot bypass initialization checks.
pub async fn login_with_persistence(
    cfg: &Config,
    store: &crate::session_store::SessionStore,
    password: Option<&str>,
    totp_callback: Option<TwoFactorCallback>,
    persistence: PersistencePolicy,
) -> Result<TutaSession, Box<dyn std::error::Error + Send + Sync>> {
    if !store.matches_config(cfg) {
        return Err("Session store does not match the configured account and backend".into());
    }
    // Only an explicit password login may continue without persistence when
    // the OS keychain cannot be accessed. Automatic resume remains strict.
    let credentials = if persistence != PersistencePolicy::AllowVolatile {
        store.load_persisted(password.is_some()).await?
    } else if password.is_some() {
        store.load_interactive().await?
    } else {
        store.load().await?
    };
    let raw_client: Arc<dyn RestClient> =
        Arc::new(tutasdk::net::native_rest_client::NativeRestClient::try_new()?);
    // Route every SDK request through the rate governor (bounded concurrency +
    // 429/503 backoff), replacing the SDK's header-only suspension layer so
    // there is exactly one authoritative choke point for outbound API traffic.
    let rest_client: Arc<dyn RestClient> =
        Arc::new(crate::governed_client::GovernedRestClient::new(raw_client));
    let file_client: Arc<dyn FileClient> = Arc::new(DiskFileClient::new());
    let sdk = Sdk::new_without_suspension(cfg.api_url.clone(), rest_client, file_client);

    if let Some(credentials) = credentials {
        if persistence == PersistencePolicy::PreserveExistingRequired {
            log::debug!("Resuming saved session...");
        } else {
            log::info!("Resuming saved session...");
        }
        let access_token = credentials.access_token.clone();
        match sdk.login(credentials).await {
            Ok(logged_in) => {
                return Ok(TutaSession {
                    logged_in,
                    email: cfg.email.clone(),
                    access_token,
                    self_send_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                    sent_id_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                    message_ids: std::sync::OnceLock::new(),
                    strict_threading: std::sync::atomic::AtomicBool::new(false),
                });
            }
            Err(e) => match classify_resume_failure(&e) {
                ResumeFailure::SessionInvalid
                    if persistence == PersistencePolicy::PreserveExistingRequired =>
                {
                    if password.is_none() {
                        return Err(ResumeError::SessionInvalid.into());
                    }
                    // Explicit reauthentication may replace it only after success.
                }
                ResumeFailure::SessionInvalid => {
                    log::warn!("Saved session is no longer valid, re-authenticating: {e}");
                    if let Err(error) = store.delete().await {
                        log::warn!("Could not remove the stale session: {error}");
                    }
                }
                ResumeFailure::ClientRejected => {
                    if persistence == PersistencePolicy::PreserveExistingRequired {
                        return Err(ResumeError::ClientRejected.into());
                    }
                    return Err(format!(
                        "Tuta rejects this version of the bridge ({e}); update the bridge. \
                         The saved session was kept."
                    )
                    .into());
                }
                ResumeFailure::Transient => {
                    if persistence == PersistencePolicy::PreserveExistingRequired {
                        return Err(ResumeError::Transient.into());
                    }
                    return Err(format!(
                        "Could not resume the saved session ({e}); it was kept, try again later."
                    )
                    .into());
                }
            },
        }
    }

    let password = password.ok_or("No saved session and no password provided")?;

    log::info!("Authenticating with Tuta servers...");
    let session = sdk
        .initiate_session(&cfg.email, password, CLIENT_IDENTIFIER)
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

    let saved = if persistence == PersistencePolicy::PreserveExistingRequired {
        store.save_preserving(&credentials).await
    } else {
        store.save(&credentials).await
    };
    accept_session_save(persistence, saved)?;

    Ok(TutaSession {
        logged_in,
        email: cfg.email.clone(),
        access_token: credentials.access_token,
        self_send_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        sent_id_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        message_ids: std::sync::OnceLock::new(),
        strict_threading: std::sync::atomic::AtomicBool::new(false),
    })
}

/// What a failed resume of a saved session says about that session.
#[derive(Debug, PartialEq, Eq)]
enum ResumeFailure {
    /// Tuta no longer accepts the token or the account behind it: forget the
    /// session and log in again.
    SessionInvalid,
    /// Tuta refuses this client version (HTTP 474) before looking at the
    /// token. The session may well be fine; only an update can help.
    ClientRejected,
    /// Network, server or anything unexpected: the session is not the problem.
    Transient,
}

/// Decide whether a failed resume means the saved session is dead. Only a
/// verdict on the token or the account is; a 474 or an outage says nothing
/// about the session, and forgetting it on those made every Tuta hiccup a
/// password-and-TOTP prompt.
fn classify_resume_failure(e: &tutasdk::login::LoginError) -> ResumeFailure {
    use tutasdk::login::LoginError;
    use tutasdk::rest_error::HttpError;
    use tutasdk::ApiCallError;
    match e {
        // The stored token or key cannot be used at all.
        LoginError::InvalidAccessToken { .. }
        | LoginError::InvalidPassphrase { .. }
        | LoginError::InvalidKey { .. } => ResumeFailure::SessionInvalid,
        LoginError::ApiCall {
            source: ApiCallError::ServerResponseError { source },
        } => match source {
            HttpError::NotAuthenticatedError
            | HttpError::SessionExpiredError
            | HttpError::AccessExpiredError
            | HttpError::AccessDeactivatedError
            | HttpError::AccessBlockedError => ResumeFailure::SessionInvalid,
            HttpError::InvalidSoftwareVersionError => ResumeFailure::ClientRejected,
            _ => ResumeFailure::Transient,
        },
        LoginError::ApiCall { .. } => ResumeFailure::Transient,
    }
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
///
/// A plaintext submission is stored as the reversible encoding of its raw
/// SMTP bytes, never the `<pre>` rendering — see [`plaintext_to_tuta_html`].
fn build_draft_data(
    msg: &ParsedMessage,
    sender_email: &str,
    added_attachments: Vec<DraftAttachment>,
) -> DraftData {
    let body = match (msg.is_plaintext, &msg.body_text) {
        (true, Some(text)) => plaintext_to_tuta_html(text),
        _ => msg.body_html.clone(),
    };
    DraftData {
        _id: None,
        subject: msg.subject.clone(),
        bodyText: body.clone(),
        senderMailAddress: sender_email.to_string(),
        senderName: if msg.from_name.is_empty() {
            sender_email.to_string()
        } else {
            msg.from_name.clone()
        },
        confidential: false,
        method: 0,
        compressedBodyText: Some(body),
        toRecipients: build_draft_recipients(&msg.to),
        ccRecipients: build_draft_recipients(&msg.cc),
        bccRecipients: build_draft_recipients(&msg.bcc),
        addedAttachments: added_attachments,
        removedAttachments: vec![],
        // Always empty: the server drops this field. Probing a live account
        // showed a submitted `replyTos` comes back empty on the stored mail,
        // no `Reply-To:` header reaches an external recipient, and the
        // official client's Reply button falls through to the sender. See
        // `send_mail_impl`, which warns instead of pretending it worked.
        replyTos: vec![],
        _errors: Default::default(),
    }
}

/// Encode a raw text/plain body for Tuta draft storage: escape `&` `<` `>`,
/// then one `<br>` per newline (any convention). Nothing else — no `<pre>`,
/// no wrapping.
///
/// Why this exact shape (the server is closed source; established by
/// loopback probes against it):
/// - Tuta stores `bodyText` verbatim, so what we write is what every later
///   consumer reads.
/// - With `SendDraftData.plaintext` set, the server derives the outbound
///   text/plain by a lossy HTML→text conversion: it strips tags, decodes
///   entities and maps `<br>`/block structure to newlines (it must — that
///   is how plaintext-only accounts' editor HTML goes out readable). So
///   `<pre>`-wrapped content loses all line structure, and fully-raw text
///   loses `</dev/null`-style spans to the tag stripper. Escape+`<br>` is
///   the unique fixed point: strip+decode is exactly its inverse.
/// - The bridge's own IMAP rendering (`html_to_text`) inverts it the same
///   way, so the Sent-folder text/plain part reproduces the submission.
pub(crate) fn plaintext_to_tuta_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace("\r\n", "<br>")
        .replace('\n', "<br>")
        .replace('\r', "<br>")
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
/// generated `_id`), which the current server model reads. `plaintext` asks
/// the server to generate the outbound external mail as text/plain instead of
/// HTML — set per-send from the submission's MIME type (the official client
/// feeds it the account-wide `sendPlaintextOnly` setting instead). Recipient
/// key arrays stay empty for a non-confidential send.
fn build_send_draft_data(
    session_key_bytes: Vec<u8>,
    draft_id: IdTupleGenerated,
    parameters_id: CustomId,
    attachment_key_data: Vec<AttachmentKeyData>,
    plaintext: bool,
) -> SendDraftData {
    SendDraftData {
        _format: 0,
        language: "en".to_string(),
        mailSessionKey: Some(session_key_bytes.clone()),
        bucketEncMailSessionKey: None,
        senderNameUnencrypted: None,
        plaintext,
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
            plaintext,
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
mod resume_failure_tests {
    use super::{classify_resume_failure, ResumeFailure};
    use tutasdk::bindings::rest_client::RestClientError;
    use tutasdk::login::LoginError;
    use tutasdk::rest_error::HttpError;
    use tutasdk::ApiCallError;

    fn server(source: HttpError) -> LoginError {
        LoginError::ApiCall {
            source: ApiCallError::ServerResponseError { source },
        }
    }

    #[test]
    fn a_verdict_on_the_token_or_account_invalidates_the_session() {
        for e in [
            server(HttpError::NotAuthenticatedError),
            server(HttpError::SessionExpiredError),
            server(HttpError::AccessExpiredError),
            server(HttpError::AccessDeactivatedError),
            server(HttpError::AccessBlockedError),
            LoginError::InvalidAccessToken {
                error_message: "bad token".into(),
            },
            LoginError::InvalidPassphrase {
                error_message: "changed".into(),
            },
            LoginError::InvalidKey {
                error_message: "changed".into(),
            },
        ] {
            assert_eq!(
                classify_resume_failure(&e),
                ResumeFailure::SessionInvalid,
                "{e}"
            );
        }
    }

    #[test]
    fn a_refused_client_version_keeps_the_session() {
        // The 474 that took every user's saved session with it: Tuta never
        // looked at the token, so the token is not what is wrong.
        assert_eq!(
            classify_resume_failure(&server(HttpError::InvalidSoftwareVersionError)),
            ResumeFailure::ClientRejected
        );
    }

    #[test]
    fn an_outage_or_an_unexpected_error_keeps_the_session() {
        for e in [
            server(HttpError::ServiceUnavailableError {
                suspension_time_sec: None,
            }),
            server(HttpError::InternalServerError),
            server(HttpError::BadGatewayError),
            server(HttpError::NotAuthorizedError),
            LoginError::ApiCall {
                source: ApiCallError::RestClient {
                    source: RestClientError::NetworkError,
                },
            },
            LoginError::ApiCall {
                source: ApiCallError::InternalSdkError {
                    error_message: "InvalidDataSizeError".into(),
                },
            },
        ] {
            assert_eq!(classify_resume_failure(&e), ResumeFailure::Transient, "{e}");
        }
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
            reply_to: vec![],
            subject: subject.to_string(),
            body_html: "<p>body</p>".to_string(),
            body_text: None,
            message_id: None,
            in_reply_to: None,
            is_plaintext: false,
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
mod send_tests {
    use super::*;

    fn sample_msg() -> ParsedMessage {
        ParsedMessage {
            from_address: "me@tuta.io".to_string(),
            from_name: "Me".to_string(),
            to: vec![("Bob".to_string(), "bob@example.com".to_string())],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            subject: "Hi".to_string(),
            body_html: "<p>hello</p>".to_string(),
            body_text: None,
            message_id: None,
            in_reply_to: None,
            is_plaintext: false,
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
    fn draft_data_encodes_plaintext_submissions_reversibly() {
        // Neither the <pre> rendering nor the raw bytes may reach the
        // server: the outbound converter collapses <pre> newlines and eats
        // raw `</dev/null ... <tag>` spans as markup. Only escape+<br>
        // survives strip+decode.
        let mut msg = sample_msg();
        msg.is_plaintext = true;
        msg.body_text = Some("read </dev/null\r\na && b\n".to_string());
        msg.body_html = "<pre>ignored</pre>".to_string();
        let d = build_draft_data(&msg, "me@tuta.io", vec![]);
        assert_eq!(d.bodyText, "read &lt;/dev/null<br>a &amp;&amp; b<br>");
        assert_eq!(d.compressedBodyText.as_deref(), Some(d.bodyText.as_str()));
    }

    #[test]
    fn bridge_local_id_parses_and_rejects() {
        assert_eq!(
            parse_bridge_local_id("Lst-1.Elem_2@tutabridge.local"),
            Some(("Lst-1".to_string(), "Elem_2".to_string()))
        );
        assert_eq!(parse_bridge_local_id("20260802.1234@scarrone.co"), None);
        assert_eq!(parse_bridge_local_id("no-dot@tutabridge.local"), None);
        assert_eq!(parse_bridge_local_id(".x@tutabridge.local"), None);
    }

    #[test]
    fn plaintext_encoding_round_trips_through_html_to_text() {
        // The bridge's own receive-side conversion must invert the storage
        // encoding exactly (modulo CRLF→LF, which the encoding folds).
        let patch = "diff --git a/x.c b/x.c\n-if (a < b && c > d)\n+while (p != NULL)\n\t\"quoted\"  two  spaces\n";
        let stored = plaintext_to_tuta_html(patch);
        assert_eq!(crate::mail::rfc2822::html_to_text(&stored), patch);
    }

    #[test]
    fn draft_data_never_sends_reply_tos() {
        // Deliberate: the server drops `DraftData.replyTos` (a submitted value
        // comes back empty on the stored mail, no `Reply-To:` header reaches an
        // external recipient, and the official client's Reply button falls
        // through to the sender). Filling it in only creates the illusion of a
        // working feature, so the parsed Reply-To is warned about instead. This
        // guards against re-adding the plumbing.
        let mut msg = sample_msg();
        msg.reply_to = vec![
            ("Paul".to_string(), "paul@scarrone.co".to_string()),
            (String::new(), "other@example.com".to_string()),
        ];
        let d = build_draft_data(&msg, "me@tuta.io", vec![]);
        assert!(
            d.replyTos.is_empty(),
            "replyTos must stay empty; the server ignores it"
        );
    }

    #[test]
    fn draft_data_sender_is_the_resolved_address_not_the_from() {
        // `build_draft_data` takes the sender its caller resolved, which is
        // the account address whenever the requested one was refused. The
        // submission's own `From:` must not leak past that decision.
        let mut msg = sample_msg();
        msg.from_address = "paul@scarrone.co".to_string();
        let d = build_draft_data(&msg, "me@tuta.io", vec![]);
        assert_eq!(d.senderMailAddress, "me@tuta.io");
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
        let sd = build_send_draft_data(sk.clone(), draft_id.clone(), pid.clone(), vec![], false);

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

    #[test]
    fn send_draft_data_plaintext_flag_set_in_both_places() {
        let draft_id = IdTupleGenerated::new(
            tutasdk::GeneratedId("list".to_string()),
            tutasdk::GeneratedId("elem".to_string()),
        );
        let pid = CustomId("aggId".to_string());
        let sd = build_send_draft_data(vec![1u8], draft_id, pid, vec![], true);
        assert!(sd.plaintext);
        assert!(sd.parameters.expect("parameters must be set").plaintext);
    }

    #[test]
    fn an_unresolved_reply_is_refused_only_when_asked() {
        let unresolved = (None, CONVERSATION_TYPE_NEW);
        // Send with strict_threading: refused, before anything is uploaded.
        assert!(refuse_if_unthreaded("x@example.org", unresolved.clone(), true).is_err());
        // Saving a draft, or strict_threading off: goes out unthreaded.
        assert_eq!(
            refuse_if_unthreaded("x@example.org", unresolved.clone(), false).unwrap(),
            unresolved
        );
    }

    #[test]
    fn a_resolved_reply_passes_through_either_way() {
        let resolved = (Some("x@example.org".to_string()), CONVERSATION_TYPE_REPLY);
        for refuse in [true, false] {
            assert_eq!(
                refuse_if_unthreaded("x@example.org", resolved.clone(), refuse).unwrap(),
                resolved
            );
        }
    }
}
