//! `cove`: the server, the CLI and the MCP server in one binary.

mod client;
mod mcp;
mod server;
mod term;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use client::Client;
use cove_core::manager::EgressReport;
use cove_core::model::{Mount, Secret, TimeoutAction};
use cove_core::paths::Paths;
use cove_core::{backend, image, Manager, Network, VmStatus};
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
    /// Create a VM (or reuse one) and open a shell in it: the quickest way in.
    Sh {
        /// The VM to use; created if it doesn't exist yet.
        #[arg(default_value = "dev")]
        name: String,
        /// Start a new VM from this snapshot (default: the server's settings).
        #[arg(long)]
        snapshot: Option<String>,
        /// Share a folder into a new VM: HOST:GUEST or HOST:GUEST:ro.
        #[arg(long = "mount")]
        mounts: Vec<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Create and start a VM. Options left out come from the server's settings
    /// (`cove settings`), then Cove's defaults (2 CPUs, 4096 MB, 20 GB, debian-13).
    Up {
        name: String,
        /// The base image (default: debian-13).
        #[arg(long, conflicts_with = "snapshot")]
        image: Option<String>,
        /// Start from a snapshot instead of an image (`cove snapshot ls`).
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory in MB.
        #[arg(long)]
        memory: Option<u64>,
        /// Disk in GB.
        #[arg(long)]
        disk: Option<u64>,
        /// Share a folder: HOST:GUEST or HOST:GUEST:ro.
        #[arg(long = "mount")]
        mounts: Vec<String>,
        /// Delete the VM when it stops.
        #[arg(long)]
        ephemeral: bool,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// key=value metadata for finding it later (`cove ls --label key=value`).
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        /// auto (the best this machine has), vfkit, cloud-hypervisor, gvisor or host
        /// (development only).
        #[arg(long)]
        backend: Option<String>,
        /// full (NAT to the internet), off (no network device at all) or allowlist
        /// (only the hosts given with --allow, through Cove's proxy).
        #[arg(long)]
        network: Option<Network>,
        #[command(flatten)]
        egress: EgressArgs,
        /// Pause the VM (memory and processes kept) after this many minutes with no
        /// commands, streams or running named processes. Using it resumes it.
        #[arg(long = "auto-pause", value_name = "MINUTES")]
        auto_pause: Option<u64>,
        /// Stop the VM instead after this many idle minutes.
        #[arg(long = "idle-stop", value_name = "MINUTES")]
        idle_stop: Option<u64>,
        /// Pause or delete the VM this long from now: 30m, 2h, 1h30m (1 minute to 24
        /// hours).
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,
        /// What the timeout does: pause or delete.
        #[arg(long = "on-timeout", default_value = "pause")]
        on_timeout: TimeoutAction,
        /// Create without starting.
        #[arg(long)]
        no_start: bool,
    },
    /// List VMs.
    Ls {
        /// Only VMs with this label; repeat for more (all must match).
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
    },
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
    /// Freeze a running VM: memory and processes are kept, it uses no CPU.
    #[command(alias = "suspend")]
    Pause {
        name: String,
    },
    /// Carry on where a paused VM left off.
    Resume {
        name: String,
    },
    /// Pause or delete a VM this long from now (30m, 2h); 0 clears it.
    Timeout {
        name: String,
        duration: String,
        /// pause or delete.
        #[arg(long = "on-timeout", default_value = "pause")]
        on_timeout: TimeoutAction,
    },
    /// Pause a VM once it has been idle this many minutes; 0 turns it off.
    #[command(name = "auto-pause", alias = "auto-suspend")]
    AutoPause {
        name: String,
        minutes: u64,
    },
    /// Set a VM's labels (KEY=VALUE), or remove some with --rm KEY.
    Label {
        name: String,
        #[arg(value_name = "KEY=VALUE")]
        set: Vec<String>,
        #[arg(long = "rm", value_name = "KEY")]
        remove: Vec<String>,
    },
    /// Rename a stopped VM.
    Rename {
        from: String,
        to: String,
    },
    /// Save VMs and start new ones from them.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// The server's defaults for new VMs.
    #[command(subcommand)]
    Settings(SettingsCmd),
    /// Print a one-time code that connects Runo or another computer to this server.
    Pair {
        /// What to call the key it hands out (the client may name it instead).
        #[arg(long)]
        name: Option<String>,
    },
    /// Connect this computer's CLI to a server with a code from `cove pair`.
    Login {
        /// The server's URL, such as http://127.0.0.1:7457 through an SSH tunnel.
        url: String,
        code: String,
        /// What to call this computer's key on the server.
        #[arg(long)]
        name: Option<String>,
    },
    /// Forget the server `cove login` saved.
    Logout,
    /// Named API keys: one per person or device, each revocable.
    #[command(subcommand)]
    Keys(KeysCmd),
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
    /// Show what a VM may reach, its secrets (names only) and what it tried to reach.
    Egress {
        name: String,
    },
    /// Replace a VM's allowlist; a running VM uses it straight away.
    Allow {
        name: String,
        /// host, *.domain, * or a preset (@dev, @agents). None allows only the
        /// secrets' hosts.
        rules: Vec<String>,
    },
    /// Run the MCP server (stdio) for agents.
    Mcp,
    /// Print the client's and the server's version and the server's features.
    Version,
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
    #[arg(long)]
    cpus: Option<u32>,
    #[arg(long)]
    memory: Option<u64>,
    /// With --create: auto, vfkit, cloud-hypervisor, gvisor or host (development only).
    #[arg(long, env = "COVE_BACKEND")]
    backend: Option<String>,
    /// With --create: full, off or allowlist.
    #[arg(long)]
    network: Option<Network>,
    #[command(flatten)]
    egress: EgressArgs,
    /// With --create: stop the VM after this many idle minutes.
    #[arg(long = "idle-stop", value_name = "MINUTES")]
    idle_stop: Option<u64>,
    /// With --create: start from this snapshot.
    #[arg(skip)]
    snapshot: Option<String>,
}

/// Where a new VM may connect, and the secrets Cove adds for it.
#[derive(clap::Args)]
struct EgressArgs {
    /// A host the VM may reach with --network allowlist: example.com, *.example.com,
    /// `*`, or a preset: @dev (git hosts, package registries and model APIs) or
    /// @agents (model APIs only). Repeat for more. Default with allowlist: @dev.
    #[arg(long = "allow", value_name = "RULE")]
    allow: Vec<String>,
    /// NAME or NAME@HOST,HOST: take NAME from your environment and keep it out of the
    /// VM. The VM sees a placeholder, which Cove's proxy swaps for the real value in
    /// HTTPS requests to those hosts (well-known names like ANTHROPIC_API_KEY,
    /// OPENAI_API_KEY and GH_TOKEN know theirs).
    #[arg(long = "secret", value_name = "NAME[@HOSTS]")]
    secrets: Vec<String>,
}

impl EgressArgs {
    /// `allow` and `secrets` for a create request.
    fn body(&self, network: Option<Network>) -> Result<(Vec<String>, Vec<Secret>)> {
        let mut allow = self.allow.clone();
        if network == Some(Network::Allowlist) && allow.is_empty() {
            allow.push("@dev".into());
        }
        let secrets = self
            .secrets
            .iter()
            .map(|s| parse_secret(s))
            .collect::<Result<Vec<_>>>()?;
        Ok((allow, secrets))
    }
}

fn parse_secret(text: &str) -> Result<Secret> {
    let (name, hosts) = match text.split_once('@') {
        Some((n, h)) => (
            n,
            h.split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect(),
        ),
        None => (text, Vec::new()),
    };
    if name.contains('=') {
        bail!(
            "--secret takes a variable name, not a value: export {} and pass --secret {}",
            name.split('=').next().unwrap_or(name),
            name.split('=').next().unwrap_or(name)
        );
    }
    let value = std::env::var(name)
        .map_err(|_| anyhow::anyhow!("--secret {name}: {name} is not set in your environment"))?;
    Ok(Secret {
        name: name.to_string(),
        value,
        hosts,
        placeholder: String::new(),
    })
}

/// Make sure `name` is running: resume it if paused, start it if stopped, create it
/// with `--create`.
async fn ensure_running(client: &Client, name: &str, ensure: &Ensure) -> Result<()> {
    let found = client.get::<VmStatus>(&format!("/vms/{name}")).await;
    let state = match &found {
        Ok(vm) => Some(vm.state),
        Err(e) if ensure.create && e.to_string().starts_with("no VM named") => None,
        Err(_) => return found.map(|_| ()),
    };
    match state {
        Some(cove_core::VmState::Running) => return Ok(()),
        Some(cove_core::VmState::Paused) => {
            eprintln!("Resuming VM {name}…");
            client
                .post::<VmStatus>(&format!("/vms/{name}/resume"), &())
                .await?;
            return Ok(());
        }
        Some(_) => eprintln!("Starting VM {name}…"),
        None => {
            eprintln!("Creating VM {name} (the first boot takes a little longer)…");
            let mounts = ensure
                .mounts
                .iter()
                .map(|m| parse_mount(m))
                .collect::<Result<Vec<_>>>()?;
            let (allow, secrets) = ensure.egress.body(ensure.network)?;
            let mut body = serde_json::json!({
                "name": name,
                "mounts": mounts,
                "secrets": secrets,
                "start": true,
            });
            // Only what was asked for, so the server's settings fill the rest.
            put(&mut body, "cpus", ensure.cpus);
            put(&mut body, "memory_mb", ensure.memory);
            put(&mut body, "backend", ensure.backend.clone());
            put(&mut body, "network", ensure.network);
            put(&mut body, "idle_stop_minutes", ensure.idle_stop);
            put(&mut body, "snapshot", ensure.snapshot.clone());
            if !allow.is_empty() {
                body["allow"] = serde_json::json!(allow);
            }
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

/// Set `key` in a JSON object when there is a value.
fn put<T: serde::Serialize>(body: &mut serde_json::Value, key: &str, value: Option<T>) {
    if let Some(v) = value {
        body[key] = serde_json::json!(v);
    }
}

#[derive(Subcommand)]
enum ImageCmd {
    Ls,
    Pull { name: String },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// Save a VM as a snapshot. A running VM is frozen while its disk is copied,
    /// then carries on.
    Save {
        vm: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// List snapshots.
    Ls,
    /// Delete a snapshot. VMs made from it are not affected.
    Rm { name: String },
    /// Build a snapshot by running a setup script in a fresh VM, such as
    /// scripts/templates/universal.sh.
    Build {
        name: String,
        /// The setup script, run as root with bash.
        #[arg(long)]
        script: String,
        /// The image to start from.
        #[arg(long, default_value = "debian-13")]
        image: String,
        /// Start from another snapshot instead.
        #[arg(long, conflicts_with = "image")]
        from: Option<String>,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        cpus: Option<u32>,
        #[arg(long)]
        memory: Option<u64>,
        #[arg(long)]
        disk: Option<u64>,
        #[arg(long, default_value = "")]
        description: String,
        /// Keep the VM it was built in, to look around.
        #[arg(long)]
        keep: bool,
    },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Show the server's defaults for new VMs.
    Show,
    /// Change defaults: cpus=4 memory_mb=8192 network=allowlist auto_pause_minutes=30
    /// snapshot=universal …
    Set {
        #[arg(value_name = "KEY=VALUE", required = true)]
        pairs: Vec<String>,
    },
    /// Go back to Cove's own default for these settings.
    Unset {
        #[arg(required = true)]
        keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum KeysCmd {
    Ls,
    /// Make a key and print it once.
    Create {
        name: String,
    },
    /// Revoke a key by id or name.
    Rm {
        id: String,
    },
}

/// `30m`, `2h`, `1h30m`, `1d`, or a bare number of minutes.
fn parse_minutes(text: &str) -> Result<u64> {
    let text = text.trim();
    if let Ok(m) = text.parse::<u64>() {
        return Ok(m);
    }
    let mut total = 0u64;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: u64 = number
            .parse()
            .map_err(|_| anyhow::anyhow!("can't read the duration {text:?} (try 30m or 2h)"))?;
        number.clear();
        total += match c {
            's' => n.div_ceil(60),
            'm' => n,
            'h' => n * 60,
            'd' => n * 24 * 60,
            _ => bail!("can't read the duration {text:?} (try 30m or 2h)"),
        };
    }
    if !number.is_empty() {
        bail!("can't read the duration {text:?}: give a unit (30m, 2h)");
    }
    Ok(total)
}

fn parse_labels(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => Ok((k.to_string(), v.to_string())),
            None => bail!("labels are KEY=VALUE, got {kv:?}"),
        })
        .collect()
}

/// A settings value from the command line: a number, true/false, a list
/// (`a,b`) for `allow`, else text.
fn setting_value(key: &str, value: &str) -> serde_json::Value {
    if key == "allow" {
        return serde_json::json!(value
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>());
    }
    if let Ok(n) = value.parse::<u64>() {
        return serde_json::json!(n);
    }
    serde_json::json!(value)
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
        "{:<20} {:<9} {:<10} {:>4} {:>8} {:>6} {:<9} {:<10}  MOUNTS",
        "NAME", "STATE", "IMAGE", "CPU", "MEMORY", "DISK", "NET", "IDLE"
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
        let idle = match (vm.auto_pause_minutes, vm.idle_stop_minutes) {
            (Some(m), _) => format!("pause {m}m"),
            (None, Some(m)) => format!("stop {m}m"),
            (None, None) => "-".into(),
        };
        let image = match &vm.snapshot {
            Some(s) => s.clone(),
            None => vm.image.clone(),
        };
        println!(
            "{:<20} {:<9} {:<10} {:>4} {:>7}M {:>5}G {:<9} {:<10}  {}",
            vm.name,
            state,
            if vm.backend == backend::HOST {
                "(host)"
            } else {
                &image
            },
            vm.cpus,
            vm.memory_mb,
            vm.disk_gb,
            vm.network,
            idle,
            mounts.join(", ")
        );
        if let Some(by) = vm.paused_by {
            let why = match by {
                cove_core::model::PauseReason::User => "paused; cove resume wakes it",
                cove_core::model::PauseReason::Idle => "paused while idle; using it wakes it",
                cove_core::model::PauseReason::Timeout => {
                    "paused by its timeout; cove resume wakes it"
                }
            };
            println!("  {why}");
        }
        if let Some(e) = &vm.error {
            println!("  error: {e}");
        }
    }
}

fn print_egress(r: &EgressReport) {
    let allow = if r.allow.is_empty() {
        "(only the secrets' hosts)".to_string()
    } else {
        r.allow.join(" ")
    };
    if r.enforced {
        println!("Network: allowlist. Reaches only: {allow}");
    } else {
        println!(
            "Network: {}. The allowlist isn't enforced; the proxy only adds secrets.",
            r.network
        );
    }
    for s in &r.secrets {
        println!("Secret {} goes only to {}", s.name, s.hosts.join(", "));
    }
    if r.log.is_empty() {
        println!("Nothing has gone through the proxy since the VM started.");
        return;
    }
    println!();
    for e in &r.log {
        let what = if e.allowed {
            if e.secrets.is_empty() {
                "allowed".to_string()
            } else {
                format!("allowed, added {}", e.secrets.join(", "))
            }
        } else {
            format!("BLOCKED: {}", e.reason.as_deref().unwrap_or("refused"))
        };
        println!("{:<40} {:>5}  {what}", e.host, e.port);
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
                    let state = match (i.size > 0, i.rootfs) {
                        (true, true) => format!("pulled, {} MB disk and rootfs", i.size >> 20),
                        (true, false) => format!("pulled, {} MB", i.size >> 20),
                        (false, true) => "pulled, rootfs (gVisor)".into(),
                        (false, false) => "not pulled".into(),
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
        Cmd::Sh {
            name,
            snapshot,
            mounts,
            cwd,
        } => {
            let ensure = Ensure {
                create: true,
                mounts,
                cpus: None,
                memory: None,
                backend: std::env::var("COVE_BACKEND").ok(),
                network: None,
                egress: EgressArgs {
                    allow: vec![],
                    secrets: vec![],
                },
                idle_stop: None,
                snapshot,
            };
            let client = client()?;
            ensure_running(&client, &name, &ensure).await?;
            let command = GuestCommand {
                cmd: vec!["/bin/bash".into(), "-l".into()],
                cwd,
                env: vec![],
                pty: Some(term::size()),
            };
            return term::run(&client, &name, Request::Exec(command)).await;
        }
        Cmd::Up {
            name,
            image,
            snapshot,
            cpus,
            memory,
            disk,
            mounts,
            ephemeral,
            tags,
            labels,
            backend,
            network,
            egress,
            auto_pause,
            idle_stop,
            timeout,
            on_timeout,
            no_start,
        } => {
            let mounts = mounts
                .iter()
                .map(|m| parse_mount(m))
                .collect::<Result<Vec<_>>>()?;
            let (allow, secrets) = egress.body(network)?;
            let mut body = serde_json::json!({
                "name": name,
                "mounts": mounts,
                "ephemeral": ephemeral,
                "tags": tags,
                "labels": parse_labels(&labels)?,
                "secrets": secrets,
                "start": !no_start,
            });
            // Only what was given, so the server's settings fill the rest.
            put(&mut body, "image", image);
            put(&mut body, "snapshot", snapshot);
            put(&mut body, "cpus", cpus);
            put(&mut body, "memory_mb", memory);
            put(&mut body, "disk_gb", disk);
            put(&mut body, "backend", backend);
            put(&mut body, "network", network);
            put(&mut body, "auto_pause_minutes", auto_pause);
            put(&mut body, "idle_stop_minutes", idle_stop);
            if !allow.is_empty() {
                body["allow"] = serde_json::json!(allow);
            }
            if let Some(t) = timeout {
                body["timeout_minutes"] = serde_json::json!(parse_minutes(&t)?);
                body["on_timeout"] = serde_json::json!(on_timeout);
            }
            if !no_start {
                eprintln!("Starting {name} (the first boot takes a little longer)…");
            }
            let vm: VmStatus = client()?.post("/vms", &body).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Ls { labels } => {
            let query: Vec<String> = labels
                .iter()
                .map(|l| format!("label={}", url_escape(l)))
                .collect();
            let path = if query.is_empty() {
                "/vms".to_string()
            } else {
                format!("/vms?{}", query.join("&"))
            };
            let vms: Vec<VmStatus> = client()?.get(&path).await?;
            show(json, &vms, || print_vms(&vms));
        }
        Cmd::Pause { name } => {
            let vm: VmStatus = client()?.post(&format!("/vms/{name}/pause"), &()).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Resume { name } => {
            let vm: VmStatus = client()?.post(&format!("/vms/{name}/resume"), &()).await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Timeout {
            name,
            duration,
            on_timeout,
        } => {
            let minutes = parse_minutes(&duration)?;
            let timeout = if minutes == 0 {
                serde_json::Value::Null
            } else {
                serde_json::json!({ "minutes": minutes, "action": on_timeout })
            };
            let vm: VmStatus = client()?
                .patch(
                    &format!("/vms/{name}"),
                    &serde_json::json!({ "timeout": timeout }),
                )
                .await?;
            show(json, &vm, || match vm.timeout {
                Some(t) => println!(
                    "{name} will {} in {} minutes.",
                    match t.action {
                        TimeoutAction::Pause => "pause",
                        TimeoutAction::Delete => "be deleted",
                    },
                    t.at.saturating_sub(cove_core::now()).div_ceil(60)
                ),
                None => println!("{name} has no timeout."),
            });
        }
        Cmd::AutoPause { name, minutes } => {
            let value = if minutes == 0 {
                serde_json::Value::Null
            } else {
                serde_json::json!(minutes)
            };
            let vm: VmStatus = client()?
                .patch(
                    &format!("/vms/{name}"),
                    &serde_json::json!({ "auto_pause_minutes": value }),
                )
                .await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Label { name, set, remove } => {
            let client = client()?;
            let vm: VmStatus = client.get(&format!("/vms/{name}")).await?;
            let mut labels = vm.labels;
            labels.extend(parse_labels(&set)?);
            for k in &remove {
                labels.remove(k);
            }
            let vm: VmStatus = client
                .patch(
                    &format!("/vms/{name}"),
                    &serde_json::json!({ "labels": labels }),
                )
                .await?;
            show(json, &vm, || {
                for (k, v) in &vm.labels {
                    println!("{k}={v}");
                }
            });
        }
        Cmd::Rename { from, to } => {
            let vm: VmStatus = client()?
                .post(
                    &format!("/vms/{from}/rename"),
                    &serde_json::json!({ "to": to }),
                )
                .await?;
            show(json, &vm, || print_vms(std::slice::from_ref(&vm)));
        }
        Cmd::Snapshot(cmd) => return snapshot_cmd(&client()?, json, cmd).await,
        Cmd::Settings(cmd) => {
            let client = client()?;
            let mut settings: serde_json::Value = client.get("/settings").await?;
            match cmd {
                SettingsCmd::Show => {}
                SettingsCmd::Set { pairs } => {
                    for kv in &pairs {
                        let Some((k, v)) = kv.split_once('=') else {
                            bail!("settings are KEY=VALUE, got {kv:?}");
                        };
                        settings[k] = setting_value(k, v);
                    }
                    settings = client.put("/settings", &settings).await?;
                }
                SettingsCmd::Unset { keys } => {
                    if let Some(obj) = settings.as_object_mut() {
                        for k in &keys {
                            obj.remove(k);
                        }
                    }
                    settings = client.put("/settings", &settings).await?;
                }
            }
            show(json, &settings, || {
                let obj = settings.as_object().cloned().unwrap_or_default();
                if obj.is_empty() {
                    println!(
                        "No settings: new VMs get Cove's defaults \
                         (2 CPUs, 4096 MB, 20 GB, debian-13, full network)."
                    );
                }
                for (k, v) in obj {
                    println!("{k} = {v}");
                }
            });
        }
        Cmd::Pair { name } => {
            let client = client()?;
            let v: serde_json::Value = client
                .post("/pair/codes", &serde_json::json!({ "name": name }))
                .await?;
            show(json, &v, || {
                let code = v["code"].as_str().unwrap_or("");
                println!("Pairing code: {code}  (works once, for 10 minutes)");
                println!();
                println!("In Runo: add a Cove connection with this server's URL and the code.");
                println!("From another computer: cove login <this server's URL> {code}");
                println!("This server listens on {}.", client.url);
            });
        }
        Cmd::Login { url, code, name } => {
            let url = client::server_url(Some(url));
            let name = name.unwrap_or_else(|| {
                format!(
                    "cli on {}",
                    hostname().unwrap_or_else(|| "a computer".into())
                )
            });
            let v = client::pair(&url, &code, &name).await?;
            let key = v["key"].as_str().unwrap_or_default().to_string();
            client::save_login(&Paths::from_env(), &url, &key)?;
            let name = v["name"].as_str().unwrap_or("").to_string();
            show(
                json,
                &serde_json::json!({ "url": url, "name": name }),
                || {
                    println!(
                        "Logged in to {url} as \"{name}\". cove now talks to this server; \
                     cove logout undoes that."
                    );
                },
            );
        }
        Cmd::Logout => {
            let path = Paths::from_env().login_file();
            if std::fs::remove_file(&path).is_ok() {
                println!(
                    "Logged out. The key stays valid on the server until cove keys rm \
                     removes it there."
                );
            } else {
                println!("Not logged in.");
            }
        }
        Cmd::Keys(cmd) => {
            let client = client()?;
            match cmd {
                KeysCmd::Ls => {
                    let keys: Vec<serde_json::Value> = client.get("/keys").await?;
                    show(json, &keys, || {
                        if keys.is_empty() {
                            println!(
                                "No named keys. Make one with cove keys create NAME or cove pair."
                            );
                        }
                        for k in &keys {
                            println!(
                                "{:<16} {}",
                                k["id"].as_str().unwrap_or(""),
                                k["name"].as_str().unwrap_or("")
                            );
                        }
                    });
                }
                KeysCmd::Create { name } => {
                    let v: serde_json::Value = client
                        .post("/keys", &serde_json::json!({ "name": name }))
                        .await?;
                    show(json, &v, || println!("{}", v["key"].as_str().unwrap_or("")));
                }
                KeysCmd::Rm { id } => {
                    let v: serde_json::Value = client.delete(&format!("/keys/{id}")).await?;
                    show(json, &v, || {
                        println!("Removed {}.", v["name"].as_str().unwrap_or(&id))
                    });
                }
            }
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
        Cmd::Egress { name } => {
            let r: EgressReport = client()?.get(&format!("/vms/{name}/egress")).await?;
            show(json, &r, || print_egress(&r));
        }
        Cmd::Allow { name, rules } => {
            let r: EgressReport = client()?
                .post(
                    &format!("/vms/{name}/egress"),
                    &serde_json::json!({ "allow": rules }),
                )
                .await?;
            show(json, &r, || print_egress(&r));
        }
        Cmd::Mcp => mcp::serve(client()?).await?,
        Cmd::Version => version(json, host).await,
    }
    Ok(0)
}

async fn snapshot_cmd(client: &Client, json: bool, cmd: SnapshotCmd) -> Result<i32> {
    use cove_core::snapshot::SnapshotInfo;
    match cmd {
        SnapshotCmd::Save {
            vm,
            name,
            description,
        } => {
            eprintln!("Saving {vm} as {name}…");
            let snap: SnapshotInfo = client
                .post(
                    &format!("/vms/{vm}/snapshots"),
                    &serde_json::json!({ "name": name, "description": description }),
                )
                .await?;
            show(json, &snap, || print_snapshots(std::slice::from_ref(&snap)));
        }
        SnapshotCmd::Ls => {
            let snaps: Vec<SnapshotInfo> = client.get("/snapshots").await?;
            show(json, &snaps, || print_snapshots(&snaps));
        }
        SnapshotCmd::Rm { name } => {
            let v: serde_json::Value = client.delete(&format!("/snapshots/{name}")).await?;
            show(json, &v, || println!("Deleted snapshot {name}."));
        }
        SnapshotCmd::Build {
            name,
            script,
            image,
            from,
            backend,
            cpus,
            memory,
            disk,
            description,
            keep,
        } => {
            let script_text =
                std::fs::read(&script).map_err(|e| anyhow::anyhow!("reading {script}: {e}"))?;
            let vm = format!("build-{}", cove_core::random_hex(3));
            let mut body = serde_json::json!({ "name": vm, "network": "full", "start": true });
            match &from {
                Some(s) => body["snapshot"] = serde_json::json!(s),
                None => body["image"] = serde_json::json!(image),
            }
            put(&mut body, "backend", backend);
            put(&mut body, "cpus", cpus);
            put(&mut body, "memory_mb", memory);
            put(&mut body, "disk_gb", disk);
            eprintln!("Starting {vm} to build {name}…");
            let _: VmStatus = client.post("/vms", &body).await?;
            let result = async {
                client
                    .write_file(&vm, "/root/cove-setup.sh", script_text, Some(0o755))
                    .await?;
                eprintln!("Running {script}…");
                let code = term::run(
                    client,
                    &vm,
                    Request::Exec(GuestCommand {
                        cmd: vec!["bash".into(), "-l".into(), "/root/cove-setup.sh".into()],
                        cwd: Some("/root".into()),
                        env: vec![],
                        pty: None,
                    }),
                )
                .await?;
                if code != 0 {
                    bail!("{script} exited with {code}");
                }
                let _: serde_json::Value = client
                    .post(
                        &format!("/vms/{vm}/exec"),
                        &serde_json::json!({ "cmd": ["rm", "-f", "/root/cove-setup.sh"] }),
                    )
                    .await?;
                eprintln!("Stopping {vm} and saving it as {name}…");
                let _: VmStatus = client.post(&format!("/vms/{vm}/stop"), &()).await?;
                let snap: SnapshotInfo = client
                    .post(
                        &format!("/vms/{vm}/snapshots"),
                        &serde_json::json!({ "name": name, "description": description }),
                    )
                    .await?;
                Ok(snap)
            }
            .await;
            if keep {
                eprintln!("Kept {vm}.");
            } else {
                let _ = client
                    .delete::<serde_json::Value>(&format!("/vms/{vm}"))
                    .await;
            }
            let snap = result?;
            show(json, &snap, || print_snapshots(std::slice::from_ref(&snap)));
        }
    }
    Ok(0)
}

fn print_snapshots(snaps: &[cove_core::snapshot::SnapshotInfo]) {
    if snaps.is_empty() {
        println!("No snapshots. Save one with: cove snapshot save <vm> --name <name>");
        return;
    }
    println!(
        "{:<20} {:<7} {:<10} {:>8}  {:<16} DESCRIPTION",
        "NAME", "KIND", "IMAGE", "SIZE", "FROM"
    );
    for s in snaps {
        println!(
            "{:<20} {:<7} {:<10} {:>7}M  {:<16} {}",
            s.name,
            serde_json::to_value(s.kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            s.image,
            s.size >> 20,
            s.source_vm,
            s.description
        );
    }
}

/// Escape a query value (labels can hold `&`, spaces and `=`).
fn url_escape(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Ask the server what it is and can do. Needs no API key; when the server can't be
/// reached, only the client's version is printed.
async fn version(json: bool, host: Option<String>) {
    let client_version = env!("CARGO_PKG_VERSION");
    let url = client::server_url(host);
    match client::health(&url).await {
        Ok(health) => show(json, &health, || {
            println!("cove {client_version} (client)");
            println!(
                "server {} at {url}",
                health["version"].as_str().unwrap_or("?")
            );
            let features: Vec<&str> = health["features"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str())
                .collect();
            if !features.is_empty() {
                println!("features: {}", features.join(", "));
            }
        }),
        Err(_) => show(
            json,
            &serde_json::json!({ "version": client_version }),
            || println!("cove {client_version} (client; no server at {url})"),
        ),
    }
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
    // Each backend this platform has, with what it needs; only one has to work, so
    // only the one `auto` picks counts towards the result.
    let mark = |good: bool| if good { "✓" } else { "✗" };
    for b in backend::platform_backends() {
        let result = backend::check(b);
        println!(
            "{} {b}: {}",
            mark(result.is_ok()),
            match &result {
                Ok(d) => d.clone(),
                Err(e) if *b == backend::CLOUD_HYPERVISOR => {
                    format!("can't run VMs here ({})", e.split(':').next().unwrap_or(e))
                }
                Err(e) => e.clone(),
            }
        );
        if *b == backend::CLOUD_HYPERVISOR {
            for c in cove_core::chv::checks(&cove_core::chv::Host::detect(&paths)) {
                println!("    {} {}: {}", mark(c.ok), c.label, c.detail);
            }
        }
    }
    match backend::auto().map(|b| (b, backend::check(b))) {
        Ok((b, Ok(_))) => line(true, "backend", format!("{b} (what auto picks)")),
        Ok((b, Err(e))) => line(
            false,
            "backend",
            format!("auto picks {b}, which can't run: {e}"),
        ),
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
