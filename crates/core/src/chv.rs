//! The Linux backend: cloud-hypervisor with KVM.
//!
//! Each VM is a detached `cloud-hypervisor` process, booted with cloud-hypervisor's
//! EDK2 firmware from the same raw Debian disk vfkit uses. The guest agent is reached
//! over hybrid vsock (a Unix socket plus a `CONNECT <port>` handshake), each shared
//! folder is served by its own `virtiofsd`, and a VM with the network on gets a tap
//! device of its own, a /30 subnet in 10.177.0.0/16, and NAT through the host.
//!
//! Network setup needs root or `CAP_NET_ADMIN`; everything else needs read and write
//! access to `/dev/kvm`.

use crate::model::{Mount, Network, VmRecord};
use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Every VM's /30 comes out of this range.
pub const SUBNET: &str = "10.177.0.0/16";
/// Host-side tap devices are `cove<slot>`.
const TAP_PREFIX: &str = "cove";
/// Resolvers the guest uses: the VMs have no DNS server on the host side.
const NAMESERVERS: [&str; 2] = ["1.1.1.1", "8.8.8.8"];
/// cloud-hypervisor's EDK2 builds, which boot EFI disk images such as Debian's.
pub const FIRMWARE_URL: &str = "https://github.com/cloud-hypervisor/edk2/releases/latest/download/";

pub fn firmware_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "CLOUDHV_EFI.fd"
    } else {
        "CLOUDHV.fd"
    }
}

/// The firmware to boot with: `COVE_CH_FIRMWARE`, else one Cove downloaded, else a
/// distribution's copy.
pub fn firmware(paths: &Paths) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("COVE_CH_FIRMWARE") {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(paths.firmware_dir().join(firmware_name()));
    candidates.push(Path::new("/usr/share/cloud-hypervisor").join(firmware_name()));
    candidates.into_iter().find(|p| p.is_file())
}

/// A VM's place on the host network: tap `cove<index>` with the subnet's first
/// address, the guest on the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSlot(pub u32);

impl NetSlot {
    /// 16384 /30s fit in the /16.
    pub const MAX: u32 = 16384;

    fn address(self, host: u32) -> String {
        let base = self.0 * 4 + host;
        format!("10.177.{}.{}", base >> 8, base & 0xff)
    }
    pub fn tap(self) -> String {
        format!("{TAP_PREFIX}{}", self.0)
    }
    pub fn host_ip(self) -> String {
        self.address(1)
    }
    pub fn guest_ip(self) -> String {
        self.address(2)
    }
    pub fn mac(self) -> String {
        format!("52:54:00:c0:{:02x}:{:02x}", self.0 >> 8, self.0 & 0xff)
    }

    /// cloud-init `network-config` (v2) that gives the guest its static address.
    pub fn network_config(self) -> String {
        format!(
            "version: 2\n\
             ethernets:\n  \
               cove:\n    \
                 match:\n      \
                   macaddress: \"{mac}\"\n    \
                 addresses: [{guest}/30]\n    \
                 routes:\n      \
                   - to: default\n        \
                     via: {host}\n    \
                 nameservers:\n      \
                   addresses: [{dns}]\n",
            mac = self.mac(),
            guest = self.guest_ip(),
            host = self.host_ip(),
            dns = NAMESERVERS.join(", "),
        )
    }
}

/// The lowest slot no running VM uses.
pub fn free_slot(used: &[u32]) -> Result<NetSlot> {
    (0..NetSlot::MAX)
        .find(|i| !used.contains(i))
        .map(NetSlot)
        .context("every network slot is in use")
}

pub fn fs_socket(vm_dir: &Path, index: usize) -> PathBuf {
    vm_dir.join(format!("fs{index}.sock"))
}

/// One `virtiofsd` per shared folder. Without root it can't build its namespace
/// sandbox (Ubuntu limits unprivileged user namespaces), so it runs without one; the
/// VM still sees only the shared folder.
pub fn virtiofsd_args(mount: &Mount, socket: &Path, root: bool) -> Vec<String> {
    let mut args = vec![
        format!("--socket-path={}", socket.display()),
        format!("--shared-dir={}", mount.host),
        "--cache=auto".into(),
    ];
    if mount.read_only {
        args.push("--readonly".into());
    }
    if !root {
        args.push("--sandbox=none".into());
    }
    args
}

/// Whether this cloud-hypervisor wants `image_type=` on its disks. v54 refuses
/// disks without it, and versions before it refuse the option, so ask its `--help`.
pub fn wants_image_type(program: &Path) -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        std::process::Command::new(program)
            .arg("--help")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("image_type="))
    })
}

pub fn cloud_hypervisor_args(
    record: &VmRecord,
    vm_dir: &Path,
    firmware: &Path,
    net: Option<NetSlot>,
    image_type: bool,
) -> Vec<String> {
    let d = |name: &str| vm_dir.join(name).display().to_string();
    let raw = if image_type { ",image_type=raw" } else { "" };
    let mut args = vec![
        "--firmware".into(),
        firmware.display().to_string(),
        "--cpus".into(),
        format!("boot={}", record.spec.cpus),
        // virtio-fs needs guest memory the host can share with virtiofsd.
        "--memory".into(),
        format!("size={}M,shared=on", record.spec.memory_mb),
        "--disk".into(),
        format!("path={}{raw}", d("disk.raw")),
        format!("path={},readonly=on{raw}", d("seed.img")),
        "--vsock".into(),
        format!("cid=3,socket={}", d("vsock.sock")),
        "--api-socket".into(),
        format!("path={}", d("api.sock")),
        "--serial".into(),
        format!("file={}", d("console.log")),
        "--console".into(),
        "off".into(),
        "--rng".into(),
        "src=/dev/urandom".into(),
    ];
    // With the network off there is no NIC at all, as with vfkit.
    if let (Network::Full, Some(slot)) = (record.spec.network, net) {
        args.push("--net".into());
        args.push(format!(
            "tap={},mac={},ip={},mask=255.255.255.252",
            slot.tap(),
            slot.mac(),
            slot.host_ip()
        ));
    }
    if !record.spec.mounts.is_empty() {
        args.push("--fs".into());
        for i in 0..record.spec.mounts.len() {
            args.push(format!(
                "tag={},socket={},num_queues=1,queue_size=512,id={}",
                crate::backend::mount_tag(i),
                fs_socket(vm_dir, i).display(),
                fs_id(i)
            ));
        }
    }
    args
}

/// iptables rules for VM traffic, in the order they must end up in. VMs reach the
/// internet through NAT but not each other, the host's own services, or the cloud
/// metadata endpoint (which hands out the VPS's credentials on many providers).
pub fn nat_rules() -> Vec<(&'static str, &'static str, Vec<String>)> {
    let tap = format!("{TAP_PREFIX}+");
    let rule = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    vec![
        (
            "filter",
            "INPUT",
            rule(&format!(
                "-i {tap} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"
            )),
        ),
        ("filter", "INPUT", rule(&format!("-i {tap} -j DROP"))),
        (
            "filter",
            "FORWARD",
            rule(&format!("-i {tap} -o {tap} -j DROP")),
        ),
        (
            "filter",
            "FORWARD",
            rule(&format!("-i {tap} -d 169.254.0.0/16 -j DROP")),
        ),
        ("filter", "FORWARD", rule(&format!("-i {tap} -j ACCEPT"))),
        (
            "filter",
            "FORWARD",
            rule(&format!(
                "-o {tap} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"
            )),
        ),
        (
            "nat",
            "POSTROUTING",
            rule(&format!("-s {SUBNET} ! -o {tap} -j MASQUERADE")),
        ),
    ]
}

/// Turn on forwarding and add any missing NAT rules. Safe to run on every start.
pub fn setup_nat() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        .context("enabling IP forwarding needs root or CAP_NET_ADMIN")?;
    let iptables = crate::backend::which("iptables")
        .context("iptables is not installed: apt install iptables")?;
    let rules = nat_rules();
    // Insert from the last rule up, each at the top, so they end up in order and
    // ahead of a distribution's own rules.
    for (table, chain, spec) in rules.iter().rev() {
        let present = std::process::Command::new(&iptables)
            .args(["-w", "-t", table, "-C", chain])
            .args(spec)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if present {
            continue;
        }
        let out = std::process::Command::new(&iptables)
            .args(["-w", "-t", table, "-I", chain, "1"])
            .args(spec)
            .output()
            .context("running iptables")?;
        if !out.status.success() {
            bail!(
                "iptables -t {table} -I {chain} {}: {}",
                spec.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Ask cloud-hypervisor to press the VM's power button (ACPI shutdown), for when the
/// guest agent doesn't answer.
pub async fn power_button(vm_dir: &Path) -> Result<()> {
    api_put(vm_dir, "vm.power-button").await
}

/// Freeze the VM's vCPUs; memory stays as it is.
pub async fn pause(vm_dir: &Path) -> Result<()> {
    api_put(vm_dir, "vm.pause").await
}

pub async fn resume(vm_dir: &Path) -> Result<()> {
    api_put(vm_dir, "vm.resume").await
}

/// `PUT /api/v1/<action>` on the VM's API socket.
async fn api_put(vm_dir: &Path, action: &str) -> Result<()> {
    api_put_body(vm_dir, action, None).await
}

async fn api_put_body(vm_dir: &Path, action: &str, body: Option<&str>) -> Result<()> {
    let status = crate::backend::unix_http(
        &vm_dir.join("api.sock"),
        "PUT",
        &format!("/api/v1/{action}"),
        body,
    )
    .await?;
    if !(200..300).contains(&status.0) {
        bail!("cloud-hypervisor {action}: {}", status.1);
    }
    Ok(())
}

/// Where a VM paused to disk keeps its memory and device state.
pub fn state_dir(vm_dir: &Path) -> std::path::PathBuf {
    vm_dir.join("state")
}

/// The device id of shared folder `index`, so it can be unplugged and plugged back.
pub fn fs_id(index: usize) -> String {
    format!("fs{index}")
}

/// Unplug a VM's shared folders: cloud-hypervisor can't snapshot vhost-user devices,
/// which is what virtio-fs is. The guest must have unmounted them.
pub async fn unplug_folders(record: &VmRecord, vm_dir: &Path) -> Result<()> {
    for i in 0..record.spec.mounts.len() {
        let body = serde_json::json!({ "id": fs_id(i) }).to_string();
        api_put_body(vm_dir, "vm.remove-device", Some(&body)).await?;
    }
    // The guest acknowledges the unplug on its own time.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}

/// Start virtiofsd again and plug the VM's shared folders back in; returns the
/// helpers' pids. The guest mounts them afterwards.
pub async fn plug_folders(record: &VmRecord, vm_dir: &Path) -> Result<Vec<u32>> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(vm_dir.join("backend.log"))?;
    let helpers = start_virtiofsd(record, vm_dir, &log)?;
    for i in 0..record.spec.mounts.len() {
        let body = serde_json::json!({
            "tag": crate::backend::mount_tag(i),
            "socket": fs_socket(vm_dir, i).display().to_string(),
            "num_queues": 1,
            "queue_size": 512,
            "id": fs_id(i),
        })
        .to_string();
        if let Err(e) = api_put_body(vm_dir, "vm.add-fs", Some(&body)).await {
            stop_helpers(&helpers);
            return Err(e);
        }
    }
    Ok(helpers)
}

/// Write a paused VM's memory and devices to [`state_dir`].
pub async fn save(vm_dir: &Path) -> Result<()> {
    let state = state_dir(vm_dir);
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state)?;
    crate::paths::restrict(&state, 0o700);
    let body =
        serde_json::json!({ "destination_url": format!("file://{}", state.display()) }).to_string();
    // Writing out the memory takes a while for a big VM.
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        api_put_body(vm_dir, "vm.snapshot", Some(&body)),
    )
    .await
    .context("saving the VM's memory took too long")?
}

/// The command line that brings a saved VM back (paused; `resume` starts it).
pub fn restore_args(vm_dir: &Path) -> Vec<String> {
    vec![
        "--api-socket".into(),
        format!("path={}", vm_dir.join("api.sock").display()),
        "--restore".into(),
        format!("source_url=file://{}", state_dir(vm_dir).display()),
    ]
}

/// Whether this process may create tap devices and change iptables.
pub fn net_admin() -> bool {
    let root = unsafe { libc::geteuid() } == 0;
    root || std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| cap_eff(&s))
        .is_some_and(|caps| caps & (1 << 12) != 0)
}

/// The effective capability mask from `/proc/<pid>/status`.
pub fn cap_eff(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
}

/// What the Linux backend needs, gathered so the checks can be tested with fakes.
pub struct Host {
    pub kvm: PathBuf,
    pub cloud_hypervisor: Option<PathBuf>,
    pub virtiofsd: Option<PathBuf>,
    pub iptables: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
    pub net_admin: bool,
}

impl Host {
    pub fn detect(paths: &Paths) -> Host {
        Host {
            kvm: PathBuf::from("/dev/kvm"),
            cloud_hypervisor: crate::backend::which("cloud-hypervisor"),
            virtiofsd: crate::backend::which("virtiofsd"),
            iptables: crate::backend::which("iptables"),
            firmware: firmware(paths),
            net_admin: net_admin(),
        }
    }
}

pub struct Check {
    pub label: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Can this process open `/dev/kvm`? `Err` says what to do.
pub fn kvm_check(kvm: &Path) -> std::result::Result<String, String> {
    if !kvm.exists() {
        return Err(format!(
            "{} is missing: this machine can't run VMs. On a VPS, pick a plan with nested \
             virtualization or KVM (or a bare-metal server); on your own machine, turn on \
             virtualization (VT-x/AMD-V) in the firmware settings",
            kvm.display()
        ));
    }
    match std::fs::OpenOptions::new().read(true).write(true).open(kvm) {
        Ok(_) => Ok(format!("{} is usable", kvm.display())),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "no permission to use {}: run the server as root, or add this user to the kvm \
             group (sudo usermod -aG kvm $USER, then log in again)",
            kvm.display()
        )),
        Err(e) => Err(format!("{}: {e}", kvm.display())),
    }
}

/// Everything `cove doctor` reports for the Linux backend, with plain fixes.
pub fn checks(host: &Host) -> Vec<Check> {
    let mut out = Vec::new();
    let mut add = |label, result: std::result::Result<String, String>| {
        let (ok, detail) = match result {
            Ok(d) => (true, d),
            Err(d) => (false, d),
        };
        out.push(Check { label, ok, detail });
    };
    add("kvm", kvm_check(&host.kvm));
    add(
        "cloud-hypervisor",
        host.cloud_hypervisor
            .as_ref()
            .map(|p| p.display().to_string())
            .ok_or_else(|| {
                "not installed: download cloud-hypervisor-static from \
                 https://github.com/cloud-hypervisor/cloud-hypervisor/releases into \
                 /usr/local/bin/cloud-hypervisor (scripts/install.sh does this)"
                    .to_string()
            }),
    );
    add(
        "virtiofsd",
        host.virtiofsd
            .as_ref()
            .map(|p| format!("{} (for shared folders)", p.display()))
            .ok_or_else(|| {
                "not installed, so --mount won't work: apt install virtiofsd".to_string()
            }),
    );
    add(
        "firmware",
        Ok(match &host.firmware {
            Some(p) => p.display().to_string(),
            None => format!("{} is downloaded on the first start", firmware_name()),
        }),
    );
    add(
        "network",
        match (host.net_admin, &host.iptables) {
            (true, Some(_)) => Ok("can create tap devices and NAT".into()),
            (true, None) => {
                Err("iptables is not installed, so VMs get no network: apt install iptables".into())
            }
            (false, _) => Err(
                "VMs with --network full need root or CAP_NET_ADMIN: run the server as root \
                 (the systemd service does), or use --network off"
                    .into(),
            ),
        },
    );
    out
}

/// Refuse a VM this host can't run, saying why, before anything is created.
pub fn check_spec(spec: &crate::model::VmSpec, host: &Host) -> Result<()> {
    if !spec.mounts.is_empty() && host.virtiofsd.is_none() {
        bail!("shared folders need virtiofsd: apt install virtiofsd");
    }
    if spec.network == Network::Full && !host.net_admin {
        bail!(
            "a VM with the network on needs the server to run as root or with CAP_NET_ADMIN; \
             use --network off otherwise"
        );
    }
    if spec.network == Network::Full && host.iptables.is_none() {
        bail!("a VM with the network on needs iptables: apt install iptables");
    }
    Ok(())
}

/// Start `virtiofsd` for each mount and wait for its socket. Returns the pids.
pub fn start_virtiofsd(record: &VmRecord, vm_dir: &Path, log: &std::fs::File) -> Result<Vec<u32>> {
    if record.spec.mounts.is_empty() {
        return Ok(vec![]);
    }
    let program = crate::backend::which("virtiofsd")
        .context("shared folders need virtiofsd: apt install virtiofsd")?;
    let root = unsafe { libc::geteuid() } == 0;
    let mut pids = Vec::new();
    for (i, m) in record.spec.mounts.iter().enumerate() {
        let socket = fs_socket(vm_dir, i);
        let _ = std::fs::remove_file(&socket);
        let mut cmd = std::process::Command::new(&program);
        cmd.args(virtiofsd_args(m, &socket, root))
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?);
        match crate::backend::spawn_detached(cmd) {
            Ok(pid) => pids.push(pid),
            Err(e) => {
                stop_helpers(&pids);
                return Err(e);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                stop_helpers(&pids);
                bail!("virtiofsd didn't start for {} (see backend.log)", m.host);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(pids)
}

pub fn stop_helpers(pids: &[u32]) {
    for &pid in pids {
        crate::backend::signal(pid, libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VmSpec;

    fn record(network: Network, mounts: Vec<Mount>) -> VmRecord {
        VmRecord {
            spec: VmSpec {
                name: "a".into(),
                image: "debian-13".into(),
                cpus: 4,
                memory_mb: 8192,
                disk_gb: 20,
                mounts,
                ephemeral: false,
                tags: vec![],
                backend: crate::backend::CLOUD_HYPERVISOR.into(),
                network,
                idle_stop_minutes: None,
                allow: Vec::new(),
                secrets: Vec::new(),
                ..VmSpec::default()
            },
            created_at: 0,
            instance_id: "x".into(),
        }
    }

    fn mount(host: &str, guest: &str, read_only: bool) -> Mount {
        Mount {
            host: host.into(),
            guest: guest.into(),
            read_only,
        }
    }

    #[test]
    fn command_line() {
        let r = record(Network::Full, vec![mount("/srv/app", "/work", false)]);
        let args = cloud_hypervisor_args(
            &r,
            Path::new("/vms/a"),
            Path::new("/fw/CLOUDHV.fd"),
            Some(NetSlot(5)),
            false,
        )
        .join(" ");
        assert!(args.starts_with("--firmware /fw/CLOUDHV.fd --cpus boot=4"));
        assert!(args.contains("--memory size=8192M,shared=on"));
        assert!(
            args.contains("--disk path=/vms/a/disk.raw path=/vms/a/seed.img,readonly=on --vsock")
        );
        assert!(args.contains("--vsock cid=3,socket=/vms/a/vsock.sock"));
        assert!(args.contains("--api-socket path=/vms/a/api.sock"));
        assert!(args.contains("--serial file=/vms/a/console.log --console off"));
        assert!(args
            .contains("--net tap=cove5,mac=52:54:00:c0:00:05,ip=10.177.0.21,mask=255.255.255.252"));
        assert!(args
            .ends_with("--fs tag=cove0,socket=/vms/a/fs0.sock,num_queues=1,queue_size=512,id=fs0"));
    }

    #[test]
    fn newer_versions_name_the_image_type() {
        let r = record(Network::Off, vec![]);
        let args =
            cloud_hypervisor_args(&r, Path::new("/v"), Path::new("/fw"), None, true).join(" ");
        assert!(args.contains(
            "--disk path=/v/disk.raw,image_type=raw path=/v/seed.img,readonly=on,image_type=raw"
        ));
    }

    #[test]
    fn no_network_no_nic() {
        let r = record(Network::Off, vec![]);
        let args =
            cloud_hypervisor_args(&r, Path::new("/v"), Path::new("/fw"), None, false).join(" ");
        assert!(!args.contains("--net"));
        assert!(!args.contains("--fs"));
        assert!(args.contains("--vsock"));
    }

    #[test]
    fn slots_and_addresses() {
        let s = NetSlot(0);
        assert_eq!(
            (s.host_ip(), s.guest_ip()),
            ("10.177.0.1".into(), "10.177.0.2".into())
        );
        let s = NetSlot(64);
        assert_eq!(s.host_ip(), "10.177.1.1");
        assert_eq!(s.tap(), "cove64");
        let last = NetSlot(NetSlot::MAX - 1);
        assert_eq!(last.guest_ip(), "10.177.255.254");
        assert_eq!(last.mac(), "52:54:00:c0:3f:ff");
        assert!(last.tap().len() < 16, "tap names are at most 15 bytes");
        assert_eq!(free_slot(&[]).unwrap(), NetSlot(0));
        assert_eq!(free_slot(&[0, 1, 3]).unwrap(), NetSlot(2));
        let all: Vec<u32> = (0..NetSlot::MAX).collect();
        assert!(free_slot(&all).is_err());
    }

    #[test]
    fn network_config_is_static() {
        let text = NetSlot(1).network_config();
        assert!(text.starts_with("version: 2\nethernets:\n  cove:\n"));
        assert!(text.contains("macaddress: \"52:54:00:c0:00:01\""));
        assert!(text.contains("addresses: [10.177.0.6/30]"));
        assert!(text.contains("via: 10.177.0.5"));
        assert!(text.contains("addresses: [1.1.1.1, 8.8.8.8]"));
    }

    #[test]
    fn virtiofsd_command_line() {
        let args = virtiofsd_args(
            &mount("/srv/app", "/work", true),
            Path::new("/v/fs0.sock"),
            false,
        );
        assert_eq!(
            args,
            [
                "--socket-path=/v/fs0.sock",
                "--shared-dir=/srv/app",
                "--cache=auto",
                "--readonly",
                "--sandbox=none"
            ]
        );
        let args = virtiofsd_args(
            &mount("/srv/app", "/work", false),
            Path::new("/v/fs0.sock"),
            true,
        );
        assert!(!args
            .iter()
            .any(|a| a == "--readonly" || a.starts_with("--sandbox")));
    }

    #[test]
    fn nat_keeps_vms_apart() {
        let rules: Vec<String> = nat_rules()
            .into_iter()
            .map(|(t, c, s)| format!("{t} {c} {}", s.join(" ")))
            .collect();
        assert!(rules.contains(&"filter FORWARD -i cove+ -o cove+ -j DROP".to_string()));
        assert!(rules.contains(&"filter FORWARD -i cove+ -d 169.254.0.0/16 -j DROP".to_string()));
        assert!(rules
            .contains(&"nat POSTROUTING -s 10.177.0.0/16 ! -o cove+ -j MASQUERADE".to_string()));
        // Drops come before the catch-all accept.
        let drop = rules
            .iter()
            .position(|r| r.contains("-o cove+ -j DROP"))
            .unwrap();
        let accept = rules
            .iter()
            .position(|r| r == "filter FORWARD -i cove+ -j ACCEPT")
            .unwrap();
        assert!(drop < accept);
    }

    #[test]
    fn capabilities() {
        let status = "Name:\tcove\nCapEff:\t0000000000001000\n";
        assert_eq!(cap_eff(status), Some(1 << 12));
        assert_eq!(cap_eff("Name: x\n"), None);
    }

    fn fake_host(dir: &Path) -> Host {
        let bin = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, "").unwrap();
            Some(p)
        };
        Host {
            kvm: dir.join("kvm"),
            cloud_hypervisor: bin("cloud-hypervisor"),
            virtiofsd: bin("virtiofsd"),
            iptables: bin("iptables"),
            firmware: None,
            net_admin: true,
        }
    }

    #[test]
    fn doctor_checks() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = fake_host(dir.path());
        let by = |checks: &[Check], label: &str| {
            let c = checks.iter().find(|c| c.label == label).unwrap();
            (c.ok, c.detail.clone())
        };
        // No /dev/kvm: say what to do about it.
        let checks = checks(&host);
        let (ok, detail) = by(&checks, "kvm");
        assert!(!ok);
        assert!(detail.contains("nested virtualization"), "{detail}");
        // A usable one passes; the rest pass with their tools present.
        std::fs::write(&host.kvm, "").unwrap();
        let all = super::checks(&host);
        assert!(
            all.iter().all(|c| c.ok),
            "{:?}",
            all.iter().map(|c| &c.detail).collect::<Vec<_>>()
        );
        assert!(by(&all, "firmware")
            .1
            .contains("downloaded on the first start"));
        // Missing tools and privileges are named with their fix.
        host.cloud_hypervisor = None;
        host.virtiofsd = None;
        host.net_admin = false;
        let all = super::checks(&host);
        assert!(by(&all, "cloud-hypervisor")
            .1
            .contains("cloud-hypervisor-static"));
        assert!(by(&all, "virtiofsd").1.contains("apt install virtiofsd"));
        assert!(by(&all, "network").1.contains("CAP_NET_ADMIN"));
    }

    #[test]
    fn specs_this_host_cant_run_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = fake_host(dir.path());
        let mut spec = record(Network::Full, vec![mount("/srv", "/work", false)]).spec;
        assert!(check_spec(&spec, &host).is_ok());
        host.virtiofsd = None;
        assert!(check_spec(&spec, &host)
            .unwrap_err()
            .to_string()
            .contains("virtiofsd"));
        spec.mounts.clear();
        host.net_admin = false;
        assert!(check_spec(&spec, &host)
            .unwrap_err()
            .to_string()
            .contains("--network off"));
        spec.network = Network::Off;
        assert!(check_spec(&spec, &host).is_ok());
    }
}
