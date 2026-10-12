//! Talking to `cove-guest` from the host.

use anyhow::{anyhow, bail, Context, Result};
use cove_proto::{line, Frame, Request};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// How to open a stream to a VM's guest agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuestAddr {
    /// The dev backend: the agent listens on host TCP.
    Tcp { addr: String },
    /// vfkit in `listen` mode: each connection to this socket reaches guest vsock port 1024.
    Unix { path: PathBuf },
    /// cloud-hypervisor hybrid vsock: connect, send `CONNECT <port>`, read `OK <n>`.
    HybridVsock { path: PathBuf, port: u32 },
}

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

pub type GuestStream = Box<dyn Io>;

impl GuestAddr {
    pub async fn connect(&self) -> Result<GuestStream> {
        match self {
            GuestAddr::Tcp { addr } => {
                let s = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect {addr}"))?;
                s.set_nodelay(true).ok();
                Ok(Box::new(s))
            }
            GuestAddr::Unix { path } => {
                let s = connect_unix(path)
                    .await
                    .with_context(|| format!("connect {}", path.display()))?;
                Ok(Box::new(s))
            }
            GuestAddr::HybridVsock { path, port } => {
                let mut s = connect_unix(path)
                    .await
                    .with_context(|| format!("connect {}", path.display()))?;
                s.write_all(format!("CONNECT {port}\n").as_bytes()).await?;
                // Read the reply a byte at a time so nothing after it is consumed.
                let mut reply = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    use tokio::io::AsyncReadExt;
                    if s.read(&mut byte).await? == 0 {
                        bail!("vsock closed during handshake");
                    }
                    if byte[0] == b'\n' {
                        break;
                    }
                    reply.push(byte[0]);
                }
                if !reply.starts_with(b"OK") {
                    bail!(
                        "vsock handshake failed: {}",
                        String::from_utf8_lossy(&reply)
                    );
                }
                Ok(Box::new(s))
            }
        }
    }

    /// Open a stream and send `request`. The caller reads frames from the result.
    pub async fn open(&self, request: &Request) -> Result<GuestStream> {
        let mut s = self.connect().await?;
        s.write_all(&line(request)).await?;
        s.flush().await?;
        Ok(s)
    }

    /// One request, one `Ok` value back.
    pub async fn call(&self, request: &Request) -> Result<serde_json::Value> {
        let s = self.open(request).await?;
        let mut lines = BufReader::new(s).lines();
        let text = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("guest closed the connection"))?;
        match serde_json::from_str::<Frame>(&text)? {
            Frame::Ok { value } => Ok(value),
            Frame::Error { message } => Err(anyhow!(message)),
            other => Err(anyhow!("unexpected reply: {other:?}")),
        }
    }

    pub async fn call_as<T: for<'de> Deserialize<'de>>(&self, request: &Request) -> Result<T> {
        Ok(serde_json::from_value(self.call(request).await?)?)
    }

    pub async fn ping(&self) -> Result<serde_json::Value> {
        self.call(&Request::Ping).await
    }

    /// Run a command to completion and collect its output.
    pub async fn exec(&self, command: cove_proto::Command) -> Result<ExecOutput> {
        let s = self.open(&Request::Exec(command)).await?;
        let mut lines = BufReader::new(s).lines();
        let mut out = ExecOutput::default();
        while let Some(text) = lines.next_line().await? {
            match serde_json::from_str::<Frame>(&text)? {
                Frame::Stdout { data, .. } => out.stdout.extend(data.0),
                Frame::Stderr { data, .. } => out.stderr.extend(data.0),
                Frame::Exit { code, signal } => {
                    out.code = code;
                    out.signal = signal;
                    return Ok(out);
                }
                Frame::Error { message } => bail!(message),
                _ => {}
            }
        }
        bail!("guest closed the connection before the command exited")
    }
}

/// Unix socket paths are limited to about 100 bytes, which a deep `COVE_HOME` can
/// exceed. On Linux, reach a long one through its folder's open descriptor instead.
pub(crate) async fn connect_unix(
    path: &std::path::Path,
) -> std::io::Result<tokio::net::UnixStream> {
    const MAX_PATH: usize = 100;
    if cfg!(target_os = "linux") && path.as_os_str().len() > MAX_PATH {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            let dir = std::fs::File::open(dir)?;
            let short = PathBuf::from(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(&dir)
            ))
            .join(name);
            // `dir` stays open until the connection is made.
            let stream = tokio::net::UnixStream::connect(&short).await;
            drop(dir);
            return stream;
        }
    }
    tokio::net::UnixStream::connect(path).await
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ExecOutput {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    #[serde(with = "lossy")]
    pub stdout: Vec<u8>,
    #[serde(with = "lossy")]
    pub stderr: Vec<u8>,
}

/// Output is shown as text in the API; binary output should use files or streaming.
mod lossy {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&String::from_utf8_lossy(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Ok(String::deserialize(d)?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn long_socket_paths_still_connect() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a".repeat(60)).join("b".repeat(60));
        std::fs::create_dir_all(&deep).unwrap();
        let path = deep.join("guest.sock");
        assert!(path.as_os_str().len() > 108);
        // Bind through the same trick, since binding has the same limit.
        let fd_dir = std::fs::File::open(&deep).unwrap();
        let short = format!(
            "/proc/self/fd/{}/guest.sock",
            std::os::fd::AsRawFd::as_raw_fd(&fd_dir)
        );
        let listener = tokio::net::UnixListener::bind(&short).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.is_ok() });
        connect_unix(&path).await.unwrap();
        assert!(accept.await.unwrap());
    }
}
