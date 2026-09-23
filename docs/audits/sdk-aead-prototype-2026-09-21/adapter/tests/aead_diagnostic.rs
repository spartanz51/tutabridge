use crypto_primitives::{
    aead_facade::{AeadFacade, AeadSubKeys},
    aes::{Aes256Key, InitializationVector},
    key::GenericAesKey,
    randomizer_facade::RandomizerFacade,
    versioned::Versioned,
};
use base64::{prelude::BASE64_STANDARD, Engine};
use serde_json::{json, Value};

#[test]
fn diagnose_aead_primitives_and_legacy_dispatch() {
    let key_bytes = [0x11u8; 32]; // Synthetic public test key, no account data.
    let key = Aes256Key::from_bytes(&key_bytes).unwrap();
    let generic = GenericAesKey::Aes256(key.clone());
    let nonce = [0x22u8; 32];
    let aead = AeadFacade::new(RandomizerFacade::from_core(rand_core::OsRng));
    let sk3 = AeadSubKeys::derive_from_session_key(&key, "tutanota/97");
    let sk2 = AeadSubKeys::derive_from_group_key(&Versioned { object: generic.clone(), version: 7 }, &nonce, "tutanota/97");
    let mut vectors = Vec::new();
    for (version, subkeys, domain) in [(3, &sk3, "attributeEncSK"), (2, &sk2, "attributeEncGK")] {
        for path in ["105", "3/aggregateId/9/anotherCustomId/17"] {
            let aad = format!("{domain}\u{001f}{path}");
            let plaintext = "AEAD diagnostic: été ✓".as_bytes().to_vec();
            let ciphertext = aead.encrypt(subkeys, plaintext.clone(), aad.as_bytes()).unwrap();
            assert_eq!(ciphertext[0], version);
            assert_eq!(aead.decrypt(subkeys, &ciphertext, aad.as_bytes()).unwrap(), plaintext);
            assert!(aead.decrypt(subkeys, &ciphertext, b"wrong field").is_err());
            let mut corrupted = ciphertext.clone();
            *corrupted.last_mut().unwrap() ^= 1;
            assert!(aead.decrypt(subkeys, &corrupted, aad.as_bytes()).is_err());
            let legacy_error = generic.decrypt_data(&ciphertext).unwrap_err().to_string();
            assert_eq!(legacy_error, "MacError");
            vectors.push(json!({"version":version,"type":"tutanota/97","path":path,"aad":aad,"key":BASE64_STANDARD.encode(key_bytes),"kdf_nonce":BASE64_STANDARD.encode(nonce),"group_version":7,"ciphertext":BASE64_STANDARD.encode(ciphertext),"plaintext":String::from_utf8(plaintext).unwrap(),"legacy_error":legacy_error}));
        }
    }
    let legacy = generic.encrypt_data(b"legacy still works", InitializationVector::generate(&RandomizerFacade::from_core(rand_core::OsRng))).unwrap();
    assert_eq!(legacy[0], 1);
    assert_eq!(generic.decrypt_data(&legacy).unwrap(), b"legacy still works");
    if let Ok(path) = std::env::var("AEAD_RUST_VECTORS") {
        std::fs::write(path, serde_json::to_string_pretty(&vectors).unwrap()).unwrap();
    }
    if let Ok(path) = std::env::var("AEAD_TS_VECTORS") {
        let input: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for vector in &input {
            let subkeys = if vector["version"] == 3 { &sk3 } else { &sk2 };
            let ciphertext = BASE64_STANDARD.decode(vector["ciphertext"].as_str().unwrap()).unwrap();
            let aad = vector["aad"].as_str().unwrap().as_bytes();
            assert_eq!(aead.decrypt(subkeys, &ciphertext, aad).unwrap(), vector["plaintext"].as_str().unwrap().as_bytes());
        }
        println!("Rust decrypted {} official TS ciphertexts", input.len());
    }
    println!("4 AEAD v2/v3 cases succeed in Rust primitives, fail as MacError in the entity decoder's legacy entry point; wrong AAD and corrupted tags rejected; legacy v1 succeeds.");
}
