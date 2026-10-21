//! HTTPS for the API: the server's own self-signed certificate, and the client side
//! that trusts it by its SHA-256 fingerprint instead of by a certificate authority.
//!
//! The fingerprint travels with the pairing code, so a client that pairs knows exactly
//! which certificate to expect, the way SSH knows a host key.

use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A certificate and its key, ready to serve.
pub struct ServerCert {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl ServerCert {
    /// `sha256:<hex>` of the leaf certificate, what clients pin.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.chain[0])
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(self.chain.clone(), self.key.clone_key())?;
        // WebSockets (the stream endpoint) need HTTP/1.1.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// A fingerprint as people may paste it: with or without `sha256:`, in either case,
/// with or without colons (as `openssl x509 -fingerprint` prints it).
pub fn normalize_fingerprint(text: &str) -> Result<String> {
    let t = text.trim();
    let t = t
        .strip_prefix("sha256:")
        .or_else(|| t.strip_prefix("SHA256:"))
        .or_else(|| t.strip_prefix("sha256/"))
        .unwrap_or(t);
    let hex: String = t
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "{text} isn't a SHA-256 fingerprint (64 hex digits, as cove pair prints it)"
        ));
    }
    Ok(format!("sha256:{hex}"))
}

/// The server's self-signed certificate in `dir` (`self-signed.pem`, `self-signed.key`),
/// made on first use. `names` are the host names and addresses it lists, for clients
/// that check names; Cove's own clients check only the fingerprint, so a changed IP
/// doesn't mean a new certificate.
pub fn load_or_create_self_signed(dir: &Path, names: &[String]) -> Result<ServerCert> {
    std::fs::create_dir_all(dir)?;
    crate::paths::restrict(dir, 0o700);
    let cert_path = dir.join("self-signed.pem");
    let key_path = dir.join("self-signed.key");
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok(ServerCert {
            chain: CertificateDer::pem_slice_iter(&cert)
                .collect::<Result<Vec<_>, _>>()
                .context("reading self-signed.pem")?,
            key: PrivateKeyDer::from_pem_slice(&key).context("reading self-signed.key")?,
        });
    }
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "Cove server");
    let mut sans = vec![SanType::DnsName("localhost".try_into()?)];
    for name in names {
        match name.parse::<std::net::IpAddr>() {
            Ok(ip) => sans.push(SanType::IpAddress(ip)),
            Err(_) => {
                if let Ok(dns) = name.as_str().try_into() {
                    sans.push(SanType::DnsName(dns));
                }
            }
        }
    }
    params.subject_alt_names = sans;
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2046, 1, 1);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    std::fs::write(&key_path, key.serialize_pem())?;
    crate::paths::restrict(&key_path, 0o600);
    std::fs::write(&cert_path, cert.pem())?;
    Ok(ServerCert {
        chain: vec![cert.der().clone()],
        key: PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| anyhow!(e))?,
    })
}

/// A client config that accepts the certificate with this fingerprint, or else one a
/// public authority signed for the server's name. The second is for a server that has
/// since got a Let's Encrypt certificate: it is as trustworthy as the pinned one, and
/// paired clients carry on without pairing again.
pub fn pinned_client_config(fingerprint: &str) -> Result<rustls::ClientConfig> {
    let want = normalize_fingerprint(fingerprint)?;
    let provider = provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let public = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()?;
    let verifier = Arc::new(Pinned {
        want,
        provider: provider.clone(),
        public,
    });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[derive(Debug)]
struct Pinned {
    want: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
    public: Arc<rustls::client::WebPkiServerVerifier>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = fingerprint(end_entity);
        let public = || {
            self.public
                .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
                .is_ok()
        };
        if got == self.want || public() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "the server's certificate is {got}, not the {} this computer paired with. \
                 If the server's certificate really changed, pair again",
                self.want
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_normalize() {
        let hex = "ab".repeat(32);
        let colons = hex
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap().to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        let want = format!("sha256:{hex}");
        assert_eq!(normalize_fingerprint(&hex).unwrap(), want);
        assert_eq!(normalize_fingerprint(&colons).unwrap(), want);
        assert_eq!(
            normalize_fingerprint(&format!("SHA256:{colons}")).unwrap(),
            want
        );
        assert!(normalize_fingerprint("sha256:abc").is_err());
    }

    #[test]
    fn self_signed_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_self_signed(dir.path(), &["203.0.113.7".into()]).unwrap();
        let b = load_or_create_self_signed(dir.path(), &[]).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        a.server_config().unwrap();
        pinned_client_config(&a.fingerprint()).unwrap();
    }
}
