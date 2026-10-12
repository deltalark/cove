//! TLS for the egress proxy: a local certificate authority that signs a certificate
//! for each host whose requests carry secrets, and the client config for the real
//! upstream servers.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct Tls {
    /// The authority's certificate, as installed in VMs.
    pub ca_pem: String,
    /// The authority as rcgen signs with it. Rebuilt from the saved key on load; leaf
    /// certificates chain to the saved certificate by name and key.
    issuer: (Certificate, KeyPair),
    leaves: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    pub upstream: Arc<rustls::ClientConfig>,
}

fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "Cove egress proxy");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2044, 1, 1);
    Ok(params)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl Tls {
    /// Load the authority from `dir` (`ca.pem`, `ca.key`), creating it on first use.
    /// Upstream servers are checked against the public web roots, plus the
    /// certificates in `COVE_EGRESS_EXTRA_CA` (a PEM file) when set.
    pub fn load_or_create(dir: &Path) -> Result<Tls> {
        let extra = match std::env::var("COVE_EGRESS_EXTRA_CA") {
            Ok(path) => {
                let pem = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
                CertificateDer::pem_slice_iter(&pem).collect::<Result<Vec<_>, _>>()?
            }
            Err(_) => Vec::new(),
        };
        Tls::load(dir, extra)
    }

    /// Like [`Tls::load_or_create`], trusting `extra` upstream as well as the web roots.
    pub fn load(dir: &Path, extra: Vec<CertificateDer<'static>>) -> Result<Tls> {
        std::fs::create_dir_all(dir)?;
        crate::paths::restrict(dir, 0o700);
        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca.key");
        let (ca_pem, key) = match (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            (Ok(cert), Ok(key)) => (cert, KeyPair::from_pem(&key).context("reading ca.key")?),
            _ => {
                let key = KeyPair::generate()?;
                let pem = ca_params()?.self_signed(&key)?.pem();
                std::fs::write(&key_path, key.serialize_pem())?;
                crate::paths::restrict(&key_path, 0o600);
                std::fs::write(&cert_path, &pem)?;
                (pem, key)
            }
        };
        let issuer = (ca_params()?.self_signed(&key)?, key);
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in extra {
            roots.add(cert)?;
        }
        let mut upstream = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        upstream.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Tls {
            ca_pem,
            issuer,
            leaves: Mutex::new(HashMap::new()),
            upstream: Arc::new(upstream),
        })
    }

    /// A server config presenting a certificate for `host`, signed by the authority.
    pub fn server_for(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
        if let Some(c) = self.leaves.lock().unwrap().get(host) {
            return Ok(c.clone());
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2044, 1, 1);
        let cert = params.signed_by(&key, &self.issuer.0, &self.issuer.1)?;
        let chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes())?;
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)?;
        // Only HTTP/1.1, so clients don't try HTTP/2 through the proxy.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.leaves
            .lock()
            .unwrap()
            .insert(host.to_string(), config.clone());
        Ok(config)
    }
}
