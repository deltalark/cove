//! `cove mcp install`: register `cove mcp` with the agent CLIs on this computer, each the
//! way that agent expects it.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How an agent keeps its MCP servers.
enum How {
    /// Its own CLI adds and removes servers. `scope` goes before the name when given.
    Cli {
        bin: &'static str,
        scope: &'static [&'static str],
        /// Whether `add` wants `--` before the server's command.
        dashdash: bool,
    },
    /// A JSON file in the home folder, with servers under `key`.
    File {
        path: &'static str,
        key: &'static str,
        style: Style,
    },
}

#[derive(Clone, Copy)]
enum Style {
    /// `{"command": "...", "args": [...]}`, the Claude Desktop shape most clients copy.
    McpServers,
    /// OpenCode's `{"type": "local", "command": [...], "enabled": true}`.
    OpenCode,
}

pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    how: How,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        name: "Claude Code",
        how: How::Cli {
            bin: "claude",
            scope: &["-s", "user"],
            dashdash: true,
        },
    },
    Agent {
        id: "codex",
        name: "Codex",
        how: How::Cli {
            bin: "codex",
            scope: &[],
            dashdash: true,
        },
    },
    Agent {
        id: "gemini",
        name: "Gemini CLI",
        how: How::Cli {
            bin: "gemini",
            scope: &["-s", "user"],
            dashdash: false,
        },
    },
    Agent {
        id: "qwen",
        name: "Qwen Code",
        how: How::Cli {
            bin: "qwen",
            scope: &["-s", "user"],
            dashdash: false,
        },
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
        how: How::File {
            path: ".config/opencode/opencode.json",
            key: "mcp",
            style: Style::OpenCode,
        },
    },
    Agent {
        id: "cursor",
        name: "Cursor",
        how: How::File {
            path: ".cursor/mcp.json",
            key: "mcpServers",
            style: Style::McpServers,
        },
    },
];

/// The name Cove is registered under in every agent.
const NAME: &str = "cove";

pub fn find(id: &str) -> Result<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id).ok_or_else(|| {
        let ids: Vec<&str> = AGENTS.iter().map(|a| a.id).collect();
        anyhow!("no agent called {id}. Cove knows: {}", ids.join(", "))
    })
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

impl Agent {
    /// Whether this agent looks installed here.
    pub fn present(&self) -> bool {
        match &self.how {
            How::Cli { bin, .. } => on_path(bin),
            How::File { path, .. } => {
                let dir = Path::new(path).parent().unwrap_or(Path::new(""));
                on_path(self.id) || home().map(|h| h.join(dir).is_dir()).unwrap_or(false)
            }
        }
    }

    /// What `install` runs or writes, for `--print`.
    pub fn describe(&self, cmd: &[String]) -> String {
        match &self.how {
            How::Cli { .. } => shell_line(&self.add_args(cmd)),
            How::File { path, key, style } => {
                format!(
                    "in ~/{path}: {}",
                    json!({ *key: { NAME: entry(*style, cmd) } })
                )
            }
        }
    }

    fn add_args(&self, cmd: &[String]) -> Vec<String> {
        let How::Cli {
            bin,
            scope,
            dashdash,
        } = &self.how
        else {
            return vec![];
        };
        let mut args: Vec<String> = vec![bin.to_string(), "mcp".into(), "add".into()];
        args.extend(scope.iter().map(|s| s.to_string()));
        args.push(NAME.into());
        if *dashdash {
            args.push("--".into());
        }
        args.extend(cmd.iter().cloned());
        args
    }

    /// Register `cmd` as this agent's `cove` MCP server, replacing an older entry.
    pub fn install(&self, cmd: &[String]) -> Result<()> {
        match &self.how {
            How::Cli { bin, scope, .. } => {
                // Remove first, so running it again replaces the entry instead of failing.
                let _ = run(bin, &remove_args(scope), true);
                let args = self.add_args(cmd);
                run(bin, &args[1..], false)
            }
            How::File { path, key, style } => {
                let file = home()?.join(path);
                edit_json(&file, |v| set_server(v, key, Some(entry(*style, cmd))))
            }
        }
    }

    pub fn uninstall(&self) -> Result<()> {
        match &self.how {
            How::Cli { bin, scope, .. } => run(bin, &remove_args(scope), false),
            How::File { path, key, .. } => {
                let file = home()?.join(path);
                if !file.exists() {
                    return Ok(());
                }
                edit_json(&file, |v| set_server(v, key, None))
            }
        }
    }
}

fn remove_args(scope: &[&str]) -> Vec<String> {
    let mut args = vec!["mcp".to_string(), "remove".into()];
    args.extend(scope.iter().map(|s| s.to_string()));
    args.push(NAME.into());
    args
}

fn run(bin: &str, args: &[String], quiet: bool) -> Result<()> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("couldn't run {bin}"))?;
    if !out.status.success() && !quiet {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = String::from_utf8_lossy(&out.stdout);
        let detail = if err.trim().is_empty() { msg } else { err };
        bail!("{bin} mcp failed: {}", detail.trim());
    }
    Ok(())
}

fn entry(style: Style, cmd: &[String]) -> Value {
    match style {
        Style::McpServers => json!({ "command": cmd[0], "args": cmd[1..] }),
        Style::OpenCode => json!({ "type": "local", "command": cmd, "enabled": true }),
    }
}

/// Put `server` under `config[key].cove`, or take it out when `None`.
fn set_server(config: &mut Value, key: &str, server: Option<Value>) -> Result<()> {
    if !config.is_object() {
        bail!("the file isn't a JSON object");
    }
    let servers = config
        .as_object_mut()
        .unwrap()
        .entry(key)
        .or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        bail!("\"{key}\" isn't a JSON object");
    };
    match server {
        Some(s) => {
            servers.insert(NAME.into(), s);
        }
        None => {
            servers.remove(NAME);
        }
    }
    Ok(())
}

/// Change a JSON config file in place. A file that doesn't parse (comments, say) is
/// left alone, so we never throw away someone's settings.
fn edit_json(file: &Path, change: impl FnOnce(&mut Value) -> Result<()>) -> Result<()> {
    let mut config = match std::fs::read_to_string(file) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} isn't plain JSON, so Cove left it alone; add the entry by hand",
                file.display()
            )
        })?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("couldn't read {}", file.display())),
    };
    change(&mut config).with_context(|| format!("in {}", file.display()))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.cove-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&config)? + "\n")?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

fn shell_line(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command an agent runs: this binary, by its full path so an app started outside a
/// shell finds it, plus `--host` when one was given.
pub fn server_command(host: Option<&str>) -> Vec<String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "cove".into());
    let mut cmd = vec![exe];
    if let Some(h) = host {
        cmd.push("--host".into());
        cmd.push(h.into());
    }
    cmd.push("mcp".into());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> Vec<String> {
        vec!["/opt/cove".into(), "mcp".into()]
    }

    #[test]
    fn cli_lines() {
        assert_eq!(
            find("claude").unwrap().describe(&cmd()),
            "claude mcp add -s user cove -- /opt/cove mcp"
        );
        assert_eq!(
            find("codex").unwrap().describe(&cmd()),
            "codex mcp add cove -- /opt/cove mcp"
        );
        assert_eq!(
            find("gemini").unwrap().describe(&cmd()),
            "gemini mcp add -s user cove /opt/cove mcp"
        );
        assert!(find("nope").is_err());
    }

    #[test]
    fn files_keep_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/mcp.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"theme": "dark", "mcpServers": {"other": {"command": "x"}}}"#,
        )
        .unwrap();
        let e = entry(Style::McpServers, &cmd());
        edit_json(&file, |v| set_server(v, "mcpServers", Some(e))).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["cove"]["args"], json!(["mcp"]));

        edit_json(&file, |v| set_server(v, "mcpServers", None)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(v["mcpServers"].get("cove").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn new_file_and_opencode_shape() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a/b/opencode.json");
        let e = entry(Style::OpenCode, &cmd());
        edit_json(&file, |v| set_server(v, "mcp", Some(e))).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            v["mcp"]["cove"],
            json!({"type": "local", "command": ["/opt/cove", "mcp"], "enabled": true})
        );
    }

    #[test]
    fn leaves_jsonc_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opencode.json");
        let text = "{\n  // mine\n  \"theme\": \"x\"\n}\n";
        std::fs::write(&file, text).unwrap();
        let e = entry(Style::OpenCode, &cmd());
        assert!(edit_json(&file, |v| set_server(v, "mcp", Some(e))).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    }
}
