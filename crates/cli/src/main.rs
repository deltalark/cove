//! `cove`: the server, the CLI and the MCP server in one binary.

mod client;
mod mcp;
mod server;
mod term;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use client::Client;
use cove_core::model::{Mount, VmSpec};
use cove_core::paths::Paths;
use cove_core::{backend, image, Manager, VmStatus};
use cove_proto::{Command as GuestCommand, Request};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7457";

#[derive(Parser)]
#[command(
    name = "cove",
    version,
    about = "Self-hosted microVMs for people and coding agents"
)]
struct Cli {
    /// Server URL (default: the local server).
    #[arg(long, global = true, env = "COVE_URL")]
    host: Option<String>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server.
    Serve {
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: String,
    },
    /// Check that this machine can run VMs.
    Doctor,
    /// Print the API key for this machine's server.
    Key,
    /// Base images.
    #[command(subcommand)]
    Image(ImageCmd),
    /// Create and start a VM.
    Up {
        name: String,
        #[arg(long, default_value = "debian-13")]
        image: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Memory in MB.
        #[arg(long, default_value_t = 4096)]
        memory: u64,
        /// Disk in GB.
        #[arg(long, default_value_t = 20)]
        disk: u64,
        /// Share a folder: HOST:GUEST or HOST:GUEST:ro.
        #[arg(long = "mount")]
        mounts: Vec<String>,
        /// Delete the VM when it stops.
        #[arg(long)]
        ephemeral: bool,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// vfkit, cloud-hypervisor or host (development only).
        #[arg(long, default_value = "")]
        backend: String,
        /// Create without starting.
        #[arg(long)]
        no_start: bool,
    },
    /// List VMs.
    Ls,
    /// Show one VM.
    Show {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    /// Delete a VM and its disk.
    Rm {
        name: String,
    },
    /// Copy a stopped VM (disk and settings) to a new name.
    Copy {
        from: String,
        to: String,
    },
    /// Run a command in a VM.
    Exec {
        name: String,
        #[command(flatten)]
        ensure: Ensure,
        /// Use a terminal (interactive programs).
        #[arg(short = 't', long)]
        pty: bool,
        #[arg(long)]
        cwd: Option<String>,
        /// KEY=VALUE
        #[arg(short, long = "env")]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open a shell in a VM.
    Shell {
        name: String,
        #[command(flatten)]
        ensure: Ensure,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Start a named process that keeps running after you disconnect.
    Spawn {
        name: String,
        proc: String,
        #[arg(short = 't', long)]
        pty: bool,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(short, long = "env")]
        env: Vec<String>,
        /// Stay attached instead of returning straight away.
        #[arg(short, long)]
        attach: bool,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Reattach to a named process, replaying output from byte FROM.
    Attach {
        name: String,
        proc: String,
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// List named processes in a VM.
    Procs {
        name: String,
    },
    /// Signal a named process (SIGTERM by default).
    Kill {
        name: String,
        proc: String,
        #[arg(long)]
        signal: Option<i32>,
    },
    /// Copy a file into a VM.
    Push {
        name: String,
        local: String,
        remote: String,
    },
    /// Copy a file out of a VM.
    Pull {
        name: String,
        remote: String,
        local: String,
    },
    /// Ports listening in a VM, and forwards.
    Ports {
        name: String,
    },
    /// Forward a VM port to this machine and print its URL.
    Forward {
        name: String,
        port: u16,
    },
    /// Run the MCP server (stdio) for agents.
    Mcp,
}

/// Start the VM first if it is stopped, or create it if it doesn't exist.
#[derive(clap::Args)]
struct Ensure {
    /// Create the VM if it doesn't exist (with --mount, --cpus, --memory).
    #[arg(long)]
    create: bool,
    /// With --create: HOST:GUEST or HOST:GUEST:ro.
    #[arg(long = "mount")]
    mounts: Vec<String>,
    #[arg(long, default_value_t = 2)]
    cpus: u32,
    #[arg(long, default_value_t = 4096)]
    memory: u64,
    /// With --create: vfkit, cloud-hypervisor or host (development only).
    #[arg(long, default_value = "")]
    backend: String,
}

/// Make sure `name` is running: start it if stopped, create it with `--create`.
async fn ensure_running(client: &Client, name: &str, ensure: &Ensure) -> Result<()> {
    let found = client.get::<VmStatus>(&format!("/vms/{name}")).await;
    let state = match &found {
        Ok(vm) => Some(vm.state),
        Err(e) if ensure.create && e.to_string().starts_with("no VM named") => None,
        Err(_) => return found.map(|_| ()),
    };
    match state {
        Some(cove_core::VmState::Running) => return Ok(()),
        Some(_) => eprintln!("Starting VM {name}…"),
        None => {
            eprintln!("Creating VM {name} (the first boot takes a little longer)…");
            let mounts = ensure
                .mounts
                .iter()
                .map(|m| parse_mount(m))
                .collect::<Result<Vec<_>>>()?;
            let body = serde_json::json!({
                "name": name,
                "cpus": ensure.cpus,
                "memory_mb": ensure.memory,
                "mounts": mounts,
                "backend": ensure.backend,
                "start": true,
            });
            match client.post::<VmStatus>("/vms", &body).await {
                Ok(_) => return Ok(()),
                // Created by someone else in the meantime: just start it.
                Err(e) if e.to_string().contains("already exists") => {}
                Err(e) => return Err(e),
            }
        }
    }
    client
        .post::<VmStatus>(&format!("/vms/{name}/start"), &())
        .await?;
    Ok(())
}

#[derive(Subcommand)]
enum ImageCmd {
    Ls,
    Pull { name: String },
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cove: {e:#}");
            1
        }
    };
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}

fn parse_env(env: &[String]) -> Result<Vec<(String, String)>> {
    env.iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => Ok((k.to_string(), v.to_string())),
            None => bail!("--env takes KEY=VALUE, got {kv:?}"),
        })
        .collect()
}

fn parse_mount(text: &str) -> Result<Mount> {
    let parts: Vec<&str> = text.split(':').collect();
    let (host, guest, read_only) = match parts.as_slice() {
        [h, g] => (*h, *g, false),
        [h, g, "ro"] => (*h, *g, true),
        [h, g, "rw"] => (*h, *g, false),
        _ => bail!("--mount takes HOST:GUEST or HOST:GUEST:ro"),
    };
    let host = std::fs::canonicalize(host)?.display().to_string();
    Ok(Mount {
        host,
        guest: guest.to_string(),
        read_only,
    })
}

fn print_vms(vms: &[VmStatus]) {
    if vms.is_empty() {
        println!("No VMs. Create one with: cove up <name>");
        return;
    }
    println!(
        "{:<20} {:<9} {:<10} {:>4} {:>8} {:>6}  MOUNTS",
        "NAME", "STATE", "IMAGE", "CPU", "MEMORY", "DISK"
    );
    for vm in vms {
        let state = serde_json::to_value(vm.state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let mounts: Vec<String> = vm
            .mounts
            .iter()
            .map(|m| format!("{}:{}", m.host, m.guest))
            .collect();
        println!(
            "{:<20} {:<9} {:<10} {:>4} {:>7}M {:>5}G  {}",
            vm.name,
            state,
            if vm.backend == backend::HOST {
                "(host)"
            } else {
                &vm.image
            },
            vm.cpus,
            vm.memory_mb,
            vm.disk_gb,
            mounts.join(", ")
        );
        if let Some(e) = &vm.error {
            println!("  error: {e}");
        }
    }
}

fn show<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce()) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    } else {
        text();
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let json = cli.json;
    let host = cli.host.clone();
    let client = || Client::from_env(host.clone());
    match cli.command {
        Cmd::Serve { listen } => {
            let manager = Manager::new(Paths::from_env())?;
            server::serve(manager, &listen).await?;
        }
        Cmd::Doctor => return doctor(client().ok()).await,
        Cmd::Key => println!("{}", Paths::from_env().load_or_create_api_key()?),
        Cmd::Image(ImageCmd::Ls) => {
            let images: Vec<image::ImageInfo> = client()?.get("/images").await?;
            show(json, &images, || {
                for i in &images {
                    let state = if i.pulled {
                        format!("pulled, {} MB", i.size / 1024 / 1024)
                    } else {
                        "not pulled".into()
                    };
                    println!("{:<12} {:<45} {state}", i.name, i.description);
                }
            });
        }
        Cmd::Image(ImageCmd::Pull { name }) => {
            eprintln!("Pulling {name}; this downloads a few hundred MB once.");
            let images: Vec<image::ImageInfo> =
                client()?.post(&format!("/images/{name}/pull"), &()).await?;
            show(json, &images, || println!("Pulled {name}."));
        }
        Cmd::Up {
            name,
            image,
            cpus,
            memory,
            disk,
            mounts,
            ephemeral,
            tags,
            backend,
            no_start,
        } => {
            let mounts = mounts
                .iter()
                .map(|m| parse_mount(m))
                .collect::<Result<Vec<_>>>()?;
            let spec = VmSpec {
                name,
                image,
                cpus,
                memory_mb: memory,
                disk_gb: disk,
                mounts,
                ephemeral,
                tags,
                backend,
            };
            let mut body = serde_json::to_value(&spec)?;
            body["start"] = serde_json::json!(!no_start);
            if !no_start {
                eprintln!(
                    "Starting {} (the first boot takes a little longer)…",
                    spec.name
                );
            }
            let vm: VmStatus = client()?.post("/vms", &body).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Ls => {
            let vms: Vec<VmStatus> = client()?.get("/vms").await?;
            show(json, &vms, || print_vms(&vms));
        }
        Cmd::Show { name } => {
            let vm: VmStatus = client()?.get(&format!("/vms/{name}")).await?;
            println!("{}", serde_json::to_string_pretty(&vm)?);
        }
        Cmd::Start { name } => {
            let vm: VmStatus = client()?.post(&format!("/vms/{name}/start"), &()).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Stop { name } => {
            let vm: VmStatus = client()?.post(&format!("/vms/{name}/stop"), &()).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Rm { name } => {
            let v: serde_json::Value = client()?.delete(&format!("/vms/{name}")).await?;
            show(json, &v, || println!("Deleted {name}."));
        }
        Cmd::Copy { from, to } => {
            let vm: VmStatus = client()?
                .post(
                    &format!("/vms/{from}/copy"),
                    &serde_json::json!({ "to": to }),
                )
                .await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Exec {
            name,
            ensure,
            pty,
            cwd,
            env,
            cmd,
        } => {
            let command = GuestCommand {
                cmd,
                cwd,
                env: parse_env(&env)?,
                pty: pty.then(term::size),
            };
            let client = client()?;
            ensure_running(&client, &name, &ensure).await?;
            return term::run(&client, &name, Request::Exec(command)).await;
        }
        Cmd::Shell { name, ensure, cwd } => {
            let command = GuestCommand {
                cmd: vec!["/bin/bash".into(), "-l".into()],
                cwd,
                env: vec![],
                pty: Some(term::size()),
            };
            let client = client()?;
            ensure_running(&client, &name, &ensure).await?;
            return term::run(&client, &name, Request::Exec(command)).await;
        }
        Cmd::Spawn {
            name,
            proc,
            pty,
            cwd,
            env,
            attach,
            cmd,
        } => {
            let command = GuestCommand {
                cmd,
                cwd,
                env: parse_env(&env)?,
                pty: pty.then(term::size),
            };
            let request = Request::Spawn {
                name: proc.clone(),
                command,
            };
            if attach {
                return term::run(&client()?, &name, request).await;
            }
            let mut stream = client()?.stream(&name, &request).await?;
            match stream.next().await? {
                Some(cove_proto::Frame::Ok { value }) => show(json, &value, || {
                    println!("Started {proc} in {name}. Attach with: cove attach {name} {proc}")
                }),
                Some(cove_proto::Frame::Error { message }) => bail!(message),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Attach { name, proc, from } => {
            return term::run(&client()?, &name, Request::Attach { name: proc, from }).await;
        }
        Cmd::Procs { name } => {
            let procs: Vec<cove_proto::ProcInfo> =
                client()?.get(&format!("/vms/{name}/procs")).await?;
            show(json, &procs, || {
                for p in &procs {
                    let state = if p.running {
                        "running".to_string()
                    } else {
                        format!(
                            "exited {}",
                            p.exit_code.map(|c| c.to_string()).unwrap_or("?".into())
                        )
                    };
                    println!(
                        "{:<20} {:<12} {:>10}B  {}",
                        p.name,
                        state,
                        p.output_len,
                        p.cmd.join(" ")
                    );
                }
            });
        }
        Cmd::Kill { name, proc, signal } => {
            let path = match signal {
                Some(s) => format!("/vms/{name}/procs/{proc}?signal={s}"),
                None => format!("/vms/{name}/procs/{proc}"),
            };
            let _: serde_json::Value = client()?.delete(&path).await?;
        }
        Cmd::Push {
            name,
            local,
            remote,
        } => {
            use std::os::unix::fs::PermissionsExt;
            let data = std::fs::read(&local)?;
            let mode = std::fs::metadata(&local)?.permissions().mode() & 0o777;
            client()?
                .write_file(&name, &remote, data, Some(mode))
                .await?;
        }
        Cmd::Pull {
            name,
            remote,
            local,
        } => {
            let data = client()?.read_file(&name, &remote).await?;
            std::fs::write(&local, data)?;
        }
        Cmd::Ports { name } => {
            let v: serde_json::Value = client()?.get(&format!("/vms/{name}/ports")).await?;
            show(json, &v, || {
                for p in v["listening"].as_array().into_iter().flatten() {
                    let port = p["port"].as_u64().unwrap_or(0);
                    let forward = v["forwards"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|f| f["guest_port"].as_u64() == Some(port))
                        .and_then(|f| f["url"].as_str())
                        .unwrap_or("");
                    println!(
                        "{:>5}  {:<16} {forward}",
                        port,
                        p["address"].as_str().unwrap_or("")
                    );
                }
            });
        }
        Cmd::Forward { name, port } => {
            let f: cove_core::model::Forward = client()?
                .post(
                    &format!("/vms/{name}/forwards"),
                    &serde_json::json!({ "guest_port": port }),
                )
                .await?;
            show(json, &f, || println!("{}", f.url));
        }
        Cmd::Mcp => mcp::serve(client()?).await?,
    }
    Ok(0)
}

async fn doctor(client: Option<Client>) -> Result<i32> {
    let paths = Paths::from_env();
    let mut ok = true;
    let mut line = |good: bool, label: &str, detail: String| {
        ok &= good;
        println!("{} {label}: {detail}", if good { "✓" } else { "✗" });
    };
    line(
        paths.ensure().is_ok(),
        "data folder",
        paths.root.display().to_string(),
    );
    let backend = backend::platform_default();
    match backend::check(backend) {
        Ok(d) => line(true, "backend", d),
        Err(e) => line(false, "backend", e),
    }
    match backend::guest_binary(&paths) {
        Ok(p) => line(true, "guest agent", p.display().to_string()),
        Err(e) => line(false, "guest agent", e.to_string()),
    }
    let images = image::list(&paths);
    let pulled = images.iter().any(|i| i.pulled);
    line(
        pulled,
        "images",
        if pulled {
            images
                .iter()
                .filter(|i| i.pulled)
                .map(|i| i.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "none pulled: cove image pull debian-13".into()
        },
    );
    match client {
        Some(c) => match c.get::<serde_json::Value>("/vms").await {
            Ok(_) => line(true, "server", c.url.clone()),
            Err(e) => line(false, "server", format!("{e:#}")),
        },
        None => line(false, "server", "not set up: run `cove serve`".into()),
    }
    Ok(if ok { 0 } else { 1 })
}
