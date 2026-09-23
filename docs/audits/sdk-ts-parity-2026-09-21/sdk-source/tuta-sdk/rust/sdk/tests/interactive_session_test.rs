//! Session creation uses an injected in-memory transport, never a real account.
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};
use tutasdk::{
	bindings::{
		rest_client::{HttpMethod, RestClient, RestClientError, RestClientOptions, RestResponse},
		test_file_client::TestFileClient,
		test_rest_client::TestRestClient,
	},
	Sdk,
};
struct Rest {
	model: TestRestClient,
	kdf: i64,
	posts: Mutex<Vec<serde_json::Value>>,
	challenge: bool,
}
#[async_trait]
impl RestClient for Rest {
	async fn request_binary(
		&self,
		url: String,
		method: HttpMethod,
		options: RestClientOptions,
	) -> Result<RestResponse, RestClientError> {
		let body = if url.contains("/saltservice") {
			serde_json::json!({"421":"0","422":STANDARD.encode([7;16]),"2133":self.kdf.to_string()})
		} else if url.ends_with("/sessionservice") {
			assert_eq!(method, HttpMethod::POST);
			self.posts
				.lock()
				.unwrap()
				.push(serde_json::from_slice(&options.body.unwrap()).unwrap());
			serde_json::json!({"1220":"0","1221":"ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ","1222":if self.challenge {vec![serde_json::json!({"1188":"challenge","1189":"1","1190":[],"1247":[]})]} else {vec![]},"1223":["user"]})
		} else if url.ends_with("/secondfactorauthservice") && method == HttpMethod::POST {
			self.posts
				.lock()
				.unwrap()
				.push(serde_json::from_slice(&options.body.unwrap()).unwrap());
			return Ok(RestResponse {
				status: 200,
				headers: HashMap::new(),
				body: None,
			});
		} else if url.contains("/secondfactorauthservice?") {
			serde_json::json!({"1237":"0","1238":"0"})
		} else {
			return self.model.request_binary(url, method, options).await;
		};
		Ok(RestResponse {
			status: 200,
			headers: HashMap::new(),
			body: Some(serde_json::to_vec(&body).unwrap()),
		})
	}
}
fn sdk(kdf: i64, challenge: bool) -> (Sdk, Arc<Rest>) {
	let rest = Arc::new(Rest {
		model: TestRestClient::new("http://test"),
		kdf,
		posts: Mutex::new(vec![]),
		challenge,
	});
	(
		Sdk::new(
			"http://test".into(),
			rest.clone(),
			Arc::new(TestFileClient::default()),
		),
		rest,
	)
}
#[tokio::test]
async fn initiation_returns_credentials_and_challenges_without_resuming_login() {
	for challenge in [false, true] {
		let (sdk, rest) = sdk(1, challenge);
		let result = sdk
			.initiate_session(" USER@example.test ", "test password", "Test client")
			.await
			.unwrap();
		assert_eq!(result.credentials.login, "user@example.test");
		assert_eq!(result.challenges.len(), usize::from(challenge));
		assert!(!result.credentials.encrypted_passphrase_key.is_empty());
		let posts = rest.posts.lock().unwrap();
		assert_eq!(posts.len(), 1);
		assert_eq!(posts[0]["1215"], "Test client");
		assert_eq!(posts[0]["1213"], "user@example.test");
		assert_eq!(
			STANDARD
				.decode(posts[0]["1216"].as_str().unwrap())
				.unwrap()
				.len(),
			32
		);
	}
}
#[tokio::test]
async fn unsupported_kdf_fails_before_creating_a_session() {
	for kdf in [0, 99] {
		let (sdk, rest) = sdk(kdf, false);
		assert!(sdk
			.initiate_session("user@example.test", "test password", "Test client")
			.await
			.is_err());
		assert!(rest.posts.lock().unwrap().is_empty());
	}
}
#[tokio::test]
async fn totp_serialization_and_polling_use_existing_services() {
	let (sdk, rest) = sdk(1, false);
	let token = "ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ";
	assert!(sdk
		.authenticate_with_second_factor_totp(token, 1_000_000)
		.await
		.is_err());
	assert!(rest.posts.lock().unwrap().is_empty());
	sdk.authenticate_with_second_factor_totp(token, 123)
		.await
		.unwrap();
	{
		let posts = rest.posts.lock().unwrap();
		assert_eq!(posts[0]["1230"], "1");
		assert_eq!(posts[0]["1243"], "123");
		assert_eq!(
			posts[0]["1232"],
			serde_json::json!([[
				"O1qC702-1J-0",
				"3u3i8Lr9_7TnDDdAVw7w3TypTD2k1L00vIUTMF0SIPY"
			]])
		);
	}

	assert!(!sdk.is_second_factor_pending(token).await.unwrap());
}
