//! Client for the cove HTTP API, shared by the CLI and the MCP server.

use anyhow::{anyhow, bail, Context, Result};
use cove_core::paths::Paths;
use cove_proto::{Frame, Input, Request};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct Client {
    pub url: String,
    key: String,
    http: reqwest::Client,
    /// The server's certificate when we pin it rather than check it against the web's
    /// authorities: its self-signed one, from pairing.
    fingerprint: Option<String>,
}

impl Client {
    /// `--host`/`COVE_URL`, else the server `cove login` saved, else the local
    /// server's `server.json`, else the default port. `COVE_API_KEY`, else the saved
    /// login's key for that server, else the local `api-key` file.
    pub fn from_env(host: Option<String>) -> Result<Client> {
        let paths = Paths::from_env();
        let url = server_url(host);
        let login = load_login(&paths).filter(|l| l.url == url);
        let key = match std::env::var("COVE_API_KEY") {
            Ok(k) => k,
            Err(_) if login.is_some() => login.map(|l| l.key).unwrap_or_default(),
            Err(_) => std::fs::read_to_string(paths.api_key())
                .map(|k| k.trim().to_string())
                .with_context(|| {
                    format!(
                        "no API key: set COVE_API_KEY or start the server once ({})",
                        paths.api_key().display()
                    )
                })?,
        };
        let fingerprint = pinned_fingerprint(&paths, &url);
        Ok(Client {
            http: http_client(fingerprint.as_deref(), None)?,
            url,
            key,
            fingerprint,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/v1{path}", self.url)
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request
            .bearer_auth(&self.key)
            .send()
            .await
            .with_context(|| {
                format!(
                    "can't reach the cove server at {} (is `cove serve` running?)",
                    self.url
                )
            })?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            bail!("{message}");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.get(self.endpoint(path))).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(self.http.post(self.endpoint(path)).json(body))
            .await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(self.http.put(self.endpoint(path)).json(body))
            .await
    }

    pub async fn patch<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(self.http.patch(self.endpoint(path)).json(body))
            .await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.delete(self.endpoint(path))).await
    }

    pub async fn read_file(&self, vm: &str, path: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(self.endpoint(&format!("/vms/{vm}/files")))
            .query(&[("path", path)])
            .bearer_auth(&self.key)
            .send()
            .await?;
        if !response.status().is_success() {
            let v: serde_json::Value = response.json().await.unwrap_or_default();
            bail!("{}", v["error"].as_str().unwrap_or("read failed"));
        }
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn write_file(
        &self,
        vm: &str,
        path: &str,
        data: Vec<u8>,
        mode: Option<u32>,
    ) -> Result<()> {
        let mut query = vec![("path", path.to_string())];
        if let Some(m) = mode {
            query.push(("mode", format!("{m:o}")));
        }
        let _: serde_json::Value = self
            .send(
                self.http
                    .put(self.endpoint(&format!("/vms/{vm}/files")))
                    .query(&query)
                    .body(data),
            )
            .await?;
        Ok(())
    }

    /// Open a streaming request (exec, spawn or attach).
    pub async fn stream(&self, vm: &str, request: &Request) -> Result<Stream> {
        let ws_url = format!("{}/v1/vms/{vm}/stream", self.url.replacen("http", "ws", 1));
        let mut req = ws_url.into_client_request()?;
        req.headers_mut()
            .insert("authorization", format!("Bearer {}", self.key).parse()?);
        let connector = match &self.fingerprint {
            Some(fp) => Some(tokio_tungstenite::Connector::Rustls(Arc::new(
                cove_core::https::pinned_client_config(fp)?,
            ))),
            None => None,
        };
        let (mut socket, _) =
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector)
                .await
                .map_err(|e| match e {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        let body = response.body().clone().unwrap_or_default();
                        let message = serde_json::from_slice::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| v["error"].as_str().map(String::from))
                            .unwrap_or_else(|| format!("HTTP {}", response.status()));
                        anyhow!(message)
                    }
                    other => anyhow!("opening the stream: {other}"),
                })?;
        socket
            .send(Message::Text(serde_json::to_string(request)?.into()))
            .await?;
        Ok(Stream { socket })
    }
}

/// A server this client logged in to with `cove login`.
#[derive(serde::Deserialize, Serialize)]
pub struct Login {
    pub url: String,
    pub key: String,
    /// The server's self-signed certificate, pinned when we paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

pub fn load_login(paths: &Paths) -> Option<Login> {
    let text = std::fs::read_to_string(paths.login_file()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_login(paths: &Paths, url: &str, key: &str, fingerprint: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(&paths.root)?;
    let path = paths.login_file();
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&Login {
            url: url.to_string(),
            key: key.to_string(),
            fingerprint: fingerprint.map(String::from),
        })?,
    )?;
    cove_core::paths::restrict(&path, 0o600);
    Ok(())
}

/// Trade a pairing code for a key of our own (`POST /v1/pair`, which needs none).
/// With `fingerprint`, the server must show exactly that certificate, checked before
/// the code is sent.
pub async fn pair(
    url: &str,
    code: &str,
    name: &str,
    fingerprint: Option<&str>,
) -> Result<serde_json::Value> {
    let response = http_client(fingerprint, Some(15))?
        .post(format!("{url}/v1/pair"))
        .json(&serde_json::json!({ "code": code, "name": name }))
        .send()
        .await
        .with_context(|| format!("can't reach the cove server at {url}"))?;
    let status = response.status();
    let v: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{}",
            v["error"].as_str().unwrap_or("the server refused the code")
        );
    }
    Ok(v)
}

/// The server to talk to: `--host`/`COVE_URL`, else the server `cove login` saved,
/// else the local server's `server.json`, else the default port.
pub fn server_url(host: Option<String>) -> String {
    let url = host
        .or_else(|| std::env::var("COVE_URL").ok())
        .or_else(|| load_login(&Paths::from_env()).map(|l| l.url))
        .or_else(|| {
            let text = std::fs::read_to_string(Paths::from_env().server_file()).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            v["url"].as_str().map(String::from)
        })
        .unwrap_or_else(|| format!("http://{}", crate::DEFAULT_LISTEN));
    let url = if url.contains("://") {
        url
    } else {
        format!("http://{url}")
    };
    url.trim_end_matches('/').to_string()
}

/// `GET /v1/health`, which needs no API key.
pub async fn health(url: &str) -> Result<serde_json::Value> {
    let fingerprint = pinned_fingerprint(&Paths::from_env(), url);
    let response = http_client(fingerprint.as_deref(), Some(5))?
        .get(format!("{url}/v1/health"))
        .send()
        .await
        .with_context(|| format!("can't reach the cove server at {url}"))?;
    if !response.status().is_success() {
        bail!("the server at {url} answered {}", response.status());
    }
    Ok(response.json().await?)
}

/// An HTTP client that, given a fingerprint, trusts only that certificate.
fn http_client(fingerprint: Option<&str>, timeout_secs: Option<u64>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(fp) = fingerprint {
        builder = builder.use_preconfigured_tls(cove_core::https::pinned_client_config(fp)?);
    }
    if let Some(t) = timeout_secs {
        builder = builder.timeout(std::time::Duration::from_secs(t));
    }
    Ok(builder.build()?)
}

/// The certificate to pin for `url`: `COVE_FINGERPRINT`, else the one saved when this
/// computer paired with it, else the local server's own when `url` is its HTTPS address.
fn pinned_fingerprint(paths: &Paths, url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    if let Ok(fp) = std::env::var("COVE_FINGERPRINT") {
        return Some(fp);
    }
    if let Some(fp) = load_login(paths)
        .filter(|l| l.url == url)
        .and_then(|l| l.fingerprint)
    {
        return Some(fp);
    }
    let text = std::fs::read_to_string(paths.server_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v["https_url"].as_str() == Some(url) {
        return v["fingerprint"].as_str().map(String::from);
    }
    None
}

/// What `cove login` was given: a `cove://` pair string, or a URL and a code.
#[derive(Debug, PartialEq)]
pub struct PairTarget {
    pub url: String,
    pub code: String,
    pub fingerprint: Option<String>,
}

pub fn parse_pair_target(
    first: &str,
    code: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<PairTarget> {
    if let Some(rest) = first.strip_prefix("cove://") {
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (host, code) = path
            .trim_end_matches('/')
            .rsplit_once('/')
            .ok_or_else(|| anyhow!("{first} has no pairing code after the address"))?;
        let mut fp = fingerprint.map(String::from);
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("fp=") {
                fp = Some(v.to_string());
            }
        }
        let fingerprint = fp
            .map(|f| cove_core::https::normalize_fingerprint(&f))
            .transpose()?;
        return Ok(PairTarget {
            url: format!("https://{host}"),
            code: code.to_string(),
            fingerprint,
        });
    }
    let code = code.ok_or_else(|| {
        anyhow!("give the server's URL and the code from cove pair, or the cove:// line it prints")
    })?;
    let url = server_url(Some(first.to_string()));
    let fingerprint = fingerprint
        .map(cove_core::https::normalize_fingerprint)
        .transpose()?;
    if fingerprint.is_some() && !url.starts_with("https://") {
        bail!("--fingerprint is for an https:// address");
    }
    Ok(PairTarget {
        url,
        code: code.to_string(),
        fingerprint,
    })
}

pub struct Stream {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Stream {
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        while let Some(message) = self.socket.next().await {
            match message? {
                Message::Text(t) => return Ok(Some(serde_json::from_str(&t)?)),
                Message::Close(_) => return Ok(None),
                _ => continue,
            }
        }
        Ok(None)
    }

    pub fn split(self) -> (StreamSender, StreamReceiver) {
        let (tx, rx) = self.socket.split();
        (StreamSender { tx }, StreamReceiver { rx })
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct StreamSender {
    tx: futures_util::stream::SplitSink<Socket, Message>,
}

impl StreamSender {
    pub async fn send(&mut self, input: &Input) -> Result<()> {
        self.tx
            .send(Message::Text(serde_json::to_string(input)?.into()))
            .await?;
        Ok(())
    }
}

pub struct StreamReceiver {
    rx: futures_util::stream::SplitStream<Socket>,
}

impl StreamReceiver {
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        while let Some(message) = self.rx.next().await {
            match message? {
                Message::Text(t) => return Ok(Some(serde_json::from_str(&t)?)),
                Message::Close(_) => return Ok(None),
                _ => continue,
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_strings() {
        let fp = format!("sha256:{}", "ab".repeat(32));
        let t = parse_pair_target(
            &format!("cove://203.0.113.7:7443/K7QF-2MXD?fp={fp}"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            t,
            PairTarget {
                url: "https://203.0.113.7:7443".into(),
                code: "K7QF-2MXD".into(),
                fingerprint: Some(fp.clone()),
            }
        );
        let t = parse_pair_target("cove://cove.example.com/ABCD", None, None).unwrap();
        assert_eq!(t.url, "https://cove.example.com");
        assert_eq!(t.fingerprint, None);
        let t = parse_pair_target("https://h:7443/", Some("ABCD"), Some(&"AB".repeat(32))).unwrap();
        assert_eq!(t.url, "https://h:7443");
        assert_eq!(t.fingerprint, Some(fp));
        assert!(parse_pair_target("http://h:7457", Some("ABCD"), Some(&"ab".repeat(32))).is_err());
        assert!(parse_pair_target("https://h", None, None).is_err());
    }
}
