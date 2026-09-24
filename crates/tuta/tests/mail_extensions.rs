//! Integration test for `CryptoEntityClient::decrypt_inline_and_parse`.
//!
//! The realtime event bus delivers a still-encrypted JSON of the affected
//! entity inside `EntityUpdate.instance`. A client should be able to
//! decrypt it locally instead of spending a REST round-trip to load the
//! same data. This test reuses the existing `download_mail_test` fixture
//! (a real captured server response for a Mail load) as the inline JSON
//! payload — the wire shape is identical.

use std::collections::HashMap;
use std::sync::Arc;

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use tutabridge_tuta::mail::MailExtensions;
use tutasdk::bindings::rest_client::{HttpMethod, RestClient};
use tutasdk::bindings::test_file_client::TestFileClient;
use tutasdk::bindings::test_rest_client::TestRestClient;
use tutasdk::entities::generated::tutanota::Mail;
use tutasdk::login::{CredentialType, Credentials};
use tutasdk::{GeneratedId, Sdk};

fn make_rest_client_for_login() -> Arc<dyn RestClient> {
    let mut client = TestRestClient::new("http://localhost:9000");
    client.insert_response(
		"http://localhost:9000/rest/sys/Session/O1qC702-1J-0/3u3i8Lr9_7TnDDdAVw7w3TypTD2k1L00vIUTMF0SIPY",
		HttpMethod::GET,
		200,
		HashMap::default(),
		Some(include_bytes!("download_mail_test/session.json")),
	);
    client.insert_response(
        "http://localhost:9000/rest/sys/User/O1qC700----0",
        HttpMethod::GET,
        200,
        HashMap::default(),
        Some(include_bytes!("download_mail_test/user.json")),
    );
    Arc::new(client)
}

async fn login_for_test() -> Arc<tutasdk::LoggedInSdk> {
    let rest_client = make_rest_client_for_login();
    let file_client = Arc::new(TestFileClient::default());
    // Same credentials as `download_mail_test.rs` — the fixture's session is
    // keyed against them.
    let encrypted_passphrase_key = BASE64_STANDARD
		.decode("AZWEA/KTrHu0bW52CsctsBTTV4U3jrU51TadSxf6Nqs3xbEs3WfoOpPtxUDCNjHNppt6LHCfgTioejjGUJ2cCsXosZAysUiau5Nvyi8mtjLz")
		.unwrap();
    let credentials = Credentials {
        login: "bed-free@tutanota.de".to_string(),
        user_id: GeneratedId("O1qC700----0".to_owned()),
        access_token: "ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ".to_string(),
        encrypted_passphrase_key,
        credential_type: CredentialType::Internal,
    };
    let sdk = Sdk::new(
        "http://localhost:9000".to_string(),
        rest_client,
        file_client,
    );
    sdk.login(credentials).await.unwrap()
}

#[tokio::test]
async fn decrypts_an_inline_mail_payload_without_rest() {
    // The fixture is the same encrypted Mail JSON the server returned for
    // the REST `load_mail_test` — it is the exact shape an event-bus
    // `EntityUpdate.instance` carries on Mail CREATE/UPDATE.
    let logged_in = login_for_test().await;
    let mail_json = include_str!("download_mail_test/mail.json");

    let mail: Option<Mail> = MailExtensions::new(logged_in)
        .decrypt_inline_and_parse::<Mail>(mail_json)
        .await
        .expect("decrypt_inline must succeed on a valid fixture");

    let mail = mail.expect("session key is resolvable for this fixture");
    // Subject is the same one asserted by `download_mail_test.rs`, so
    // passing here proves the inline pipeline yields the same Mail as a
    // full REST load.
    assert_eq!(mail.subject, "Html email features");
    assert_eq!(mail.recipientCount, 1);
}

#[tokio::test]
async fn rejects_malformed_inline_json() {
    let logged_in = login_for_test().await;
    let err = MailExtensions::new(logged_in)
        .decrypt_inline_and_parse::<Mail>("{ not json at all")
        .await
        .expect_err("malformed JSON must surface as an error, not a silent None");
    assert!(
        format!("{err:?}").contains("malformed JSON"),
        "unexpected error shape: {err:?}",
    );
}

#[tokio::test]
async fn empty_inline_json_object_is_a_session_key_miss() {
    // A well-formed JSON with no encryption metadata cannot resolve a
    // session key — the inline contract is to return `Ok(None)` so the
    // caller can fall back (e.g. to a REST load) rather than panic.
    let logged_in = login_for_test().await;
    let outcome = MailExtensions::new(logged_in)
        .decrypt_inline_and_parse::<Mail>("{}")
        .await;
    // Either Err on parse (no required fields) OR Ok(None) on missing key
    // is acceptable — but never Ok(Some(mail)) and never a panic.
    match outcome {
        Ok(None) => {}
        Err(_) => {}
        Ok(Some(_)) => panic!("must not synthesise a Mail from an empty payload"),
    }
}

use std::sync::Mutex;
use tutasdk::bindings::rest_client::{RestClientError, RestClientOptions, RestResponse};
use tutasdk::IdTupleGenerated;

/// Blob contents served per archive id, as (blob id, bytes).
type BlobsByArchive = HashMap<String, Vec<(GeneratedId, Vec<u8>)>>;

struct RecordingClient {
    fixtures: Arc<dyn RestClient>,
    requests: Mutex<Vec<(String, HttpMethod, RestClientOptions)>>,
    move_status: u32,
    extra: Mutex<HashMap<String, Vec<u8>>>,
    blobs: Mutex<BlobsByArchive>,
}
#[async_trait::async_trait]
impl RestClient for RecordingClient {
    async fn request_binary(
        &self,
        url: String,
        method: HttpMethod,
        options: RestClientOptions,
    ) -> Result<RestResponse, RestClientError> {
        if url.ends_with("/rest/tutanota/movemailservice") {
            self.requests.lock().unwrap().push((url, method, options));
            return Ok(RestResponse {
                status: self.move_status,
                headers: HashMap::default(),
                body: Some(br#"{"1722":"0","1723":[]}"#.to_vec()),
            });
        }
        if url.contains("/rest/storage/blobservice?") {
            assert!(options.body.is_none());
            let query: HashMap<_, _> =
                form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            let raw: serde_json::Value = serde_json::from_str(&query["_body"]).unwrap();
            let archive = raw["52"].as_str().unwrap();
            let blobs = self.blobs.lock().unwrap();
            let entries = &blobs[archive];
            let mut body = (entries.len() as u32).to_be_bytes().to_vec();
            for (id, data) in entries.iter().rev() {
                body.extend(tutasdk::util::BASE64_EXT.decode(id.as_str()).unwrap());
                body.extend([0; 6]);
                body.extend((data.len() as u32).to_be_bytes());
                body.extend(data);
            }
            return Ok(RestResponse {
                status: 200,
                headers: HashMap::new(),
                body: Some(body),
            });
        }
        if url.ends_with("/rest/storage/blobaccesstokenservice") {
            let raw: serde_json::Value =
                serde_json::from_slice(options.body.as_ref().unwrap()).unwrap();
            assert_eq!(raw["180"], "1"); // ArchiveDataType.Attachments
            assert_eq!(raw["80"], serde_json::json!([])); // no write grant
            let read = &raw["181"][0];
            assert!(read["176"].is_string());
            assert!(matches!(
                read["177"].as_str(),
                Some("archive-a" | "archive-b")
            ));
            assert_eq!(read["178"], "files");
            assert_eq!(read["179"][0]["174"], "file");
            assert!(read["179"][0]["173"].is_string());
        }
        if let Some(body) = self.extra.lock().unwrap().get(&url) {
            return Ok(RestResponse {
                status: 200,
                headers: HashMap::default(),
                body: Some(body.clone()),
            });
        }
        self.fixtures.request_binary(url, method, options).await
    }
}
async fn recording_session(
    status: u32,
) -> (
    MailExtensions,
    Arc<RecordingClient>,
    Arc<tutasdk::LoggedInSdk>,
) {
    let rest = Arc::new(RecordingClient {
        fixtures: make_rest_client_for_login(),
        requests: Mutex::new(vec![]),
        move_status: status,
        extra: Mutex::new(HashMap::new()),
        blobs: Mutex::new(HashMap::new()),
    });
    // Public upstream test fixtures; requests are handled in memory only.
    let credentials = Credentials {
        login: "bed-free@tutanota.de".into(), user_id: GeneratedId("O1qC700----0".into()),
        access_token: "ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ".into(),
        encrypted_passphrase_key: BASE64_STANDARD.decode("AZWEA/KTrHu0bW52CsctsBTTV4U3jrU51TadSxf6Nqs3xbEs3WfoOpPtxUDCNjHNppt6LHCfgTioejjGUJ2cCsXosZAysUiau5Nvyi8mtjLz").unwrap(),
        credential_type: CredentialType::Internal,
    };
    let sdk = Sdk::new(
        "http://localhost:9000".into(),
        rest.clone(),
        Arc::new(TestFileClient::default()),
    );
    let logged = sdk.login(credentials).await.unwrap();
    (MailExtensions::new(logged.clone()), rest, logged)
}
fn mail_id(i: usize) -> IdTupleGenerated {
    IdTupleGenerated::new(
        GeneratedId("mail-list".into()),
        GeneratedId(format!("mail-{i}")),
    )
}
#[tokio::test]
async fn move_posts_target_and_batches_through_real_sdk_serializer() {
    let (ext, rest, _) = recording_session(200).await;
    let target = IdTupleGenerated::new(GeneratedId("folders".into()), GeneratedId("target".into()));
    let mut mails = vec![mail_id(0)];
    mails.extend((0..101).map(mail_id)); // adjacent duplicate of id 0
    ext.move_mails(mails, target).await.unwrap();
    let requests = rest.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (index, (_, method, options)) in requests.iter().enumerate() {
        assert_eq!(*method, HttpMethod::POST);
        assert_eq!(options.headers.get("cv").unwrap(), tutasdk::CLIENT_VERSION);
        let json: serde_json::Value =
            serde_json::from_slice(options.body.as_ref().unwrap()).unwrap();
        assert_eq!(json["447"], serde_json::json!([["folders", "target"]]));
        let batch = json["448"].as_array().unwrap();
        assert_eq!(batch.len(), if index == 2 { 1 } else { 50 });
        assert_eq!(
            batch[0],
            serde_json::json!(["mail-list", format!("mail-{}", index * 50)])
        );
    }
}
#[tokio::test]
async fn move_empty_does_not_request_and_failure_stops_later_batches() {
    let (ext, rest, _) = recording_session(500).await;
    ext.move_mails(vec![], mail_id(0)).await.unwrap();
    assert!(rest.requests.lock().unwrap().is_empty());
    assert!(ext
        .move_mails((0..101).map(mail_id).collect(), mail_id(0))
        .await
        .is_err());
    assert_eq!(rest.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn draft_reader_rejects_received_mail_before_fetching() {
    let logged = login_for_test().await;
    let ext = MailExtensions::new(logged);
    let mail = ext
        .decrypt_inline_and_parse::<Mail>(include_str!("download_mail_test/mail.json"))
        .await
        .unwrap()
        .unwrap();
    assert!(mail.mailDetails.is_some());
    let error = ext.load_mail_details_draft(&mail).await.unwrap_err();
    assert!(error.to_string().contains("Expected draft"));
}

#[tokio::test]
async fn draft_body_uses_parent_mail_key_even_when_draft_has_no_key() {
    use crypto_primitives::{
        aes::{Aes256Key, InitializationVector},
        key::GenericAesKey,
        randomizer_facade::RandomizerFacade,
    };
    use tutasdk::entities::generated::tutanota::{Body, MailDetails, MailDetailsDraft, Recipients};
    use tutasdk::{date::DateTime, CustomId};
    let (ext, rest, logged) = recording_session(200).await;
    let mut mail = ext
        .decrypt_inline_and_parse::<Mail>(include_str!("download_mail_test/mail.json"))
        .await
        .unwrap()
        .unwrap();
    let parent_version = mail._ownerKeyVersion.unwrap_or(0).unsigned_abs();
    let client = logged.mail_facade().get_crypto_entity_client();
    let group_key = client
        .get_crypto_facade()
        .get_key_loader_facade()
        .load_sym_group_key(mail._ownerGroup.as_ref().unwrap(), parent_version, None)
        .await
        .unwrap();
    let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[42; 32]).unwrap());
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    mail._ownerEncSessionKey =
        Some(group_key.encrypt_key(&key, InitializationVector::generate(&rng)));
    mail.mailDetails = None;
    let draft_id = IdTupleGenerated::new(
        GeneratedId("draft-list".into()),
        GeneratedId("draft-id".into()),
    );
    mail.mailDetailsDraft = Some(draft_id.clone());
    let details = MailDetails {
        _id: Some(CustomId("details".into())),
        sentDate: DateTime::from_millis(123456),
        authStatus: 0,
        replyTos: vec![],
        headers: None,
        recipients: Recipients {
            _id: Some(CustomId("recipients".into())),
            toRecipients: vec![],
            ccRecipients: vec![],
            bccRecipients: vec![],
        },
        body: Body {
            _id: Some(CustomId("body".into())),
            text: Some("Draft body: été ✓".into()),
            compressedText: None,
            _errors: Default::default(),
        },
    };
    let draft = MailDetailsDraft {
        _id: Some(draft_id),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerEncSessionKey: None,
        _ownerKeyVersion: None,
        _kdfNonce: None,
        details: details.clone(),
        _errors: Default::default(),
    };
    let json = logged.serialize_instance_to_json(draft, key).unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&json).unwrap();
    raw["1296"] = serde_json::Value::Null;
    raw["1407"] = serde_json::Value::Null;
    rest.extra.lock().unwrap().insert(
        "http://localhost:9000/rest/tutanota/MailDetailsDraft/draft-list/draft-id".into(),
        serde_json::to_vec(&raw).unwrap(),
    );
    let result = ext.load_mail_details_draft(&mail).await.unwrap();
    assert_eq!(result, details);
    let encrypted = mail._ownerEncSessionKey.as_mut().unwrap();
    let last = encrypted.len() - 1;
    encrypted[last] ^= 1;
    assert!(
        ext.load_mail_details_draft(&mail).await.is_err(),
        "corrupted parent key must not produce a body"
    );
}

/// Acceptance gate, run separately with --ignored. A failure rejects promotion.
#[tokio::test]
#[ignore = "protocol acceptance gate; upstream 359 currently fails"]
async fn protocol_accepts_valid_aead_v3_mail_subject() {
    use crypto_primitives::{
        aead_facade::{AeadFacade, AeadSubKeys},
        key::GenericAesKey,
        randomizer_facade::RandomizerFacade,
    };
    use tutasdk::entities::Entity;
    let logged = login_for_test().await;
    let ext = MailExtensions::new(logged.clone());
    let original = include_str!("download_mail_test/mail.json");
    assert!(ext
        .decrypt_inline_and_parse::<Mail>(original)
        .await
        .unwrap()
        .is_some());
    let client = logged.get_entity_client();
    let parsed = client
        .parse_raw(&Mail::type_ref(), serde_json::from_str(original).unwrap())
        .unwrap();
    let model = client.resolve_server_type_ref(&Mail::type_ref()).unwrap();
    let crypto = logged.mail_facade().get_crypto_entity_client();
    let resolved = crypto
        .get_crypto_facade()
        .resolve_session_key(&parsed, &model)
        .await
        .unwrap()
        .unwrap();
    let GenericAesKey::Aes256(key) = resolved.session_key else {
        panic!("expected fixture AES256 session key")
    };
    let aead = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
    let subkeys = AeadSubKeys::derive_from_session_key(&key, "tutanota/97");
    let aad = b"attributeEncSK\x1f105";
    let plaintext = b"AEAD subject acceptance".to_vec();
    let ciphertext = aead.encrypt(&subkeys, plaintext.clone(), aad).unwrap();
    assert_eq!(ciphertext[0], 3);
    assert_eq!(aead.decrypt(&subkeys, &ciphertext, aad).unwrap(), plaintext);
    let mut raw: serde_json::Value = serde_json::from_str(original).unwrap();
    raw["105"] = serde_json::Value::String(BASE64_STANDARD.encode(ciphertext));
    let mail = ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .expect("valid AEAD ciphertext must be accepted by entity decoder")
        .unwrap();
    assert_eq!(mail.subject, "AEAD subject acceptance");
}

#[tokio::test]
#[ignore = "protocol acceptance gate; upstream 359 currently fails"]
async fn protocol_accepts_optional_empty_body_text() {
    use crypto_primitives::{
        aes::{Aes256Key, InitializationVector},
        key::GenericAesKey,
        randomizer_facade::RandomizerFacade,
    };
    use tutasdk::entities::generated::tutanota::{Body, MailDetails, MailDetailsDraft, Recipients};
    use tutasdk::{date::DateTime, CustomId};
    let (ext, rest, logged) = recording_session(200).await;
    let mut mail = ext
        .decrypt_inline_and_parse::<Mail>(include_str!("download_mail_test/mail.json"))
        .await
        .unwrap()
        .unwrap();
    let parent_version = mail._ownerKeyVersion.unwrap_or(0).unsigned_abs();
    let client = logged.mail_facade().get_crypto_entity_client();
    let group_key = client
        .get_crypto_facade()
        .get_key_loader_facade()
        .load_sym_group_key(mail._ownerGroup.as_ref().unwrap(), parent_version, None)
        .await
        .unwrap();
    let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[42; 32]).unwrap());
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    mail._ownerEncSessionKey =
        Some(group_key.encrypt_key(&key, InitializationVector::generate(&rng)));
    mail.mailDetails = None;
    let draft_id = IdTupleGenerated::new(
        GeneratedId("draft-list".into()),
        GeneratedId("draft-id".into()),
    );
    mail.mailDetailsDraft = Some(draft_id.clone());
    let mut details = MailDetails {
        _id: Some(CustomId("details".into())),
        sentDate: DateTime::from_millis(123456),
        authStatus: 0,
        replyTos: vec![],
        headers: None,
        recipients: Recipients {
            _id: Some(CustomId("recipients".into())),
            toRecipients: vec![],
            ccRecipients: vec![],
            bccRecipients: vec![],
        },
        body: Body {
            _id: Some(CustomId("body".into())),
            text: Some("Draft body: été ✓".into()),
            compressedText: None,
            _errors: Default::default(),
        },
    };
    let draft = MailDetailsDraft {
        _id: Some(draft_id),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerEncSessionKey: None,
        _ownerKeyVersion: None,
        _kdfNonce: None,
        details: details.clone(),
        _errors: Default::default(),
    };
    let json = logged.serialize_instance_to_json(draft, key).unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&json).unwrap();
    raw["1296"] = serde_json::Value::Null;
    raw["1407"] = serde_json::Value::Null;
    raw["1297"][0]["1288"][0]["1275"] = serde_json::Value::String(String::new());
    details.body.text = None;
    rest.extra.lock().unwrap().insert(
        "http://localhost:9000/rest/tutanota/MailDetailsDraft/draft-list/draft-id".into(),
        serde_json::to_vec(&raw).unwrap(),
    );
    let result = ext.load_mail_details_draft(&mail).await.unwrap();
    assert_eq!(result, details);
}

#[tokio::test]
async fn attachments_decrypt_and_reassemble_across_archives_and_reject_corruption() {
    use crypto_primitives::{
        aes::{Aes256Key, InitializationVector},
        key::GenericAesKey,
        randomizer_facade::RandomizerFacade,
    };
    use tutasdk::entities::generated::{sys::Blob, tutanota::TutanotaFile};
    let (ext, rest, logged) = recording_session(200).await;
    let mail = ext
        .decrypt_inline_and_parse::<Mail>(include_str!("download_mail_test/mail.json"))
        .await
        .unwrap()
        .unwrap();
    let group_key = logged
        .mail_facade()
        .get_crypto_entity_client()
        .get_crypto_facade()
        .get_key_loader_facade()
        .load_sym_group_key(
            mail._ownerGroup.as_ref().unwrap(),
            mail._ownerKeyVersion.unwrap_or(0).unsigned_abs(),
            None,
        )
        .await
        .unwrap();
    let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[42; 32]).unwrap());
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    rest.extra.lock().unwrap().insert("http://localhost:9000/rest/storage/blobaccesstokenservice".into(), serde_json::to_vec(&serde_json::json!({
        "82":"0", "161":[{"158":"info","159":"token","192":"4102444800000","209":"0","160":[{"155":"server","156":"http://blobs"}]}]
    })).unwrap());
    let mut file = TutanotaFile {
        _id: Some(IdTupleGenerated::new(
            GeneratedId("files".into()),
            GeneratedId("file".into()),
        )),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup,
        _ownerKeyVersion: mail._ownerKeyVersion,
        _ownerEncSessionKey: Some(
            group_key.encrypt_key(&key, InitializationVector::generate(&rng)),
        ),
        _kdfNonce: None,
        name: "attachment.txt".into(),
        size: 6,
        mimeType: Some("text/plain".into()),
        cid: None,
        parent: None,
        subFiles: None,
        blobs: vec![],
        _errors: Default::default(),
    };
    // Reuse one blob ID across archives, and return server entries backwards.
    for (archive, id, data) in [
        ("archive-a", 1, b"ab"),
        ("archive-b", 1, b"cd"),
        ("archive-a", 2, b"ef"),
    ] {
        let id = GeneratedId(tutasdk::util::BASE64_EXT.encode([id; 9]));
        let encrypted = key
            .encrypt_data(data, InitializationVector::generate(&rng))
            .unwrap();
        file.blobs.push(Blob {
            _id: None,
            archiveId: GeneratedId(archive.into()),
            blobId: id.clone(),
            size: encrypted.len() as i64,
        });
        rest.blobs
            .lock()
            .unwrap()
            .entry(archive.into())
            .or_default()
            .push((id, encrypted));
    }
    assert_eq!(
        ext.load_file_attachment_data(&file).await.unwrap(),
        b"abcdef"
    );
    {
        let mut blobs = rest.blobs.lock().unwrap();
        let bytes = &mut blobs.get_mut("archive-a").unwrap()[0].1;
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
    }
    assert!(ext.load_file_attachment_data(&file).await.is_err());
}

/// Re-encode existing fixture attributes with the official AEAD primitive.
/// Keeps the root type for subkeys and uses association/aggregate IDs for AAD.
fn encode_group_attributes(
    raw: &mut serde_json::Value,
    type_ref: &tutasdk::TypeRef,
    provider: &tutasdk::type_model_provider::TypeModelProvider,
    session: &crypto_primitives::key::GenericAesKey,
    subkeys: &crypto_primitives::aead_facade::AeadSubKeys,
    facade: &crypto_primitives::aead_facade::AeadFacade,
    prefix: &str,
) {
    let model = provider.resolve_server_type_ref(type_ref).unwrap();
    for (id, value) in &model.values {
        if !value.encrypted {
            continue;
        }
        let id = String::from(*id);
        if let Some(encoded) = raw[&id].as_str().filter(|s| !s.is_empty()) {
            let plaintext = session
                .decrypt_data(&BASE64_STANDARD.decode(encoded).unwrap())
                .unwrap();
            let encrypted = facade
                .encrypt(
                    subkeys,
                    plaintext,
                    format!("attributeEncGK\x1f{prefix}{id}").as_bytes(),
                )
                .unwrap();
            raw[&id] = serde_json::Value::String(BASE64_STANDARD.encode(encrypted));
        }
    }
    for (id, association) in &model.associations {
        let id = String::from(*id);
        if !raw[&id]
            .as_array()
            .is_some_and(|values| values.iter().any(serde_json::Value::is_object))
        {
            continue;
        }
        let child = provider
            .resolve_server_type_ref(&tutasdk::TypeRef::new(
                association.dependency.unwrap_or(model.app),
                association.ref_type_id,
            ))
            .unwrap();
        let id_attribute = child.get_attribute_id_by_attribute_name("_id").unwrap();
        if let Some(aggregates) = raw[&id].as_array_mut() {
            for aggregate in aggregates {
                let aggregate_id = aggregate[&id_attribute].as_str().unwrap();
                let prefix = format!("{prefix}{id}/{aggregate_id}/");
                encode_group_attributes(
                    aggregate,
                    &child.type_ref(),
                    provider,
                    session,
                    subkeys,
                    facade,
                    &prefix,
                );
            }
        }
    }
}

#[tokio::test]
async fn aead_v2_mail_without_session_key_loads_through_sdk_and_inline() {
    use crypto_primitives::{
        aead_facade::{AeadFacade, AeadSubKeys},
        randomizer_facade::RandomizerFacade,
        versioned::Versioned,
    };
    use tutasdk::entities::Entity;
    let (ext, rest, logged) = recording_session(200).await;
    let original = include_str!("download_mail_test/mail.json");
    let expected = ext
        .decrypt_inline_and_parse::<Mail>(original)
        .await
        .unwrap()
        .unwrap();
    let client = logged.get_entity_client();
    let model = client.resolve_server_type_ref(&Mail::type_ref()).unwrap();
    let parsed = client
        .parse_raw(&Mail::type_ref(), serde_json::from_str(original).unwrap())
        .unwrap();
    let crypto_client = logged.mail_facade().get_crypto_entity_client();
    let crypto = crypto_client.get_crypto_facade();
    let session = crypto
        .resolve_session_key(&parsed, &model)
        .await
        .unwrap()
        .unwrap();
    let version = expected._ownerKeyVersion.unwrap_or(0).unsigned_abs();
    let group_key = crypto
        .get_key_loader_facade()
        .load_sym_group_key(expected._ownerGroup.as_ref().unwrap(), version, None)
        .await
        .unwrap();
    let nonce = [0x22; 32];
    let subkeys = AeadSubKeys::derive_from_group_key(
        &Versioned {
            object: group_key,
            version,
        },
        &nonce,
        "tutanota/97",
    );
    let facade = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
    let mut raw: serde_json::Value = serde_json::from_str(original).unwrap();
    encode_group_attributes(
        &mut raw,
        &Mail::type_ref(),
        &logged.type_model_provider,
        &session.session_key,
        &subkeys,
        &facade,
        "",
    );
    raw["1839"] = serde_json::Value::String(BASE64_STANDARD.encode(nonce));
    raw["102"] = serde_json::Value::Null;
    raw["1395"] = serde_json::Value::Null;
    let actual = ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .unwrap()
        .expect("AEAD v2 mail must not require a session key");
    assert_eq!(actual.subject, expected.subject);
    assert_eq!(actual.sender, expected.sender);
    assert!(actual._ownerEncSessionKey.is_none());
    let id = expected._id.as_ref().unwrap();
    rest.extra.lock().unwrap().insert(
        format!("http://localhost:9000/rest/tutanota/Mail/{id}"),
        serde_json::to_vec(&raw).unwrap(),
    );
    let actual: Mail = crypto_client.load(id).await.unwrap();
    assert_eq!(actual.subject, expected.subject);
    assert_eq!(actual.sender, expected.sender);
    // A valid v2 message moved to another aggregate must fail authentication.
    raw["111"][0]["93"] = serde_json::Value::String("moved-sender".into());
    assert!(ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .is_err());
}

#[tokio::test]
async fn aead_v2_draft_body_uses_its_group_context_without_parent_session_key() {
    use crypto_primitives::{
        aead_facade::{AeadFacade, AeadSubKeys},
        versioned::Versioned,
    };
    use crypto_primitives::{
        aes::{Aes256Key, InitializationVector},
        key::GenericAesKey,
        randomizer_facade::RandomizerFacade,
    };
    use tutasdk::entities::generated::tutanota::{Body, MailDetails, MailDetailsDraft, Recipients};
    use tutasdk::entities::Entity;
    use tutasdk::{date::DateTime, CustomId};
    let (ext, rest, logged) = recording_session(200).await;
    let mut mail = ext
        .decrypt_inline_and_parse::<Mail>(include_str!("download_mail_test/mail.json"))
        .await
        .unwrap()
        .unwrap();
    let parent_version = mail._ownerKeyVersion.unwrap_or(0).unsigned_abs();
    let client = logged.mail_facade().get_crypto_entity_client();
    let group_key = client
        .get_crypto_facade()
        .get_key_loader_facade()
        .load_sym_group_key(mail._ownerGroup.as_ref().unwrap(), parent_version, None)
        .await
        .unwrap();
    let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[42; 32]).unwrap());
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    mail._ownerEncSessionKey =
        Some(group_key.encrypt_key(&key, InitializationVector::generate(&rng)));
    mail.mailDetails = None;
    let draft_id = IdTupleGenerated::new(
        GeneratedId("draft-list".into()),
        GeneratedId("draft-id".into()),
    );
    mail.mailDetailsDraft = Some(draft_id.clone());
    let details = MailDetails {
        _id: Some(CustomId("details".into())),
        sentDate: DateTime::from_millis(123456),
        authStatus: 0,
        replyTos: vec![],
        headers: None,
        recipients: Recipients {
            _id: Some(CustomId("recipients".into())),
            toRecipients: vec![],
            ccRecipients: vec![],
            bccRecipients: vec![],
        },
        body: Body {
            _id: Some(CustomId("body".into())),
            text: Some("Draft body: été ✓".into()),
            compressedText: None,
            _errors: Default::default(),
        },
    };
    let draft = MailDetailsDraft {
        _id: Some(draft_id),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerEncSessionKey: None,
        _ownerKeyVersion: None,
        _kdfNonce: None,
        details: details.clone(),
        _errors: Default::default(),
    };
    let json = logged
        .serialize_instance_to_json(draft, key.clone())
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&json).unwrap();
    raw["1296"] = serde_json::Value::Null;
    raw["1407"] = serde_json::Value::Null;
    let nonce = [0x22; 32];
    let subkeys = AeadSubKeys::derive_from_group_key(
        &Versioned {
            object: group_key,
            version: parent_version,
        },
        &nonce,
        "tutanota/1290",
    );
    let facade = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
    encode_group_attributes(
        &mut raw,
        &MailDetailsDraft::type_ref(),
        &logged.type_model_provider,
        &key,
        &subkeys,
        &facade,
        "",
    );
    raw["1830"] = serde_json::Value::String(BASE64_STANDARD.encode(nonce));
    mail._ownerEncSessionKey = None;
    mail._ownerKeyVersion = None;
    rest.extra.lock().unwrap().insert(
        "http://localhost:9000/rest/tutanota/MailDetailsDraft/draft-list/draft-id".into(),
        serde_json::to_vec(&raw).unwrap(),
    );
    let result = ext.load_mail_details_draft(&mail).await.unwrap();
    assert_eq!(result, details);
    raw["1830"] = serde_json::Value::String(BASE64_STANDARD.encode([0x33; 32]));
    rest.extra.lock().unwrap().insert(
        "http://localhost:9000/rest/tutanota/MailDetailsDraft/draft-list/draft-id".into(),
        serde_json::to_vec(&raw).unwrap(),
    );
    assert!(
        ext.load_mail_details_draft(&mail).await.is_err(),
        "wrong nonce must not produce a body"
    );
}
