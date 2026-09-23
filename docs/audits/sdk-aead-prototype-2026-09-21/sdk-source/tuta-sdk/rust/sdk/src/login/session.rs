//! Interactive session creation, kept separate from SDK construction.
use crate::crypto::crypto_facade::create_auth_verifier;
use crate::entities::entity_facade::EntityFacadeImpl;
use crate::entities::generated::sys::{
	Challenge, CreateSessionReturn, SecondFactorAuthData, SecondFactorAuthGetData,
};
use crate::entities::generated::sys::{CreateSessionData, SaltData};
use crate::login::login_facade::parse_session_id;
use crate::login::login_facade::{derive_user_passphrase_key, KdfType};
use crate::login::{CredentialType, Credentials, LoginError};
use crate::services::generated::sys::SecondFactorAuthService;
use crate::services::generated::sys::{SaltService, SessionService};
use crate::services::{service_executor::ServiceExecutor, ExtraServiceParams};
use crate::tutanota_constants::SecondFactorType;
use crate::{ApiCallError, HeadersProvider, Sdk};
use crypto_primitives::{
	aes::{Aes256Key, InitializationVector},
	key::GenericAesKey,
	randomizer_facade::RandomizerFacade,
};
use std::sync::Arc;

/// Response returned by [`Sdk::initiate_session`]. When `challenges` is empty
/// the caller can directly pass `credentials` to [`Sdk::login`]. Otherwise
/// the second factor challenges must first be resolved.
#[derive(uniffi::Record, Clone)]
pub struct SessionInitResponse {
	pub credentials: Credentials,
	pub challenges: Vec<Challenge>,
}

#[uniffi::export]
impl Sdk {
	/// Creates a persistent Argon2 session without resuming login.
	/// The caller supplies the client name displayed in the session list.
	pub async fn initiate_session(
		&self,
		mail_address: &str,
		passphrase: &str,
		client_identifier: &str,
	) -> Result<SessionInitResponse, LoginError> {
		// Mirror TS `LoginFacade.createSession`, which normalizes the mail
		// address before deriving the salt and creating the session.
		let mail_address = mail_address.trim().to_lowercase();
		let service_executor = self.make_unauthenticated_service_executor();
		let salt_return = service_executor
			.get::<SaltService>(
				SaltData {
					_format: 0,
					mailAddress: mail_address.clone(),
				},
				ExtraServiceParams::default(),
			)
			.await?;

		if !matches!(
			KdfType::try_from(salt_return.kdfVersion)?,
			KdfType::Argon2id
		) {
			return Err(LoginError::InvalidKey {
				error_message: "Unsupported password KDF".to_owned(),
			});
		}

		let Ok(salt) = salt_return.salt.try_into() else {
			return Err(LoginError::InvalidKey {
				error_message: "salt has wrong length".to_string(),
			});
		};

		let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
		let access_key = Aes256Key::generate(&randomizer);
		let user_passphrase_key = derive_user_passphrase_key(KdfType::Argon2id, passphrase, salt);
		let auth_verifier = create_auth_verifier(user_passphrase_key.clone());
		let session_data = CreateSessionData {
			_format: 0,
			accessKey: Some(access_key.as_bytes().to_vec()),
			authToken: None,
			authVerifier: Some(auth_verifier),
			clientIdentifier: client_identifier.to_owned(),
			mailAddress: Some(mail_address.clone()),
			recoverCodeVerifier: None,
			user: None,
		};
		let encrypted_passphrase_key = GenericAesKey::Aes256(access_key).encrypt_key(
			&GenericAesKey::Aes256(user_passphrase_key),
			InitializationVector::generate(&randomizer),
		);
		let session_return: CreateSessionReturn = service_executor
			.post::<SessionService>(session_data, ExtraServiceParams::default())
			.await?;

		Ok(SessionInitResponse {
			credentials: Credentials {
				login: mail_address,
				user_id: session_return.user.clone(),
				access_token: session_return.accessToken.clone(),
				encrypted_passphrase_key,
				credential_type: CredentialType::Internal,
			},
			challenges: session_return.challenges,
		})
	}

	/// Submits the numeric TOTP for a pending session using the generated service.
	pub async fn authenticate_with_second_factor_totp(
		&self,
		access_token: &str,
		totp_code: u32,
	) -> Result<(), LoginError> {
		if totp_code >= 1_000_000 {
			return Err(
				ApiCallError::internal("TOTP must contain at most six digits".to_owned()).into(),
			);
		}
		let session_id =
			parse_session_id(access_token).map_err(|e| LoginError::InvalidAccessToken {
				error_message: format!("{e}"),
			})?;

		let service_executor = self.make_unauthenticated_service_executor();
		let auth_data = SecondFactorAuthData {
			_format: 0,
			r#type: Some(SecondFactorType::Totp.into()),
			otpCode: Some(i64::from(totp_code)),
			session: Some(session_id),
			u2f: None,
			webauthn: None,
		};
		service_executor
			.post::<SecondFactorAuthService>(auth_data, ExtraServiceParams::default())
			.await?;
		Ok(())
	}

	/// Performs one challenge-status request; the caller owns polling and cancellation.
	pub async fn is_second_factor_pending(&self, access_token: &str) -> Result<bool, LoginError> {
		let service_executor = self.make_unauthenticated_service_executor();
		let result = service_executor
			.get::<SecondFactorAuthService>(
				SecondFactorAuthGetData {
					_format: 0,
					accessToken: access_token.to_string(),
				},
				ExtraServiceParams::default(),
			)
			.await?;
		Ok(result.secondFactorPending)
	}
}
impl Sdk {
	fn make_unauthenticated_service_executor(&self) -> ServiceExecutor {
		let headers_provider = Arc::new(HeadersProvider::new(None));
		let entity_facade = Arc::new(EntityFacadeImpl::new(
			self.type_model_provider.clone(),
			RandomizerFacade::from_core(rand_core::OsRng),
		));
		ServiceExecutor::new(
			headers_provider,
			None,
			entity_facade,
			self.instance_mapper.clone(),
			self.json_serializer.clone(),
			self.rest_client.clone(),
			self.type_model_provider.clone(),
			self.base_url.clone(),
		)
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	use crate::bindings::{file_client::MockFileClient, rest_client::MockRestClient};

	#[test]
	fn second_factor_type_values_match_typescript() {
		assert_eq!(i64::from(SecondFactorType::U2f), 0);
		assert_eq!(i64::from(SecondFactorType::Totp), 1);
		assert_eq!(i64::from(SecondFactorType::Webauthn), 2);
	}

	fn make_test_sdk() -> Sdk {
		Sdk::new(
			"http://localhost:9000".to_string(),
			Arc::new(MockRestClient::default()),
			Arc::new(MockFileClient::default()),
		)
	}

	#[tokio::test]
	async fn authenticate_with_second_factor_totp_rejects_invalid_access_token() {
		let sdk = make_test_sdk();
		let err = sdk
			.authenticate_with_second_factor_totp("not-base64!", 123_456)
			.await
			.expect_err("must reject invalid access token");
		assert!(
			matches!(err, LoginError::InvalidAccessToken { .. }),
			"expected InvalidAccessToken, got: {err:?}",
		);
	}
}
