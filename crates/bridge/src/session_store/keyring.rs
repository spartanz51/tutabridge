use super::{KeyringUnavailableKind, SessionStoreError};

const SERVICE: &str = "tutabridge";

pub(super) fn read(email: &str) -> Result<Option<Vec<u8>>, SessionStoreError> {
    classify_read_result(keyring::Entry::new(SERVICE, email).and_then(|entry| entry.get_password()))
}

fn classify_read_result(
    result: keyring::Result<String>,
) -> Result<Option<Vec<u8>>, SessionStoreError> {
    match result {
        Ok(secret) => Ok(Some(secret.into_bytes())),
        // The keyring read succeeded even if its bytes were not UTF-8. Pass
        // those bytes to the shared codec so all backends classify corrupt
        // sessions alike, without confusing corruption with denied access.
        Err(keyring::Error::BadEncoding(bytes)) => Ok(Some(bytes)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => {
            let message = diagnostic(&error);
            match error {
                keyring::Error::NoStorageAccess(_) => Err(SessionStoreError::KeyringUnavailable {
                    kind: KeyringUnavailableKind::StorageAccess,
                    message,
                }),
                keyring::Error::PlatformFailure(_) => Err(SessionStoreError::KeyringUnavailable {
                    kind: KeyringUnavailableKind::Platform,
                    message,
                }),
                _ => Err(SessionStoreError::Read {
                    backend: "the keychain",
                    message,
                }),
            }
        }
    }
}

fn diagnostic(error: &keyring::Error) -> String {
    match error {
        keyring::Error::BadEncoding(_) => "the saved session is not UTF-8".to_owned(),
        // The library's Display implementation prints the matching credential
        // objects. They are unnecessary for diagnostics and may contain secrets.
        keyring::Error::Ambiguous(_) => "multiple matching keyring entries".to_owned(),
        error => error.to_string(),
    }
}

pub(super) fn write(email: &str, secret: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, email)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|error| diagnostic(&error))
}

pub(super) fn delete(email: &str) -> Result<(), String> {
    match keyring::Entry::new(SERVICE, email).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(diagnostic(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_no_entry_means_absence() {
        assert!(classify_read_result(Err(keyring::Error::NoEntry))
            .unwrap()
            .is_none());
        assert_eq!(
            classify_read_result(Ok("session".into()))
                .unwrap()
                .as_deref(),
            Some(b"session".as_slice())
        );
        for error in [
            keyring::Error::Ambiguous(Vec::new()),
            keyring::Error::Invalid("service".into(), "invalid".into()),
            keyring::Error::TooLong("service".into(), 1),
        ] {
            let error = classify_read_result(Err(error)).unwrap_err();
            assert!(!error.allows_volatile_login(), "{error}");
        }
    }

    #[test]
    fn invalid_utf8_and_empty_entries_are_present_bytes_not_storage_failures() {
        for bytes in [vec![0xff, 0xfe], Vec::new()] {
            let result =
                classify_read_result(Err(keyring::Error::BadEncoding(bytes.clone()))).unwrap();
            assert_eq!(result, Some(bytes));
        }
    }

    #[test]
    fn unavailable_keyrings_are_distinct_from_bad_entries() {
        for (error, expected_kind) in [
            (
                keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))),
                KeyringUnavailableKind::StorageAccess,
            ),
            (
                keyring::Error::PlatformFailure(Box::new(std::io::Error::other("unavailable"))),
                KeyringUnavailableKind::Platform,
            ),
        ] {
            let error = classify_read_result(Err(error)).unwrap_err();
            assert!(error.allows_volatile_login());
            assert!(
                matches!(error, SessionStoreError::KeyringUnavailable { kind, .. } if kind == expected_kind)
            );
        }
    }

    #[test]
    fn diagnostics_omit_invalid_bytes_and_matching_credential_objects() {
        let message = diagnostic(&keyring::Error::BadEncoding(b"secret-token".to_vec()));
        assert!(!message.contains("secret-token"));
        assert_eq!(
            diagnostic(&keyring::Error::Ambiguous(Vec::new())),
            "multiple matching keyring entries"
        );
    }
}
