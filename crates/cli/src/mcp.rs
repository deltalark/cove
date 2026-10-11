//! `cove mcp`: an MCP server (JSON-RPC over stdio) that lets agents create and use
//! VMs through the cove API.

use crate::client::Client;
use anyhow::{anyhow, Result};
use cove_core::VmStatus;
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
                VMs are isolated from the user's machine. Use vm_exec to run shell commands."
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
                "ephemeral": { "type": "boolean", "description": "delete it when stopped" }
            }}
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
            for key in ["cpus", "memory_mb", "ephemeral"] {
                if !args[key].is_null() {
                    body[key] = args[key].clone();
                }
            }
            let vm: VmStatus = client.post("/vms", &body).await?;
            Ok(format!("VM {} is running.", vm.name))
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
