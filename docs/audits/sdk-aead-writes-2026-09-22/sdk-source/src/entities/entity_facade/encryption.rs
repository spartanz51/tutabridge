use crate::ApiCallError;
use crypto_primitives::aead_facade::{AeadFacade, AeadSubKeys};
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use crypto_primitives::versioned::VersionedAesKey;

/// Input key for AEAD attributes. Group-key encryption uses the instance's KDF nonce.
pub enum AeadEncryptionKey {
	SessionKey(GenericAesKey),
	GroupKey(VersionedAesKey),
}

#[derive(Clone, Copy)]
pub(super) enum EncryptionContext<'a> {
	Cbc(&'a GenericAesKey),
	Aead(&'a InstanceEncryptor),
}

pub(super) struct InstanceEncryptor {
	subkeys: AeadSubKeys,
	domain: &'static str,
	aead: AeadFacade,
}

impl InstanceEncryptor {
	pub(super) fn new(
		key: &AeadEncryptionKey,
		kdf_nonce: Option<&[u8]>,
		instance_type: &str,
		randomizer: RandomizerFacade,
	) -> Result<Self, ApiCallError> {
		let (subkeys, domain) = match key {
			AeadEncryptionKey::SessionKey(GenericAesKey::Aes256(key)) => (
				AeadSubKeys::derive_from_session_key(key, instance_type),
				"attributeEncSK\u{001f}",
			),
			AeadEncryptionKey::SessionKey(_) => {
				return Err(ApiCallError::internal(
					"AEAD session keys must be 256 bits".into(),
				));
			},
			AeadEncryptionKey::GroupKey(key) => {
				let nonce = kdf_nonce.filter(|nonce| nonce.len() == 32).ok_or_else(|| {
					ApiCallError::internal("Missing or invalid AEAD KDF nonce".into())
				})?;
				// Validate before a caller changes server-side nonce metadata.
				if key.version > u64::from(u8::MAX) {
					return Err(ApiCallError::internal(
						"Unsupported AEAD group key version".into(),
					));
				}
				(
					AeadSubKeys::derive_from_group_key(key, nonce, instance_type),
					"attributeEncGK\u{001f}",
				)
			},
		};
		Ok(Self {
			subkeys,
			domain,
			aead: AeadFacade::new(randomizer),
		})
	}

	pub(super) fn encrypt(
		&self,
		bytes: Vec<u8>,
		field_path: &str,
	) -> Result<Vec<u8>, ApiCallError> {
		self.aead
			.encrypt(
				&self.subkeys,
				bytes,
				format!("{}{field_path}", self.domain).as_bytes(),
			)
			.map_err(|error| {
				ApiCallError::internal_with_err(error, "Failed to encrypt AEAD attribute")
			})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use base64::{prelude::BASE64_STANDARD, Engine};
	use crypto_primitives::randomizer_facade::test_util::DeterministicRng;
	use crypto_primitives::versioned::Versioned;
	use rand_core::{CryptoRng, Error, RngCore};
	use serde_json::Value;

	struct FixedNonce([u8; 16]);
	impl CryptoRng for FixedNonce {}
	impl RngCore for FixedNonce {
		fn next_u32(&mut self) -> u32 {
			unreachable!()
		}
		fn next_u64(&mut self) -> u64 {
			unreachable!()
		}
		fn fill_bytes(&mut self, bytes: &mut [u8]) {
			bytes.copy_from_slice(&self.0);
		}
		fn try_fill_bytes(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
			self.fill_bytes(bytes);
			Ok(())
		}
	}

	#[test]
	fn ciphertext_matches_official_typescript_vectors_byte_for_byte() {
		let vectors: Vec<Value> = serde_json::from_str(include_str!(
			"../../../tests/fixtures/aead_attributes_ts.json"
		))
		.unwrap();
		for vector in vectors {
			let version = vector["version"].as_u64().unwrap();
			let expected = BASE64_STANDARD
				.decode(vector["ciphertext"].as_str().unwrap())
				.unwrap();
			let key = GenericAesKey::from_bytes(
				&BASE64_STANDARD
					.decode(vector["key"].as_str().unwrap())
					.unwrap(),
			)
			.unwrap();
			let nonce = BASE64_STANDARD
				.decode(vector["kdf_nonce"].as_str().unwrap())
				.unwrap();
			let header_len = if version == 3 { 1 } else { 3 };
			let randomizer = RandomizerFacade::from_core(FixedNonce(
				expected[header_len..header_len + 16].try_into().unwrap(),
			));
			let key = if version == 3 {
				AeadEncryptionKey::SessionKey(key)
			} else {
				AeadEncryptionKey::GroupKey(Versioned {
					object: key,
					version: 7,
				})
			};
			let encoder =
				InstanceEncryptor::new(&key, Some(&nonce), "tutanota/97", randomizer).unwrap();
			assert_eq!(
				encoder
					.encrypt(
						vector["plaintext"].as_str().unwrap().as_bytes().to_vec(),
						vector["path"].as_str().unwrap()
					)
					.unwrap(),
				expected
			);
		}
	}

	#[test]
	fn invalid_key_contexts_are_rejected() {
		let rng = || RandomizerFacade::from_core(DeterministicRng(0x44));
		let session =
			AeadEncryptionKey::SessionKey(GenericAesKey::from_bytes(&[0x11; 16]).unwrap());
		assert!(InstanceEncryptor::new(&session, None, "tutanota/97", rng()).is_err());
		let group = |version| {
			AeadEncryptionKey::GroupKey(Versioned {
				object: GenericAesKey::from_bytes(&[0x11; 32]).unwrap(),
				version,
			})
		};
		for nonce in [None, Some(&[0; 31][..])] {
			assert!(InstanceEncryptor::new(&group(7), nonce, "tutanota/97", rng()).is_err());
		}
		assert!(InstanceEncryptor::new(&group(256), Some(&[0; 32]), "tutanota/97", rng()).is_err());
	}

	#[test]
	fn mapper_matches_ts_for_empty_unicode_and_aggregate_values() {
		use crate::element_value::{ElementValue, ParsedEntity};
		use crate::entities::entity_facade::{EntityFacade, EntityFacadeImpl};
		use crate::entities::{generated::tutanota::Mail, Entity};
		use std::sync::Arc;
		let provider = Arc::new(crate::util::test_utils::mock_type_model_provider());
		let mut model = provider
			.resolve_client_type_ref(&Mail::type_ref())
			.unwrap()
			.clone();
		model
			.values
			.retain(|id, _| *id == 105.into() || *id == 1839.into());
		model.associations.retain(|id, _| *id == 111.into());
		let facade = EntityFacadeImpl::new(
			provider,
			RandomizerFacade::from_core(DeterministicRng(0x44)),
		);
		let vectors: Vec<Value> =
			serde_json::from_str(include_str!("../../../tests/fixtures/aead_mapper_ts.json"))
				.unwrap();
		for pair in vectors.chunks_exact(2) {
			let vector = &pair[0];
			let key = GenericAesKey::from_bytes(&[0x11; 32]).unwrap();
			let key = if vector["version"] == 3 {
				AeadEncryptionKey::SessionKey(key)
			} else {
				AeadEncryptionKey::GroupKey(Versioned {
					object: key,
					version: 7,
				})
			};
			let child = ParsedEntity::from([
				("93".into(), ElementValue::String("sender-id".into())),
				("94".into(), ElementValue::String("Alice".into())),
				(
					"95".into(),
					ElementValue::String("alice@example.org".into()),
				),
				("96".into(), ElementValue::Array(vec![])),
			]);
			let entity = ParsedEntity::from([
				(
					"105".into(),
					ElementValue::String(vector["plaintext"].as_str().unwrap().into()),
				),
				("1839".into(), ElementValue::Bytes(vec![0x22; 32])),
				(
					"111".into(),
					ElementValue::Array(vec![ElementValue::Dict(child)]),
				),
			]);
			let encrypted = facade
				.encrypt_and_map_with_aead(&model, &entity, &key)
				.unwrap();
			assert_eq!(
				encrypted["105"].assert_bytes(),
				&BASE64_STANDARD
					.decode(vector["ciphertext"].as_str().unwrap())
					.unwrap()
			);
			let child = &encrypted["111"].assert_array_ref()[0].assert_dict_ref();
			assert_eq!(
				child["94"].assert_bytes(),
				&BASE64_STANDARD
					.decode(pair[1]["ciphertext"].as_str().unwrap())
					.unwrap()
			);
			assert_eq!(child["93"], ElementValue::String("sender-id".into()));
			assert_eq!(
				child["95"],
				ElementValue::String("alice@example.org".into())
			);
		}
	}

	#[test]
	fn sibling_aggregate_paths_are_independent_and_authenticated() {
		use super::super::decryption::InstanceDecryptor;
		use crate::element_value::{ElementValue, ParsedEntity};
		use crate::entities::entity_facade::{EntityFacade, EntityFacadeImpl};
		use crate::entities::{generated::tutanota::Mail, Entity};
		use std::collections::HashMap;
		use std::sync::Arc;
		let provider = Arc::new(crate::util::test_utils::mock_type_model_provider());
		let mut model = provider
			.resolve_client_type_ref(&Mail::type_ref())
			.unwrap()
			.clone();
		model.values.retain(|id, _| *id == 1839.into());
		model.associations.retain(|id, _| *id == 111.into());
		let facade = EntityFacadeImpl::new(
			provider,
			RandomizerFacade::from_core(DeterministicRng(0x44)),
		);
		for version in [2, 3] {
			let key = GenericAesKey::from_bytes(&[0x11; 32]).unwrap();
			let input = if version == 3 {
				AeadEncryptionKey::SessionKey(key.clone())
			} else {
				AeadEncryptionKey::GroupKey(Versioned {
					object: key.clone(),
					version: 7,
				})
			};
			let children = ["first", "second"]
				.iter()
				.map(|id| {
					ElementValue::Dict(ParsedEntity::from([
						("93".into(), ElementValue::String((*id).into())),
						("94".into(), ElementValue::String((*id).into())),
						(
							"95".into(),
							ElementValue::String("someone@example.org".into()),
						),
						("96".into(), ElementValue::Array(vec![])),
					]))
				})
				.collect();
			let entity = ParsedEntity::from([
				("1839".into(), ElementValue::Bytes(vec![0x22; 32])),
				("111".into(), ElementValue::Array(children)),
			]);
			let encrypted = facade
				.encrypt_and_map_with_aead(&model, &entity, &input)
				.unwrap();
			let group_keys = HashMap::from([(7, key.clone())]);
			let decryptor = InstanceDecryptor::with_group_keys(
				Some(&key),
				&group_keys,
				Some(&[0x22; 32]),
				"tutanota/97".into(),
				RandomizerFacade::from_core(DeterministicRng(0x44)),
			);
			for (child, id) in encrypted["111"]
				.assert_array_ref()
				.iter()
				.zip(["first", "second"])
			{
				let cipher = child.assert_dict_ref()["94"].assert_bytes();
				assert_eq!(
					decryptor
						.decrypt(cipher, Some(&format!("111/{id}/94")))
						.unwrap(),
					id.as_bytes()
				);
				assert!(decryptor
					.decrypt(cipher, Some("111/first/second/94"))
					.is_err());
			}
		}
	}
}
