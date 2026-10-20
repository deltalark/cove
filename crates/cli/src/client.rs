//! Client for the cove HTTP API, shared by the CLI and the MCP server.

use anyhow::{anyhow, bail, Context, Result};
use cove_core::paths::Paths;
use cove_proto::{Frame, Input, Request};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct Client {
    pub url: String,
    key: String,
    http: reqwest::Client,
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
        Ok(Client {
            url,
            key,
            http: reqwest::Client::new(),
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
        let (mut socket, _) = tokio_tungstenite::connect_async(req)
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
}

pub fn load_login(paths: &Paths) -> Option<Login> {
    let text = std::fs::read_to_string(paths.login_file()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_login(paths: &Paths, url: &str, key: &str) -> Result<()> {
    std::fs::create_dir_all(&paths.root)?;
    let path = paths.login_file();
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&Login {
            url: url.to_string(),
            key: key.to_string(),
        })?,
    )?;
    cove_core::paths::restrict(&path, 0o600);
    Ok(())
}

/// Trade a pairing code for a key of our own (`POST /v1/pair`, which needs none).
pub async fn pair(url: &str, code: &str, name: &str) -> Result<serde_json::Value> {
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/pair"))
        .json(&serde_json::json!({ "code": code, "name": name }))
        .timeout(std::time::Duration::from_secs(15))
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
    let response = reqwest::Client::new()
        .get(format!("{url}/v1/health"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("can't reach the cove server at {url}"))?;
    if !response.status().is_success() {
        bail!("the server at {url} answered {}", response.status());
    }
    Ok(response.json().await?)
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
