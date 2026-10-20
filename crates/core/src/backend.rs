//! Backends boot a VM and say how to reach its guest agent.
//!
//! VMs run as detached processes (their own session), so restarting the server does
//! not stop them; the server finds them again from `run.json`.

use crate::guest::GuestAddr;
use crate::model::{Network, PauseReason, VmRecord};
use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub const HOST: &str = "host";
pub const VFKIT: &str = "vfkit";
pub const CLOUD_HYPERVISOR: &str = "cloud-hypervisor";
pub const GVISOR: &str = "gvisor";

/// The backends this platform has, best first (`host` is for development only).
pub fn platform_backends() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[VFKIT]
    } else {
        &[CLOUD_HYPERVISOR, GVISOR]
    }
}

/// The platform's main backend, for messages and `cove doctor`.
pub fn platform_default() -> &'static str {
    platform_backends()[0]
}

/// What `auto` picks: Apple's VMs on a Mac; on Linux, cloud-hypervisor when KVM is
/// usable and it is installed, else gVisor when installed, else cloud-hypervisor if
/// there is KVM (so the error says what to install), else nothing.
pub fn choose_auto(
    macos: bool,
    kvm: bool,
    cloud_hypervisor: bool,
    runsc: bool,
) -> std::result::Result<&'static str, String> {
    if macos {
        return Ok(VFKIT);
    }
    match (kvm, cloud_hypervisor, runsc) {
        (true, true, _) => Ok(CLOUD_HYPERVISOR),
        (_, _, true) => Ok(GVISOR),
        (true, false, false) => Ok(CLOUD_HYPERVISOR),
        (false, _, false) => Err(
            "this machine has no KVM, so Cove needs gVisor to run VMs here: install it with \
             scripts/install.sh, or see https://gvisor.dev/docs/user_guide/install/"
                .into(),
        ),
    }
}

/// The backend `auto` picks on this machine.
pub fn auto() -> std::result::Result<&'static str, String> {
    choose_auto(
        cfg!(target_os = "macos"),
        crate::chv::kvm_check(Path::new("/dev/kvm")).is_ok(),
        which("cloud-hypervisor").is_some(),
        which("runsc").is_some(),
    )
}

pub fn resolve(requested: &str) -> Result<&'static str> {
    Ok(match requested {
        "" | "auto" => auto().map_err(anyhow::Error::msg)?,
        HOST => HOST,
        VFKIT => VFKIT,
        CLOUD_HYPERVISOR => CLOUD_HYPERVISOR,
        GVISOR => GVISOR,
        other => {
            bail!("unknown backend {other:?} (use auto, vfkit, cloud-hypervisor, gvisor or host)")
        }
    })
}

/// Whether the dev backend may be used. It runs commands directly on the host with no
/// isolation, so it is off unless the server was started with it allowed.
pub fn host_allowed() -> bool {
    std::env::var("COVE_ALLOW_HOST_BACKEND").is_ok_and(|v| v == "1")
}

/// What `cove doctor` and VM creation check. `Err` says what to do.
pub fn check(backend: &str) -> std::result::Result<String, String> {
    match backend {
        HOST => {
            if host_allowed() {
                Ok("dev backend: commands run directly on this machine, with no isolation".into())
            } else {
                Err("the host backend is for development; start the server with COVE_ALLOW_HOST_BACKEND=1".into())
            }
        }
        VFKIT => {
            if !cfg!(target_os = "macos") {
                return Err("vfkit runs on macOS only".into());
            }
            if !cfg!(target_arch = "aarch64") {
                return Err("cove supports Apple Silicon Macs only".into());
            }
            match which("vfkit") {
                Some(p) => Ok(format!("vfkit at {}", p.display())),
                None => Err("vfkit is not installed: brew install vfkit".into()),
            }
        }
        CLOUD_HYPERVISOR => {
            if !cfg!(target_os = "linux") {
                return Err("cloud-hypervisor runs on Linux only".into());
            }
            crate::chv::kvm_check(Path::new("/dev/kvm"))?;
            match which("cloud-hypervisor") {
                Some(p) => Ok(format!("cloud-hypervisor at {}", p.display())),
                None => Err(
                    "cloud-hypervisor is not installed: download cloud-hypervisor-static \
                     from https://github.com/cloud-hypervisor/cloud-hypervisor/releases into \
                     /usr/local/bin/cloud-hypervisor (scripts/install.sh does this)"
                        .into(),
                ),
            }
        }
        GVISOR => crate::gvisor::check(),
        other => Err(format!("unknown backend {other:?}")),
    }
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    // Homebrew isn't on PATH for launchd services, sbin often isn't for other users,
    // and Debian and Ubuntu install virtiofsd outside PATH.
    dirs.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/sbin",
            "/sbin",
            "/usr/libexec",
            "/usr/lib/qemu",
        ]
        .map(PathBuf::from),
    );
    dirs.into_iter()
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// How to find a running VM again. Saved as `run.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launch {
    pub pid: u32,
    pub guest: GuestAddr,
    pub started_at: u64,
    /// Helper processes that live as long as the VM (virtiofsd), stopped with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helpers: Vec<u32>,
    /// cloud-hypervisor: the VM's network slot, so no other VM takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<u32>,
    /// Set while the VM is frozen, and why. Kept here so a restarted server knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PauseReason>,
    /// Paused to disk: its memory is in `state/` and no process runs (`pid` is 0).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub saved: bool,
    /// gVisor: the platform the sandbox runs on, which a restore must use too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl Launch {
    pub fn load(vm_dir: &Path) -> Option<Launch> {
        let text = std::fs::read_to_string(vm_dir.join("run.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, vm_dir: &Path) -> Result<()> {
        std::fs::write(vm_dir.join("run.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn clear(vm_dir: &Path) {
        let _ = std::fs::remove_file(vm_dir.join("run.json"));
    }

    /// Whether its process runs. A VM paused to disk has none.
    pub fn alive(&self) -> bool {
        self.pid != 0 && pid_alive(self.pid)
    }

    /// Running, or paused to disk: the VM exists past a stop.
    pub fn present(&self) -> bool {
        self.saved || self.alive()
    }

    /// Clean up after the VM: stop helpers (they exit by themselves when the VM does,
    /// but don't rely on it) and remove a gVisor sandbox's network namespace.
    pub fn stop_helpers(&self) {
        crate::chv::stop_helpers(&self.helpers);
        if let (Some(slot), GuestAddr::Unix { .. }) = (self.net, &self.guest) {
            crate::gvisor::teardown_netns(crate::chv::NetSlot(slot));
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub fn signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// The guest agent binary that goes into VMs (a static Linux build for this arch).
pub fn guest_binary(paths: &Paths) -> Result<PathBuf> {
    let arch = crate::image::rust_arch();
    let name = format!("cove-guest-linux-{arch}");
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("COVE_GUEST_BINARY") {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(paths.guest_dir().join(&name));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&name));
            candidates.push(dir.join("../share/cove").join(&name));
            // A Linux host can use its own build when it is the right arch.
            if cfg!(target_os = "linux") {
                candidates.push(dir.join("cove-guest"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "no guest agent found: put {name} in {}",
                paths.guest_dir().display()
            )
        })
}

/// The host-native guest agent for the dev backend.
fn host_guest_binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("COVE_HOST_GUEST_BINARY") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let p = exe
        .parent()
        .map(|d| d.join("cove-guest"))
        .unwrap_or_default();
    if p.is_file() {
        Ok(p)
    } else {
        bail!("cove-guest is not next to {}", exe.display())
    }
}

fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).process_group(0);
}

/// Spawn and reap in the background so a finished VM doesn't linger as a zombie.
pub(crate) fn spawn_detached(mut cmd: std::process::Command) -> Result<u32> {
    detach(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// What a backend needs beyond the record. Only cloud-hypervisor uses these.
#[derive(Debug, Default, Clone)]
pub struct LaunchOptions {
    pub firmware: Option<PathBuf>,
    pub net: Option<crate::chv::NetSlot>,
    /// gVisor: the agent binary, bind-mounted into the sandbox.
    pub guest_binary: Option<PathBuf>,
}

/// Boot the VM. Disks and the seed already exist in `vm_dir`.
pub fn launch(record: &VmRecord, vm_dir: &Path, opts: &LaunchOptions) -> Result<Launch> {
    let log = std::fs::File::create(vm_dir.join("backend.log"))?;
    let launch = match record.spec.backend.as_str() {
        // The dev backend runs the agent as a plain host process, so it shares the
        // host's network whatever `network` says; only real VMs enforce the policy.
        HOST => {
            for m in &record.spec.mounts {
                if m.host != m.guest {
                    bail!("the host backend can't remap folders; use the same path for host and guest");
                }
            }
            let root = vm_dir.join("root");
            std::fs::create_dir_all(&root)?;
            let port = free_port()?;
            let mut cmd = std::process::Command::new(host_guest_binary()?);
            cmd.args(["--listen", &format!("tcp:127.0.0.1:{port}")])
                .current_dir(&root)
                .stdout(log.try_clone()?)
                .stderr(log);
            Launch {
                pid: spawn_detached(cmd)?,
                guest: GuestAddr::Tcp {
                    addr: format!("127.0.0.1:{port}"),
                },
                started_at: crate::now(),
                helpers: vec![],
                net: None,
                paused: None,
                saved: false,
                platform: None,
            }
        }
        VFKIT => {
            let vfkit = which("vfkit").context("vfkit is not installed: brew install vfkit")?;
            let socket = vm_dir.join("guest.sock");
            let _ = std::fs::remove_file(&socket);
            let _ = std::fs::remove_file(vm_dir.join("rest.sock"));
            let mut cmd = std::process::Command::new(&vfkit);
            cmd.args(vfkit_args(record, vm_dir, vfkit_vsock_mode(&vfkit)))
                .stdout(log.try_clone()?)
                .stderr(log);
            Launch {
                pid: spawn_detached(cmd)?,
                guest: GuestAddr::Unix { path: socket },
                started_at: crate::now(),
                helpers: vec![],
                net: None,
                paused: None,
                saved: false,
                platform: None,
            }
        }
        CLOUD_HYPERVISOR => {
            let program = which("cloud-hypervisor")
                .context("cloud-hypervisor is not installed (see cove doctor)")?;
            let firmware = opts
                .firmware
                .clone()
                .context("no cloud-hypervisor firmware (see cove doctor)")?;
            // cloud-hypervisor binds these itself, within the ~100-byte socket limit.
            if vm_dir.join("vsock.sock").as_os_str().len() > 100 {
                bail!(
                    "{} is too deep for cloud-hypervisor's sockets; use a shorter COVE_HOME",
                    vm_dir.display()
                );
            }
            for stale in ["vsock.sock", "api.sock"] {
                let _ = std::fs::remove_file(vm_dir.join(stale));
            }
            let net = match record.spec.network {
                Network::Full => {
                    crate::chv::setup_nat()?;
                    Some(opts.net.context("no network slot for this VM")?)
                }
                Network::Off | Network::Allowlist => None,
            };
            let helpers = crate::chv::start_virtiofsd(record, vm_dir, &log)?;
            let mut cmd = std::process::Command::new(&program);
            let image_type = crate::chv::wants_image_type(&program);
            cmd.args(crate::chv::cloud_hypervisor_args(
                record, vm_dir, &firmware, net, image_type,
            ))
            .stdout(log.try_clone()?)
            .stderr(log);
            let pid = match spawn_detached(cmd) {
                Ok(pid) => pid,
                Err(e) => {
                    crate::chv::stop_helpers(&helpers);
                    return Err(e);
                }
            };
            Launch {
                pid,
                guest: GuestAddr::HybridVsock {
                    path: vm_dir.join("vsock.sock"),
                    port: cove_proto::GUEST_PORT,
                },
                started_at: crate::now(),
                helpers,
                net: net.map(|n| n.0),
                paused: None,
                saved: false,
                platform: None,
            }
        }
        GVISOR => {
            let guest = opts
                .guest_binary
                .clone()
                .context("no guest agent for the sandbox")?;
            crate::gvisor::launch(record, vm_dir, &guest, opts.net, &log)?
        }
        other => bail!(check(other)
            .err()
            .unwrap_or_else(|| format!("{other} can't start VMs yet"))),
    };
    launch.save(vm_dir)?;
    Ok(launch)
}

/// Freeze a running VM: its vCPUs (or processes) stop, memory stays where it is.
pub async fn pause(record: &VmRecord, vm_dir: &Path, launch: &Launch) -> Result<()> {
    match record.spec.backend.as_str() {
        // The dev backend's "VM" is the agent's process group.
        HOST => signal_group(launch.pid, libc::SIGSTOP),
        VFKIT => vfkit_state(vm_dir, "Pause").await?,
        CLOUD_HYPERVISOR => crate::chv::pause(vm_dir).await?,
        GVISOR => crate::gvisor::pause(&record.spec.name, vm_dir)?,
        other => bail!("{other} can't pause VMs"),
    }
    Ok(())
}

pub async fn resume(record: &VmRecord, vm_dir: &Path, launch: &Launch) -> Result<()> {
    match record.spec.backend.as_str() {
        HOST => signal_group(launch.pid, libc::SIGCONT),
        VFKIT => vfkit_state(vm_dir, "Resume").await?,
        CLOUD_HYPERVISOR => crate::chv::resume(vm_dir).await?,
        GVISOR => crate::gvisor::resume(&record.spec.name, vm_dir)?,
        other => bail!("{other} can't resume VMs"),
    }
    Ok(())
}

/// Whether this VM can be paused to disk, freeing its memory.
pub fn can_save(record: &VmRecord) -> bool {
    match record.spec.backend.as_str() {
        // With shared folders, only while nothing in the VM uses them (see save).
        CLOUD_HYPERVISOR | GVISOR => true,
        _ => false,
    }
}

/// Write a running VM's memory to disk and end its process. `launch` becomes saved.
/// On failure the VM is left running, as it was.
pub async fn save(record: &VmRecord, vm_dir: &Path, launch: &mut Launch) -> Result<()> {
    match record.spec.backend.as_str() {
        CLOUD_HYPERVISOR => {
            let folders = !record.spec.mounts.is_empty();
            if folders {
                unmount_folders(record, launch).await?;
                if let Err(e) = crate::chv::unplug_folders(record, vm_dir).await {
                    replug(record, vm_dir, launch).await;
                    return Err(e);
                }
                crate::chv::stop_helpers(&launch.helpers);
                launch.helpers.clear();
            }
            let saved = match crate::chv::pause(vm_dir).await {
                Ok(()) => crate::chv::save(vm_dir).await,
                Err(e) => Err(e),
            };
            if let Err(e) = saved {
                let _ = crate::chv::resume(vm_dir).await;
                if folders {
                    replug(record, vm_dir, launch).await;
                }
                return Err(e);
            }
        }
        GVISOR => crate::gvisor::save(&record.spec.name, vm_dir, launch).await?,
        other => bail!("{other} can't pause VMs to disk"),
    }
    signal(launch.pid, libc::SIGKILL);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pid_alive(launch.pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    crate::chv::stop_helpers(&launch.helpers);
    launch.helpers.clear();
    launch.pid = 0;
    launch.saved = true;
    Ok(())
}

/// Start a VM paused to disk again, from where it was, and running.
pub async fn restore(record: &VmRecord, vm_dir: &Path, launch: &mut Launch) -> Result<()> {
    match record.spec.backend.as_str() {
        CLOUD_HYPERVISOR => {
            let program = which("cloud-hypervisor")
                .context("cloud-hypervisor is not installed (see cove doctor)")?;
            if record.spec.network == Network::Full {
                crate::chv::setup_nat()?;
            }
            for stale in ["vsock.sock", "api.sock"] {
                let _ = std::fs::remove_file(vm_dir.join(stale));
            }
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(vm_dir.join("backend.log"))?;
            let mut cmd = std::process::Command::new(&program);
            cmd.args(crate::chv::restore_args(vm_dir))
                .stdout(log.try_clone()?)
                .stderr(log);
            let pid = spawn_detached(cmd)?;
            // Wait for its API, which answers once the memory is loaded.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if !pid_alive(pid) {
                    bail!("cloud-hypervisor exited while restoring (see backend.log)");
                }
                if vm_dir.join("api.sock").exists()
                    && unix_http(&vm_dir.join("api.sock"), "GET", "/api/v1/vm.info", None)
                        .await
                        .is_ok_and(|(code, _)| code == 200)
                {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    signal(pid, libc::SIGKILL);
                    bail!("cloud-hypervisor didn't finish restoring");
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if let Err(e) = crate::chv::resume(vm_dir).await {
                signal(pid, libc::SIGKILL);
                return Err(e);
            }
            launch.pid = pid;
            if !record.spec.mounts.is_empty() {
                match crate::chv::plug_folders(record, vm_dir).await {
                    Ok(helpers) => launch.helpers = helpers,
                    Err(e) => {
                        signal(pid, libc::SIGKILL);
                        return Err(e.context("plugging the shared folders back in"));
                    }
                }
            }
        }
        GVISOR => {
            let platform = launch.platform.clone().unwrap_or_else(|| "systrap".into());
            let net = match record.spec.network {
                Network::Full => launch.net.map(crate::chv::NetSlot),
                _ => None,
            };
            launch.pid = crate::gvisor::restore(record, vm_dir, &platform, net)?;
        }
        other => bail!("{other} can't restore VMs from disk"),
    }
    launch.saved = false;
    Ok(())
}

/// Unmount a VM's shared folders in the guest. Fails, leaving them all mounted,
/// when something in the VM is using one (an open file, a shell's working folder).
async fn unmount_folders(record: &VmRecord, launch: &Launch) -> Result<()> {
    let mut done = Vec::new();
    for (i, m) in record.spec.mounts.iter().enumerate() {
        let out = launch
            .guest
            .exec(cove_proto::Command {
                cmd: vec!["umount".into(), m.guest.clone()],
                ..Default::default()
            })
            .await?;
        if out.code != Some(0) {
            for j in done {
                remount(record, launch, j).await;
            }
            bail!(
                "{} is in use in the VM ({})",
                m.guest,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        done.push(i);
    }
    Ok(())
}

async fn remount(record: &VmRecord, launch: &Launch, i: usize) {
    let m = &record.spec.mounts[i];
    let _ = launch
        .guest
        .call(&cove_proto::Request::Mount {
            tag: mount_tag(i),
            path: m.guest.clone(),
            read_only: m.read_only,
        })
        .await;
}

/// Plug a VM's shared folders back in and mount them, after an unplug.
async fn replug(record: &VmRecord, vm_dir: &Path, launch: &mut Launch) {
    match crate::chv::plug_folders(record, vm_dir).await {
        Ok(helpers) => launch.helpers = helpers,
        Err(e) => eprintln!(
            "{}: couldn't plug its shared folders back: {e:#}",
            record.spec.name
        ),
    }
    remount_all(record, launch).await;
}

/// Mount every shared folder in the guest, waiting a little for hotplugged devices.
pub async fn remount_all(record: &VmRecord, launch: &Launch) {
    for i in 0..record.spec.mounts.len() {
        let m = &record.spec.mounts[i];
        for _ in 0..50 {
            let ok = launch
                .guest
                .call(&cove_proto::Request::Mount {
                    tag: mount_tag(i),
                    path: m.guest.clone(),
                    read_only: m.read_only,
                })
                .await
                .is_ok();
            if ok {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

fn signal_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Change a vfkit VM's state through its REST socket (`Pause`, `Resume`).
async fn vfkit_state(vm_dir: &Path, state: &str) -> Result<()> {
    let socket = vm_dir.join("rest.sock");
    if !socket.exists() {
        bail!("this VM was started by an older Cove, which can't pause it; stop and start it once");
    }
    let body = serde_json::json!({ "state": state }).to_string();
    let (code, line) = unix_http(&socket, "POST", "/vm/state", Some(&body)).await?;
    if !(200..300).contains(&code) {
        bail!("vfkit {state}: {line}");
    }
    Ok(())
}

/// One HTTP/1.1 request over a Unix socket; returns the status code and line.
pub async fn unix_http(
    socket: &Path,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = crate::guest::connect_unix(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    let body = body.unwrap_or("");
    let mut request = format!(
        "{method} {uri} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    s.write_all(request.as_bytes()).await?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    // The status line is all we need.
    while !reply.contains(&b'\n') {
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), s.read(&mut buf))
            .await
            .context("no answer")??;
        if n == 0 {
            break;
        }
        reply.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&reply);
    let line = text.lines().next().unwrap_or("").to_string();
    let code = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .with_context(|| format!("bad HTTP reply: {line:?}"))?;
    Ok((code, line))
}

pub fn mount_tag(index: usize) -> String {
    format!("cove{index}")
}

/// How to ask vfkit to expose the agent's vsock port on a Unix socket that Cove
/// connects to. vfkit 0.6 swapped the meaning of its two modes: before it, `listen`
/// did this; from it, `connect` does.
pub fn vfkit_vsock_mode(vfkit: &Path) -> &'static str {
    static MODE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    MODE.get_or_init(|| {
        let text = std::process::Command::new(vfkit)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        vsock_mode_for(&text)
    })
}

/// The mode for `vfkit --version` output such as "vfkit version: v0.6.4".
pub fn vsock_mode_for(version: &str) -> &'static str {
    let numbers: Vec<u32> = version
        .rsplit(['v', ' '])
        .next()
        .unwrap_or("")
        .split('.')
        .map_while(|p| p.trim().parse().ok())
        .collect();
    match numbers.as_slice() {
        [0, minor, ..] if *minor < 6 => "listen",
        // Unknown output: assume a current vfkit.
        _ => "connect",
    }
}

pub fn vfkit_args(record: &VmRecord, vm_dir: &Path, vsock_mode: &str) -> Vec<String> {
    let d = |name: &str| vm_dir.join(name).display().to_string();
    let mut args = vec![
        "--cpus".into(),
        record.spec.cpus.to_string(),
        "--memory".into(),
        record.spec.memory_mb.to_string(),
        "--bootloader".into(),
        format!("efi,variable-store={},create", d("efi-vars")),
        "--device".into(),
        format!("virtio-blk,path={}", d("disk.raw")),
        "--device".into(),
        format!("virtio-blk,path={}", d("seed.img")),
    ];
    // With the network off there is no NIC at all, not just no route: the guest
    // agent talks over vsock, which needs none.
    if record.spec.network == Network::Full {
        args.push("--device".into());
        args.push("virtio-net,nat".into());
    }
    args.extend([
        "--device".into(),
        format!(
            "virtio-vsock,port={},socketURL={},{vsock_mode}",
            cove_proto::GUEST_PORT,
            d("guest.sock")
        ),
        // For pause and resume.
        "--restful-uri".into(),
        format!("unix://{}", d("rest.sock")),
        "--device".into(),
        "virtio-rng".into(),
        "--device".into(),
        format!("virtio-serial,logFilePath={}", d("console.log")),
    ]);
    for (i, m) in record.spec.mounts.iter().enumerate() {
        args.push("--device".into());
        args.push(format!(
            "virtio-fs,sharedDir={},mountTag={}",
            m.host,
            mount_tag(i)
        ));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mount, VmSpec};

    #[test]
    fn vfkit_command_line() {
        let record = VmRecord {
            spec: VmSpec {
                name: "a".into(),
                image: "debian-13".into(),
                cpus: 4,
                memory_mb: 8192,
                disk_gb: 20,
                mounts: vec![Mount {
                    host: "/Users/me/app".into(),
                    guest: "/work".into(),
                    read_only: false,
                }],
                ephemeral: false,
                tags: vec![],
                backend: VFKIT.into(),
                network: Network::Full,
                idle_stop_minutes: None,
                allow: Vec::new(),
                secrets: Vec::new(),
                ..VmSpec::default()
            },
            created_at: 0,
            instance_id: "x".into(),
        };
        let args = vfkit_args(&record, Path::new("/vms/a"), "connect").join(" ");
        assert!(args.starts_with(
            "--cpus 4 --memory 8192 --bootloader efi,variable-store=/vms/a/efi-vars,create"
        ));
        assert!(args.contains("--device virtio-net,nat"));
        assert!(
            args.contains("--device virtio-vsock,port=1024,socketURL=/vms/a/guest.sock,connect")
        );
        assert!(args.ends_with("--device virtio-fs,sharedDir=/Users/me/app,mountTag=cove0"));
    }

    #[test]
    fn vsock_mode_follows_the_vfkit_version() {
        assert_eq!(vsock_mode_for("vfkit version: v0.6.4\n"), "connect");
        assert_eq!(vsock_mode_for("vfkit version: v0.5.1\n"), "listen");
        assert_eq!(vsock_mode_for("vfkit version: v1.0.0"), "connect");
        assert_eq!(vsock_mode_for(""), "connect");
    }

    #[test]
    fn vfkit_without_network() {
        let record = VmRecord {
            spec: VmSpec {
                name: "a".into(),
                image: "debian-13".into(),
                cpus: 2,
                memory_mb: 4096,
                disk_gb: 20,
                mounts: vec![],
                ephemeral: false,
                tags: vec![],
                backend: VFKIT.into(),
                network: Network::Off,
                idle_stop_minutes: None,
                allow: Vec::new(),
                secrets: Vec::new(),
                ..VmSpec::default()
            },
            created_at: 0,
            instance_id: "x".into(),
        };
        let args = vfkit_args(&record, Path::new("/vms/a"), "connect").join(" ");
        assert!(!args.contains("virtio-net"));
        // The agent's channel is still there.
        assert!(args.contains("virtio-vsock"));
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;

    #[test]
    fn run_json_round_trips_and_old_files_load() {
        let dir = tempfile::tempdir().unwrap();
        let launch = Launch {
            pid: 42,
            guest: GuestAddr::HybridVsock {
                path: dir.path().join("vsock.sock"),
                port: 1024,
            },
            started_at: 7,
            helpers: vec![43, 44],
            net: Some(3),
            paused: Some(PauseReason::Idle),
            saved: false,
            platform: None,
        };
        launch.save(dir.path()).unwrap();
        let back = Launch::load(dir.path()).unwrap();
        assert_eq!(back.guest, launch.guest);
        assert_eq!((back.helpers, back.net), (vec![43, 44], Some(3)));
        assert_eq!(back.paused, Some(PauseReason::Idle));
        // run.json from before helpers and network slots existed.
        std::fs::write(
            dir.path().join("run.json"),
            r#"{"pid":1,"guest":{"kind":"unix","path":"/x"},"started_at":0}"#,
        )
        .unwrap();
        let old = Launch::load(dir.path()).unwrap();
        assert!(old.helpers.is_empty() && old.net.is_none());
        Launch::clear(dir.path());
        assert!(Launch::load(dir.path()).is_none());
    }

    #[test]
    fn linux_defaults_to_cloud_hypervisor() {
        if cfg!(target_os = "linux") {
            assert_eq!(platform_default(), CLOUD_HYPERVISOR);
            assert_eq!(platform_backends(), [CLOUD_HYPERVISOR, GVISOR]);
        }
        assert_eq!(resolve("gvisor").unwrap(), GVISOR);
        assert!(resolve("qemu").is_err());
        assert!(resolve("container").is_err());
    }

    #[test]
    fn auto_picks_the_strongest_available() {
        assert_eq!(choose_auto(true, false, false, false), Ok(VFKIT));
        assert_eq!(choose_auto(false, true, true, true), Ok(CLOUD_HYPERVISOR));
        assert_eq!(choose_auto(false, true, true, false), Ok(CLOUD_HYPERVISOR));
        // No KVM (or KVM but no cloud-hypervisor): gVisor if it is there.
        assert_eq!(choose_auto(false, false, true, true), Ok(GVISOR));
        assert_eq!(choose_auto(false, true, false, true), Ok(GVISOR));
        // KVM but nothing installed: cloud-hypervisor, so the error says to install it.
        assert_eq!(choose_auto(false, true, false, false), Ok(CLOUD_HYPERVISOR));
        let refused = choose_auto(false, false, false, false).unwrap_err();
        assert!(refused.contains("gVisor"), "{refused}");
    }
}
