//! The on-disk format predates `SessionStore`; keep it compatible with existing
//! keyring and password-store entries.

use base64::{engine::general_purpose::STANDARD, Engine};
use tutasdk::login::{CredentialType, Credentials};

pub(super) fn encode(credentials: &Credentials) -> String {
    serde_json::json!({
        "login": credentials.login,
        "user_id": credentials.user_id.0,
        "access_token": credentials.access_token,
        "encrypted_passphrase_key": STANDARD.encode(&credentials.encrypted_passphrase_key),
        "credential_type": match credentials.credential_type {
            CredentialType::Internal => "Internal",
            CredentialType::External => "External",
        },
    })
    .to_string()
}

pub(super) fn decode(json: impl AsRef<[u8]>) -> Option<Credentials> {
    let value: serde_json::Value = serde_json::from_slice(json.as_ref()).ok()?;
    Some(Credentials {
        login: value["login"].as_str()?.to_owned(),
        user_id: tutasdk::GeneratedId(value["user_id"].as_str()?.to_owned()),
        access_token: value["access_token"].as_str()?.to_owned(),
        encrypted_passphrase_key: STANDARD
            .decode(value["encrypted_passphrase_key"].as_str()?)
            .ok()?,
        // Older entries treat unknown credential types as Internal. Preserve
        // that behavior while moving storage out of the login implementation.
        credential_type: match value["credential_type"].as_str()? {
            "External" => CredentialType::External,
            _ => CredentialType::Internal,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_SESSION: &str = r#"{
        "login": "me@tuta.io",
        "user_id": "fakeuser0",
        "access_token": "example-token",
        "encrypted_passphrase_key": "AAECA/7/",
        "credential_type": "Internal"
    }"#;

    #[test]
    fn reads_legacy_entries_and_writes_the_same_format() {
        let credentials = decode(LEGACY_SESSION).unwrap();
        assert_eq!(credentials.login, "me@tuta.io");
        assert_eq!(credentials.user_id.0, "fakeuser0");
        assert_eq!(credentials.access_token, "example-token");
        assert_eq!(credentials.encrypted_passphrase_key, [0, 1, 2, 3, 254, 255]);
        assert!(matches!(
            credentials.credential_type,
            CredentialType::Internal
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encode(&credentials)).unwrap(),
            serde_json::from_str::<serde_json::Value>(LEGACY_SESSION).unwrap()
        );
    }

    #[test]
    fn reads_the_same_legacy_session_from_text_or_bytes() {
        let from_text = decode(LEGACY_SESSION).unwrap();
        let from_slice = decode(LEGACY_SESSION.as_bytes()).unwrap();
        assert_eq!(encode(&from_text), encode(&from_slice));
    }

    #[test]
    fn rejects_non_utf8_bytes_in_an_otherwise_valid_session() {
        let mut bytes = LEGACY_SESSION.as_bytes().to_vec();
        let token_start = LEGACY_SESSION.find("example-token").unwrap();
        bytes[token_start] = 0xff;
        assert!(decode(bytes.as_slice()).is_none());
        assert!(decode([0xff, 0xfe]).is_none());
        assert!(decode([]).is_none());
    }

    #[test]
    fn preserves_external_credentials_and_legacy_unknown_type_fallback() {
        for (name, external) in [("External", true), ("FutureType", false)] {
            let json = LEGACY_SESSION.replace("Internal", name);
            let credentials = decode(json.as_bytes()).unwrap();
            assert_eq!(
                matches!(credentials.credential_type, CredentialType::External),
                external
            );
            let roundtrip = decode(encode(&credentials)).unwrap();
            assert_eq!(roundtrip.access_token, credentials.access_token);
            assert_eq!(
                roundtrip.encrypted_passphrase_key,
                credentials.encrypted_passphrase_key
            );
            assert_eq!(
                matches!(roundtrip.credential_type, CredentialType::External),
                external
            );
        }
    }

    #[test]
    fn rejects_malformed_or_incomplete_entries() {
        assert!(decode("not json").is_none());
        assert!(decode("[]").is_none());
        assert!(decode(LEGACY_SESSION.replace("AAECA/7/", "invalid base64")).is_none());
        let value: serde_json::Value = serde_json::from_str(LEGACY_SESSION).unwrap();
        for key in [
            "login",
            "user_id",
            "access_token",
            "encrypted_passphrase_key",
            "credential_type",
        ] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(decode(missing.to_string()).is_none(), "missing {key}");
            let mut wrong_type = value.clone();
            wrong_type[key] = serde_json::json!(42);
            assert!(decode(wrong_type.to_string()).is_none(), "non-string {key}");
        }
    }
}
