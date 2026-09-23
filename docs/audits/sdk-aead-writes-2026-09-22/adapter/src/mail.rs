//! Mail orchestration owned by the bridge, leaving upstream MailFacade intact.
use crate::folder_system::FolderSystem;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tutasdk::crypto::crypto_facade::ResolvedSessionKey;
use tutasdk::entities::generated::tutanota::{
    Mail, MailBox, MailDetails, MailDetailsBlob, MailDetailsDraft, MailSet, MoveMailData,
    TutanotaFile,
};
use tutasdk::entities::{
    entity_facade::{has_kdf_nonce, EntityDecryptionRequirements, EntityFacade, EntityFacadeImpl},
    Entity,
};
use tutasdk::services::generated::tutanota::MoveMailService;
use tutasdk::tutanota_constants::ArchiveDataType;
use tutasdk::{ApiCallError, GeneratedId, IdTupleGenerated, ListLoadDirection, LoggedInSdk};

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

    pub async fn move_mails(
        &self,
        mut mails: Vec<IdTupleGenerated>,
        target_folder: IdTupleGenerated,
    ) -> Result<(), ApiCallError> {
        // Preserve the existing operation's ordering and adjacent-dedup semantics.
        mails.dedup();
        for chunk in mails.chunks(50) {
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

    async fn owner_key(
        &self,
        owner: Option<&GeneratedId>,
        encrypted: Option<&Vec<u8>>,
        version: Option<i64>,
    ) -> Result<ResolvedSessionKey, ApiCallError> {
        let owner = owner.ok_or_else(|| ApiCallError::internal("Missing _ownerGroup".into()))?;
        let encrypted = encrypted
            .ok_or_else(|| ApiCallError::internal("Missing _ownerEncSessionKey".into()))?;
        let version = version.unwrap_or(0).unsigned_abs();
        let client = self.sdk.mail_facade().get_crypto_entity_client();
        let group_key = client
            .get_crypto_facade()
            .get_key_loader_facade()
            .load_sym_group_key(owner, version, None)
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
        let type_ref = MailDetailsBlob::type_ref();
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
        let client = self.sdk.get_entity_client();
        let parsed = client.parse_raw(&type_ref, raw)?;
        let model = client.resolve_server_type_ref(&type_ref)?;
        let requirements = EntityDecryptionRequirements::for_entity(
            &model,
            &parsed,
            &self.sdk.type_model_provider,
        )?;
        let key = if requirements.session_key && mail._ownerEncSessionKey.is_some() {
            Some(
                self.owner_key(
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
        let blob: MailDetailsBlob = self
            .sdk
            .instance_mapper
            .parse_entity(decrypted)
            .map_err(|e| ApiCallError::internal(format!("Failed to map blob: {e}")))?;
        Ok(blob.details)
    }

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
        let type_ref = MailDetailsDraft::type_ref();
        let client = self.sdk.get_entity_client();
        let parsed = client.load(&type_ref, id).await?;
        let model = client.resolve_server_type_ref(&type_ref)?;
        let requirements = EntityDecryptionRequirements::for_entity(
            &model,
            &parsed,
            &self.sdk.type_model_provider,
        )?;
        let key = if requirements.session_key && mail._ownerEncSessionKey.is_some() {
            Some(
                self.owner_key(
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
        let draft: MailDetailsDraft = self
            .sdk
            .instance_mapper
            .parse_entity(decrypted)
            .map_err(|e| ApiCallError::internal(format!("Failed to map draft: {e}")))?;
        Ok(draft.details)
    }

    pub async fn load_file_attachment_data(
        &self,
        file: &TutanotaFile,
    ) -> Result<Vec<u8>, ApiCallError> {
        let key = self
            .owner_key(
                file._ownerGroup.as_ref(),
                file._ownerEncSessionKey.as_ref(),
                file._ownerKeyVersion,
            )
            .await?;
        if file.blobs.is_empty() {
            return Ok(Vec::new());
        }
        let instance = file
            ._id
            .as_ref()
            .ok_or_else(|| ApiCallError::internal("File has no ID".into()))?;
        let mut archives = std::collections::HashMap::new();
        for blob in &file.blobs {
            archives
                .entry(blob.archiveId.clone())
                .or_insert_with(Vec::new)
                .push(blob.blobId.clone());
        }
        let mut chunks = std::collections::HashMap::new();
        for (archive, ids) in archives {
            let encrypted = self
                .sdk
                .blob_facade()
                .download_blobs(&archive, instance, ArchiveDataType::Attachments, &ids)
                .await?;
            for (id, bytes) in encrypted {
                let data = key.session_key.decrypt_data(&bytes).map_err(|e| {
                    ApiCallError::internal(format!("Cannot decrypt attachment: {e}"))
                })?;
                chunks.insert((archive.clone(), id), data);
            }
        }
        let mut result = Vec::new();
        for blob in &file.blobs {
            let data = chunks
                .get(&(blob.archiveId.clone(), blob.blobId.clone()))
                .ok_or_else(|| ApiCallError::internal("Missing attachment chunk".into()))?;
            result.extend_from_slice(data);
        }
        Ok(result)
    }

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
                // Preserve the existing bridge fallback to a REST reload.
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
