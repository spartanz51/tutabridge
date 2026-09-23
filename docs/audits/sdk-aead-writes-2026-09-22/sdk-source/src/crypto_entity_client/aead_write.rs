use super::CryptoEntityClient;
use crate::element_value::{ElementValue, ParsedEntity};
use crate::entities::entity_facade::{
	make_random_aggregate_id, AeadEncryptionKey, ID_FIELD, KDF_NONCE_FIELD, OWNER_GROUP_FIELD,
};
use crate::entities::generated::base::PersistenceResourcePostReturn;
use crate::entities::generated::sys::{InstanceKdfNonce, TypeInfo, UpdateKdfNoncePostIn};
use crate::entities::Entity;
use crate::metamodel::TypeModel;
use crate::services::generated::sys::UpdateKdfNonceService;
#[cfg_attr(test, mockall_double::double)]
use crate::services::service_executor::ServiceExecutor;
use crate::services::ExtraServiceParams;
use crate::ApiCallError;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use crypto_primitives::versioned::VersionedAesKey;
use serde::Serialize;

/// Writes persistent entities after the caller has selected AEAD for the account.
/// Service payloads retain their existing session-key CBC path, as in TypeScript.
/// The optional owner key is a caller-resolved key, for shared-owner contexts.
/// On update, a supplied key must use the version selected by the owner key
/// provider (the instance's owner-key version in the TypeScript path).
/// This facade does not enable account rollout flags or change the default
/// encryption scheme for new entities.
pub struct AeadEntityWriter<'a> {
	client: &'a CryptoEntityClient,
	service_executor: &'a ServiceExecutor,
	randomizer: RandomizerFacade,
}

impl<'a> AeadEntityWriter<'a> {
	#[must_use]
	pub fn new(
		client: &'a CryptoEntityClient,
		service_executor: &'a ServiceExecutor,
		randomizer: RandomizerFacade,
	) -> Self {
		Self {
			client,
			service_executor,
			randomizer,
		}
	}

	pub async fn create_instance<T: Entity + Serialize>(
		&self,
		instance: T,
		owner_key: Option<VersionedAesKey>,
	) -> Result<PersistenceResourcePostReturn, ApiCallError> {
		let type_ref = T::type_ref();
		let model = self
			.client
			.entity_client
			.resolve_client_type_ref(&type_ref)?;
		let mut entity = self
			.client
			.instance_mapper
			.serialize_entity(instance)
			.map_err(|error| {
				ApiCallError::internal_with_err(error, "Failed to map AEAD instance")
			})?;
		if model.is_encrypted() {
			let key = self
				.client
				.aead_owner_key(model, &entity, owner_key)
				.await?;
			let nonce_id = model.get_attribute_id_by_attribute_name(KDF_NONCE_FIELD)?;
			// A new instance must not inherit a copied instance's key derivation nonce.
			entity.insert(
				nonce_id,
				ElementValue::Bytes(self.randomizer.generate_random_array::<32>().to_vec()),
			);
			entity = self.client.entity_facade.encrypt_and_map_with_aead(
				model,
				&entity,
				&AeadEncryptionKey::GroupKey(key),
			)?;
		}
		self.client
			.entity_client
			.create_instance(&type_ref, entity, &self.client.instance_mapper)
			.await
	}

	pub async fn update_instance<T: Entity + Serialize>(
		&self,
		instance: T,
		owner_key: Option<VersionedAesKey>,
	) -> Result<(), ApiCallError> {
		let type_ref = T::type_ref();
		let model = self
			.client
			.entity_client
			.resolve_client_type_ref(&type_ref)?;
		let mut entity = self
			.client
			.instance_mapper
			.serialize_entity(instance)
			.map_err(|error| {
				ApiCallError::internal_with_err(error, "Failed to map AEAD instance")
			})?;
		if model.is_encrypted() {
			let key = self
				.client
				.aead_owner_key(model, &entity, owner_key)
				.await?;
			let nonce_id = model.get_attribute_id_by_attribute_name(KDF_NONCE_FIELD)?;
			match entity.get(&nonce_id) {
				Some(ElementValue::Bytes(nonce)) if nonce.len() == 32 => {},
				None | Some(ElementValue::Null) => {
					let nonce = self.register_nonce(model, &entity).await?;
					entity.insert(nonce_id, ElementValue::Bytes(nonce));
				},
				_ => return Err(ApiCallError::internal("Invalid AEAD KDF nonce".into())),
			}
			entity = self.client.entity_facade.encrypt_and_map_with_aead(
				model,
				&entity,
				&AeadEncryptionKey::GroupKey(key),
			)?;
		}
		self.client
			.entity_client
			.update_instance(&type_ref, entity)
			.await
	}

	async fn register_nonce(
		&self,
		model: &TypeModel,
		entity: &ParsedEntity,
	) -> Result<Vec<u8>, ApiCallError> {
		let id = model.get_attribute_id_by_attribute_name(ID_FIELD)?;
		let (instance_list, instance_id) = match entity.get(&id) {
			Some(ElementValue::IdGeneratedId(id)) => (None, id.clone()),
			Some(ElementValue::IdTupleGeneratedElementId(id)) => {
				(Some(id.list_id.clone()), id.element_id.clone())
			},
			_ => {
				return Err(ApiCallError::internal(
					"Nonce migration requires a persistent generated ID".into(),
				))
			},
		};
		let type_info = TypeInfo {
			_id: Some(
				make_random_aggregate_id(&self.randomizer)
					.assert_custom_id()
					.clone(),
			),
			application: model.app.to_string(),
			typeId: String::from(model.id)
				.parse()
				.map_err(|error| ApiCallError::internal_with_err(error, "Invalid type ID"))?,
		};
		let input = UpdateKdfNoncePostIn {
			_format: 0,
			instanceKdfNonce: InstanceKdfNonce {
				_id: Some(
					make_random_aggregate_id(&self.randomizer)
						.assert_custom_id()
						.clone(),
				),
				instanceList: instance_list,
				instanceId: instance_id,
				kdfNonce: self.randomizer.generate_random_array::<32>().to_vec(),
				typeInfo: type_info,
			},
		};
		let response = self
			.service_executor
			.post::<UpdateKdfNonceService>(input, ExtraServiceParams::default())
			.await?;
		if response.kdfNonce.len() != 32 {
			return Err(ApiCallError::internal(
				"Invalid AEAD nonce returned by server".into(),
			));
		}
		// Another writer may already have registered a different nonce.
		// Always use the authoritative server response, not our proposal.
		Ok(response.kdfNonce)
	}
}

impl CryptoEntityClient {
	async fn aead_owner_key(
		&self,
		model: &TypeModel,
		entity: &ParsedEntity,
		owner_key: Option<VersionedAesKey>,
	) -> Result<VersionedAesKey, ApiCallError> {
		let key = match owner_key {
			Some(key) => key,
			None => {
				let owner_id = model.get_attribute_id_by_attribute_name(OWNER_GROUP_FIELD)?;
				let Some(ElementValue::IdGeneratedId(owner)) = entity.get(&owner_id) else {
					return Err(ApiCallError::internal("Missing AEAD owner group".into()));
				};
				self.key_loader_facade
					.get_current_sym_group_key(owner)
					.await
					.map_err(|error| {
						ApiCallError::internal_with_err(
							error,
							"Failed to load current AEAD owner key",
						)
					})?
			},
		};
		if key.version > u64::from(u8::MAX) {
			return Err(ApiCallError::internal(
				"Unsupported AEAD group key version".into(),
			));
		}
		Ok(key)
	}

	pub(super) async fn encrypt_group_key_instance(
		&self,
		model: &TypeModel,
		entity: &ParsedEntity,
	) -> Result<ParsedEntity, ApiCallError> {
		let key = self.aead_owner_key(model, entity, None).await?;
		self.entity_facade.encrypt_and_map_with_aead(
			model,
			entity,
			&AeadEncryptionKey::GroupKey(key),
		)
	}
}
