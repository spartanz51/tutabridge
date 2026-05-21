use std::sync::Arc;
use crypto_primitives::aes::{Aes256Key, Iv};
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use tutasdk::bindings::file_client::{FileClient, FileClientError};
use tutasdk::bindings::rest_client::RestClient;
use tutasdk::crypto_entity_client::CryptoEntityClient;
use tutasdk::entities::generated::tutanota::{
    DraftCreateData, DraftData, DraftRecipient, Mail, MailBox, MailDetails, MailDetailsBlob,
    MailSetEntry, SendDraftData,
};
use tutasdk::folder_system::{FolderSystem, MailSetKind};
use tutasdk::services::generated::tutanota::{DraftService, SendDraftService};
use tutasdk::services::ExtraServiceParams;
use tutasdk::{ApiCallError, CustomId, IdTupleGenerated, ListLoadDirection, LoggedInSdk, Sdk};

use crate::config::Config;
use crate::mail::ParsedMessage;

#[async_trait::async_trait]
pub trait MailBackend: Send + Sync {
    async fn load_mail_ids_for_folder(&self, kind: MailSetKind) -> Result<Vec<Mail>, String>;
    async fn load_mail_details(&self, mail: &Mail) -> Result<Option<MailDetails>, String>;
    async fn load_folder_list(&self) -> Result<Vec<(String, String)>, String>;
    async fn set_unread_status(&self, mail_ids: Vec<IdTupleGenerated>, unread: bool) -> Result<(), String>;
    async fn trash_mails(&self, mail_ids: Vec<IdTupleGenerated>) -> Result<(), String>;
    async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String>;
}

pub struct TutaSession {
    pub logged_in: Arc<LoggedInSdk>,
    pub email: String,
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

    fn crypto_client(&self) -> Arc<CryptoEntityClient> {
        self.logged_in.mail_facade().get_crypto_entity_client()
    }

    async fn load_mail_ids_for_folder_impl(
        &self,
        folder_kind: MailSetKind,
    ) -> Result<Vec<Mail>, ApiCallError> {
        let mailbox = self.load_mailbox().await?;
        let folders = self.load_folders(&mailbox).await?;
        let folder = folders
            .system_folder_by_type(folder_kind)
            .ok_or_else(|| ApiCallError::internal(format!("Folder {:?} not found", folder_kind)))?;

        let entries_list_id = &folder.entries;
        let entries: Vec<MailSetEntry> = self
            .crypto_client()
            .load_range(
                entries_list_id,
                &CustomId::default(),
                100,
                ListLoadDirection::DESC,
            )
            .await?;

        let mut mails = Vec::new();
        for entry in &entries {
            match self.crypto_client().load::<Mail, _>(&entry.mail).await {
                Ok(mail) => mails.push(mail),
                Err(e) => log::warn!("Failed to load mail {:?}: {}", entry.mail, e),
            }
        }

        Ok(mails)
    }

    async fn load_mail_details_impl(
        &self,
        mail: &Mail,
    ) -> Result<Option<MailDetailsBlob>, ApiCallError> {
        if mail.mailDetails.is_some() {
            let blob = self.logged_in.load_mail_details_blob(mail).await?;
            Ok(Some(blob))
        } else {
            Ok(None)
        }
    }

    async fn send_mail_impl(&self, msg: &ParsedMessage) -> Result<(), ApiCallError> {
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

        let owner_enc_session_key =
            group_key.object.encrypt_key(&session_key, Iv::generate(&randomizer));
        let owner_key_version = group_key.version as i64;

        let to_recips: Vec<DraftRecipient> = msg
            .to
            .iter()
            .map(|(name, addr)| DraftRecipient {
                _id: None,
                name: name.clone(),
                mailAddress: addr.clone(),
                _errors: Default::default(),
            })
            .collect();
        let cc_recips: Vec<DraftRecipient> = msg
            .cc
            .iter()
            .map(|(name, addr)| DraftRecipient {
                _id: None,
                name: name.clone(),
                mailAddress: addr.clone(),
                _errors: Default::default(),
            })
            .collect();
        let bcc_recips: Vec<DraftRecipient> = msg
            .bcc
            .iter()
            .map(|(name, addr)| DraftRecipient {
                _id: None,
                name: name.clone(),
                mailAddress: addr.clone(),
                _errors: Default::default(),
            })
            .collect();

        let draft_data = DraftData {
            _id: None,
            subject: msg.subject.clone(),
            bodyText: msg.body_html.clone(),
            senderMailAddress: self.email.clone(),
            senderName: msg.from_name.clone(),
            confidential: false,
            method: 0,
            compressedBodyText: None,
            toRecipients: to_recips,
            ccRecipients: cc_recips,
            bccRecipients: bcc_recips,
            addedAttachments: vec![],
            removedAttachments: vec![],
            replyTos: vec![],
            _errors: Default::default(),
        };

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

        let send_data = SendDraftData {
            _format: 0,
            language: "en".to_string(),
            mailSessionKey: Some(session_key.as_bytes().to_vec()),
            bucketEncMailSessionKey: None,
            senderNameUnencrypted: None,
            plaintext: true,
            calendarMethod: false,
            sessionEncEncryptionAuthStatus: None,
            sendAt: None,
            allowUndo: false,
            internalRecipientKeyData: vec![],
            secureExternalRecipientKeyData: vec![],
            attachmentKeyData: vec![],
            mail: draft_return.draft,
            symEncInternalRecipientKeyData: vec![],
            parameters: None,
        };

        let send_return = executor
            .post::<SendDraftService>(send_data, ExtraServiceParams::default())
            .await?;

        log::info!("Mail sent, message_id: {}", send_return.messageId);
        Ok(())
    }
}

#[async_trait::async_trait]
impl MailBackend for TutaSession {
    async fn load_mail_ids_for_folder(&self, kind: MailSetKind) -> Result<Vec<Mail>, String> {
        self.load_mail_ids_for_folder_impl(kind)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn load_mail_details(&self, mail: &Mail) -> Result<Option<MailDetails>, String> {
        self.load_mail_details_impl(mail)
            .await
            .map(|opt| opt.map(|blob| blob.details))
            .map_err(|e| format!("{e}"))
    }

    async fn load_folder_list(&self) -> Result<Vec<(String, String)>, String> {
        let mailbox = self.load_mailbox().await.map_err(|e| format!("{e}"))?;
        let folder_system = self.load_folders(&mailbox).await.map_err(|e| format!("{e}"))?;

        let known_folders = [
            (MailSetKind::Inbox, "INBOX", ""),
            (MailSetKind::Sent, "Sent", "\\Sent"),
            (MailSetKind::Draft, "Drafts", "\\Drafts"),
            (MailSetKind::Trash, "Trash", "\\Trash"),
            (MailSetKind::Archive, "Archive", "\\Archive"),
            (MailSetKind::Spam, "Spam", "\\Junk"),
        ];

        let mut result = Vec::new();
        for (kind, name, flags) in &known_folders {
            if folder_system.system_folder_by_type(*kind).is_some() {
                result.push((name.to_string(), flags.to_string()));
            }
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

pub async fn login(cfg: &Config) -> Result<TutaSession, Box<dyn std::error::Error + Send + Sync>> {
    let rest_client: Arc<dyn RestClient> =
        Arc::new(tutasdk::net::native_rest_client::NativeRestClient::try_new()?);
    let file_client: Arc<dyn FileClient> = Arc::new(DiskFileClient::new());
    let sdk = Sdk::new(cfg.api_url.clone(), rest_client, file_client);

    if let Some(credentials) = load_credentials(&cfg.email) {
        log::info!("Resuming saved session...");
        match sdk.login(credentials).await {
            Ok(logged_in) => {
                return Ok(TutaSession {
                    logged_in,
                    email: cfg.email.clone(),
                });
            }
            Err(e) => {
                log::warn!("Session expired, re-authenticating: {e}");
                delete_credentials(&cfg.email);
            }
        }
    }

    let password = rpassword_prompt(&cfg.email)?;

    log::info!("Authenticating with Tuta servers...");
    let (session_return, credentials) = sdk
        .initiate_session(&cfg.email, &password)
        .await
        .map_err(|e| {
            Box::<dyn std::error::Error + Send + Sync>::from(format!("Login failed: {e}"))
        })?;

    if !session_return.challenges.is_empty() {
        for c in &session_return.challenges {
            log::info!("2FA challenge: type={}, id={:?}", c.r#type, c._id);
        }

        let has_totp = session_return
            .challenges
            .iter()
            .any(|c| c.r#type == 1);

        if !has_totp {
            return Err("Account requires U2F/WebAuthn 2FA which is not supported — only TOTP is supported".into());
        }

        let totp_code = totp_prompt()?;
        sdk.submit_2fa(&session_return.accessToken, totp_code)
            .await
            .map_err(|e| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!("2FA failed: {e}"))
            })?;

        let mut cleared = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let pending = sdk
                .check_2fa_pending(&session_return.accessToken)
                .await
                .map_err(|e| {
                    Box::<dyn std::error::Error + Send + Sync>::from(format!("2FA poll failed: {e}"))
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
    })
}

const KEYRING_SERVICE: &str = "tutabridge";

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
}

fn load_credentials(email: &str) -> Option<tutasdk::login::Credentials> {
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
}

fn rpassword_prompt(email: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::Write;
    print!("Password for {}: ", email);
    std::io::stdout().flush()?;
    let password = rpassword::read_password()?;
    Ok(password)
}

fn totp_prompt() -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::{BufRead, Write};
    print!("TOTP code: ");
    std::io::stdout().flush()?;
    let mut code_str = String::new();
    std::io::stdin().lock().read_line(&mut code_str)?;
    let code: u32 = code_str
        .trim()
        .parse()
        .map_err(|_| "Invalid TOTP code — must be a number")?;
    Ok(code)
}
