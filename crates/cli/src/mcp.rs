//! `cove mcp`: an MCP server (JSON-RPC over stdio) that lets agents create and use
//! VMs through the cove API.

use crate::client::Client;
use anyhow::{anyhow, Result};
use cove_core::{VmState, VmStatus};
use cove_proto::{Frame, ProcInfo, Request};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Tool output is truncated so one noisy command can't flood the agent's context.
const MAX_OUTPUT: usize = 30_000;

pub async fn serve(client: Client) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = handle(&client, &request).await {
            out.write_all(format!("{response}\n").as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

pub async fn handle(client: &Client, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let params = &request["params"];
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "cove", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Linux VMs you can create, run commands in, and preview web apps from. \
                VMs are isolated from the user's machine. Use vm_exec to run shell commands, and \
                vm_spawn with vm_logs for long jobs and servers that should keep running."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = &params["arguments"];
            Ok(match call(client, name, args).await {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => {
                    json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true })
                }
            })
        }
        method if method.starts_with("notifications/") => return None,
        other => Err(json!({ "code": -32601, "message": format!("unknown method {other}") })),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

fn tools() -> Value {
    let name = json!({ "type": "string", "description": "VM name" });
    let proc = json!({ "type": "string", "description": "process name, unique per VM" });
    json!([
        {
            "name": "vm_list",
            "description": "List VMs with their state.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "vm_create",
            "description": "Create and start a Linux VM (Debian, you are root). Takes a few seconds; the first boot longer.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "lowercase letters, digits and dashes" },
                "cpus": { "type": "integer" },
                "memory_mb": { "type": "integer" },
                "ephemeral": { "type": "boolean", "description": "delete it when stopped" },
                "network": { "type": "string", "enum": ["full", "off", "allowlist"], "description": "full (default); off: no network at all, for untrusted code; allowlist: only the hosts in `allow`" },
                "allow": { "type": "array", "items": { "type": "string" }, "description": "with network allowlist: hosts it may reach (example.com, *.example.com) or presets (@dev: git hosts, package registries and model APIs; @agents: model APIs). Default @dev" },
                "idle_stop_minutes": { "type": "integer", "description": "stop the VM after this many idle minutes" },
                "auto_pause_minutes": { "type": "integer", "description": "pause the VM (memory kept) after this many idle minutes; using it resumes it" },
                "snapshot": { "type": "string", "description": "start from this snapshot (see vm_snapshot_list) instead of a fresh Debian" },
                "timeout_minutes": { "type": "integer", "description": "delete the VM this many minutes from now (1 to 1440), for scratch work" }
            }}
        },
        {
            "name": "vm_egress",
            "description": "Show what a VM may reach over the network, the names of the secrets Cove adds to its requests, and the hosts it recently reached or was refused.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        },
        {
            "name": "vm_exec",
            "description": "Run a shell command in a VM (bash -lc) and return exit code and output.",
            "inputSchema": { "type": "object", "required": ["name", "command"], "properties": {
                "name": name,
                "command": { "type": "string" },
                "cwd": { "type": "string" }
            }}
        },
        {
            "name": "vm_spawn",
            "description": "Start a named background process in a VM (bash -lc). It keeps running after this call returns; read its output with vm_logs.",
            "inputSchema": { "type": "object", "required": ["name", "proc", "command"], "properties": {
                "name": name,
                "proc": proc,
                "command": { "type": "string" },
                "cwd": { "type": "string" }
            }}
        },
        {
            "name": "vm_logs",
            "description": "Return a named process's output so far, without waiting, and whether it is still running. Pass the returned next offset as `from` to get only newer output.",
            "inputSchema": { "type": "object", "required": ["name", "proc"], "properties": {
                "name": name,
                "proc": proc,
                "from": { "type": "integer", "description": "byte offset to start from (default 0)" }
            }}
        },
        {
            "name": "vm_kill",
            "description": "Signal a named process (SIGTERM by default). Killing one that already exited forgets it.",
            "inputSchema": { "type": "object", "required": ["name", "proc"], "properties": {
                "name": name,
                "proc": proc,
                "signal": { "type": "integer", "description": "signal number, e.g. 9 for SIGKILL" }
            }}
        },
        {
            "name": "vm_read_file",
            "description": "Read a text file from a VM.",
            "inputSchema": { "type": "object", "required": ["name", "path"], "properties": {
                "name": name, "path": { "type": "string" }
            }}
        },
        {
            "name": "vm_write_file",
            "description": "Write a text file in a VM, creating folders as needed.",
            "inputSchema": { "type": "object", "required": ["name", "path", "content"], "properties": {
                "name": name, "path": { "type": "string" }, "content": { "type": "string" }
            }}
        },
        {
            "name": "vm_share_port",
            "description": "Make a port a server in the VM listens on reachable from the user's machine; returns the URL.",
            "inputSchema": { "type": "object", "required": ["name", "port"], "properties": {
                "name": name, "port": { "type": "integer" }
            }}
        },
        {
            "name": "vm_stop",
            "description": "Stop a VM to free its CPU and memory. Its disk is kept unless it is ephemeral; vm_start boots it again.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        },
        {
            "name": "vm_start",
            "description": "Start a stopped VM.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        },
        {
            "name": "vm_pause",
            "description": "Pause a running VM: memory and processes are kept and it uses no CPU until vm_resume.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        },
        {
            "name": "vm_resume",
            "description": "Resume a paused VM where it left off.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        },
        {
            "name": "vm_snapshot_save",
            "description": "Save a VM's disk as a named snapshot (installed software and files, not memory). A running VM keeps running. New VMs can start from it with vm_create's snapshot, e.g. to set up a project once.",
            "inputSchema": { "type": "object", "required": ["name", "snapshot"], "properties": {
                "name": name,
                "snapshot": { "type": "string", "description": "name for the snapshot: lowercase letters, digits and dashes" },
                "description": { "type": "string" }
            }}
        },
        {
            "name": "vm_snapshot_list",
            "description": "List saved snapshots.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "vm_copy",
            "description": "Copy a stopped VM (disk and settings) to a new VM, e.g. to try something risky on a copy.",
            "inputSchema": { "type": "object", "required": ["name", "to"], "properties": {
                "name": name,
                "to": { "type": "string", "description": "name for the copy" }
            }}
        },
        {
            "name": "vm_destroy",
            "description": "Stop and delete a VM and its disk.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": { "name": name } }
        }
    ])
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().ok_or_else(|| anyhow!("missing {key}"))
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
}

async fn call(client: &Client, tool: &str, args: &Value) -> Result<String> {
    match tool {
        "vm_list" => {
            let vms: Vec<VmStatus> = client.get("/vms").await?;
            if vms.is_empty() {
                return Ok("No VMs.".into());
            }
            Ok(vms
                .iter()
                .map(|v| {
                    format!(
                        "{} ({:?}, {} CPUs, {} MB)",
                        v.name, v.state, v.cpus, v.memory_mb
                    )
                    .to_lowercase()
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "vm_create" => {
            let mut body = json!({ "name": arg(args, "name")? });
            for key in [
                "cpus",
                "memory_mb",
                "ephemeral",
                "network",
                "allow",
                "idle_stop_minutes",
                "auto_pause_minutes",
                "snapshot",
                "timeout_minutes",
            ] {
                if !args[key].is_null() {
                    body[key] = args[key].clone();
                }
            }
            if body["network"] == "allowlist" && body["allow"].is_null() {
                body["allow"] = json!(["@dev"]);
            }
            if !body["timeout_minutes"].is_null() {
                body["on_timeout"] = json!("delete");
            }
            let vm: VmStatus = client.post("/vms", &body).await?;
            Ok(format!("VM {} is running.", vm.name))
        }
        "vm_pause" => {
            let name = arg(args, "name")?;
            let vm: VmStatus = client.post(&format!("/vms/{name}/pause"), &()).await?;
            Ok(format!("VM {} is {}.", vm.name, state_text(vm.state)))
        }
        "vm_resume" => {
            let name = arg(args, "name")?;
            let vm: VmStatus = client.post(&format!("/vms/{name}/resume"), &()).await?;
            Ok(format!("VM {} is {}.", vm.name, state_text(vm.state)))
        }
        "vm_snapshot_save" => {
            let name = arg(args, "name")?;
            let snap: cove_core::snapshot::SnapshotInfo = client
                .post(
                    &format!("/vms/{name}/snapshots"),
                    &json!({
                        "name": arg(args, "snapshot")?,
                        "description": args["description"].as_str().unwrap_or(""),
                    }),
                )
                .await?;
            Ok(format!(
                "Saved {name} as snapshot {} ({} MB).",
                snap.name,
                snap.size >> 20
            ))
        }
        "vm_snapshot_list" => {
            let snaps: Vec<cove_core::snapshot::SnapshotInfo> = client.get("/snapshots").await?;
            if snaps.is_empty() {
                return Ok("No snapshots yet.".into());
            }
            Ok(snaps
                .iter()
                .map(|s| {
                    let mut line =
                        format!("{} (from {}, {} MB)", s.name, s.source_vm, s.size >> 20);
                    if !s.description.is_empty() {
                        line.push_str(": ");
                        line.push_str(&s.description);
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "vm_egress" => {
            let name = arg(args, "name")?;
            let r: Value = client.get(&format!("/vms/{name}/egress")).await?;
            Ok(serde_json::to_string_pretty(&r)?)
        }
        "vm_exec" => {
            let name = arg(args, "name")?;
            let command = json!({
                "cmd": ["bash", "-lc", arg(args, "command")?],
                "cwd": args["cwd"].as_str(),
            });
            let out: Value = client.post(&format!("/vms/{name}/exec"), &command).await?;
            let mut text = format!(
                "exit code: {}\n",
                out["code"]
                    .as_i64()
                    .map(|c| c.to_string())
                    .unwrap_or("killed".into())
            );
            let stdout = out["stdout"].as_str().unwrap_or("");
            let stderr = out["stderr"].as_str().unwrap_or("");
            if !stdout.is_empty() {
                text.push_str(&format!("stdout:\n{}\n", clip(stdout)));
            }
            if !stderr.is_empty() {
                text.push_str(&format!("stderr:\n{}\n", clip(stderr)));
            }
            Ok(text)
        }
        "vm_spawn" => {
            let name = arg(args, "name")?;
            let proc = arg(args, "proc")?;
            let request = Request::Spawn {
                name: proc.to_string(),
                command: cove_proto::Command {
                    cmd: vec!["bash".into(), "-lc".into(), arg(args, "command")?.into()],
                    cwd: args["cwd"].as_str().map(String::from),
                    ..Default::default()
                },
            };
            // The process outlives the stream, so read the reply and hang up.
            let mut stream = client.stream(name, &request).await?;
            match stream.next().await? {
                Some(Frame::Ok { .. }) => Ok(format!(
                    "Started {proc} in {name}. Read its output with vm_logs."
                )),
                Some(Frame::Error { message }) => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected reply: {other:?}")),
            }
        }
        "vm_logs" => {
            let name = arg(args, "name")?;
            let proc = arg(args, "proc")?;
            let from = args["from"].as_u64().unwrap_or(0);
            logs(client, name, proc, from).await
        }
        "vm_kill" => {
            let name = arg(args, "name")?;
            let proc = arg(args, "proc")?;
            let path = match args["signal"].as_i64() {
                Some(s) => format!("/vms/{name}/procs/{proc}?signal={s}"),
                None => format!("/vms/{name}/procs/{proc}"),
            };
            let _: Value = client.delete(&path).await?;
            Ok(format!("Signalled {proc}."))
        }
        "vm_stop" => {
            let name = arg(args, "name")?;
            let vm: VmStatus = client.post(&format!("/vms/{name}/stop"), &()).await?;
            Ok(format!("VM {} is {}.", vm.name, state_text(vm.state)))
        }
        "vm_start" => {
            let name = arg(args, "name")?;
            let vm: VmStatus = client.post(&format!("/vms/{name}/start"), &()).await?;
            Ok(format!("VM {} is {}.", vm.name, state_text(vm.state)))
        }
        "vm_copy" => {
            let name = arg(args, "name")?;
            let to = arg(args, "to")?;
            let vm: VmStatus = client
                .post(&format!("/vms/{name}/copy"), &json!({ "to": to }))
                .await?;
            Ok(format!("Copied {name} to {} (stopped).", vm.name))
        }
        "vm_read_file" => {
            let data = client
                .read_file(arg(args, "name")?, arg(args, "path")?)
                .await?;
            Ok(clip(&String::from_utf8_lossy(&data)))
        }
        "vm_write_file" => {
            let path = arg(args, "path")?;
            client
                .write_file(
                    arg(args, "name")?,
                    path,
                    arg(args, "content")?.as_bytes().to_vec(),
                    None,
                )
                .await?;
            Ok(format!("Wrote {path}."))
        }
        "vm_share_port" => {
            let name = arg(args, "name")?;
            let port = args["port"]
                .as_u64()
                .ok_or_else(|| anyhow!("missing port"))?;
            let f: Value = client
                .post(
                    &format!("/vms/{name}/forwards"),
                    &json!({ "guest_port": port }),
                )
                .await?;
            Ok(f["url"].as_str().unwrap_or_default().to_string())
        }
        "vm_destroy" => {
            let name = arg(args, "name")?;
            let _: Value = client.delete(&format!("/vms/{name}")).await?;
            Ok(format!("Deleted {name}."))
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

fn state_text(state: VmState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// A named process's output from byte `from` up to what it had produced when asked.
/// Attaching follows forever, so stop at that point (or after a few seconds) instead.
async fn logs(client: &Client, vm: &str, proc: &str, from: u64) -> Result<String> {
    let procs: Vec<ProcInfo> = client.get(&format!("/vms/{vm}/procs")).await?;
    let info = procs
        .into_iter()
        .find(|p| p.name == proc)
        .ok_or_else(|| anyhow!("no process named {proc:?}"))?;
    let end = info.output_len;
    let mut data = Vec::new();
    let mut start = from.min(end);
    let mut note = String::new();
    if start < end {
        let mut stream = client
            .stream(
                vm,
                &Request::Attach {
                    name: proc.to_string(),
                    from: start,
                },
            )
            .await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while start + (data.len() as u64) < end && data.len() < MAX_OUTPUT {
            let next = tokio::time::timeout_at(deadline, stream.next()).await;
            match next {
                Ok(Ok(Some(Frame::Stdout { data: d, .. } | Frame::Stderr { data: d, .. }))) => {
                    data.extend(d.0)
                }
                Ok(Ok(Some(Frame::Truncated { offset }))) => {
                    note = format!("(output before byte {offset} is no longer kept)\n");
                    start = offset;
                }
                Ok(Ok(Some(Frame::Error { message }))) => return Err(anyhow!(message)),
                Ok(Ok(Some(Frame::Exit { .. }))) | Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(Some(Frame::Ok { .. }))) => {}
            }
        }
    }
    let keep = keep_len(&data, (end.saturating_sub(start) as usize).min(MAX_OUTPUT));
    data.truncate(keep);
    let next = start + data.len() as u64;
    let state = if info.running {
        "is running".to_string()
    } else {
        match info.exit_code {
            Some(c) => format!("exited with code {c}"),
            None => "was killed".to_string(),
        }
    };
    let more = if next < end {
        format!(", {} more bytes", end - next)
    } else {
        String::new()
    };
    Ok(format!(
        "{proc} {state}; output bytes {start}..{next}{more}. Next from: {next}\n{note}{}",
        String::from_utf8_lossy(&data)
    ))
}

/// How many of `data`'s bytes to return, at most `max`, without splitting a UTF-8
/// character, so the next call's `from` starts on a character boundary.
fn keep_len(data: &[u8], max: usize) -> usize {
    let mut end = data.len().min(max);
    if end == data.len() {
        return end;
    }
    let floor = end.saturating_sub(3);
    while end > floor && end < data.len() && (data[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_listed() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for t in [
            "vm_create",
            "vm_spawn",
            "vm_logs",
            "vm_kill",
            "vm_stop",
            "vm_start",
            "vm_copy",
            "vm_egress",
            "vm_pause",
            "vm_resume",
            "vm_snapshot_save",
            "vm_snapshot_list",
        ] {
            assert!(names.contains(&t), "{t} missing");
        }
        let create = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "vm_create")
            .unwrap();
        assert_eq!(
            create["inputSchema"]["properties"]["network"]["enum"],
            json!(["full", "off", "allowlist"])
        );
        assert!(create["inputSchema"]["properties"]["allow"].is_object());
        // Agents can't hand a VM secrets or widen an allowlist through MCP.
        assert!(create["inputSchema"]["properties"]["secrets"].is_null());
        assert!(!names.contains(&"vm_allow"));
    }

    #[test]
    fn logs_cut_on_character_boundaries() {
        let text = "aé".as_bytes(); // 'é' is two bytes
        assert_eq!(keep_len(text, 10), 3);
        assert_eq!(keep_len(text, 3), 3);
        assert_eq!(keep_len(text, 2), 1);
        assert_eq!(keep_len(b"abc", 2), 2);
    }
}
