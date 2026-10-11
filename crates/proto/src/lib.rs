//! Wire types for cove.
//!
//! Host and guest talk over one stream per request (vsock in a real VM, TCP for the
//! dev backend). The host writes one [`Request`] as a JSON line. The guest answers
//! with [`Frame`]s, one JSON line each. Streaming requests (exec, spawn, attach) keep
//! the stream open: the host may send [`Input`] lines while the guest sends output,
//! until the guest sends [`Frame::Exit`] or [`Frame::Error`].
//!
//! The HTTP API reuses the same types, so a WebSocket client sees exactly these frames.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// The vsock port `cove-guest` listens on inside every VM.
pub const GUEST_PORT: u32 = 1024;

/// Bump when a change would break an older guest or host.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// What to run. `cmd[0]` is looked up on `PATH`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Run in a pseudo-terminal of this size. Output then arrives as `Stdout` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<PtySize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Health check. Answered with `Ok({"version", "protocol", "hostname"})`.
    Ping,
    /// Run a command tied to this stream: it is killed if the stream closes.
    Exec(Command),
    /// Start a supervised process that outlives the stream, then attach to it.
    /// Fails if a running process already has this name.
    Spawn {
        name: String,
        command: Command,
    },
    /// Stream a supervised process's output from byte `from` (0 replays everything
    /// still buffered), then follow it. Input lines go to the process.
    Attach {
        name: String,
        #[serde(default)]
        from: u64,
    },
    /// List supervised processes.
    Procs,
    /// Signal a supervised process (default SIGTERM). Killing one that already exited
    /// removes it from the list.
    Kill {
        name: String,
        #[serde(default)]
        signal: Option<i32>,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        data: Bytes,
        #[serde(default)]
        mode: Option<u32>,
    },
    ListDir {
        path: String,
    },
    /// TCP ports listening inside the guest.
    Ports,
    /// After `Ok`, the stream becomes a raw byte pipe to `127.0.0.1:port` in the guest.
    Tcp {
        port: u16,
    },
    /// Mount a virtio-fs share by tag.
    Mount {
        tag: String,
        path: String,
        #[serde(default)]
        read_only: bool,
    },
    /// Power the guest off.
    Shutdown,
}

/// Lines the host may send during a streaming request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Stdin {
        data: Bytes,
    },
    /// Close the process's stdin (ignored for PTYs).
    Eof,
    Resize(PtySize),
    Signal {
        signal: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Ok {
        value: serde_json::Value,
    },
    /// `offset` is the position of this chunk in the process's whole output, so a
    /// client that reconnects can `Attach { from: offset + len }` and miss nothing.
    Stdout {
        data: Bytes,
        #[serde(default)]
        offset: u64,
    },
    Stderr {
        data: Bytes,
        #[serde(default)]
        offset: u64,
    },
    /// Older output was dropped from the buffer; replay starts at `offset`.
    Truncated {
        offset: u64,
    },
    Exit {
        code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
    },
    Error {
        message: String,
    },
}

impl Frame {
    pub fn error(message: impl Into<String>) -> Self {
        Frame::Error {
            message: message.into(),
        }
    }

    pub fn ok(value: impl Serialize) -> Self {
        Frame::Ok {
            value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// Bytes carried as base64 in JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Bytes(v.to_vec())
    }
}

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcInfo {
    pub name: String,
    pub cmd: Vec<String>,
    pub pid: Option<u32>,
    pub running: bool,
    pub exit_code: Option<i32>,
    /// Total bytes of output produced so far.
    pub output_len: u64,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPort {
    pub port: u16,
    pub address: String,
}

/// Encode one value as a JSON line.
pub fn line<T: Serialize>(value: &T) -> Vec<u8> {
    let mut out = serde_json::to_vec(value).expect("wire types always serialize");
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let req = Request::Exec(Command {
            cmd: vec!["echo".into(), "hi".into()],
            cwd: Some("/work".into()),
            env: vec![("A".into(), "1".into())],
            pty: Some(PtySize {
                rows: 40,
                cols: 120,
            }),
        });
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("\"op\":\"exec\""));
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), req);

        let attach: Request = serde_json::from_str(r#"{"op":"attach","name":"s1"}"#).unwrap();
        assert_eq!(
            attach,
            Request::Attach {
                name: "s1".into(),
                from: 0
            }
        );
    }

    #[test]
    fn bytes_are_base64() {
        let frame = Frame::Stdout {
            data: Bytes(b"hi\n".to_vec()),
            offset: 3,
        };
        let text = serde_json::to_string(&frame).unwrap();
        assert_eq!(text, r#"{"type":"stdout","data":"aGkK","offset":3}"#);
        assert_eq!(serde_json::from_str::<Frame>(&text).unwrap(), frame);
    }
}
