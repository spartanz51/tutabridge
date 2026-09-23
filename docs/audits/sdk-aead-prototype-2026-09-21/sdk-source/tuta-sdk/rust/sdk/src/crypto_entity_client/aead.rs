use super::CryptoEntityClient;
use crate::element_value::{ElementValue, ParsedEntity};
use crate::entities::entity_facade::{
	EntityDecryptionKeys, EntityDecryptionRequirements, BUCKET_KEY_FIELD, OWNER_GROUP_FIELD,
};
use crate::metamodel::TypeModel;
use crate::ApiCallError;

impl CryptoEntityClient {
	/// Decrypt an already parsed response, optionally using an inherited session key.
	/// Group-key attributes use their own instance owner and ciphertext key version.
	pub async fn decrypt_parsed(
		&self,
		type_ref: &crate::TypeRef,
		entity: ParsedEntity,
		inherited_session_key: Option<crate::crypto::crypto_facade::ResolvedSessionKey>,
	) -> Result<ParsedEntity, ApiCallError> {
		let model = self.entity_client.resolve_server_type_ref(type_ref)?;
		self.process_entity_with_group_keys(&model, entity, inherited_session_key)
			.await
	}

	/// Group-key attributes do not require an owner-encrypted session key.
	pub(super) async fn process_entity_with_group_keys(
		&self,
		model: &TypeModel,
		entity: ParsedEntity,
		inherited_session_key: Option<crate::crypto::crypto_facade::ResolvedSessionKey>,
	) -> Result<ParsedEntity, ApiCallError> {
		let requirements =
			EntityDecryptionRequirements::with_resolver(model, &entity, &|reference| {
				self.entity_client.resolve_server_type_ref(reference)
			})?;
		// Bucket-key resolution also establishes sender identity and caches keys for
		// attachments. Preserve that work even if every attribute is group-encrypted.
		let has_bucket_key = model
			.get_attribute_id_by_attribute_name(BUCKET_KEY_FIELD)
			.ok()
			.and_then(|id| entity.get(&id))
			.is_some_and(
				|value| matches!(value, ElementValue::Array(values) if !values.is_empty()),
			);
		let mut keys = EntityDecryptionKeys::default();
		if requirements.session_key || has_bucket_key {
			keys.session_key = match inherited_session_key {
				Some(key) if !has_bucket_key => Some(key),
				_ => self
					.crypto_facade
					.resolve_session_key(&entity, model)
					.await
					.map_err(|_| {
						ApiCallError::internal(
							"Failed to resolve required attribute session key".into(),
						)
					})?,
			};
			if keys.session_key.is_none() {
				return Err(ApiCallError::internal(
					"Missing required attribute session key".into(),
				));
			}
		}
		if !requirements.group_key_versions.is_empty() {
			let owner_id = model.get_attribute_id_by_attribute_name(OWNER_GROUP_FIELD)?;
			let Some(ElementValue::IdGeneratedId(owner)) = entity.get(&owner_id) else {
				return Err(ApiCallError::internal("Missing AEAD owner group".into()));
			};
			for version in requirements.group_key_versions {
				let key = self
					.key_loader_facade
					.load_sym_group_key(owner, version, None)
					.await
					.map_err(|_| {
						ApiCallError::internal(format!(
							"Failed to load AEAD group key version {version}"
						))
					})?;
				keys.group_keys.insert(version, key);
			}
		}
		let sender_identity = keys
			.session_key
			.as_ref()
			.and_then(|key| key.sender_identity_pub_key.clone());
		let mut decrypted = self
			.entity_facade
			.decrypt_and_map_with_keys(model, entity, keys)?;
		if let Some(status) = self
			.get_encryption_auth_status_or_none(model, &mut decrypted, sender_identity)
			.await?
		{
			let id = model.get_attribute_id_by_attribute_name("encryptionAuthStatus")?;
			decrypted.insert(id, ElementValue::Number(status));
		}
		Ok(decrypted)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bindings::{file_client::MockFileClient, rest_client::MockRestClient};
	use crate::crypto::{
		asymmetric_crypto_facade::MockAsymmetricCryptoFacade, crypto_facade::MockCryptoFacade,
	};
	use crate::entities::{entity_facade::MockEntityFacade, generated::tutanota::Mail, Entity};
	use crate::entity_client::MockEntityClient;
	use crate::instance_mapper::InstanceMapper;
	use crate::key_loader_facade::MockKeyLoaderFacade;
	use crate::type_model_provider::TypeModelProvider;
	use crate::GeneratedId;
	use crypto_primitives::key::GenericAesKey;
	use std::sync::Arc;

	#[tokio::test]
	async fn loads_ciphertext_group_versions_without_requesting_a_session_key() {
		let provider = Arc::new(TypeModelProvider::new_test(
			Arc::new(MockRestClient::new()),
			Arc::new(MockFileClient::new()),
			"localhost".into(),
		));
		let mut model = (*provider.resolve_server_type_ref(&Mail::type_ref()).unwrap()).clone();
		model.id = 99999.into(); // No mail sender-authentication step in this key-loading test.
		model.associations.clear();
		let mut subject = model.values[&105.into()].clone();
		subject.id = 106.into();
		model.values.insert(106.into(), subject);
		let entity = ParsedEntity::from([
			("105".into(), ElementValue::Bytes(vec![2, 0, 7])),
			("106".into(), ElementValue::Bytes(vec![2, 0, 8])),
			(
				"587".into(),
				ElementValue::IdGeneratedId(GeneratedId("owner".into())),
			),
			("1839".into(), ElementValue::Bytes(vec![0x22; 32])),
		]);
		let mut loader = MockKeyLoaderFacade::default();
		for version in [7, 8] {
			loader
				.expect_load_sym_group_key()
				.withf(move |owner, requested, current| {
					owner.as_str() == "owner" && *requested == version && current.is_none()
				})
				.times(1)
				.returning(move |_, _, _| {
					Ok(GenericAesKey::from_bytes(&[version as u8; 32]).unwrap())
				});
		}
		let mut facade = MockEntityFacade::default();
		facade
			.expect_decrypt_and_map_with_keys()
			.times(1)
			.withf(|_, _, keys| {
				keys.session_key.is_none()
					&& keys.group_keys.len() == 2
					&& keys.group_keys.contains_key(&7)
					&& keys.group_keys.contains_key(&8)
			})
			.returning(|_, entity, _| Ok(entity));
		let client = CryptoEntityClient::new(
			Arc::new(MockEntityClient::default()),
			Arc::new(facade),
			Arc::new(MockCryptoFacade::default()),
			Arc::new(InstanceMapper::new(provider)),
			Arc::new(MockAsymmetricCryptoFacade::default()),
			Arc::new(loader),
		);
		client
			.process_entity_with_group_keys(&model, entity, None)
			.await
			.unwrap();
	}
}
