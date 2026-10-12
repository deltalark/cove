//! The Linux backend for machines without KVM: gVisor sandboxes (`runsc`).
//!
//! A "VM" here is a gVisor sandbox: its own kernel in user space (gVisor's Sentry)
//! between the processes and the host, so commands in it don't make system calls to
//! the host kernel directly. It is weaker than a hardware VM but far stronger than a
//! container, and it needs no KVM, which many cheap VPS plans don't offer.
//!
//! Each VM keeps its own copy of the Debian root filesystem in `vms/<name>/rootfs`,
//! written through directly (`--overlay2=none`), so it persists like a disk. The guest
//! agent is the sandbox's first process (`cove-guest --init`) and listens on a Unix
//! socket in `vms/<name>/run`, which gVisor exposes to the host (`--host-uds=create`).
//! Shared folders are bind mounts. With the network on, the sandbox gets a network
//! namespace with a veth pair, the same /30 addressing and NAT as cloud-hypervisor's
//! taps, and gVisor's own network stack on top; with it off, only loopback.
//!
//! Sandboxes are their own processes, so they survive the server restarting.

use crate::backend::{which, Launch};
use crate::chv::NetSlot;
use crate::guest::GuestAddr;
use crate::model::{Network, VmRecord};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// systrap is the fast default; ptrace works where systrap can't (some nested hosts).
const PLATFORMS: [&str; 2] = ["systrap", "ptrace"];
/// Where the agent binary and its socket appear inside the sandbox.
const GUEST_IN_SANDBOX: &str = "/.cove/cove-guest";
const RUN_IN_SANDBOX: &str = "/run/cove";

pub fn container_id(name: &str) -> String {
    format!("cove-{name}")
}

pub fn rootfs(vm_dir: &Path) -> PathBuf {
    vm_dir.join("rootfs")
}

pub fn socket(vm_dir: &Path) -> PathBuf {
    vm_dir.join("run").join("guest.sock")
}

/// The sandbox's network namespace, named like its host-side veth.
pub fn netns(slot: NetSlot) -> String {
    slot.tap()
}

/// `runsc` flags shared by every command for this VM.
pub fn runsc_flags(vm_dir: &Path, platform: &str, network: Network) -> Vec<String> {
    vec![
        format!("--root={}", vm_dir.join("runsc").display()),
        format!("--platform={platform}"),
        format!(
            "--network={}",
            match network {
                Network::Full => "sandbox",
                Network::Off => "none",
            }
        ),
        // The agent's socket, bound inside the sandbox, must be reachable from here.
        "--host-uds=create".into(),
        // Without this, writes to the root filesystem go to a throwaway overlay.
        "--overlay2=none".into(),
    ]
}

/// Inside gVisor, capabilities apply to its kernel, not the host's; root in the
/// sandbox can install packages, mount tmpfs and so on.
const CAPABILITIES: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",
    "CAP_NET_ADMIN",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_ADMIN",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_MKNOD",
    "CAP_AUDIT_WRITE",
    "CAP_SETFCAP",
];

/// The OCI runtime spec (`config.json`) for a VM's sandbox.
pub fn oci_spec(
    record: &VmRecord,
    vm_dir: &Path,
    guest_binary: &Path,
    netns: Option<&str>,
) -> Value {
    let mut mounts = vec![
        json!({ "destination": "/proc", "type": "proc", "source": "proc" }),
        json!({ "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
                "options": ["nosuid", "strictatime", "mode=755", "size=65536k"] }),
        json!({ "destination": "/dev/pts", "type": "devpts", "source": "devpts",
                "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"] }),
        json!({ "destination": "/dev/shm", "type": "tmpfs", "source": "shm",
                "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"] }),
        json!({ "destination": "/sys", "type": "sysfs", "source": "sysfs",
                "options": ["nosuid", "noexec", "nodev", "ro"] }),
        json!({ "destination": GUEST_IN_SANDBOX, "type": "bind",
                "source": guest_binary.display().to_string(), "options": ["rbind", "ro"] }),
        json!({ "destination": RUN_IN_SANDBOX, "type": "bind",
                "source": vm_dir.join("run").display().to_string(), "options": ["rbind", "rw"] }),
    ];
    for m in &record.spec.mounts {
        mounts.push(json!({
            "destination": m.guest,
            "type": "bind",
            "source": m.host,
            "options": ["rbind", if m.read_only { "ro" } else { "rw" }],
        }));
    }
    let network = match netns {
        Some(name) => json!({ "type": "network", "path": format!("/var/run/netns/{name}") }),
        // A namespace of its own with nothing in it: loopback only.
        None => json!({ "type": "network" }),
    };
    json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": [GUEST_IN_SANDBOX, "--init", "--listen",
                     format!("unix:{RUN_IN_SANDBOX}/guest.sock")],
            "env": [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "HOME=/root",
            ],
            "cwd": "/root",
            "capabilities": {
                "bounding": CAPABILITIES,
                "effective": CAPABILITIES,
                "permitted": CAPABILITIES,
            },
        },
        "root": { "path": rootfs(vm_dir).display().to_string(), "readonly": false },
        "hostname": record.spec.name,
        "mounts": mounts,
        "linux": {
            "namespaces": [
                { "type": "pid" }, { "type": "ipc" }, { "type": "uts" }, { "type": "mount" },
                network,
            ],
            // gVisor sizes the sandbox's view of the machine from these.
            "resources": {
                "memory": { "limit": record.spec.memory_mb * 1024 * 1024 },
                "cpu": { "quota": record.spec.cpus as u64 * 100_000, "period": 100_000 },
            },
        },
    })
}

/// `ip` invocations that give `slot` a namespace with a veth to the host.
pub fn netns_commands(slot: NetSlot) -> Vec<Vec<String>> {
    let ns = netns(slot);
    let host = slot.tap();
    let cmd = |s: String| s.split(' ').map(String::from).collect::<Vec<_>>();
    vec![
        cmd(format!("netns add {ns}")),
        cmd(format!(
            "link add {host} type veth peer name eth0 netns {ns}"
        )),
        cmd(format!("addr add {}/30 dev {host}", slot.host_ip())),
        cmd(format!("link set {host} up")),
        cmd(format!("-n {ns} link set lo up")),
        cmd(format!("-n {ns} addr add {}/30 dev eth0", slot.guest_ip())),
        cmd(format!("-n {ns} link set eth0 up")),
        cmd(format!("-n {ns} route add default via {}", slot.host_ip())),
    ]
}

fn ip(args: &[String]) -> Result<()> {
    let program = which("ip").context("the network needs iproute2: apt install iproute2")?;
    let out = Command::new(program).args(args).output()?;
    if !out.status.success() {
        bail!(
            "ip {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn setup_netns(slot: NetSlot) -> Result<()> {
    teardown_netns(slot);
    for args in netns_commands(slot) {
        if let Err(e) = ip(&args) {
            teardown_netns(slot);
            return Err(e);
        }
    }
    Ok(())
}

/// Deleting the namespace also removes the veth pair.
pub fn teardown_netns(slot: NetSlot) {
    let _ = ip(&["netns".into(), "del".into(), netns(slot)]);
    let _ = ip(&["link".into(), "del".into(), slot.tap()]);
}

/// Files the root filesystem needs for this VM: its name, and resolvers when it has
/// a network (gVisor has no DHCP or DNS of its own).
pub fn prepare_rootfs(record: &VmRecord, root: &Path) -> Result<()> {
    let etc = root.join("etc");
    std::fs::create_dir_all(&etc)?;
    let name = &record.spec.name;
    std::fs::write(etc.join("hostname"), format!("{name}\n"))?;
    std::fs::write(
        etc.join("hosts"),
        format!("127.0.0.1\tlocalhost\n127.0.1.1\t{name}\n::1\tlocalhost ip6-localhost\n"),
    )?;
    if record.spec.network == Network::Full {
        let _ = std::fs::remove_file(etc.join("resolv.conf"));
        std::fs::write(
            etc.join("resolv.conf"),
            "nameserver 1.1.1.1\nnameserver 8.8.8.8\n",
        )?;
    }
    Ok(())
}

/// Copy a root filesystem, keeping owners and modes (and sharing blocks where the
/// filesystem can).
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let status = Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(from)
        .arg(to)
        .status()
        .context("running cp")?;
    if !status.success() {
        bail!("copying {} to {} failed", from.display(), to.display());
    }
    Ok(())
}

fn runsc() -> Result<PathBuf> {
    which("runsc").context("gVisor is not installed (see cove doctor)")
}

fn runsc_quiet(vm_dir: &Path, args: &[&str]) -> bool {
    let Ok(program) = runsc() else {
        return false;
    };
    Command::new(program)
        .arg(format!("--root={}", vm_dir.join("runsc").display()))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Boot a VM's sandbox. `log` gets runsc's and the sandbox's output.
pub fn launch(
    record: &VmRecord,
    vm_dir: &Path,
    guest_binary: &Path,
    net: Option<NetSlot>,
    log: &std::fs::File,
) -> Result<Launch> {
    use std::os::unix::process::CommandExt;
    let program = runsc()?;
    let root = rootfs(vm_dir);
    if !root.is_dir() {
        bail!("{} is missing; recreate the VM", root.display());
    }
    let id = container_id(&record.spec.name);
    std::fs::create_dir_all(vm_dir.join("run"))?;
    let _ = std::fs::remove_file(socket(vm_dir));
    // A sandbox left over from a crash would hold the name.
    runsc_quiet(vm_dir, &["delete", "--force", &id]);

    let net = match record.spec.network {
        Network::Full => {
            let slot = net.context("no network slot for this VM")?;
            crate::chv::setup_nat()?;
            setup_netns(slot)?;
            Some(slot)
        }
        Network::Off => None,
    };
    prepare_rootfs(record, &root)?;
    let bundle = vm_dir.join("bundle");
    std::fs::create_dir_all(&bundle)?;
    let spec = oci_spec(record, vm_dir, guest_binary, net.map(netns).as_deref());
    std::fs::write(
        bundle.join("config.json"),
        serde_json::to_vec_pretty(&spec)?,
    )?;

    let pid_file = vm_dir.join("runsc.pid");
    let platforms: Vec<String> = match std::env::var("COVE_GVISOR_PLATFORM") {
        Ok(p) if !p.is_empty() => vec![p],
        _ => PLATFORMS.map(String::from).to_vec(),
    };
    let mut last_error = String::new();
    for platform in &platforms {
        let _ = std::fs::remove_file(&pid_file);
        let run = |args: &[&str]| -> Result<bool> {
            let mut cmd = Command::new(&program);
            cmd.args(runsc_flags(vm_dir, platform, record.spec.network))
                .args(args)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log.try_clone()?)
                // The sandbox outlives the server, so keep it out of our group.
                .process_group(0);
            Ok(cmd.status()?.success())
        };
        let bundle_arg = bundle.display().to_string();
        let pid_arg = pid_file.display().to_string();
        let created = run(&[
            "create",
            "--bundle",
            &bundle_arg,
            "--pid-file",
            &pid_arg,
            &id,
        ])?;
        if created && run(&["start", &id])? {
            let pid = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|p| p.trim().parse::<u32>().ok())
                .context("runsc wrote no pid")?;
            return Ok(Launch {
                pid,
                guest: GuestAddr::Unix {
                    path: socket(vm_dir),
                },
                started_at: crate::now(),
                helpers: vec![],
                net: net.map(|n| n.0),
            });
        }
        last_error = format!("runsc couldn't start the sandbox on the {platform} platform");
        runsc_quiet(vm_dir, &["delete", "--force", &id]);
    }
    if let Some(slot) = net {
        teardown_netns(slot);
    }
    bail!(
        "{last_error} (see {})",
        vm_dir.join("backend.log").display()
    )
}

/// Stop a sandbox: SIGTERM to everything in it, then SIGKILL, then remove it.
pub async fn halt(name: &str, vm_dir: &Path, launch: &Launch) {
    let id = container_id(name);
    runsc_quiet(vm_dir, &["kill", "--all", &id, "TERM"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while launch.alive() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    runsc_quiet(vm_dir, &["kill", "--all", &id, "KILL"]);
    runsc_quiet(vm_dir, &["delete", "--force", &id]);
}

/// Mount points at or under `dir`, from `/proc/self/mounts` text (where spaces and
/// other odd characters are octal escapes), deepest first.
pub fn mounts_under(mounts: &str, dir: &Path) -> Vec<PathBuf> {
    let unescape = |field: &str| {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 3 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&field[i + 1..i + 4], 8) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        PathBuf::from(String::from_utf8_lossy(&out).into_owned())
    };
    let mut found: Vec<PathBuf> = mounts
        .lines()
        .filter_map(|l| l.split(' ').nth(1))
        .map(unescape)
        .filter(|p| p.starts_with(dir))
        .collect();
    found.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    found
}

/// runsc leaves a namespace mounted in its state folder; detach anything mounted
/// under a VM's folder so it can be deleted.
pub fn release_mounts(vm_dir: &Path) {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else {
        return;
    };
    for path in mounts_under(&text, vm_dir) {
        // Only Linux has /proc/self/mounts, so elsewhere there is nothing to detach.
        #[cfg(target_os = "linux")]
        if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            unsafe {
                libc::umount2(c.as_ptr(), libc::MNT_DETACH);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = path;
    }
}

/// Can this server run gVisor sandboxes? `Err` says what to do.
pub fn check() -> std::result::Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("gVisor runs on Linux only".into());
    }
    let Some(runsc) = which("runsc") else {
        return Err(
            "gVisor is not installed: scripts/install.sh installs it, or see \
             https://gvisor.dev/docs/user_guide/install/"
                .into(),
        );
    };
    if unsafe { libc::geteuid() } != 0 {
        return Err("gVisor sandboxes need the server to run as root".into());
    }
    Ok(format!("gVisor at {}", runsc.display()))
}

/// Refuse a VM this host can't run, before anything is created.
pub fn check_spec(spec: &crate::model::VmSpec, host: &crate::chv::Host) -> Result<()> {
    if spec.network == Network::Full {
        if !host.net_admin {
            bail!("a VM with the network on needs the server to run as root; use --network off otherwise");
        }
        if host.iptables.is_none() {
            bail!("a VM with the network on needs iptables: apt install iptables");
        }
        if which("ip").is_none() {
            bail!("a VM with the network on needs iproute2: apt install iproute2");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mount, VmSpec};

    fn record(network: Network) -> VmRecord {
        VmRecord {
            spec: VmSpec {
                name: "box".into(),
                image: "debian-13".into(),
                cpus: 2,
                memory_mb: 1024,
                disk_gb: 20,
                mounts: vec![
                    Mount {
                        host: "/srv/app".into(),
                        guest: "/work".into(),
                        read_only: false,
                    },
                    Mount {
                        host: "/srv/data".into(),
                        guest: "/data".into(),
                        read_only: true,
                    },
                ],
                ephemeral: false,
                tags: vec![],
                backend: crate::backend::GVISOR.into(),
                network,
                idle_stop_minutes: None,
            },
            created_at: 0,
            instance_id: "x".into(),
        }
    }

    #[test]
    fn spec_runs_the_agent_as_init() {
        let spec = oci_spec(
            &record(Network::Full),
            Path::new("/vms/box"),
            Path::new("/bin/cove-guest-linux-x86_64"),
            Some("cove3"),
        );
        assert_eq!(
            spec["process"]["args"],
            json!([
                "/.cove/cove-guest",
                "--init",
                "--listen",
                "unix:/run/cove/guest.sock"
            ])
        );
        assert_eq!(spec["root"]["path"], "/vms/box/rootfs");
        assert_eq!(spec["hostname"], "box");
        let mounts = spec["mounts"].as_array().unwrap();
        let find = |dest: &str| mounts.iter().find(|m| m["destination"] == dest).unwrap();
        assert_eq!(
            find("/.cove/cove-guest")["source"],
            "/bin/cove-guest-linux-x86_64"
        );
        assert_eq!(find("/.cove/cove-guest")["options"], json!(["rbind", "ro"]));
        assert_eq!(find("/run/cove")["source"], "/vms/box/run");
        assert_eq!(find("/work")["options"], json!(["rbind", "rw"]));
        assert_eq!(find("/data")["options"], json!(["rbind", "ro"]));
        let namespaces = spec["linux"]["namespaces"].as_array().unwrap();
        assert!(namespaces.contains(&json!({ "type": "network", "path": "/var/run/netns/cove3" })));
        assert_eq!(
            spec["linux"]["resources"]["memory"]["limit"],
            1024u64 * 1024 * 1024
        );
        assert_eq!(spec["linux"]["resources"]["cpu"]["quota"], 200_000);
        assert!(spec["process"]["capabilities"]["bounding"]
            .as_array()
            .unwrap()
            .contains(&json!("CAP_CHOWN")));
    }

    #[test]
    fn network_off_gets_an_empty_namespace() {
        let spec = oci_spec(
            &record(Network::Off),
            Path::new("/v"),
            Path::new("/g"),
            None,
        );
        let namespaces = spec["linux"]["namespaces"].as_array().unwrap();
        assert!(namespaces.contains(&json!({ "type": "network" })));
        let flags = runsc_flags(Path::new("/v"), "systrap", Network::Off);
        assert!(flags.contains(&"--network=none".to_string()));
    }

    #[test]
    fn runsc_flags_keep_writes_and_expose_the_socket() {
        let flags = runsc_flags(Path::new("/vms/box"), "ptrace", Network::Full);
        assert_eq!(
            flags,
            [
                "--root=/vms/box/runsc",
                "--platform=ptrace",
                "--network=sandbox",
                "--host-uds=create",
                "--overlay2=none"
            ]
        );
    }

    #[test]
    fn namespace_wiring() {
        let cmds: Vec<String> = netns_commands(NetSlot(2))
            .iter()
            .map(|c| c.join(" "))
            .collect();
        assert_eq!(cmds[0], "netns add cove2");
        assert!(cmds.contains(&"link add cove2 type veth peer name eth0 netns cove2".to_string()));
        assert!(cmds.contains(&"addr add 10.177.0.9/30 dev cove2".to_string()));
        assert!(cmds.contains(&"-n cove2 addr add 10.177.0.10/30 dev eth0".to_string()));
        assert_eq!(
            cmds.last().unwrap(),
            "-n cove2 route add default via 10.177.0.9"
        );
    }

    #[test]
    fn rootfs_gets_its_name_and_resolvers() {
        let dir = tempfile::tempdir().unwrap();
        prepare_rootfs(&record(Network::Full), dir.path()).unwrap();
        let read = |f: &str| std::fs::read_to_string(dir.path().join("etc").join(f)).unwrap();
        assert_eq!(read("hostname"), "box\n");
        assert!(read("hosts").contains("127.0.1.1\tbox"));
        assert!(read("resolv.conf").contains("nameserver 1.1.1.1"));
        let off = tempfile::tempdir().unwrap();
        prepare_rootfs(&record(Network::Off), off.path()).unwrap();
        assert!(!off.path().join("etc/resolv.conf").exists());
    }

    #[test]
    fn finds_mounts_under_a_vm() {
        let mounts = "proc /proc proc rw 0 0\n\
            nsfs /home/me/.cove/vms/a/runsc/null-netns nsfs rw 0 0\n\
            nsfs /home/me/.cove/vms/ab/runsc/null-netns nsfs rw 0 0\n\
            tmpfs /home/me/.cove/vms/a/my\\040dir tmpfs rw 0 0\n\
            tmpfs /home/me/.cove/vms/a/my\\040dir/deeper tmpfs rw 0 0\n";
        let found = mounts_under(mounts, Path::new("/home/me/.cove/vms/a"));
        assert_eq!(
            found,
            [
                PathBuf::from("/home/me/.cove/vms/a/runsc/null-netns"),
                PathBuf::from("/home/me/.cove/vms/a/my dir/deeper"),
                PathBuf::from("/home/me/.cove/vms/a/my dir"),
            ]
        );
    }

    #[test]
    fn trees_copy_with_their_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a");
        std::fs::create_dir_all(from.join("bin")).unwrap();
        std::fs::write(from.join("bin/tool"), "x").unwrap();
        std::fs::set_permissions(
            from.join("bin/tool"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        let to = dir.path().join("b");
        copy_tree(&from, &to).unwrap();
        let mode = std::fs::metadata(to.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o751);
    }
}
