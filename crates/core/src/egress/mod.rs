//! The egress proxy: the only way out of a VM whose network is `allowlist`, and the
//! place secrets are added to requests so their values never enter a VM.
//!
//! Programs in the VM use it as an ordinary HTTP proxy (`HTTPS_PROXY`). Connections
//! reach it through the guest agent (see `Request::EgressAccept`), so the VM needs no
//! network device. For each connection the proxy:
//!
//! - refuses hosts that aren't on the VM's allowlist, and addresses that belong to
//!   the host itself or the cloud metadata service;
//! - passes HTTPS straight through (`CONNECT`) to hosts that get no secrets, so it
//!   never sees that traffic;
//! - for hosts that get secrets, terminates TLS with a certificate from Cove's own
//!   authority (installed in the VM), swaps each secret's placeholder for its value
//!   in the request headers, and forwards the request to the real server over TLS.
//!
//! Every decision goes into a short log the API shows, so a person can see what an
//! agent tried to reach.

pub mod policy;
pub mod tls;

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use policy::{Allowed, Policy};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use tls::Tls;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const LOG_LEN: usize = 200;
const MAX_HEAD: usize = 64 * 1024;

/// One thing the proxy decided.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub at: u64,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    /// Why it was refused, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Secrets added to requests on this connection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

/// A VM's proxy: its current rules and its recent decisions.
pub struct Egress {
    policy: RwLock<Arc<Policy>>,
    log: Mutex<VecDeque<LogEntry>>,
    tls: Arc<Tls>,
    /// Let VMs reach this machine's own addresses. Only for tests, with
    /// `COVE_EGRESS_ALLOW_LOCAL=1`.
    pub(crate) allow_local: bool,
}

impl Egress {
    pub fn new(policy: Policy, tls: Arc<Tls>) -> Egress {
        Egress {
            policy: RwLock::new(Arc::new(policy)),
            log: Mutex::new(VecDeque::new()),
            tls,
            allow_local: std::env::var("COVE_EGRESS_ALLOW_LOCAL").is_ok_and(|v| v == "1"),
        }
    }

    pub fn set_policy(&self, policy: Policy) {
        *self.policy.write().unwrap() = Arc::new(policy);
    }

    fn policy(&self) -> Arc<Policy> {
        self.policy.read().unwrap().clone()
    }

    pub fn log(&self) -> Vec<LogEntry> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    fn record(&self, entry: LogEntry) {
        let mut log = self.log.lock().unwrap();
        if log.len() == LOG_LEN {
            log.pop_front();
        }
        log.push_back(entry);
    }

    fn refuse_entry(&self, host: &str, port: u16, reason: &str) {
        self.record(LogEntry {
            at: crate::now(),
            host: host.to_string(),
            port,
            allowed: false,
            reason: Some(reason.to_string()),
            secrets: Vec::new(),
        });
    }

    /// Serve one connection from a program in the VM.
    pub async fn handle<S>(self: Arc<Self>, mut client: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (head, rest) = match read_head(&mut client).await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        let parsed = req.parse(&head);
        let (Ok(httparse::Status::Complete(_)), Some(method), Some(target)) =
            (parsed, req.method, req.path)
        else {
            let _ = reply(&mut client, 400, "cove: not an HTTP proxy request").await;
            return;
        };
        let result = if method.eq_ignore_ascii_case("CONNECT") {
            let target = target.to_string();
            self.connect(client, &target, rest).await
        } else {
            self.plain(client, &head, rest).await
        };
        if let Err(e) = result {
            eprintln!("egress: {e:#}");
        }
    }

    /// Decide whether `host:port` may be reached, logging refusals. On success,
    /// returns the addresses to connect to.
    async fn admit(&self, host: &str, port: u16) -> std::result::Result<Vec<SocketAddr>, String> {
        let policy = self.policy();
        let Some(how) = policy.check(host) else {
            let reason = "not on this VM's allowlist";
            self.refuse_entry(host, port, reason);
            return Err(format!("{host} is {reason}"));
        };
        let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
            Ok(a) => a.collect(),
            Err(e) => {
                let reason = format!("could not resolve: {e}");
                self.refuse_entry(host, port, &reason);
                return Err(format!("{host}: {reason}"));
            }
        };
        let usable: Vec<SocketAddr> = addrs
            .into_iter()
            .filter(|a| self.allow_local || !policy::forbidden(a.ip()))
            .filter(|a| how == Allowed::Exactly || !policy::private(a.ip()))
            .collect();
        if usable.is_empty() {
            let reason = "resolves to a local or private address";
            self.refuse_entry(host, port, reason);
            return Err(format!("{host} {reason}"));
        }
        Ok(usable)
    }

    async fn connect<S>(self: Arc<Self>, mut client: S, target: &str, rest: Vec<u8>) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (host, port) = split_host_port(target, 443)?;
        let addrs = match self.admit(&host, port).await {
            Ok(a) => a,
            Err(why) => {
                reply(&mut client, 403, &format!("cove: {why}")).await?;
                return Ok(());
            }
        };
        let policy = self.policy();
        let secrets: Vec<model::Secret> = policy.secrets_for(&host).into_iter().cloned().collect();
        let upstream = match tokio::net::TcpStream::connect(&addrs[..]).await {
            Ok(s) => s,
            Err(e) => {
                self.refuse_entry(&host, port, &format!("could not connect: {e}"));
                reply(
                    &mut client,
                    502,
                    &format!("cove: could not reach {host}: {e}"),
                )
                .await?;
                return Ok(());
            }
        };
        let _ = upstream.set_nodelay(true);
        self.record(LogEntry {
            at: crate::now(),
            host: host.clone(),
            port,
            allowed: true,
            reason: None,
            secrets: secrets.iter().map(|s| s.name.clone()).collect(),
        });
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        client.flush().await?;
        let client = Prefixed::new(rest, client);
        if secrets.is_empty() {
            let mut client = client;
            let mut upstream = upstream;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            return Ok(());
        }
        self.broker(client, upstream, &host, secrets).await
    }

    /// Terminate TLS on both sides and add secrets to each request.
    async fn broker<S>(
        &self,
        client: Prefixed<S>,
        upstream: tokio::net::TcpStream,
        host: &str,
        secrets: Vec<model::Secret>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls.server_for(host)?);
        let client = acceptor.accept(client).await?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| anyhow!("{host} is not a valid TLS name"))?;
        let upstream = tokio_rustls::TlsConnector::from(self.tls.upstream.clone())
            .connect(name, upstream)
            .await?;
        let (sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(upstream)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let sender = Arc::new(tokio::sync::Mutex::new(sender));
        let secrets = Arc::new(secrets);
        let service =
            hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                let sender = sender.clone();
                let secrets = secrets.clone();
                async move {
                    for value in req.headers_mut().values_mut() {
                        if let Some(new) = fill_secrets(value.as_bytes(), &secrets) {
                            if let Ok(v) = hyper::header::HeaderValue::from_bytes(&new) {
                                *value = v;
                            }
                        }
                    }
                    let mut sender = sender.lock().await;
                    match sender.send_request(req).await {
                        Ok(resp) => Ok::<_, hyper::Error>(resp.map(Body::Upstream)),
                        Err(e) => Ok(error_response(502, &format!("cove: upstream error: {e}"))),
                    }
                }
            });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(client), service)
            .await?;
        Ok(())
    }

    /// Plain HTTP (`GET http://host/path`): forwarded one request per connection.
    /// Secrets are never added to it.
    async fn plain<S>(&self, mut client: S, head: &[u8], rest: Vec<u8>) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let Some((host, port, head)) = origin_form(head) else {
            reply(&mut client, 400, "cove: expected an absolute http:// URL").await?;
            return Ok(());
        };
        let addrs = match self.admit(&host, port).await {
            Ok(a) => a,
            Err(why) => {
                reply(&mut client, 403, &format!("cove: {why}")).await?;
                return Ok(());
            }
        };
        let mut upstream = match tokio::net::TcpStream::connect(&addrs[..]).await {
            Ok(s) => s,
            Err(e) => {
                reply(
                    &mut client,
                    502,
                    &format!("cove: could not reach {host}: {e}"),
                )
                .await?;
                return Ok(());
            }
        };
        self.record(LogEntry {
            at: crate::now(),
            host,
            port,
            allowed: true,
            reason: None,
            secrets: Vec::new(),
        });
        upstream.write_all(&head).await?;
        upstream.write_all(&rest).await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        Ok(())
    }
}

use crate::model;

/// Replace placeholders with values in one header value, including inside a
/// `Basic` credential (how git sends a token). `None` when nothing changed.
pub fn fill_secrets(value: &[u8], secrets: &[model::Secret]) -> Option<Vec<u8>> {
    let mut out = value.to_vec();
    let mut changed = false;
    for s in secrets {
        if let Some(v) = replace_all(&out, s.placeholder.as_bytes(), s.value.as_bytes()) {
            out = v;
            changed = true;
        }
    }
    if let Some(encoded) = out.strip_prefix(b"Basic ") {
        let engine = base64::engine::general_purpose::STANDARD;
        if let Ok(decoded) = engine.decode(encoded.trim_ascii()) {
            let mut inner = decoded;
            let mut inner_changed = false;
            for s in secrets {
                if let Some(v) = replace_all(&inner, s.placeholder.as_bytes(), s.value.as_bytes()) {
                    inner = v;
                    inner_changed = true;
                }
            }
            if inner_changed {
                out = format!("Basic {}", engine.encode(inner)).into_bytes();
                changed = true;
            }
        }
    }
    changed.then_some(out)
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    if from.is_empty() || haystack.len() < from.len() {
        return None;
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    let mut found = false;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
            found = true;
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    found.then_some(out)
}

/// Read up to the end of the request head. Returns the head and any bytes after it.
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before the request head ended");
        }
        let start = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = start + i + 4;
            let rest = buf.split_off(end);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
    }
}

fn split_host_port(target: &str, default_port: u16) -> Result<(String, u16)> {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("bad target {target:?}"))?;
        (h, after.strip_prefix(':'))
    } else {
        match target.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            _ => (target, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().map_err(|_| anyhow!("bad port in {target:?}"))?,
        None => default_port,
    };
    if host.is_empty() {
        bail!("no host in {target:?}");
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Turn a proxy request head (`GET http://host:port/path HTTP/1.1`) into the head to
/// send upstream: origin-form path, hop-by-hop headers dropped, `Connection: close`.
fn origin_form(head: &[u8]) -> Option<(String, u16, Vec<u8>)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.splitn(3, ' ');
    let (method, url, version) = (parts.next()?, parts.next()?, parts.next()?);
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, 80).ok()?;
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let name = line
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "connection" | "proxy-connection" | "keep-alive" | "proxy-authorization"
        ) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    Some((host, port, out.into_bytes()))
}

async fn reply<S: AsyncWrite + Unpin>(s: &mut S, status: u16, text: &str) -> Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Bad Gateway",
    };
    let body = format!("{text}\n");
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(msg.as_bytes()).await?;
    s.flush().await?;
    Ok(())
}

/// A response body: the upstream's, or a short error of our own.
enum Body {
    Upstream(hyper::body::Incoming),
    Text(Option<hyper::body::Bytes>),
}

impl hyper::body::Body for Body {
    type Data = hyper::body::Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>>
    {
        match self.get_mut() {
            Body::Upstream(b) => std::pin::Pin::new(b).poll_frame(cx),
            Body::Text(t) => {
                std::task::Poll::Ready(t.take().map(|b| Ok(hyper::body::Frame::data(b))))
            }
        }
    }
}

fn error_response(status: u16, text: &str) -> hyper::Response<Body> {
    let mut resp = hyper::Response::new(Body::Text(Some(format!("{text}\n").into())));
    *resp.status_mut() =
        hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    resp
}

/// A stream with some bytes already read from it put back in front.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Prefixed {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
