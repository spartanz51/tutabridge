//! `MailExtensions` against an in-memory server, logged in with the official
//! SDK's `download_mail_test` fixture (a captured session, user and mail).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use crypto_primitives::aead_facade::{AeadFacade, AeadSubKeys};
use crypto_primitives::aes::{Aes256Key, InitializationVector};
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use crypto_primitives::versioned::Versioned;
use tutabridge_tuta::folder_system::MailSetKind;
use tutabridge_tuta::mail::MailExtensions;
use tutasdk::bindings::rest_client::{
    HttpMethod, RestClient, RestClientError, RestClientOptions, RestResponse,
};
use tutasdk::bindings::test_file_client::TestFileClient;
use tutasdk::bindings::test_rest_client::TestRestClient;
use tutasdk::date::DateTime;
use tutasdk::entities::generated::sys::Blob;
use tutasdk::entities::generated::tutanota::{
    Body, Mail, MailBox, MailDetails, MailDetailsDraft, MailSet, MailSetEntry, Recipients,
    TutanotaFile,
};
use tutasdk::entities::Entity;
use tutasdk::login::{CredentialType, Credentials};
use tutasdk::{CustomId, GeneratedId, IdTupleGenerated, LoggedInSdk, Sdk};

const FIXTURES: &str = "http://localhost:9000";
const DRAFT_URL: &str = "http://localhost:9000/rest/tutanota/MailDetailsDraft/draft-list/draft-id";
const MAIL_JSON: &str =
    include_str!("../../../tuta-repo/tuta-sdk/rust/sdk/tests/download_mail_test/mail.json");

/// Blob contents served per archive id, as (blob id, bytes).
type BlobsByArchive = HashMap<String, Vec<(GeneratedId, Vec<u8>)>>;

/// Serves the fixture session, records moves and blob token requests, and
/// answers blob and extra requests the test has registered.
struct TestServer {
    fixtures: TestRestClient,
    move_status: u32,
    moves: Mutex<Vec<RestClientOptions>>,
    token_requests: Mutex<Vec<serde_json::Value>>,
    responses: Mutex<HashMap<String, Vec<u8>>>,
    blobs: Mutex<BlobsByArchive>,
    blob_elements: Mutex<Vec<serde_json::Value>>,
}

#[async_trait::async_trait]
impl RestClient for TestServer {
    async fn request_binary(
        &self,
        url: String,
        method: HttpMethod,
        options: RestClientOptions,
    ) -> Result<RestResponse, RestClientError> {
        let ok = |body: Vec<u8>| RestResponse {
            status: 200,
            headers: HashMap::new(),
            body: Some(body),
        };
        if url.ends_with("/rest/tutanota/movemailservice") {
            self.moves.lock().unwrap().push(options);
            return Ok(RestResponse {
                status: self.move_status,
                headers: HashMap::new(),
                body: Some(br#"{"1722":"0","1723":[]}"#.to_vec()),
            });
        }
        if url.ends_with("/rest/storage/blobaccesstokenservice") {
            let request = serde_json::from_slice(options.body.as_ref().unwrap()).unwrap();
            self.token_requests.lock().unwrap().push(request);
            let token = serde_json::json!({"82": "0", "161": [{
                "158": "info", "159": "token", "192": "4102444800000", "209": "0",
                "160": [{"155": "server", "156": "http://blobs"}],
            }]});
            return Ok(ok(serde_json::to_vec(&token).unwrap()));
        }
        if url.contains("/rest/tutanota/maildetailsblob/") {
            let elements = self.blob_elements.lock().unwrap().clone();
            return Ok(ok(serde_json::to_vec(&elements).unwrap()));
        }
        if url.contains("/rest/storage/blobservice?") {
            let query: HashMap<_, _> =
                form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            let request: serde_json::Value = serde_json::from_str(&query["_body"]).unwrap();
            let blobs = self.blobs.lock().unwrap();
            let entries = &blobs[request["52"].as_str().unwrap()];
            // Binary framing: count, then (id, hash, size, data) per blob,
            // returned in reverse order to check reassembly.
            let mut body = (entries.len() as u32).to_be_bytes().to_vec();
            for (id, data) in entries.iter().rev() {
                body.extend(tutasdk::util::BASE64_EXT.decode(id.as_str()).unwrap());
                body.extend([0; 6]);
                body.extend((data.len() as u32).to_be_bytes());
                body.extend(data);
            }
            return Ok(ok(body));
        }
        if let Some(body) = self.responses.lock().unwrap().get(&url) {
            return Ok(ok(body.clone()));
        }
        self.fixtures.request_binary(url, method, options).await
    }
}

impl TestServer {
    fn respond(&self, url: &str, body: &serde_json::Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(url.into(), serde_json::to_vec(body).unwrap());
    }
}

struct Session {
    ext: MailExtensions,
    server: Arc<TestServer>,
    sdk: Arc<LoggedInSdk>,
}

async fn session_with_move_status(move_status: u32) -> Session {
    let mut fixtures = TestRestClient::new(FIXTURES);
    fixtures.insert_response(
        "http://localhost:9000/rest/sys/Session/O1qC702-1J-0/3u3i8Lr9_7TnDDdAVw7w3TypTD2k1L00vIUTMF0SIPY",
        HttpMethod::GET,
        200,
        HashMap::default(),
        Some(include_bytes!(
            "../../../tuta-repo/tuta-sdk/rust/sdk/tests/download_mail_test/session.json"
        )),
    );
    fixtures.insert_response(
        "http://localhost:9000/rest/sys/User/O1qC700----0",
        HttpMethod::GET,
        200,
        HashMap::default(),
        Some(include_bytes!(
            "../../../tuta-repo/tuta-sdk/rust/sdk/tests/download_mail_test/user.json"
        )),
    );
    let server = Arc::new(TestServer {
        fixtures,
        move_status,
        moves: Mutex::new(vec![]),
        token_requests: Mutex::new(vec![]),
        responses: Mutex::new(HashMap::new()),
        blobs: Mutex::new(HashMap::new()),
        blob_elements: Mutex::new(vec![]),
    });
    // The fixture's public test credentials; nothing leaves this process.
    let credentials = Credentials {
        login: "bed-free@tutanota.de".into(),
        user_id: GeneratedId("O1qC700----0".into()),
        access_token: "ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ".into(),
        encrypted_passphrase_key: BASE64_STANDARD
            .decode("AZWEA/KTrHu0bW52CsctsBTTV4U3jrU51TadSxf6Nqs3xbEs3WfoOpPtxUDCNjHNppt6LHCfgTioejjGUJ2cCsXosZAysUiau5Nvyi8mtjLz")
            .unwrap(),
        credential_type: CredentialType::Internal,
    };
    let sdk = Sdk::new(
        FIXTURES.into(),
        server.clone(),
        Arc::new(TestFileClient::default()),
    )
    .login(credentials)
    .await
    .unwrap();
    Session {
        ext: MailExtensions::new(sdk.clone()),
        server,
        sdk,
    }
}

async fn session() -> Session {
    session_with_move_status(200).await
}

async fn fixture_mail(session: &Session) -> Mail {
    session
        .ext
        .decrypt_inline_and_parse::<Mail>(MAIL_JSON)
        .await
        .unwrap()
        .expect("the fixture's session key resolves")
}

/// The owner group key of `mail`, used to wrap test session keys.
async fn mail_group_key(session: &Session, mail: &Mail) -> Versioned<GenericAesKey> {
    let version = mail._ownerKeyVersion.unwrap_or(0).unsigned_abs();
    let object = session
        .sdk
        .mail_facade()
        .get_crypto_entity_client()
        .get_crypto_facade()
        .get_key_loader_facade()
        .load_sym_group_key(mail._ownerGroup.as_ref().unwrap(), version, None)
        .await
        .unwrap();
    Versioned { object, version }
}

fn test_session_key() -> GenericAesKey {
    GenericAesKey::Aes256(Aes256Key::from_bytes(&[42; 32]).unwrap())
}

fn wrap(group_key: &Versioned<GenericAesKey>, key: &GenericAesKey) -> Vec<u8> {
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    group_key
        .object
        .encrypt_key(key, InitializationVector::generate(&rng))
}

fn test_details() -> MailDetails {
    MailDetails {
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
            text: Some("Body: été ✓".into()),
            compressedText: None,
            _errors: Default::default(),
        },
    }
}

/// Turns the fixture mail into a draft whose body is encrypted with `key`,
/// served without its own `_ownerEncSessionKey`, and returns the body JSON.
fn serve_draft(
    session: &Session,
    mail: &mut Mail,
    key: &GenericAesKey,
    details: &MailDetails,
) -> serde_json::Value {
    let draft_id = IdTupleGenerated::new(
        GeneratedId("draft-list".into()),
        GeneratedId("draft-id".into()),
    );
    mail.mailDetails = None;
    mail.mailDetailsDraft = Some(draft_id.clone());
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
    let json = session
        .sdk
        .serialize_instance_to_json(draft, key.clone())
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&json).unwrap();
    // _ownerEncSessionKey and _ownerKeyVersion of the draft itself.
    raw["1296"] = serde_json::Value::Null;
    raw["1407"] = serde_json::Value::Null;
    session.server.respond(DRAFT_URL, &raw);
    raw
}

#[tokio::test]
async fn inline_mail_payload_decrypts_like_a_rest_load() {
    let session = session().await;

    let mail = fixture_mail(&session).await;

    // The subject download_mail_test asserts after a REST load.
    assert_eq!(mail.subject, "Html email features");
    assert_eq!(mail.recipientCount, 1);
}

#[tokio::test]
async fn malformed_inline_json_is_an_error() {
    let session = session().await;

    let err = session
        .ext
        .decrypt_inline_and_parse::<Mail>("{ not json at all")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("malformed JSON"), "{err}");
}

#[tokio::test]
async fn inline_mail_without_a_resolvable_key_asks_for_a_rest_load() {
    let session = session().await;
    let mut raw: serde_json::Value = serde_json::from_str(MAIL_JSON).unwrap();
    // _ownerEncSessionKey of the mail.
    raw["102"] = serde_json::Value::Null;

    let outcome = session
        .ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .unwrap();

    assert!(outcome.is_none());
}

fn mail_id(i: usize) -> IdTupleGenerated {
    IdTupleGenerated::new(
        GeneratedId("mail-list".into()),
        GeneratedId(format!("mail-{i}")),
    )
}

#[tokio::test]
async fn moves_are_sent_in_batches_of_50_without_adjacent_duplicates() {
    let session = session().await;
    let target = IdTupleGenerated::new(GeneratedId("folders".into()), GeneratedId("target".into()));
    let mut mails = vec![mail_id(0)];
    mails.extend((0..101).map(mail_id));

    session.ext.move_mails(mails, target).await.unwrap();

    let moves = session.server.moves.lock().unwrap();
    assert_eq!(moves.len(), 3);
    for (index, options) in moves.iter().enumerate() {
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
async fn a_failed_move_batch_stops_the_later_ones() {
    let session = session_with_move_status(500).await;

    session.ext.move_mails(vec![], mail_id(0)).await.unwrap();
    assert!(session.server.moves.lock().unwrap().is_empty());

    let result = session
        .ext
        .move_mails((0..101).map(mail_id).collect(), mail_id(0))
        .await;

    assert!(result.is_err());
    assert_eq!(session.server.moves.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn received_mail_body_is_decrypted_with_the_mail_key() {
    let session = session().await;
    let mut mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let key = test_session_key();
    let details = test_details();
    // The SDK cannot serialize a blob element's id, so build the blob from a
    // draft encrypted with the same key: both carry the same details.
    let draft = MailDetailsDraft {
        _id: Some(IdTupleGenerated::new(
            GeneratedId("draft-list".into()),
            GeneratedId("draft-id".into()),
        )),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerEncSessionKey: None,
        _ownerKeyVersion: None,
        _kdfNonce: None,
        details: details.clone(),
        _errors: Default::default(),
    };
    let draft: serde_json::Value = serde_json::from_str(
        &session
            .sdk
            .serialize_instance_to_json(draft, key.clone())
            .unwrap(),
    )
    .unwrap();
    // MailDetailsBlob: _id, _permissions, _format, _ownerGroup,
    // _ownerEncSessionKey, _ownerKeyVersion, _kdfNonce, details.
    let blob = serde_json::json!({
        "1300": ["archive", "blob"],
        "1301": "permissions",
        "1302": "0",
        "1303": mail._ownerGroup.as_ref().unwrap().as_str(),
        "1304": null,
        "1408": null,
        "1833": null,
        "1305": draft["1297"],
    });
    session.server.blob_elements.lock().unwrap().push(blob);
    mail.mailDetails = Some(IdTupleGenerated::new(
        GeneratedId("archive".into()),
        GeneratedId("blob".into()),
    ));
    mail._ownerEncSessionKey = Some(wrap(&group_key, &key));

    assert_eq!(
        session.ext.load_mail_details_blob(&mail).await.unwrap(),
        details
    );

    // Without the mail's key the body must not be decrypted some other way.
    mail._ownerEncSessionKey = None;
    let err = session.ext.load_mail_details_blob(&mail).await.unwrap_err();
    assert!(
        err.to_string().contains("Mail missing _ownerEncSessionKey"),
        "{err}"
    );
}

#[tokio::test]
async fn draft_reader_rejects_a_received_mail_before_fetching() {
    let session = session().await;
    let mail = fixture_mail(&session).await;
    assert!(mail.mailDetails.is_some());

    let err = session
        .ext
        .load_mail_details_draft(&mail)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("Expected draft"), "{err}");
}

#[tokio::test]
async fn draft_body_uses_the_mail_key_even_when_the_draft_has_none() {
    let session = session().await;
    let mut mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let key = test_session_key();
    let details = test_details();
    serve_draft(&session, &mut mail, &key, &details);
    mail._ownerEncSessionKey = Some(wrap(&group_key, &key));

    assert_eq!(
        session.ext.load_mail_details_draft(&mail).await.unwrap(),
        details
    );

    let encrypted = mail._ownerEncSessionKey.as_mut().unwrap();
    let last = encrypted.len() - 1;
    encrypted[last] ^= 1;
    assert!(
        session.ext.load_mail_details_draft(&mail).await.is_err(),
        "a corrupted mail key must not produce a body"
    );
}

#[tokio::test]
async fn attachments_decrypt_and_reassemble_across_archives_and_reject_corruption() {
    let session = session().await;
    let mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let key = test_session_key();
    let rng = RandomizerFacade::from_core(rand_core::OsRng);
    let mut file = TutanotaFile {
        _id: Some(IdTupleGenerated::new(
            GeneratedId("files".into()),
            GeneratedId("file".into()),
        )),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerKeyVersion: mail._ownerKeyVersion,
        _ownerEncSessionKey: Some(wrap(&group_key, &key)),
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
    // The same blob id in two archives.
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
        session
            .server
            .blobs
            .lock()
            .unwrap()
            .entry(archive.into())
            .or_default()
            .push((id, encrypted));
    }

    assert_eq!(
        session.ext.load_file_attachment_data(&file).await.unwrap(),
        b"abcdef"
    );
    // Each archive is read with a token scoped to the attachment's file.
    for request in session.server.token_requests.lock().unwrap().iter() {
        assert_eq!(request["180"], "1"); // ArchiveDataType.Attachments
        let read = &request["181"][0];
        assert_eq!(read["178"], "files");
        assert_eq!(read["179"][0]["174"], "file");
    }

    {
        let mut blobs = session.server.blobs.lock().unwrap();
        let bytes = &mut blobs.get_mut("archive-a").unwrap()[0].1;
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
    }
    assert!(session.ext.load_file_attachment_data(&file).await.is_err());
}

/// Re-encrypts the encrypted values of a fixture entity with the AEAD v2
/// group-key scheme, recursing into aggregates for their field paths.
fn encrypt_with_group_key(
    raw: &mut serde_json::Value,
    type_ref: &tutasdk::TypeRef,
    session: &Session,
    session_key: &GenericAesKey,
    subkeys: &AeadSubKeys,
    prefix: &str,
) {
    let provider = &session.sdk.type_model_provider;
    let aead = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
    let model = provider.resolve_server_type_ref(type_ref).unwrap();
    for (id, value) in &model.values {
        let id = String::from(*id);
        let Some(encoded) = raw[&id]
            .as_str()
            .filter(|s| value.encrypted && !s.is_empty())
        else {
            continue;
        };
        let plaintext = session_key
            .decrypt_data(&BASE64_STANDARD.decode(encoded).unwrap())
            .unwrap();
        let aad = format!("attributeEncGK\x1f{prefix}{id}");
        let encrypted = aead.encrypt(subkeys, plaintext, aad.as_bytes()).unwrap();
        raw[&id] = serde_json::Value::String(BASE64_STANDARD.encode(encrypted));
    }
    for (id, association) in &model.associations {
        let id = String::from(*id);
        let child = provider.resolve_server_type_ref(&tutasdk::TypeRef::new(
            association.dependency.unwrap_or(model.app),
            association.ref_type_id,
        ));
        let (Some(child), Some(aggregates)) = (child, raw[&id].as_array_mut()) else {
            continue;
        };
        let id_attribute = child.get_attribute_id_by_attribute_name("_id").unwrap();
        for aggregate in aggregates.iter_mut().filter(|a| a.is_object()) {
            let aggregate_id = aggregate[&id_attribute].as_str().unwrap().to_owned();
            let prefix = format!("{prefix}{id}/{aggregate_id}/");
            encrypt_with_group_key(
                aggregate,
                &child.type_ref(),
                session,
                session_key,
                subkeys,
                &prefix,
            );
        }
    }
}

#[tokio::test]
async fn aead_v2_inline_mail_needs_no_session_key_and_rejects_a_moved_value() {
    let session = session().await;
    let expected = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &expected).await;
    let client = session.sdk.get_entity_client();
    let model = client.resolve_server_type_ref(&Mail::type_ref()).unwrap();
    let parsed = client
        .parse_raw(&Mail::type_ref(), serde_json::from_str(MAIL_JSON).unwrap())
        .unwrap();
    let session_key = session
        .sdk
        .mail_facade()
        .get_crypto_entity_client()
        .get_crypto_facade()
        .resolve_session_key(&parsed, &model)
        .await
        .unwrap()
        .unwrap()
        .session_key;
    let nonce = [0x22; 32];
    let subkeys = AeadSubKeys::derive_from_group_key(&group_key, &nonce, "tutanota/97");
    let mut raw: serde_json::Value = serde_json::from_str(MAIL_JSON).unwrap();
    encrypt_with_group_key(
        &mut raw,
        &Mail::type_ref(),
        &session,
        &session_key,
        &subkeys,
        "",
    );
    raw["1839"] = serde_json::Value::String(BASE64_STANDARD.encode(nonce)); // _kdfNonce
    raw["102"] = serde_json::Value::Null; // _ownerEncSessionKey
    raw["1395"] = serde_json::Value::Null; // _ownerKeyVersion

    let actual = session
        .ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .unwrap()
        .expect("an AEAD v2 mail does not need a session key");

    assert_eq!(actual.subject, expected.subject);
    assert_eq!(actual.sender, expected.sender);
    // A value moved to another aggregate fails authentication.
    raw["111"][0]["93"] = serde_json::Value::String("moved-sender".into());
    assert!(session
        .ext
        .decrypt_inline_and_parse::<Mail>(&raw.to_string())
        .await
        .is_err());
}

#[tokio::test]
async fn aead_v2_draft_body_needs_no_mail_key_and_rejects_a_wrong_nonce() {
    let session = session().await;
    let mut mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let key = test_session_key();
    let details = test_details();
    let mut raw = serve_draft(&session, &mut mail, &key, &details);
    let nonce = [0x22; 32];
    let subkeys = AeadSubKeys::derive_from_group_key(&group_key, &nonce, "tutanota/1290");
    encrypt_with_group_key(
        &mut raw,
        &MailDetailsDraft::type_ref(),
        &session,
        &key,
        &subkeys,
        "",
    );
    raw["1830"] = serde_json::Value::String(BASE64_STANDARD.encode(nonce)); // _kdfNonce
    session.server.respond(DRAFT_URL, &raw);
    mail._ownerEncSessionKey = None;
    mail._ownerKeyVersion = None;

    assert_eq!(
        session.ext.load_mail_details_draft(&mail).await.unwrap(),
        details
    );

    raw["1830"] = serde_json::Value::String(BASE64_STANDARD.encode([0x33; 32]));
    session.server.respond(DRAFT_URL, &raw);
    assert!(
        session.ext.load_mail_details_draft(&mail).await.is_err(),
        "a wrong nonce must not produce a body"
    );

    raw["1830"] = serde_json::Value::String(BASE64_STANDARD.encode([0x22; 31]));
    session.server.respond(DRAFT_URL, &raw);
    let error = session
        .ext
        .load_mail_details_draft(&mail)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("KDF nonce"), "{error}");
}

/// A mailbox whose only field of interest is its folder list id.
fn mailbox_with_folder_list(list: &str) -> MailBox {
    serde_json::from_value(serde_json::json!({
        "127": null, "128": "permissions", "129": 0, "569": 0,
        "590": null, "591": null, "1396": null, "1840": null,
        "133": "sent", "134": "received",
        "443": {"441": null, "442": list},
        "1220": {"1218": null, "1219": "spam"},
        "1318": null, "1463": [], "1464": null,
        "1512": "imported", "1585": "states", "1710": "features",
        "1754": "training", "1755": "index", "1962": null, "1963": null,
    }))
    .unwrap()
}

#[tokio::test]
async fn folders_load_and_nest_under_their_parents() {
    let session = session().await;
    let mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let key = test_session_key();
    let folder = |id: &str, name: &str, folder_type: i64, parent: Option<&str>| {
        let set = MailSet {
            _id: Some(IdTupleGenerated::new(
                GeneratedId("folders".into()),
                GeneratedId(id.into()),
            )),
            _permissions: GeneratedId("permissions".into()),
            _format: 0,
            _ownerEncSessionKey: Some(wrap(&group_key, &key)),
            name: name.into(),
            folderType: folder_type,
            _ownerGroup: mail._ownerGroup.clone(),
            _ownerKeyVersion: Some(group_key.version as i64),
            color: None,
            _kdfNonce: None,
            parentFolder: parent.map(|p| {
                IdTupleGenerated::new(GeneratedId("folders".into()), GeneratedId(p.into()))
            }),
            entries: GeneratedId(format!("entries-{id}")),
            _errors: Default::default(),
        };
        let json = session
            .sdk
            .serialize_instance_to_json(set, key.clone())
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&json).unwrap()
    };
    let sets = serde_json::json!([
        folder("inbox", "", 1, None),
        folder("work", "Work", 0, None),
        folder("projects", "Projects", 0, Some("work")),
    ]);
    session.server.respond(
        "http://localhost:9000/rest/tutanota/MailSet/folders?start=------------&count=100&reverse=false",
        &sets,
    );

    let folders = session
        .ext
        .load_folders_for_mailbox(&mailbox_with_folder_list("folders"))
        .await
        .unwrap();

    let tree: Vec<(String, usize)> = folders
        .indented_list()
        .iter()
        .map(|f| (f.folder.name.clone(), f.level))
        .collect();
    assert_eq!(
        tree,
        vec![
            (String::new(), 0),
            ("Work".to_owned(), 0),
            ("Projects".to_owned(), 1),
        ]
    );
    assert_eq!(
        folders
            .system_folder_by_type(MailSetKind::Inbox)
            .map(|f| f.entries.as_str()),
        Some("entries-inbox")
    );
}

#[tokio::test]
async fn received_body_reader_rejects_a_draft_before_fetching() {
    let session = session().await;
    let mut mail = fixture_mail(&session).await;
    mail.mailDetailsDraft = Some(IdTupleGenerated::new(
        GeneratedId("draft-list".into()),
        GeneratedId("draft-id".into()),
    ));

    let err = session.ext.load_mail_details_blob(&mail).await.unwrap_err();

    assert!(err.to_string().contains("Expected received mail"), "{err}");
    assert!(session.server.token_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_attachment_without_blobs_is_empty() {
    let session = session().await;
    let mail = fixture_mail(&session).await;
    let group_key = mail_group_key(&session, &mail).await;
    let file = TutanotaFile {
        _id: Some(IdTupleGenerated::new(
            GeneratedId("files".into()),
            GeneratedId("file".into()),
        )),
        _permissions: GeneratedId("permissions".into()),
        _format: 0,
        _ownerGroup: mail._ownerGroup.clone(),
        _ownerKeyVersion: mail._ownerKeyVersion,
        _ownerEncSessionKey: Some(wrap(&group_key, &test_session_key())),
        _kdfNonce: None,
        name: "empty.txt".into(),
        size: 0,
        mimeType: Some("text/plain".into()),
        cid: None,
        parent: None,
        subFiles: None,
        blobs: vec![],
        _errors: Default::default(),
    };

    assert!(session
        .ext
        .load_file_attachment_data(&file)
        .await
        .unwrap()
        .is_empty());
    assert!(session.server.token_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unencrypted_inline_entity_is_mapped_without_a_key() {
    let session = session().await;
    // MailSetEntry: _id, _permissions, _format, _ownerGroup, mail.
    let json = serde_json::json!({
        "1452": ["entries", "entry"],
        "1453": "permissions",
        "1454": "0",
        "1455": "owner",
        // Associations are arrays on the wire.
        "1456": [["mail-list", "mail-id"]],
    });

    let entry = session
        .ext
        .decrypt_inline_and_parse::<MailSetEntry>(&json.to_string())
        .await
        .unwrap()
        .expect("an unencrypted entity needs no session key");

    assert_eq!(entry.mail.element_id.as_str(), "mail-id");
}
