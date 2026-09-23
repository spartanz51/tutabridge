use super::{
	decryption::{cipher_version, CipherVersion},
	ID_FIELD, KDF_NONCE_FIELD,
};
use crate::crypto::crypto_facade::ResolvedSessionKey;
use crate::element_value::{ElementValue, ParsedEntity};
use crate::metamodel::{AssociationType, TypeModel};
use crate::type_model_provider::TypeModelProvider;
use crate::{ApiCallError, TypeRef};
use crypto_primitives::key::GenericAesKey;
use std::collections::{BTreeSet, HashMap};

/// Keys loaded for this instance only; group keys are indexed by the ciphertext's version.
#[derive(Default)]
pub struct EntityDecryptionKeys {
	pub session_key: Option<ResolvedSessionKey>,
	pub group_keys: HashMap<u64, GenericAesKey>,
}

#[derive(Default)]
pub struct EntityDecryptionRequirements {
	pub session_key: bool,
	pub group_key_versions: BTreeSet<u64>,
}

impl EntityDecryptionRequirements {
	/// Inspect encrypted attributes and aggregates, never arbitrary byte-valued metadata.
	pub fn for_entity(
		model: &TypeModel,
		entity: &ParsedEntity,
		provider: &TypeModelProvider,
	) -> Result<Self, ApiCallError> {
		Self::with_resolver(model, entity, &|reference| {
			provider
				.resolve_server_type_ref(reference)
				.ok_or_else(|| ApiCallError::internal("Missing aggregate type model".into()))
		})
	}

	pub(crate) fn with_resolver<F>(
		model: &TypeModel,
		entity: &ParsedEntity,
		resolve: &F,
	) -> Result<Self, ApiCallError>
	where
		F: Fn(&TypeRef) -> Result<std::sync::Arc<TypeModel>, ApiCallError>,
	{
		kdf_nonce(model, entity)?;
		let mut result = Self::default();
		result.visit(model, entity, resolve)?;
		Ok(result)
	}

	fn visit<F>(
		&mut self,
		model: &TypeModel,
		entity: &ParsedEntity,
		resolve: &F,
	) -> Result<(), ApiCallError>
	where
		F: Fn(&TypeRef) -> Result<std::sync::Arc<TypeModel>, ApiCallError>,
	{
		for (id, value) in &model.values {
			if !value.encrypted || value.name == ID_FIELD {
				continue;
			}
			if let Some(ElementValue::Bytes(bytes)) = entity.get(&String::from(*id)) {
				match cipher_version(bytes)? {
					CipherVersion::Legacy | CipherVersion::SessionKey => self.session_key = true,
					CipherVersion::GroupKey(version) => {
						self.group_key_versions.insert(version);
					},
				}
			}
		}
		for (id, association) in &model.associations {
			if association.association_type != AssociationType::Aggregation {
				continue;
			}
			let child_model = resolve(&TypeRef::new(
				association.dependency.unwrap_or(model.app),
				association.ref_type_id,
			))?;
			match entity.get(&String::from(*id)) {
				Some(ElementValue::Array(aggregates)) => {
					for aggregate in aggregates {
						let ElementValue::Dict(child) = aggregate else {
							return Err(ApiCallError::internal("Invalid aggregate value".into()));
						};
						self.visit(&child_model, child, resolve)?;
					}
				},
				None | Some(ElementValue::Null) => {},
				_ => {
					return Err(ApiCallError::internal(
						"Invalid aggregate collection".into(),
					))
				},
			}
		}
		Ok(())
	}
}

#[must_use]
pub fn has_kdf_nonce(model: &TypeModel, entity: &ParsedEntity) -> bool {
	model
		.get_attribute_id_by_attribute_name(KDF_NONCE_FIELD)
		.ok()
		.and_then(|id| entity.get(&id))
		.is_some_and(|value| !matches!(value, ElementValue::Null))
}

pub(super) fn kdf_nonce<'a>(
	model: &TypeModel,
	entity: &'a ParsedEntity,
) -> Result<Option<&'a [u8]>, ApiCallError> {
	let value = model
		.get_attribute_id_by_attribute_name(KDF_NONCE_FIELD)
		.ok()
		.and_then(|id| entity.get(&id));
	match value {
		None | Some(ElementValue::Null) => Ok(None),
		Some(ElementValue::Bytes(bytes)) if bytes.len() == 32 => Ok(Some(bytes)),
		_ => Err(ApiCallError::internal(
			"Invalid AEAD KDF nonce length or type".into(),
		)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bindings::{file_client::MockFileClient, rest_client::MockRestClient};
	use crate::entities::entity_facade::{EntityFacade, EntityFacadeImpl};
	use crate::entities::{generated::tutanota::Mail, Entity};
	use crypto_primitives::randomizer_facade::RandomizerFacade;
	use std::sync::Arc;

	#[test]
	fn requirements_ignore_unencrypted_key_metadata_and_deduplicate_versions() {
		let provider = TypeModelProvider::new_test(
			Arc::new(MockRestClient::new()),
			Arc::new(MockFileClient::new()),
			"localhost".into(),
		);
		let mut model = (*provider.resolve_server_type_ref(&Mail::type_ref()).unwrap()).clone();
		model.associations.clear();
		let mut subject = model.values[&105.into()].clone();
		subject.id = 106.into();
		model.values.insert(106.into(), subject);
		let mut entity = ParsedEntity::from([
			("105".into(), ElementValue::Bytes(vec![2, 0, 7])),
			("106".into(), ElementValue::Bytes(vec![2, 0, 7])),
			("102".into(), ElementValue::Bytes(vec![2, 0, 99])),
			("1839".into(), ElementValue::Bytes(vec![0x22; 32])),
		]);
		let requirements =
			EntityDecryptionRequirements::for_entity(&model, &entity, &provider).unwrap();
		assert!(!requirements.session_key);
		assert_eq!(requirements.group_key_versions, BTreeSet::from([7]));
		entity.insert("106".into(), ElementValue::Bytes(vec![3; 53]));
		assert!(
			EntityDecryptionRequirements::for_entity(&model, &entity, &provider)
				.unwrap()
				.session_key
		);
	}

	#[test]
	fn invalid_nonce_is_rejected_and_group_key_entities_are_not_written_as_cbc() {
		let provider = Arc::new(TypeModelProvider::new_test(
			Arc::new(MockRestClient::new()),
			Arc::new(MockFileClient::new()),
			"localhost".into(),
		));
		let model = provider.resolve_server_type_ref(&Mail::type_ref()).unwrap();
		let mut entity = ParsedEntity::new();
		for value in [
			ElementValue::Bytes(vec![]),
			ElementValue::Bytes(vec![0; 31]),
			ElementValue::String("invalid".into()),
		] {
			entity.insert("1839".into(), value);
			assert!(kdf_nonce(&model, &entity).is_err());
		}
		entity.insert("1839".into(), ElementValue::Bytes(vec![0x22; 32]));
		let decoder =
			EntityFacadeImpl::new(provider, RandomizerFacade::from_core(rand_core::OsRng));
		let key = GenericAesKey::from_bytes(&[0x11; 32]).unwrap();
		assert!(decoder
			.encrypt_and_map(&model, &entity, &key)
			.unwrap_err()
			.to_string()
			.contains("writes are not supported"));
	}
}
