use super::*;
use crate::model::Secret;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncBufReadExt, BufReader};

/// A TLS server for `localhost`, signed by a throwaway authority, that answers every
/// request with the request's `authorization` header.
async fn upstream() -> (u16, CertificateDer<'static>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "test upstream CA");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&key, &ca_cert, &ca_key)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(conn).await else {
                    return;
                };
                let service = hyper::service::service_fn(
                    |req: hyper::Request<hyper::body::Incoming>| async move {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .map(|v| v.to_str().unwrap().to_string())
                            .unwrap_or_default();
                        Ok::<_, hyper::Error>(hyper::Response::new(Body::Text(Some(auth.into()))))
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    (port, ca_cert.der().clone())
}

fn egress(
    dir: &std::path::Path,
    upstream_ca: CertificateDer<'static>,
    policy: Policy,
) -> Arc<Egress> {
    let tls = Arc::new(Tls::load(dir, vec![upstream_ca]).unwrap());
    let mut e = Egress::new(policy, tls);
    e.allow_local = true;
    Arc::new(e)
}

fn secret() -> Secret {
    Secret {
        name: "API_KEY".into(),
        value: "the-real-key".into(),
        hosts: vec!["localhost".into()],
        placeholder: "sk-cove1234".into(),
    }
}

/// Send `CONNECT` through the proxy and return the stream once it's established.
async fn tunnel(e: &Arc<Egress>, target: &str) -> (tokio::io::DuplexStream, String) {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(e.clone().handle(server));
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(client);
    let mut status = String::new();
    reader.read_line(&mut status).await.unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    (reader.into_inner(), status)
}

async fn get_over_tls(
    stream: tokio::io::DuplexStream,
    trust: CertificateDer<'static>,
    auth: &str,
) -> String {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trust).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .unwrap();
    tls.write_all(
        format!("GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn cove_ca(dir: &std::path::Path) -> CertificateDer<'static> {
    let pem = std::fs::read(dir.join("ca.pem")).unwrap();
    CertificateDer::from_pem_slice(&pem).unwrap()
}

use rustls::pki_types::pem::PemObject;

#[tokio::test]
async fn secrets_are_added_for_their_host_only() {
    let dir = tempfile::tempdir().unwrap();
    let (port, upstream_ca) = upstream().await;
    let e = egress(
        dir.path(),
        upstream_ca,
        Policy::new(&["localhost".into()], &[secret()], true),
    );
    let (stream, status) = tunnel(&e, &format!("localhost:{port}")).await;
    assert!(status.contains("200"), "{status}");
    // The VM trusts Cove's authority, and the proxy swaps the placeholder.
    let reply = get_over_tls(stream, cove_ca(dir.path()), "Bearer sk-cove1234").await;
    assert!(reply.contains("Bearer the-real-key"), "{reply}");
    assert!(!reply.contains("sk-cove1234"));
    let log = e.log();
    assert_eq!(log.len(), 1);
    assert!(log[0].allowed);
    assert_eq!(log[0].secrets, vec!["API_KEY".to_string()]);
}

#[tokio::test]
async fn hosts_without_secrets_pass_through_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (port, upstream_ca) = upstream().await;
    let e = egress(
        dir.path(),
        upstream_ca.clone(),
        Policy::new(&["localhost".into()], &[], true),
    );
    let (stream, status) = tunnel(&e, &format!("localhost:{port}")).await;
    assert!(status.contains("200"), "{status}");
    // The client sees the real server's certificate: the proxy isn't in the middle.
    let reply = get_over_tls(stream, upstream_ca, "Bearer sk-cove1234").await;
    assert!(reply.contains("Bearer sk-cove1234"), "{reply}");
}

#[tokio::test]
async fn hosts_off_the_allowlist_are_refused_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (_, upstream_ca) = upstream().await;
    let e = egress(
        dir.path(),
        upstream_ca,
        Policy::new(&["github.com".into()], &[], true),
    );
    let (_, status) = tunnel(&e, "example.org:443").await;
    assert!(status.contains("403"), "{status}");
    let log = e.log();
    assert_eq!(log.len(), 1);
    assert!(!log[0].allowed);
    assert_eq!(log[0].host, "example.org");
    assert!(log[0].reason.as_deref().unwrap().contains("allowlist"));
}

#[tokio::test]
async fn local_addresses_are_refused_even_when_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let (port, upstream_ca) = upstream().await;
    let mut e = Egress::new(
        Policy::new(&["*".into()], &[], true),
        Arc::new(Tls::load(dir.path(), vec![upstream_ca]).unwrap()),
    );
    e.allow_local = false;
    let e = Arc::new(e);
    let (_, status) = tunnel(&e, &format!("localhost:{port}")).await;
    assert!(status.contains("403"), "{status}");
    assert!(e.log()[0].reason.as_deref().unwrap().contains("local"));
}

#[tokio::test]
async fn policy_changes_apply_to_new_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (port, upstream_ca) = upstream().await;
    let e = egress(dir.path(), upstream_ca, Policy::new(&[], &[], true));
    let (_, status) = tunnel(&e, &format!("localhost:{port}")).await;
    assert!(status.contains("403"));
    e.set_policy(Policy::new(&["localhost".into()], &[], true));
    let (_, status) = tunnel(&e, &format!("localhost:{port}")).await;
    assert!(status.contains("200"), "{status}");
}

#[test]
fn basic_credentials_are_filled_inside_the_encoding() {
    let engine = base64::engine::general_purpose::STANDARD;
    let s = secret();
    let header = format!("Basic {}", engine.encode("x-access-token:sk-cove1234"));
    let filled = fill_secrets(header.as_bytes(), std::slice::from_ref(&s)).unwrap();
    let decoded = engine
        .decode(
            std::str::from_utf8(&filled)
                .unwrap()
                .strip_prefix("Basic ")
                .unwrap(),
        )
        .unwrap();
    assert_eq!(decoded, b"x-access-token:the-real-key");
    assert_eq!(
        fill_secrets(b"token sk-cove1234", std::slice::from_ref(&s)).unwrap(),
        b"token the-real-key"
    );
    assert!(fill_secrets(b"Bearer other", &[s]).is_none());
}

#[test]
fn plain_requests_become_origin_form() {
    let head = b"GET http://deb.debian.org/debian/dists HTTP/1.1\r\nHost: deb.debian.org\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
    let (host, port, out) = origin_form(head).unwrap();
    assert_eq!((host.as_str(), port), ("deb.debian.org", 80));
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("GET /debian/dists HTTP/1.1\r\n"), "{out}");
    assert!(!out.to_lowercase().contains("proxy-connection"));
    assert!(out.ends_with("Connection: close\r\n\r\n"));
    assert_eq!(
        split_host_port("[::1]:8080", 443).unwrap(),
        ("::1".into(), 8080)
    );
    assert_eq!(
        split_host_port("a.com", 443).unwrap(),
        ("a.com".into(), 443)
    );
}
