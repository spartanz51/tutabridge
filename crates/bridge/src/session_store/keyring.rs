const SERVICE: &str = "tutabridge";

pub(super) fn read(email: &str) -> Result<Option<String>, String> {
    keyring::Entry::new(SERVICE, email)
        .and_then(|entry| entry.get_password())
        .map(Some)
        .or_else(|error| match error {
            keyring::Error::NoEntry => Ok(None),
            error => Err(error.to_string()),
        })
}

pub(super) fn write(email: &str, secret: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, email)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|error| error.to_string())
}

pub(super) fn delete(email: &str) -> Result<(), String> {
    match keyring::Entry::new(SERVICE, email).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
