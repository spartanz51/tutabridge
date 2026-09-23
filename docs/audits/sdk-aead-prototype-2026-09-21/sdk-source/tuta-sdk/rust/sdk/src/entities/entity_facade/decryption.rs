//! Attribute decryption follows the instance type and field paths used by CryptoMapper.
use crate::ApiCallError;
use crypto_primitives::aead_facade::{AeadFacade, AeadSubKeys};
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;
use crypto_primitives::versioned::Versioned;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CipherVersion {
	Legacy,
	GroupKey(u64),
	SessionKey,
}

/// Even-length ciphertexts have no version byte, including those starting with 2 or 3.
pub(crate) fn cipher_version(bytes: &[u8]) -> Result<CipherVersion, ApiCallError> {
	if bytes.len() % 2 == 0 {
		return Ok(CipherVersion::Legacy);
	}
	match bytes[0] {
		0 | 1 => Ok(CipherVersion::Legacy),
		2 => {
			if bytes.len() < 3 || bytes[1] != 0 {
				return Err(ApiCallError::internal(
					"Invalid AEAD group key version header".into(),
				));
			}
			Ok(CipherVersion::GroupKey(u64::from(bytes[2])))
		},
		3 => Ok(CipherVersion::SessionKey),
		_ => Err(ApiCallError::internal(
			"Unsupported symmetric cipher version".into(),
		)),
	}
}

pub(super) struct InstanceDecryptor<'a> {
	session_key: Option<&'a GenericAesKey>,
	group_keys: Option<&'a HashMap<u64, GenericAesKey>>,
	kdf_nonce: Option<&'a [u8]>,
	group_subkeys: RefCell<HashMap<u64, AeadSubKeys>>,
	instance_type: String,
	aead: AeadFacade,
	session_subkeys: OnceCell<AeadSubKeys>,
}

impl<'a> InstanceDecryptor<'a> {
	#[cfg(test)]
	pub(super) fn new(
		session_key: &'a GenericAesKey,
		instance_type: String,
		randomizer: RandomizerFacade,
	) -> Self {
		Self {
			session_key: Some(session_key),
			group_keys: None,
			kdf_nonce: None,
			group_subkeys: RefCell::new(HashMap::new()),
			instance_type,
			aead: AeadFacade::new(randomizer),
			session_subkeys: OnceCell::new(),
		}
	}

	pub(super) fn with_group_keys(
		session_key: Option<&'a GenericAesKey>,
		group_keys: &'a HashMap<u64, GenericAesKey>,
		kdf_nonce: Option<&'a [u8]>,
		instance_type: String,
		randomizer: RandomizerFacade,
	) -> Self {
		Self {
			session_key,
			group_keys: Some(group_keys),
			kdf_nonce,
			instance_type,
			aead: AeadFacade::new(randomizer),
			session_subkeys: OnceCell::new(),
			group_subkeys: RefCell::new(HashMap::new()),
		}
	}

	fn session_key(&self) -> Result<&GenericAesKey, ApiCallError> {
		self.session_key.ok_or_else(|| {
			ApiCallError::internal("Missing session key for encrypted attribute".into())
		})
	}

	pub(super) fn decrypt(
		&self,
		ciphertext: &[u8],
		field_path: Option<&str>,
	) -> Result<Vec<u8>, ApiCallError> {
		match cipher_version(ciphertext)? {
			CipherVersion::Legacy => self
				.session_key()?
				.decrypt_data(ciphertext)
				.map_err(|e| ApiCallError::internal(e.to_string())),
			CipherVersion::SessionKey => {
				let GenericAesKey::Aes256(key) = self.session_key()? else {
					return Err(ApiCallError::internal(
						"AEAD session keys must be 256 bits".into(),
					));
				};
				let path = field_path.ok_or_else(|| {
					ApiCallError::internal("Missing aggregate ID for AEAD field path".into())
				})?;
				let keys = self
					.session_subkeys
					.get_or_init(|| AeadSubKeys::derive_from_session_key(key, &self.instance_type));
				self.aead
					.decrypt(
						keys,
						ciphertext,
						format!("attributeEncSK\u{001f}{path}").as_bytes(),
					)
					.map_err(|e| ApiCallError::internal(format!("AEAD session attribute: {e}")))
			},
			CipherVersion::GroupKey(version) => {
				let key = self
					.group_keys
					.and_then(|keys| keys.get(&version))
					.ok_or_else(|| {
						ApiCallError::internal(format!("Missing AEAD group key version {version}"))
					})?;
				let nonce = self
					.kdf_nonce
					.filter(|nonce| nonce.len() == 32)
					.ok_or_else(|| {
						ApiCallError::internal("Missing or invalid AEAD KDF nonce".into())
					})?;
				let path = field_path.ok_or_else(|| {
					ApiCallError::internal("Missing aggregate ID for AEAD field path".into())
				})?;
				let mut cache = self.group_subkeys.borrow_mut();
				let keys = cache.entry(version).or_insert_with(|| {
					AeadSubKeys::derive_from_group_key(
						&Versioned {
							object: key.clone(),
							version,
						},
						nonce,
						&self.instance_type,
					)
				});
				self.aead
					.decrypt(
						keys,
						ciphertext,
						format!("attributeEncGK\u{001f}{path}").as_bytes(),
					)
					.map_err(|e| ApiCallError::internal(format!("AEAD group attribute: {e}")))
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use base64::{prelude::BASE64_STANDARD, Engine};
	use crypto_primitives::aes::{Aes128Key, Aes256Key};
	use serde_json::Value;

	#[test]
	fn distinguishes_versions_without_misreading_legacy_ivs() {
		for first in [0, 1, 2, 3, 255] {
			assert_eq!(cipher_version(&[first; 32]).unwrap(), CipherVersion::Legacy);
		}
		assert_eq!(cipher_version(&[1; 65]).unwrap(), CipherVersion::Legacy);
		assert_eq!(cipher_version(&[3; 53]).unwrap(), CipherVersion::SessionKey);
		assert_eq!(
			cipher_version(&[2, 0, 7]).unwrap(),
			CipherVersion::GroupKey(7)
		);
		for bytes in [vec![2], vec![2, 1, 7], vec![255]] {
			assert!(cipher_version(&bytes).is_err());
		}
	}

	#[test]
	fn decrypts_official_ts_session_vectors_and_rejects_wrong_context() {
		let vectors: Vec<Value> = serde_json::from_str(include_str!(
			"../../../tests/fixtures/aead_attributes_ts.json"
		))
		.unwrap();
		for vector in vectors.iter().filter(|v| v["version"] == 3) {
			let key = GenericAesKey::from_bytes(
				&BASE64_STANDARD
					.decode(vector["key"].as_str().unwrap())
					.unwrap(),
			)
			.unwrap();
			let ciphertext = BASE64_STANDARD
				.decode(vector["ciphertext"].as_str().unwrap())
				.unwrap();
			let decryptor = InstanceDecryptor::new(
				&key,
				"tutanota/97".into(),
				RandomizerFacade::from_core(rand_core::OsRng),
			);
			assert_eq!(
				decryptor
					.decrypt(&ciphertext, vector["path"].as_str())
					.unwrap(),
				vector["plaintext"].as_str().unwrap().as_bytes()
			);
			assert!(decryptor.decrypt(&ciphertext, Some("wrong")).is_err());
			assert!(decryptor.decrypt(&ciphertext, None).is_err());
			let wrong_type = InstanceDecryptor::new(
				&key,
				"tutanota/98".into(),
				RandomizerFacade::from_core(rand_core::OsRng),
			);
			assert!(wrong_type
				.decrypt(&ciphertext, vector["path"].as_str())
				.is_err());
			let mut corrupt = ciphertext.clone();
			*corrupt.last_mut().unwrap() ^= 1;
			assert!(decryptor
				.decrypt(&corrupt, vector["path"].as_str())
				.is_err());
		}
	}

	#[test]
	fn malformed_aead_and_128_bit_session_keys_fail_without_fallback() {
		let short_key = GenericAesKey::Aes128(Aes128Key::from_bytes(&[0x11; 16]).unwrap());
		let decryptor = InstanceDecryptor::new(
			&short_key,
			"tutanota/97".into(),
			RandomizerFacade::from_core(rand_core::OsRng),
		);
		assert!(decryptor
			.decrypt(&[3; 53], Some("105"))
			.unwrap_err()
			.to_string()
			.contains("256 bits"));
		let key = GenericAesKey::Aes256(Aes256Key::from_bytes(&[0x11; 32]).unwrap());
		let decryptor = InstanceDecryptor::new(
			&key,
			"tutanota/97".into(),
			RandomizerFacade::from_core(rand_core::OsRng),
		);
		for len in [1, 3, 17, 31, 49] {
			assert!(decryptor.decrypt(&vec![3; len], Some("105")).is_err());
		}
	}
	#[test]
	fn group_vectors_require_the_correct_nonce_version_and_key() {
		let vectors: Vec<Value> = serde_json::from_str(include_str!(
			"../../../tests/fixtures/aead_attributes_ts.json"
		))
		.unwrap();
		for vector in vectors.iter().filter(|v| v["version"] == 2) {
			let key = GenericAesKey::from_bytes(
				&BASE64_STANDARD
					.decode(vector["key"].as_str().unwrap())
					.unwrap(),
			)
			.unwrap();
			let ciphertext = BASE64_STANDARD
				.decode(vector["ciphertext"].as_str().unwrap())
				.unwrap();
			let nonce = BASE64_STANDARD
				.decode(vector["kdf_nonce"].as_str().unwrap())
				.unwrap();
			let keys = HashMap::from([(7, key.clone())]);
			let make = |nonce| {
				InstanceDecryptor::with_group_keys(
					None,
					&keys,
					nonce,
					"tutanota/97".into(),
					RandomizerFacade::from_core(rand_core::OsRng),
				)
			};
			let decryptor = make(Some(nonce.as_slice()));
			assert_eq!(
				decryptor
					.decrypt(&ciphertext, vector["path"].as_str())
					.unwrap(),
				vector["plaintext"].as_str().unwrap().as_bytes()
			);
			assert!(make(None)
				.decrypt(&ciphertext, vector["path"].as_str())
				.is_err());
			let wrong_nonce = [0x33; 32];
			assert!(make(Some(&wrong_nonce))
				.decrypt(&ciphertext, vector["path"].as_str())
				.is_err());
			let wrong_keys = HashMap::from([(8, key)]);
			let decryptor = InstanceDecryptor::with_group_keys(
				None,
				&wrong_keys,
				Some(&nonce),
				"tutanota/97".into(),
				RandomizerFacade::from_core(rand_core::OsRng),
			);
			assert!(decryptor
				.decrypt(&ciphertext, vector["path"].as_str())
				.is_err());
		}
	}

	#[test]
	fn mixed_versions_share_context_without_reusing_the_wrong_subkeys() {
		let session = GenericAesKey::from_bytes(&[0x11; 32]).unwrap();
		let keys = HashMap::from([
			(7, GenericAesKey::from_bytes(&[0x44; 32]).unwrap()),
			(8, GenericAesKey::from_bytes(&[0x55; 32]).unwrap()),
		]);
		let nonce = [0x22; 32];
		let decryptor = InstanceDecryptor::with_group_keys(
			Some(&session),
			&keys,
			Some(&nonce),
			"tutanota/97".into(),
			RandomizerFacade::from_core(rand_core::OsRng),
		);
		let facade = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
		for version in [7, 8, 7] {
			let subkeys = AeadSubKeys::derive_from_group_key(
				&Versioned {
					object: keys[&version].clone(),
					version,
				},
				&nonce,
				"tutanota/97",
			);
			let ct = facade
				.encrypt(&subkeys, b"group".to_vec(), b"attributeEncGK\x1f105")
				.unwrap();
			assert_eq!(decryptor.decrypt(&ct, Some("105")).unwrap(), b"group");
		}
		assert_eq!(decryptor.group_subkeys.borrow().len(), 2);
		let vectors: Vec<Value> = serde_json::from_str(include_str!(
			"../../../tests/fixtures/aead_attributes_ts.json"
		))
		.unwrap();
		let v = vectors.iter().find(|v| v["version"] == 3).unwrap();
		let ct = BASE64_STANDARD
			.decode(v["ciphertext"].as_str().unwrap())
			.unwrap();
		assert_eq!(
			decryptor.decrypt(&ct, v["path"].as_str()).unwrap(),
			v["plaintext"].as_str().unwrap().as_bytes()
		);
	}
}
