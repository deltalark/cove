//! Certificates from Let's Encrypt (or any ACME server) for the API's HTTPS listener:
//! for a domain, or for the server's bare IP address, which Let's Encrypt has issued
//! since 2026 as short-lived (about six day) certificates.
//!
//! Validation uses TLS-ALPN-01 on the HTTPS listener itself, so nothing else needs a
//! port: the ACME server connects asking for `acme-tls/1`, and [`Certs`] answers with
//! the challenge certificate instead of the real one. Until the first certificate
//! arrives, and whenever renewal fails, the listener keeps serving what it has, the
//! self-signed certificate at first.

use crate::https::ServerCert;
use anyhow::{anyhow, bail, Context, Result};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rcgen::{CertificateParams, CustomExtension, KeyPair, SanType};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// What the certificate is for.
#[derive(Clone, Debug, PartialEq)]
pub enum Name {
    Dns(String),
    Ip(IpAddr),
}

impl Name {
    pub fn parse(text: &str) -> Result<Name> {
        let t = text.trim().trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = t.parse::<IpAddr>() {
            return Ok(Name::Ip(ip));
        }
        let t = t.trim_end_matches('.').to_lowercase();
        let ok = !t.is_empty()
            && t.contains('.')
            && t.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !ok {
            bail!("{text} isn't a domain name or an IP address");
        }
        Ok(Name::Dns(t))
    }

    fn identifier(&self) -> Identifier {
        match self {
            Name::Dns(d) => Identifier::Dns(d.clone()),
            Name::Ip(ip) => Identifier::Ip(*ip),
        }
    }

    fn san(&self) -> Result<SanType> {
        Ok(match self {
            Name::Dns(d) => SanType::DnsName(d.as_str().try_into()?),
            Name::Ip(ip) => SanType::IpAddress(*ip),
        })
    }

    /// For file names.
    fn slug(&self) -> String {
        self.to_string().replace([':', '.'], "-")
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Name::Dns(d) => f.write_str(d),
            Name::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certified(chain: Vec<CertificateDer<'static>>, key: &PrivateKeyDer<'_>) -> Result<CertifiedKey> {
    let key = provider()
        .key_provider
        .load_private_key(key.clone_key())
        .map_err(|e| anyhow!("loading the certificate's key: {e}"))?;
    Ok(CertifiedKey::new(chain, key))
}

struct Current {
    key: Arc<CertifiedKey>,
    /// Set while the certificate is self-signed, for clients to pin.
    fingerprint: Option<String>,
    /// Validity, for a certificate an authority signed.
    not_before: i64,
    not_after: i64,
}

/// The certificates the HTTPS listener serves: the current one, and challenge
/// certificates while an ACME server is validating.
pub struct Certs {
    current: RwLock<Current>,
    challenges: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Certs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Certs")
    }
}

impl Certs {
    /// Start out serving the server's self-signed certificate.
    pub fn new(self_signed: &ServerCert) -> Result<Arc<Certs>> {
        Ok(Arc::new(Certs {
            current: RwLock::new(Current {
                key: Arc::new(certified(self_signed.chain.clone(), &self_signed.key)?),
                fingerprint: Some(self_signed.fingerprint()),
                not_before: 0,
                not_after: 0,
            }),
            challenges: Mutex::default(),
        }))
    }

    /// The fingerprint to pin, while the certificate is self-signed; `None` once an
    /// authority signed it.
    pub fn fingerprint(&self) -> Option<String> {
        self.current.read().unwrap().fingerprint.clone()
    }

    /// When the authority-signed certificate expires, as Unix seconds.
    pub fn expires(&self) -> Option<i64> {
        let c = self.current.read().unwrap();
        c.fingerprint.is_none().then_some(c.not_after)
    }

    pub fn server_config(self: &Arc<Self>) -> Result<Arc<rustls::ServerConfig>> {
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        // WebSockets (the stream endpoint) need HTTP/1.1; acme-tls/1 is for validation.
        config.alpn_protocols = vec![b"http/1.1".to_vec(), ACME_TLS_ALPN.to_vec()];
        Ok(Arc::new(config))
    }

    /// Serve an authority-signed chain from now on.
    fn install(&self, chain_pem: &str, key_pem: &str) -> Result<()> {
        let chain = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .context("reading the certificate chain")?;
        let leaf = chain
            .first()
            .ok_or_else(|| anyhow!("the certificate chain is empty"))?;
        let (not_before, not_after) = validity(leaf)?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("reading the key")?;
        let key = Arc::new(certified(chain, &key)?);
        *self.current.write().unwrap() = Current {
            key,
            fingerprint: None,
            not_before,
            not_after,
        };
        Ok(())
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let acme = hello
            .alpn()
            .map(|mut protocols| protocols.any(|p| p == ACME_TLS_ALPN))
            .unwrap_or(false);
        if acme {
            let challenges = self.challenges.lock().unwrap();
            // A domain's validation names it in SNI. An IP address's can't (it sends
            // the reverse-DNS name, or none), so with one challenge pending, that's it.
            let by_name = hello.server_name().and_then(|n| challenges.get(n));
            return by_name
                .or_else(|| (challenges.len() == 1).then(|| challenges.values().next())?)
                .cloned();
        }
        Some(self.current.read().unwrap().key.clone())
    }
}

fn validity(cert: &CertificateDer<'_>) -> Result<(i64, i64)> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| anyhow!("reading the certificate: {e}"))?;
    let v = parsed.validity();
    Ok((v.not_before.timestamp(), v.not_after.timestamp()))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Renew once half the certificate's life has gone: about three days into a six day
/// one, leaving plenty of room for retries.
fn renew_at(not_before: i64, not_after: i64) -> i64 {
    not_before + (not_after - not_before) / 2
}

/// Getting and renewing a certificate for one name.
pub struct Acme {
    pub name: Name,
    /// The ACME directory: Let's Encrypt unless `COVE_ACME_DIRECTORY` says otherwise.
    pub directory: String,
    pub email: Option<String>,
    /// A PEM root to trust for the ACME server itself, for test servers like Pebble.
    pub root: Option<PathBuf>,
    /// Where the account and certificates are kept: `~/.cove/tls/acme`.
    pub dir: PathBuf,
}

impl Acme {
    fn cert_path(&self) -> PathBuf {
        self.dir.join(format!("{}.pem", self.name.slug()))
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(format!("{}.key", self.name.slug()))
    }

    /// Serve the certificate saved last time, if it's still valid.
    pub fn load_saved(&self, certs: &Certs) -> bool {
        let (Ok(chain), Ok(key)) = (
            std::fs::read_to_string(self.cert_path()),
            std::fs::read_to_string(self.key_path()),
        ) else {
            return false;
        };
        if certs.install(&chain, &key).is_err() {
            return false;
        }
        if certs.expires().unwrap_or(0) <= now() + 60 {
            // Expired: keep it out until a new one arrives.
            return false;
        }
        true
    }

    /// Keep a valid certificate in `certs`, for as long as the server runs.
    pub async fn run(self, certs: Arc<Certs>, log: impl Fn(String) + Send + 'static) {
        let mut failures = 0u32;
        loop {
            let due = {
                let c = certs.current.read().unwrap();
                if c.fingerprint.is_some() {
                    0
                } else {
                    renew_at(c.not_before, c.not_after)
                }
            };
            if now() < due {
                let wait = (due - now()).clamp(60, 12 * 3600) as u64;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            match self.obtain(&certs).await {
                Ok(()) => {
                    failures = 0;
                    let until = certs.expires().unwrap_or(0);
                    log(format!(
                        "HTTPS certificate for {} from {}, valid for {} more hours",
                        self.name,
                        self.directory,
                        (until - now()) / 3600
                    ));
                }
                Err(e) => {
                    failures += 1;
                    // 1, 2, 4… minutes, at most an hour.
                    let wait = 60u64 << failures.min(6).saturating_sub(1);
                    let wait = wait.min(3600);
                    log(format!(
                        "couldn't get an HTTPS certificate for {}: {e:#}. Trying again in {} \
                         minutes; until then it serves {}",
                        self.name,
                        wait / 60,
                        if certs.fingerprint().is_some() {
                            "its self-signed certificate"
                        } else {
                            "the certificate it has"
                        }
                    ));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }
    }

    async fn account(&self) -> Result<Account> {
        std::fs::create_dir_all(&self.dir)?;
        crate::paths::restrict(&self.dir, 0o700);
        let hash: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(self.directory.as_bytes())
                .iter()
                .take(6)
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let path = self.dir.join(format!("account-{hash}.json"));
        let builder = || -> Result<_> {
            Ok(match &self.root {
                Some(root) => Account::builder_with_root(root)?,
                None => Account::builder()?,
            })
        };
        if let Ok(text) = std::fs::read_to_string(&path) {
            let creds: AccountCredentials = serde_json::from_str(&text)?;
            return Ok(builder()?.from_credentials(creds).await?);
        }
        let contact = self.email.as_ref().map(|e| format!("mailto:{e}"));
        let contacts: Vec<&str> = contact.iter().map(|c| c.as_str()).collect();
        let (account, creds) = builder()?
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory.clone(),
                None,
            )
            .await
            .with_context(|| format!("making an account at {}", self.directory))?;
        std::fs::write(&path, serde_json::to_vec_pretty(&creds)?)?;
        crate::paths::restrict(&path, 0o600);
        Ok(account)
    }

    async fn obtain(&self, certs: &Certs) -> Result<()> {
        let account = self.account().await?;
        let ids = [self.name.identifier()];
        let mut new = NewOrder::new(&ids);
        // Let's Encrypt only issues IP certificates under its short-lived profile.
        if matches!(self.name, Name::Ip(_)) && account.profiles().any(|p| p.name == "shortlived") {
            new = new.profile("shortlived");
        }
        let mut order = account.new_order(&new).await?;
        let result = self.validate(&mut order, certs).await;
        certs.challenges.lock().unwrap().clear();
        result?;

        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.subject_alt_names = vec![self.name.san()?];
        let csr = params.serialize_request(&key)?;
        order.finalize_csr(csr.der()).await?;
        let chain = order
            .poll_certificate(&RetryPolicy::new().timeout(Duration::from_secs(120)))
            .await?;
        let key_pem = key.serialize_pem();
        certs.install(&chain, &key_pem)?;
        std::fs::write(self.key_path(), &key_pem)?;
        crate::paths::restrict(&self.key_path(), 0o600);
        std::fs::write(self.cert_path(), &chain)?;
        Ok(())
    }

    async fn validate(&self, order: &mut instant_acme::Order, certs: &Certs) -> Result<()> {
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => bail!("the authorization is {other:?}"),
            }
            let mut challenge = authz.challenge(ChallengeType::TlsAlpn01).ok_or_else(|| {
                anyhow!(
                    "the ACME server offers no tls-alpn-01 challenge for {}",
                    self.name
                )
            })?;
            let digest = challenge.key_authorization().digest();
            let mut params = CertificateParams::new(Vec::<String>::new())?;
            params.subject_alt_names = vec![self.name.san()?];
            params.custom_extensions = vec![CustomExtension::new_acme_identifier(digest.as_ref())];
            let key = KeyPair::generate()?;
            let cert = params.self_signed(&key)?;
            let der = PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| anyhow!(e))?;
            let ck = Arc::new(certified(vec![cert.der().clone()], &der)?);
            certs
                .challenges
                .lock()
                .unwrap()
                .insert(self.name.to_string(), ck);
            challenge.set_ready().await?;
        }
        let status = order
            .poll_ready(&RetryPolicy::new().timeout(Duration::from_secs(120)))
            .await?;
        if status != OrderStatus::Ready {
            let state = order.refresh().await?;
            let why = state
                .error
                .as_ref()
                .and_then(|p| p.detail.clone())
                .unwrap_or_else(|| format!("{status:?}"));
            bail!(
                "validation failed: {why}. The ACME server must reach port 443 of {} \
                 and get to this server's HTTPS listener",
                self.name
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            Name::parse("203.0.113.7").unwrap(),
            Name::Ip("203.0.113.7".parse().unwrap())
        );
        assert_eq!(
            Name::parse("[2001:db8::1]").unwrap(),
            Name::Ip("2001:db8::1".parse().unwrap())
        );
        assert_eq!(
            Name::parse("Cove.Example.com.").unwrap(),
            Name::Dns("cove.example.com".into())
        );
        assert!(Name::parse("localhost").is_err());
        assert!(Name::parse("bad_name.example").is_err());
        assert!(Name::parse("-x.example").is_err());
    }

    #[test]
    fn renews_halfway() {
        assert_eq!(renew_at(1000, 1000 + 6 * 86400), 1000 + 3 * 86400);
    }
}
