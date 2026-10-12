//! Streaming a VM process to this terminal.

use crate::client::Client;
use anyhow::Result;
use cove_proto::{Bytes, Frame, Input, PtySize, Request};
use std::io::{IsTerminal, Read, Write};

pub fn size() -> PtySize {
    crossterm::terminal::size()
        .map(|(cols, rows)| PtySize { rows, cols })
        .unwrap_or_default()
}

struct RawMode(bool);

impl RawMode {
    fn enable(on: bool) -> RawMode {
        RawMode(on && crossterm::terminal::enable_raw_mode().is_ok())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn uses_pty(request: &Request) -> bool {
    match request {
        Request::Exec(c) | Request::Spawn { command: c, .. } => c.pty.is_some(),
        // Assume an attach to a terminal process when we are on one.
        Request::Attach { .. } => std::io::stdin().is_terminal(),
        _ => false,
    }
}

/// Run a streaming request, wiring stdin, stdout, stderr and terminal size through.
/// Returns the process's exit code (or 0 when detaching from an attach).
pub async fn run(client: &Client, vm: &str, request: Request) -> Result<i32> {
    let pty = uses_pty(&request) && std::io::stdin().is_terminal();
    let forward_stdin = pty || !std::io::stdin().is_terminal();
    let stream = client.stream(vm, &request).await?;
    let (mut tx, mut rx) = stream.split();
    let _raw = RawMode::enable(pty);

    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    if forward_stdin {
        let input_tx = input_tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = input_tx.send(Input::Eof);
                        break;
                    }
                    Ok(n) => {
                        if input_tx
                            .send(Input::Stdin {
                                data: Bytes(buf[..n].to_vec()),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    if pty {
        let input_tx = input_tx.clone();
        tokio::spawn(async move {
            let mut last = size();
            let _ = input_tx.send(Input::Resize(last));
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let now = size();
                if now != last {
                    last = now;
                    if input_tx.send(Input::Resize(now)).is_err() {
                        break;
                    }
                }
            }
        });
    }
    drop(input_tx);
    let sender = tokio::spawn(async move {
        while let Some(input) = input_rx.recv().await {
            if tx.send(&input).await.is_err() {
                break;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut code = 0;
    while let Some(frame) = rx.next().await? {
        match frame {
            Frame::Stdout { data, .. } => {
                stdout.write_all(&data.0)?;
                stdout.flush()?;
            }
            Frame::Stderr { data, .. } => {
                stderr.write_all(&data.0)?;
                stderr.flush()?;
            }
            Frame::Exit { code: c, signal } => {
                code = c.unwrap_or(128 + signal.unwrap_or(0));
                break;
            }
            Frame::Error { message } => {
                drop(_raw);
                anyhow::bail!(message);
            }
            Frame::Truncated { offset } => {
                eprint!("[earlier output dropped; resuming at byte {offset}]\r\n");
            }
            Frame::Ok { .. } => {}
        }
    }
    sender.abort();
    Ok(code)
}
