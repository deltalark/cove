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

    pub fn alive(&self) -> bool {
        pid_alive(self.pid)
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
            }
        }
        VFKIT => {
            let vfkit = which("vfkit").context("vfkit is not installed: brew install vfkit")?;
            let socket = vm_dir.join("guest.sock");
            let _ = std::fs::remove_file(&socket);
            let _ = std::fs::remove_file(vm_dir.join("rest.sock"));
            let mut cmd = std::process::Command::new(vfkit);
            cmd.args(vfkit_args(record, vm_dir))
                .stdout(log.try_clone()?)
                .stderr(log);
            Launch {
                pid: spawn_detached(cmd)?,
                guest: GuestAddr::Unix { path: socket },
                started_at: crate::now(),
                helpers: vec![],
                net: None,
                paused: None,
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

pub fn vfkit_args(record: &VmRecord, vm_dir: &Path) -> Vec<String> {
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
            "virtio-vsock,port={},socketURL={},listen",
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
        let args = vfkit_args(&record, Path::new("/vms/a")).join(" ");
        assert!(args.starts_with(
            "--cpus 4 --memory 8192 --bootloader efi,variable-store=/vms/a/efi-vars,create"
        ));
        assert!(args.contains("--device virtio-net,nat"));
        assert!(args.contains("--device virtio-vsock,port=1024,socketURL=/vms/a/guest.sock,listen"));
        assert!(args.ends_with("--device virtio-fs,sharedDir=/Users/me/app,mountTag=cove0"));
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
        let args = vfkit_args(&record, Path::new("/vms/a")).join(" ");
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
