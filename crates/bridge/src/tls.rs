use std::path::PathBuf;
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

fn cert_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tutabridge")
}

fn cert_path() -> PathBuf {
    cert_dir().join("cert.pem")
}

fn key_path() -> PathBuf {
    cert_dir().join("key.pem")
}

pub fn load_or_create_tls_acceptor() -> Result<TlsAcceptor, Box<dyn std::error::Error + Send + Sync>>
{
    let cert_file = cert_path();
    let key_file = key_path();

    let existing_key = if cert_file.exists() {
        read_existing_key(&key_file)?
    } else {
        None
    };
    let (cert_pem, key_pem) = if let Some(key_pem) = existing_key {
        log::info!("Loading TLS certificate from {}", cert_file.display());
        (std::fs::read_to_string(&cert_file)?, key_pem)
    } else {
        log::info!("Generating self-signed TLS certificate...");
        let (cert, key) = generate_self_signed()?;
        crate::fs_private::create_dir_all_private(cert_dir())?;
        std::fs::write(&cert_file, &cert)?;
        persist_key(&key_file, &key)?;
        log::info!("Certificate saved to {}", cert_file.display());
        (cert, key)
    };

    let certs = load_certs(&cert_pem)?;
    let key = load_key(&key_pem)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn generate_self_signed() -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".try_into()?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn load_certs(
    pem: &str,
) -> Result<Vec<CertificateDer<'static>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err("No certificates found in PEM".into());
    }
    Ok(certs)
}

fn load_key(pem: &str) -> Result<PrivateKeyDer<'static>, Box<dyn std::error::Error + Send + Sync>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let keys: Vec<PrivatePkcs8KeyDer<'static>> =
        rustls_pemfile::pkcs8_private_keys(&mut reader).collect::<Result<Vec<_>, _>>()?;
    let key = keys
        .into_iter()
        .next()
        .ok_or("No private key found in PEM")?;
    Ok(PrivateKeyDer::Pkcs8(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_acceptor_builds() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let (cert_pem, key_pem) = generate_self_signed().unwrap();
        let certs = load_certs(&cert_pem).unwrap();
        let key = load_key(&key_pem).unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key);
        assert!(config.is_ok());
    }
}

// ---- TLS key storage ----------------------------------------------------
// Desktop: plaintext key.pem, as before. Android/Termux: the key lives in the
// OpenKeychain-encrypted store and key.pem is removed after migration.

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

#[cfg(not(target_os = "android"))]
fn read_existing_key(key_file: &PathBuf) -> Result<Option<String>, BoxErr> {
    if !key_file.exists() {
        return Ok(None);
    }
    // Heal installs whose key was written before it was created owner-only.
    crate::fs_private::tighten(key_file);
    Ok(Some(std::fs::read_to_string(key_file)?))
}

#[cfg(not(target_os = "android"))]
fn persist_key(key_file: &PathBuf, key: &str) -> Result<(), BoxErr> {
    crate::fs_private::write_private(key_file, key)?;
    Ok(())
}

#[cfg(target_os = "android")]
const KEY_SERVICE: &str = "tutabridge";
#[cfg(target_os = "android")]
const KEY_ACCOUNT: &str = "tls-key";

#[cfg(target_os = "android")]
fn read_existing_key(key_file: &PathBuf) -> Result<Option<String>, BoxErr> {
    use crate::session_store::{self, StoreError};
    match session_store::get(KEY_SERVICE, KEY_ACCOUNT) {
        Ok(k) => return Ok(Some(k)),
        Err(StoreError::NoEntry) => {}
        Err(e) => return Err(format!("could not unlock the TLS key: {e}").into()),
    }
    if !key_file.exists() {
        return Ok(None);
    }
    crate::fs_private::tighten(key_file);
    let key = std::fs::read_to_string(key_file)?;
    match session_store::set(KEY_SERVICE, KEY_ACCOUNT, &key) {
        Ok(()) => {
            let _ = std::fs::remove_file(key_file);
            log::info!("TLS key moved out of key.pem into encrypted storage");
        }
        Err(e) => log::warn!("Could not encrypt the TLS key, leaving key.pem in place: {e}"),
    }
    Ok(Some(key))
}

#[cfg(target_os = "android")]
fn persist_key(key_file: &PathBuf, key: &str) -> Result<(), BoxErr> {
    match crate::session_store::set(KEY_SERVICE, KEY_ACCOUNT, key) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("Could not encrypt the TLS key, writing key.pem instead: {e}");
            crate::fs_private::write_private(key_file, key)?;
            Ok(())
        }
    }
}
