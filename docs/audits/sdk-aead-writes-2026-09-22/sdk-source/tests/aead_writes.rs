//! AEAD writes through the real model mapper, serializer and request pipeline.
//! All transport responses and login credentials are public upstream test fixtures.
use base64::{prelude::BASE64_STANDARD, Engine};
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::{test_util::DeterministicRng, RandomizerFacade};
use crypto_primitives::versioned::Versioned;
use std::collections::HashMap;
use std::sync::{
	atomic::{AtomicU32, Ordering},
	Arc, Mutex,
};
use tutasdk::bindings::rest_client::{
	HttpMethod, RestClient, RestClientError, RestClientOptions, RestResponse,
};
use tutasdk::bindings::test_file_client::TestFileClient;
use tutasdk::bindings::test_rest_client::TestRestClient;
use tutasdk::crypto_entity_client::AeadEntityWriter;
use tutasdk::entities::generated::tutanota::Mail;
use tutasdk::login::{CredentialType, Credentials};
use tutasdk::{GeneratedId, IdTupleGenerated, LoggedInSdk, Sdk};

struct Transport {
	fixtures: Arc<dyn RestClient>,
	mail: Mutex<serde_json::Value>,
	requests: Mutex<Vec<(String, HttpMethod, serde_json::Value)>>,
	nonce: Mutex<Vec<u8>>,
	nonce_status: AtomicU32,
}

#[async_trait::async_trait]
impl RestClient for Transport {
	async fn request_binary(
		&self,
		url: String,
		method: HttpMethod,
		options: RestClientOptions,
	) -> Result<RestResponse, RestClientError> {
		if url.ends_with("/rest/sys/updatekdfnonceservice") {
			let body = serde_json::from_slice(options.body.as_ref().unwrap()).unwrap();
			self.requests.lock().unwrap().push((url, method, body));
			return Ok(RestResponse {
				status: self.nonce_status.load(Ordering::SeqCst),
				headers: HashMap::new(),
				body: Some(
					serde_json::to_vec(
						&serde_json::json!({"2756":"0", "2757":BASE64_STANDARD.encode(&*self.nonce.lock().unwrap())}),
					)
					.unwrap(),
				),
			});
		}
		if url.contains("/rest/tutanota/Mail/") || url.contains("/rest/tutanota/mail/") {
			if method == HttpMethod::GET {
				return Ok(RestResponse {
					status: 200,
					headers: HashMap::new(),
					body: Some(serde_json::to_vec(&*self.mail.lock().unwrap()).unwrap()),
				});
			}
			let body: serde_json::Value =
				serde_json::from_slice(options.body.as_ref().unwrap()).unwrap();
			let status = if method == HttpMethod::POST { 201 } else { 204 };
			let mut stored = body.clone();
			if method == HttpMethod::POST {
				stored["99"] = serde_json::json!(["O1qC705-17-0", "O1qC7an--3-0"]);
				stored["100"] = self.mail.lock().unwrap()["100"].clone();
			}
			*self.mail.lock().unwrap() = stored;
			self.requests.lock().unwrap().push((url, method, body));
			return Ok(RestResponse {
				status,
				headers: HashMap::new(),
				body: Some(br#"{"1":"0","2":"created","3":"permissions"}"#.to_vec()),
			});
		}
		self.fixtures.request_binary(url, method, options).await
	}
}

async fn session() -> (Arc<LoggedInSdk>, Arc<Transport>) {
	let mut fixtures = TestRestClient::new("http://localhost:9000");
	fixtures.insert_response("http://localhost:9000/rest/sys/Session/O1qC702-1J-0/3u3i8Lr9_7TnDDdAVw7w3TypTD2k1L00vIUTMF0SIPY", HttpMethod::GET, 200, HashMap::new(), Some(include_bytes!("download_mail_test/session.json")));
	fixtures.insert_response(
		"http://localhost:9000/rest/sys/User/O1qC700----0",
		HttpMethod::GET,
		200,
		HashMap::new(),
		Some(include_bytes!("download_mail_test/user.json")),
	);
	let rest = Arc::new(Transport {
		fixtures: Arc::new(fixtures),
		mail: Mutex::new(
			serde_json::from_str(include_str!("download_mail_test/mail.json")).unwrap(),
		),
		requests: Mutex::new(vec![]),
		nonce: Mutex::new(vec![0x55; 32]),
		nonce_status: AtomicU32::new(200),
	});
	let credentials = Credentials {
		login:"bed-free@tutanota.de".into(), user_id:GeneratedId("O1qC700----0".into()), access_token:"ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ".into(),
		encrypted_passphrase_key: BASE64_STANDARD.decode("AZWEA/KTrHu0bW52CsctsBTTV4U3jrU51TadSxf6Nqs3xbEs3WfoOpPtxUDCNjHNppt6LHCfgTioejjGUJ2cCsXosZAysUiau5Nvyi8mtjLz").unwrap(), credential_type:CredentialType::Internal,
	};
	let sdk = Sdk::new(
		"http://localhost:9000".into(),
		rest.clone(),
		Arc::new(TestFileClient::default()),
	);
	(sdk.login(credentials).await.unwrap(), rest)
}
fn mail_id() -> IdTupleGenerated {
	IdTupleGenerated::new(
		GeneratedId("O1qC705-17-0".into()),
		GeneratedId("O1qC7an--3-0".into()),
	)
}
fn randomizer() -> RandomizerFacade {
	RandomizerFacade::from_core(DeterministicRng(0x44))
}

#[tokio::test]
async fn migration_uses_server_nonce_then_existing_nonce_update_skips_service() {
	let (logged, rest) = session().await;
	let client = logged.mail_facade().get_crypto_entity_client();
	let mut mail: Mail = client.load(&mail_id()).await.unwrap();
	let sender = mail.sender.clone();
	mail.subject = "AEAD écrit en Rust — 🦀".into();
	let writer = AeadEntityWriter::new(&client, logged.get_service_executor(), randomizer());
	let mut expected = mail.clone();
	expected._kdfNonce = Some(vec![0x55; 32]);
	writer.update_instance(mail, None).await.unwrap();
	{
		let requests = rest.requests.lock().unwrap();
		assert_eq!(requests.len(), 2);
		let migration = &requests[0].2["2754"][0];
		assert_eq!(migration["2749"], "O1qC705-17-0");
		assert_eq!(migration["2750"], "O1qC7an--3-0");
		assert_eq!(migration["2748"][0]["1871"], "tutanota");
		assert_eq!(migration["2748"][0]["1872"], "97");
		assert_eq!(
			BASE64_STANDARD
				.decode(migration["2751"].as_str().unwrap())
				.unwrap(),
			[0x44; 32]
		);
		assert_eq!(
			BASE64_STANDARD
				.decode(requests[1].2["1839"].as_str().unwrap())
				.unwrap(),
			[0x55; 32]
		);
		assert_eq!(
			BASE64_STANDARD
				.decode(requests[1].2["105"].as_str().unwrap())
				.unwrap()[0],
			2
		);
	}
	let mut read: Mail = client.load(&mail_id()).await.unwrap();
	assert_eq!(read, expected);
	assert_eq!(read.subject, "AEAD écrit en Rust — 🦀");
	assert_eq!(read.sender, sender);
	read.subject = String::new();
	// Existing public update entry point must preserve AEAD, including empty values.
	client.update_instance(read).await.unwrap();
	let read: Mail = client.load(&mail_id()).await.unwrap();
	assert_eq!(read.subject, "");
	assert_eq!(rest.requests.lock().unwrap().len(), 3);
	assert_eq!(
		BASE64_STANDARD
			.decode(rest.mail.lock().unwrap()["105"].as_str().unwrap())
			.unwrap()[0],
		2
	);
}

#[tokio::test]
async fn create_replaces_copied_nonce_and_generates_aggregate_id() {
	let (logged, rest) = session().await;
	let client = logged.mail_facade().get_crypto_entity_client();
	let mut mail: Mail = client.load(&mail_id()).await.unwrap();
	mail._kdfNonce = Some(vec![0x22; 32]);
	mail.sender._id = None;
	let writer = AeadEntityWriter::new(&client, logged.get_service_executor(), randomizer());
	let created = writer.create_instance(mail, None).await.unwrap();
	assert_eq!(created.generatedId.unwrap().as_str(), "created");
	assert_eq!(rest.requests.lock().unwrap().len(), 1);
	let raw = rest.mail.lock().unwrap().clone();
	assert_eq!(
		BASE64_STANDARD
			.decode(raw["1839"].as_str().unwrap())
			.unwrap(),
		[0x44; 32]
	);
	assert!(raw["111"][0]["93"]
		.as_str()
		.is_some_and(|id| !id.is_empty()));
	let read: Mail = client.load(&mail_id()).await.unwrap();
	assert_eq!(read.sender.name, "Matthias");
	assert_eq!(read.subject, "Html email features");
}

#[tokio::test]
async fn failed_or_invalid_nonce_registration_never_puts_the_entity() {
	for (status, nonce) in [(500, vec![0x55; 32]), (200, vec![0x55; 31])] {
		let (logged, rest) = session().await;
		rest.nonce_status.store(status, Ordering::SeqCst);
		*rest.nonce.lock().unwrap() = nonce;
		let client = logged.mail_facade().get_crypto_entity_client();
		let mail: Mail = client.load(&mail_id()).await.unwrap();
		let writer = AeadEntityWriter::new(&client, logged.get_service_executor(), randomizer());
		assert!(writer.update_instance(mail, None).await.is_err());
		let requests = rest.requests.lock().unwrap();
		assert_eq!(requests.len(), 1);
		assert!(requests[0].0.ends_with("updatekdfnonceservice"));
	}
}

#[tokio::test]
async fn malformed_nonce_and_unsupported_key_version_do_not_register_or_write() {
	let (logged, rest) = session().await;
	let client = logged.mail_facade().get_crypto_entity_client();
	let mut mail: Mail = client.load(&mail_id()).await.unwrap();
	mail._kdfNonce = Some(vec![0; 31]);
	let writer = AeadEntityWriter::new(&client, logged.get_service_executor(), randomizer());
	assert!(writer.update_instance(mail.clone(), None).await.is_err());
	mail._kdfNonce = None;
	assert!(writer
		.update_instance(
			mail,
			Some(Versioned {
				object: GenericAesKey::from_bytes(&[0x11; 32]).unwrap(),
				version: 256
			})
		)
		.await
		.is_err());
	assert!(rest.requests.lock().unwrap().is_empty());
}
