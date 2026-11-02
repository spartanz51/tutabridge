//! Mail operations the bridge needs on top of the SDK: the folder tree, moves
//! to any folder, the bodies of received mails and drafts, attachments, and
//! entities that arrive inline in event bus messages.
use crate::folder_system::FolderSystem;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tutasdk::crypto::crypto_facade::{BucketKeyError, ResolvedSessionKey};
use tutasdk::entities::generated::tutanota::{
    DeleteMailData, Mail, MailBox, MailDetails, MailDetailsBlob, MailDetailsDraft, MailSet,
    MoveMailData, TutanotaFile,
};
use tutasdk::entities::{
    entity_facade::{
        has_kdf_nonce, has_owner_enc_session_key, list_element_id, owner_group,
        EntityDecryptionRequirements, EntityFacade, EntityFacadeImpl,
    },
    Entity,
};
use tutasdk::services::generated::tutanota::{MailService, MoveMailService};
use tutasdk::tutanota_constants::ArchiveDataType;
use tutasdk::{ApiCallError, GeneratedId, IdTupleGenerated, ListLoadDirection, LoggedInSdk};

/// Maximum number of mails moved or deleted per request. Larger batches
/// are rejected with HTTP Bad Request.
const MAIL_BATCH_LIMIT: usize = 50;

/// Where a mail body is stored.
enum BodySource<'a> {
    /// A `MailDetailsBlob` in a blob archive (received mail).
    Blob(&'a IdTupleGenerated),
    /// A `MailDetailsDraft` list element (draft).
    Draft(&'a IdTupleGenerated),
}

/// Why a mail's attachments could not be loaded. The bridge decides what to
/// do on the class, never on the text.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    /// A deterministic refusal: retrying cannot help.
    #[error("{0}")]
    Contract(#[from] ContractError),
    /// Transport or server availability, as the SDK typed it; the bridge may
    /// retry. The cause stays typed inside; the message is a fixed class.
    #[error("the attachment could not be fetched from the server")]
    Transport(ApiCallError),
    /// A failure the SDK reduced to text (key loading, session keys,
    /// decryption, internal errors): no claim about permanence is made, and
    /// the text, which may name instances, is not the message.
    #[error("the attachment could not be read")]
    Indeterminate(ApiCallError),
}

/// The deterministic refusals of `MailExtensions::load_attachments`. The
/// messages name the case, never an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("a loaded File is not one of the mail's attachments")]
    NotInMailAttachments,
    #[error("the server did not return every requested File")]
    IncompleteBatch,
    #[error("a File was returned or referenced twice")]
    DuplicateInBatch,
    #[error("the server returned a File from another list")]
    WrongList,
    #[error("the File has no key of its own and the mail carries no bucket")]
    KeyUnavailable,
    #[error("the mail's bucket has no entry for the File")]
    NoBucketEntry,
    #[error("the mail's bucket has more than one entry for the File")]
    AmbiguousBucketEntries,
    #[error("the mail's bucket uses a group reference, which is not supported")]
    GroupReferenceUnsupported,
    #[error("the File and the mail belong to different groups; the bucket path supports one")]
    UnsupportedGroupContext,
}

/// Transport errors keep their SDK type; everything the SDK reduced to text
/// is indeterminate.
fn classify(error: ApiCallError) -> AttachmentError {
    match error {
        ApiCallError::RestClient { .. } | ApiCallError::ServerResponseError { .. } => {
            AttachmentError::Transport(error)
        }
        ApiCallError::InternalSdkError { .. } => AttachmentError::Indeterminate(error),
    }
}

fn classify_bucket(error: BucketKeyError) -> AttachmentError {
    match error {
        BucketKeyError::NoEntry => ContractError::NoBucketEntry.into(),
        BucketKeyError::AmbiguousEntries => ContractError::AmbiguousBucketEntries.into(),
        BucketKeyError::GroupReferenceUnsupported => {
            ContractError::GroupReferenceUnsupported.into()
        }
        BucketKeyError::Resolution(error) => {
            AttachmentError::Indeterminate(ApiCallError::internal(error.to_string()))
        }
    }
}

fn indeterminate(message: &str) -> AttachmentError {
    AttachmentError::Indeterminate(ApiCallError::internal(message.to_owned()))
}

/// `IdTupleGenerated` is not hashable; its parts are.
fn pair(id: &IdTupleGenerated) -> (GeneratedId, GeneratedId) {
    (id.list_id.clone(), id.element_id.clone())
}

/// Mail operations for a logged-in session.
pub struct MailExtensions {
    sdk: Arc<LoggedInSdk>,
    decoder: EntityFacadeImpl,
}

impl MailExtensions {
    pub fn new(sdk: Arc<LoggedInSdk>) -> Self {
        let decoder = EntityFacadeImpl::new(
            sdk.type_model_provider.clone(),
            RandomizerFacade::from_core(rand_core::OsRng),
        );
        Self { sdk, decoder }
    }

    /// Loads the mailbox's folders (at most 100) and builds their tree.
    pub async fn load_folders_for_mailbox(
        &self,
        mailbox: &MailBox,
    ) -> Result<FolderSystem, ApiCallError> {
        let folders: Vec<MailSet> = self
            .sdk
            .mail_facade()
            .get_crypto_entity_client()
            .load_range(
                &mailbox.mailSets.mailSets,
                &GeneratedId::min_id(),
                100,
                ListLoadDirection::ASC,
            )
            .await?;
        Ok(FolderSystem::new(folders))
    }

    /// Moves mails to any folder, system or custom, identified by its
    /// `MailSet` id. Mirrors TS `MailFacade.moveMails`; adjacent duplicates
    /// are sent once.
    pub async fn move_mails(
        &self,
        mut mails: Vec<IdTupleGenerated>,
        target_folder: IdTupleGenerated,
    ) -> Result<(), ApiCallError> {
        mails.dedup();
        for chunk in mails.chunks(MAIL_BATCH_LIMIT) {
            self.sdk
                .get_service_executor()
                .post::<MoveMailService>(
                    MoveMailData {
                        _format: 0,
                        moveReason: None,
                        targetFolder: target_folder.clone(),
                        mails: chunk.to_vec(),
                        excludeMailSet: None,
                    },
                    Default::default(),
                )
                .await?;
        }
        Ok(())
    }

    /// Deletes mails permanently. Mirrors TS `MailFacade.deleteMails`: one
    /// request per mail list, in chunks of the same size as moves. The server
    /// only deletes mails that are in `folder`, so an id that has meanwhile
    /// moved elsewhere is left alone.
    pub async fn delete_mails(
        &self,
        mails: Vec<IdTupleGenerated>,
        folder: IdTupleGenerated,
    ) -> Result<(), ApiCallError> {
        // Keep first-seen list and mail order; hash tables only provide lookups.
        let mut by_list: Vec<Vec<IdTupleGenerated>> = Vec::new();
        let mut list_indices = HashMap::new();
        let mut seen = HashSet::new();
        for mail in mails {
            if !seen.insert(pair(&mail)) {
                continue;
            }
            let index = *list_indices.entry(mail.list_id.clone()).or_insert_with(|| {
                by_list.push(Vec::new());
                by_list.len() - 1
            });
            by_list[index].push(mail);
        }
        for group in by_list {
            for chunk in group.chunks(MAIL_BATCH_LIMIT) {
                self.sdk
                    .get_service_executor()
                    .delete::<MailService>(
                        DeleteMailData {
                            _format: 0,
                            mails: chunk.to_vec(),
                            folder: Some(folder.clone()),
                        },
                        Default::default(),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Resolves the session key of an instance from its owner group and its
    /// `_ownerEncSessionKey`. `owner` names the instance in error messages.
    async fn owner_key(
        &self,
        owner: &str,
        owner_group: Option<&GeneratedId>,
        owner_enc_session_key: Option<&Vec<u8>>,
        owner_key_version: Option<i64>,
    ) -> Result<ResolvedSessionKey, ApiCallError> {
        let owner_group = owner_group
            .ok_or_else(|| ApiCallError::internal(format!("{owner} missing _ownerGroup")))?;
        let encrypted = owner_enc_session_key.ok_or_else(|| {
            ApiCallError::internal(format!("{owner} missing _ownerEncSessionKey"))
        })?;
        let version = owner_key_version.unwrap_or(0).unsigned_abs();
        let client = self.sdk.mail_facade().get_crypto_entity_client();
        let group_key = client
            .get_crypto_facade()
            .get_key_loader_facade()
            .load_sym_group_key(owner_group, version, None)
            .await
            .map_err(|e| ApiCallError::internal(format!("Failed to load group key: {e}")))?;
        let session_key = group_key
            .decrypt_aes_key(encrypted)
            .map_err(|e| ApiCallError::internal(format!("Failed to decrypt session key: {e}")))?;
        Ok(ResolvedSessionKey {
            session_key,
            owner_enc_session_key: encrypted.clone(),
            owner_key_version: version,
            sender_identity_pub_key: None,
        })
    }

    /// Loads a mail body entity and decrypts it with the session key of its
    /// `Mail`, as TS `MailFacade` does. The body's own `_ownerEncSessionKey`
    /// is never used: it may be absent on the wire. A body whose encrypted
    /// values all use group keys needs no session key.
    async fn load_body<T: Entity + DeserializeOwned>(
        &self,
        mail: &Mail,
        source: BodySource<'_>,
    ) -> Result<T, ApiCallError> {
        let type_ref = T::type_ref();
        let client = self.sdk.get_entity_client();
        let parsed = match source {
            BodySource::Blob(id) => {
                let body = self
                    .sdk
                    .blob_facade()
                    .load_blob_element(&type_ref, id)
                    .await?;
                let raw_entities: Vec<_> = serde_json::from_slice(&body)
                    .map_err(|e| ApiCallError::internal(format!("Invalid blob response: {e}")))?;
                let raw = raw_entities
                    .into_iter()
                    .next()
                    .ok_or_else(|| ApiCallError::internal("Empty blob response".into()))?;
                client.parse_raw(&type_ref, raw)?
            }
            BodySource::Draft(id) => client.load(&type_ref, id).await?,
        };
        let model = client.resolve_server_type_ref(&type_ref)?;
        let requirements = EntityDecryptionRequirements::for_entity(
            &model,
            &parsed,
            &self.sdk.type_model_provider,
        )?;
        let key = if requirements.session_key {
            Some(
                self.owner_key(
                    "Mail",
                    mail._ownerGroup.as_ref(),
                    mail._ownerEncSessionKey.as_ref(),
                    mail._ownerKeyVersion,
                )
                .await?,
            )
        } else {
            None
        };
        let decrypted = self
            .sdk
            .mail_facade()
            .get_crypto_entity_client()
            .decrypt_parsed(&type_ref, parsed, key)
            .await?;
        self.sdk
            .instance_mapper
            .parse_entity(decrypted)
            .map_err(|e| ApiCallError::internal(format!("Failed to map {}: {e}", model.name)))
    }

    /// Loads the body of a received mail, stored as a `MailDetailsBlob` in a
    /// blob archive. Mirrors TS `MailFacade.loadMailDetailsBlob`.
    pub async fn load_mail_details_blob(&self, mail: &Mail) -> Result<MailDetails, ApiCallError> {
        if mail.mailDetailsDraft.is_some() {
            return Err(ApiCallError::internal(
                "Expected received mail, not draft".into(),
            ));
        }
        let id = mail
            .mailDetails
            .as_ref()
            .ok_or_else(|| ApiCallError::internal("Mail has no mailDetails ID".into()))?;
        let blob: MailDetailsBlob = self.load_body(mail, BodySource::Blob(id)).await?;
        Ok(blob.details)
    }

    /// Loads the body of a draft, stored as a `MailDetailsDraft` list
    /// element. Mirrors TS `MailFacade.loadMailDetailsDraft`.
    pub async fn load_mail_details_draft(&self, mail: &Mail) -> Result<MailDetails, ApiCallError> {
        if mail.mailDetails.is_some() {
            return Err(ApiCallError::internal(
                "Expected draft, not received mail".into(),
            ));
        }
        let id = mail
            .mailDetailsDraft
            .as_ref()
            .ok_or_else(|| ApiCallError::internal("Mail has no mailDetailsDraft ID".into()))?;
        let draft: MailDetailsDraft = self.load_body(mail, BodySource::Draft(id)).await?;
        Ok(draft.details)
    }

    /// Loads the attachments of `mail`, decrypted and downloaded, as TS
    /// `MailFacade.loadAttachments`: one request per list, the response
    /// checked against the request, then each File decrypted with its own
    /// key when it has one, else with the key the mail's bucket names for it
    /// (`CryptoFacade::resolve_bucket_instance_keys`), else an error. All or
    /// nothing, in the order of `mail.attachments`. Nothing is written to the
    /// server; the keys live for the call. Each returned File carries the
    /// envelope of its blob key, wrapped for the mail's group on the bucket path.
    pub async fn load_attachments(
        &self,
        mail: &Mail,
    ) -> Result<Vec<(TutanotaFile, Vec<u8>)>, AttachmentError> {
        if mail.attachments.is_empty() {
            return Ok(Vec::new());
        }
        let type_ref = TutanotaFile::type_ref();
        let client = self.sdk.get_entity_client();
        let model = client
            .resolve_server_type_ref(&type_ref)
            .map_err(AttachmentError::Indeterminate)?;

        // One request per list; a File referenced twice is refused up front.
        let mut lists: Vec<(GeneratedId, Vec<GeneratedId>)> = Vec::new();
        for id in &mail.attachments {
            match lists.iter_mut().find(|(list, _)| *list == id.list_id) {
                Some((_, elements)) if elements.contains(&id.element_id) => {
                    return Err(ContractError::DuplicateInBatch.into());
                }
                Some((_, elements)) => elements.push(id.element_id.clone()),
                None => lists.push((id.list_id.clone(), vec![id.element_id.clone()])),
            }
        }
        let mut parsed_files = HashMap::with_capacity(mail.attachments.len());
        for (list, elements) in &lists {
            let loaded = client
                .load_multiple(&type_ref, list, elements)
                .await
                .map_err(classify)?;
            for parsed in loaded {
                let id = list_element_id(&model, &parsed)
                    .ok_or_else(|| indeterminate("a loaded File has no id"))?;
                if id.list_id != *list {
                    return Err(ContractError::WrongList.into());
                }
                if !elements.contains(&id.element_id) {
                    return Err(ContractError::NotInMailAttachments.into());
                }
                if parsed_files.insert(pair(&id), parsed).is_some() {
                    return Err(ContractError::DuplicateInBatch.into());
                }
            }
        }
        if mail
            .attachments
            .iter()
            .any(|id| !parsed_files.contains_key(&pair(id)))
        {
            return Err(ContractError::IncompleteBatch.into());
        }

        // Every key before any decryption: a File's own key first, the
        // bucket for the others, in one call.
        let crypto_client = self.sdk.mail_facade().get_crypto_entity_client();
        let crypto = crypto_client.get_crypto_facade();
        let mut keys: HashMap<(GeneratedId, GeneratedId), ResolvedSessionKey> = HashMap::new();
        let mut from_bucket: Vec<IdTupleGenerated> = Vec::new();
        for id in &mail.attachments {
            let parsed = &parsed_files[&pair(id)];
            if has_owner_enc_session_key(&model, parsed) {
                let key = crypto
                    .resolve_session_key(parsed, &model)
                    .await
                    .map_err(|e| indeterminate(&format!("File session key: {e}")))?
                    .ok_or_else(|| indeterminate("File is encrypted but resolved no key"))?;
                keys.insert(pair(id), key);
            } else {
                from_bucket.push(id.clone());
            }
        }
        if !from_bucket.is_empty() {
            let Some(bucket) = mail.bucketKey.as_ref() else {
                return Err(ContractError::KeyUnavailable.into());
            };
            let Some(mail_group) = mail._ownerGroup.as_ref() else {
                return Err(indeterminate("Mail missing _ownerGroup"));
            };
            // The bucket path wraps keys for the mail's group: a File of
            // another group is a profile this fix does not cover.
            if from_bucket.iter().any(|id| {
                owner_group(&model, &parsed_files[&pair(id)]).as_ref() != Some(mail_group)
            }) {
                return Err(ContractError::UnsupportedGroupContext.into());
            }
            let resolved = crypto
                .resolve_bucket_instance_keys(bucket, mail_group, &from_bucket)
                .await
                .map_err(classify_bucket)?;
            keys.extend(from_bucket.iter().map(pair).zip(resolved));
        }

        let mut out = Vec::with_capacity(mail.attachments.len());
        for id in &mail.attachments {
            let parsed = parsed_files
                .remove(&pair(id))
                .expect("every attachment was loaded");
            let key = keys.remove(&pair(id)).expect("every attachment has a key");
            let decrypted = crypto_client
                .decrypt_parsed(&type_ref, parsed, Some(key.clone()))
                .await
                .map_err(classify)?;
            let mut file: TutanotaFile = self
                .sdk
                .instance_mapper
                .parse_entity(decrypted)
                .map_err(|e| indeterminate(&format!("Failed to map File: {e}")))?;
            if !file._errors.is_empty() {
                return Err(indeterminate("File metadata did not decrypt"));
            }
            // Group-key AEAD attributes need no session key in the entity
            // decoder, but the returned File must retain the blob key envelope.
            file._ownerKeyVersion = Some(
                i64::try_from(key.owner_key_version)
                    .map_err(|_| indeterminate("File key version is out of range"))?,
            );
            file._ownerEncSessionKey = Some(key.owner_enc_session_key);
            let data = self
                .load_file_attachment_data(&file, &key.session_key)
                .await
                .map_err(classify)?;
            out.push((file, data));
        }
        Ok(out)
    }

    /// Downloads the blobs of a file attachment, decrypts them with `key`,
    /// the File's session key, and returns the file's bytes. Mirrors TS
    /// `BlobFacade.downloadAndDecrypt` for attachments. `file` is the
    /// decrypted `TutanotaFile` referenced from `Mail::attachments`.
    pub async fn load_file_attachment_data(
        &self,
        file: &TutanotaFile,
        key: &GenericAesKey,
    ) -> Result<Vec<u8>, ApiCallError> {
        if file.blobs.is_empty() {
            return Ok(Vec::new());
        }
        let instance = file
            ._id
            .as_ref()
            .ok_or_else(|| ApiCallError::internal("File has no ID".into()))?;
        let mut archives: HashMap<GeneratedId, Vec<GeneratedId>> = HashMap::new();
        for blob in &file.blobs {
            archives
                .entry(blob.archiveId.clone())
                .or_default()
                .push(blob.blobId.clone());
        }
        let mut chunks = HashMap::new();
        for (archive, ids) in archives {
            let encrypted = self
                .sdk
                .blob_facade()
                .download_blobs(&archive, instance, ArchiveDataType::Attachments, &ids)
                .await?;
            for (id, bytes) in encrypted {
                let data = key.decrypt_data(&bytes).map_err(|e| {
                    ApiCallError::internal(format!("Cannot decrypt attachment blob {id}: {e}"))
                })?;
                chunks.insert((archive.clone(), id), data);
            }
        }
        let mut result = Vec::new();
        for blob in &file.blobs {
            let data = chunks
                .get(&(blob.archiveId.clone(), blob.blobId.clone()))
                .ok_or_else(|| {
                    ApiCallError::internal(format!(
                        "Server did not return blob {} of archive {}",
                        blob.blobId, blob.archiveId
                    ))
                })?;
            result.extend_from_slice(data);
        }
        Ok(result)
    }

    /// Decrypts and maps an entity that arrived inline in an event bus
    /// message instead of being loaded over REST, as TS
    /// `getParsedInstanceFromEntityEvent` does.
    ///
    /// Returns `Ok(None)` when no session key can be resolved from the
    /// payload itself. The caller may load the entity over REST instead:
    /// another attempt under the same resolution rules, not a wait for the
    /// server to add a key, which no server-side process does.
    pub async fn decrypt_inline_and_parse<T: Entity + DeserializeOwned>(
        &self,
        json: &str,
    ) -> Result<Option<T>, ApiCallError> {
        let type_ref = T::type_ref();
        let client = self.sdk.get_entity_client();
        let model = client.resolve_server_type_ref(&type_ref)?;
        let raw = serde_json::from_str(json)
            .map_err(|e| ApiCallError::internal(format!("decrypt_inline: malformed JSON: {e}")))?;
        let parsed = client.parse_raw(&type_ref, raw)?;
        let decrypted = if model.marked_encrypted() && has_kdf_nonce(&model, &parsed) {
            self.sdk
                .mail_facade()
                .get_crypto_entity_client()
                .decrypt_parsed(&type_ref, parsed, None)
                .await?
        } else if model.marked_encrypted() {
            let crypto_client = self.sdk.mail_facade().get_crypto_entity_client();
            let key = match crypto_client
                .get_crypto_facade()
                .resolve_session_key(&parsed, &model)
                .await
            {
                Ok(Some(key)) => key,
                Ok(None) => return Ok(None),
                Err(e) => {
                    log::debug!("Inline key resolution failed for {}: {e}", model.name);
                    return Ok(None);
                }
            };
            self.decoder.decrypt_and_map(&model, parsed, key)?
        } else {
            parsed
        };
        let entity = self
            .sdk
            .instance_mapper
            .parse_entity(decrypted)
            .map_err(|e| ApiCallError::internal(format!("decrypt_inline: map failed: {e}")))?;
        Ok(Some(entity))
    }
}
