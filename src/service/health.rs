//! Local status validation and unauthenticated IMAP/SMTP TLS probes.
use super::{runtime_dir, State};
use anyhow::{bail, Context};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Validated probe inputs borrowed from a status response, without any I/O.
#[derive(Debug, PartialEq)]
struct ProbeTarget<'a> {
    address: IpAddr,
    imap: u16,
    smtp: u16,
    certificate: &'a str,
}

fn probe_target(status: &serde_json::Value) -> anyhow::Result<ProbeTarget<'_>> {
    if status["state"].as_i64() != Some(State::Starting as i64) {
        bail!(
            "{}",
            status["reason"].as_str().unwrap_or("Service unavailable")
        );
    }
    let ports = status["ports"].as_array().context("Service is starting")?;
    if ports.len() != 2 {
        bail!("Invalid service ports");
    }
    Ok(ProbeTarget {
        imap: u16::try_from(ports[0].as_u64().context("Invalid IMAP port")?)?,
        smtp: u16::try_from(ports[1].as_u64().context("Invalid SMTP port")?)?,
        certificate: status["certificate"].as_str().context("TLS is starting")?,
        address: status["address"]
            .as_str()
            .context("Service address unavailable")?
            .parse()?,
    })
}

pub async fn check() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(4), async {
        let mut socket = UnixStream::connect(runtime_dir().join("health.sock"))
            .await
            .context("Service status unavailable")?;
        socket.write_all(b"h").await?;
        let mut bytes = Vec::new();
        (&mut socket).take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            bail!("Invalid service status");
        }
        let status: serde_json::Value = serde_json::from_slice(&bytes)?;
        let target = probe_target(&status)?;
        probe(target.address, target.imap, target.smtp, target.certificate).await
    })
    .await
    .context("Health check timed out")?
}

/// Pin the local leaf DER, independent of SAN/expiry; still verify possession
/// of its private key via the provider's TLS handshake signature algorithms.
#[derive(Debug)]
struct PinnedCertificate(tokio_rustls::rustls::pki_types::CertificateDer<'static>);
impl tokio_rustls::rustls::client::danger::ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        if end != &self.0 {
            return Err(tokio_rustls::rustls::Error::General(
                "Local certificate pin mismatch".into(),
            ));
        }
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        signature: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &tokio_rustls::rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        signature: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &tokio_rustls::rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn probe_addresses(address: IpAddr) -> (IpAddr, Option<IpAddr>) {
    match address {
        IpAddr::V4(ip) if ip.is_unspecified() => (Ipv4Addr::LOCALHOST.into(), None),
        // Some dual-stack listeners have IPv4 loopback but no ::1 route.
        // Only unspecified IPv6 permits fallback; explicit binds stay exact.
        IpAddr::V6(ip) if ip.is_unspecified() => {
            (Ipv6Addr::LOCALHOST.into(), Some(Ipv4Addr::LOCALHOST.into()))
        }
        ip => (ip, None),
    }
}

pub(super) async fn connect_health(
    address: IpAddr,
    port: u16,
) -> io::Result<tokio::net::TcpStream> {
    let (primary, fallback) = probe_addresses(address);
    if let Some(fallback) = fallback {
        if let Ok(Ok(tcp)) = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect((primary, port)),
        )
        .await
        {
            return Ok(tcp);
        }
        tokio::net::TcpStream::connect((fallback, port)).await
    } else {
        tokio::net::TcpStream::connect((primary, port)).await
    }
}

pub(super) async fn probe(
    address: std::net::IpAddr,
    imap: u16,
    smtp: u16,
    pem: &str,
) -> anyhow::Result<()> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio_rustls::{rustls, TlsConnector};
    let certificate = CertificateDer::from_pem_slice(pem.as_bytes())?;
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertificate(certificate)))
            .with_no_client_auth(),
    ));
    async fn line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> anyhow::Result<String> {
        let mut value = String::new();
        let mut limited = r.take(4097);
        limited.read_line(&mut value).await?;
        if value.len() > 4096 || !value.ends_with('\n') {
            bail!("Invalid protocol response");
        }
        Ok(value)
    }
    let tcp = connect_health(address, imap)
        .await
        .context("IMAP unavailable")?;
    let mut connection = BufReader::new(
        connector
            .connect("localhost".try_into()?, tcp)
            .await
            .context("IMAP TLS unavailable")?,
    );
    if !line(&mut connection).await?.starts_with("* OK") {
        bail!("IMAP greeting unavailable");
    }
    connection.get_mut().write_all(b"health NOOP\r\n").await?;
    if !line(&mut connection).await?.starts_with("health OK") {
        bail!("IMAP NOOP failed");
    }
    connection
        .get_mut()
        .write_all(b"healthend LOGOUT\r\n")
        .await?;
    let mut logged_out = false;
    for _ in 0..4 {
        if line(&mut connection).await?.starts_with("healthend OK") {
            logged_out = true;
            break;
        }
    }
    if !logged_out {
        bail!("IMAP LOGOUT failed");
    }
    let _ = connection.get_mut().shutdown().await;
    let tcp = connect_health(address, smtp)
        .await
        .context("SMTP unavailable")?;
    let mut connection = BufReader::new(
        connector
            .connect("localhost".try_into()?, tcp)
            .await
            .context("SMTP TLS unavailable")?,
    );
    if !line(&mut connection).await?.starts_with("220 ") {
        bail!("SMTP greeting unavailable");
    }
    connection
        .get_mut()
        .write_all(b"EHLO localhost\r\n")
        .await?;
    let mut hello = false;
    for _ in 0..32 {
        let reply = line(&mut connection).await?;
        if reply.starts_with("250 ") {
            hello = true;
            break;
        }
        if !reply.starts_with("250-") {
            break;
        }
    }
    if !hello {
        bail!("SMTP EHLO failed");
    }
    connection.get_mut().write_all(b"QUIT\r\n").await?;
    if !line(&mut connection).await?.starts_with("221 ") {
        bail!("SMTP QUIT failed");
    }
    let _ = connection.get_mut().shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready_status() -> serde_json::Value {
        json!({
            "state": State::Starting as i32,
            "ports": [1143, 1025],
            "certificate": "local certificate",
            "address": "127.0.0.2",
        })
    }

    #[test]
    fn ready_status_preserves_the_configured_probe_inputs() {
        let status = ready_status();
        assert_eq!(
            probe_target(&status).unwrap(),
            ProbeTarget {
                address: "127.0.0.2".parse().unwrap(),
                imap: 1143,
                smtp: 1025,
                certificate: "local certificate",
            }
        );
    }

    #[test]
    fn waiting_status_reports_its_reason_before_requiring_probe_fields() {
        let status =
            json!({"state": State::Session as i32, "reason": "Waiting for a saved session"});
        assert_eq!(
            probe_target(&status).unwrap_err().to_string(),
            "Waiting for a saved session"
        );
        assert_eq!(
            probe_target(&json!({})).unwrap_err().to_string(),
            "Service unavailable"
        );
        assert!(probe_target(&json!({"state": State::Starting as i32})).is_err());
    }

    #[test]
    fn malformed_ports_are_rejected_without_opening_a_socket() {
        for ports in [
            json!(null),
            json!([]),
            json!([1143]),
            json!([1143, 1025, 25]),
            json!([-1, 1025]),
            json!([65536, 1025]),
            json!([1143, 65536]),
            json!([1143, "1025"]),
            json!([1143, 1025.5]),
        ] {
            let mut status = ready_status();
            status["ports"] = ports;
            assert!(
                probe_target(&status).is_err(),
                "accepted {}",
                status["ports"]
            );
        }
    }

    #[test]
    fn incomplete_tls_or_address_status_is_rejected() {
        for (field, value) in [
            ("certificate", json!(null)),
            ("certificate", json!(42)),
            ("address", json!(null)),
            ("address", json!("not an IP")),
        ] {
            let mut status = ready_status();
            status[field] = value;
            assert!(probe_target(&status).is_err());
        }
    }

    #[test]
    fn unspecified_addresses_probe_loopback_but_explicit_binds_stay_exact() {
        for (bind, primary, fallback) in [
            ("0.0.0.0", "127.0.0.1", None),
            ("::", "::1", Some("127.0.0.1")),
            ("127.0.0.2", "127.0.0.2", None),
            ("::1", "::1", None),
            ("192.0.2.1", "192.0.2.1", None),
            ("2001:db8::1", "2001:db8::1", None),
        ] {
            assert_eq!(
                probe_addresses(bind.parse().unwrap()),
                (
                    primary.parse().unwrap(),
                    fallback.map(|ip| ip.parse().unwrap())
                )
            );
        }
    }
}
