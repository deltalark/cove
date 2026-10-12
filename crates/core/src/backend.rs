//! Backends boot a VM and say how to reach its guest agent.
//!
//! VMs run as detached processes (their own session), so restarting the server does
//! not stop them; the server finds them again from `run.json`.

use crate::guest::GuestAddr;
use crate::model::{Network, VmRecord};
use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub const HOST: &str = "host";
pub const VFKIT: &str = "vfkit";
pub const CLOUD_HYPERVISOR: &str = "cloud-hypervisor";

pub fn platform_default() -> &'static str {
    if cfg!(target_os = "macos") {
        VFKIT
    } else {
        CLOUD_HYPERVISOR
    }
}

pub fn resolve(requested: &str) -> Result<&'static str> {
    Ok(match requested {
        "" => platform_default(),
        HOST => HOST,
        VFKIT => VFKIT,
        CLOUD_HYPERVISOR => CLOUD_HYPERVISOR,
        other => bail!("unknown backend {other:?} (use vfkit, cloud-hypervisor or host)"),
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
        CLOUD_HYPERVISOR => Err(
            "the Linux backend (cloud-hypervisor) is not built yet; it is step 4 of the plan"
                .into(),
        ),
        other => Err(format!("unknown backend {other:?}")),
    }
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    // Homebrew isn't on PATH for launchd services.
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
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
fn spawn_detached(mut cmd: std::process::Command) -> Result<u32> {
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

/// Boot the VM. Disks and the seed already exist in `vm_dir`.
pub fn launch(record: &VmRecord, vm_dir: &Path) -> Result<Launch> {
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
            }
        }
        VFKIT => {
            let vfkit = which("vfkit").context("vfkit is not installed: brew install vfkit")?;
            let socket = vm_dir.join("guest.sock");
            let _ = std::fs::remove_file(&socket);
            let mut cmd = std::process::Command::new(vfkit);
            cmd.args(vfkit_args(record, vm_dir))
                .stdout(log.try_clone()?)
                .stderr(log);
            Launch {
                pid: spawn_detached(cmd)?,
                guest: GuestAddr::Unix { path: socket },
                started_at: crate::now(),
            }
        }
        other => bail!(check(other)
            .err()
            .unwrap_or_else(|| format!("{other} can't start VMs yet"))),
    };
    launch.save(vm_dir)?;
    Ok(launch)
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
