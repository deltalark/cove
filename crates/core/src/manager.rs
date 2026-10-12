//! The set of VMs on this machine and their lifecycle.

use crate::backend::{self, Launch};
use crate::guest::GuestAddr;
use crate::model::{Forward, Network, VmRecord, VmSpec, VmState, VmStatus};
use crate::paths::Paths;
use crate::{image, seed};
use anyhow::{anyhow, bail, Context, Result};
use cove_proto::Request;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// The first boot runs cloud-init and grows the disk, so allow plenty of time.
const BOOT_TIMEOUT: Duration = Duration::from_secs(240);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Live {
    state: Option<VmState>,
    error: Option<String>,
    forwards: Vec<(Forward, tokio::task::JoinHandle<()>)>,
}

/// How recently a VM was used, for idle stop.
struct Activity {
    last: Instant,
    /// Exec, spawn and attach streams open right now. An open stream is activity
    /// however quiet it is (a shell someone is reading).
    streams: usize,
}

/// Whether a VM with an idle timer of `idle` should be stopped: nothing has used it
/// for that long, no stream is open, and no named process is running.
pub fn idle_expired(
    idle: Duration,
    since_activity: Duration,
    open_streams: usize,
    running_procs: usize,
) -> bool {
    open_streams == 0 && running_procs == 0 && since_activity >= idle
}

pub struct Manager {
    pub paths: Paths,
    live: Mutex<HashMap<String, Live>>,
    /// A plain mutex: touched on every guest request, never held across an await.
    activity: std::sync::Mutex<HashMap<String, Activity>>,
    /// Serialises lifecycle changes per VM.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Held from picking a network slot until the VM holding it is recorded.
    net_lock: Mutex<()>,
    /// Host name used in forwarded-port URLs.
    pub url_host: String,
}

impl Manager {
    pub fn new(paths: Paths) -> Result<Arc<Manager>> {
        paths.ensure()?;
        Ok(Arc::new(Manager {
            paths,
            live: Mutex::new(HashMap::new()),
            activity: std::sync::Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            net_lock: Mutex::new(()),
            url_host: "127.0.0.1".into(),
        }))
    }

    async fn lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    fn record(&self, name: &str) -> Result<VmRecord> {
        let path = self.paths.vm_dir(name).join("vm.json");
        let text = std::fs::read_to_string(&path).map_err(|_| anyhow!("no VM named {name:?}"))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn save_record(&self, record: &VmRecord) -> Result<()> {
        let dir = self.paths.vm_dir(&record.spec.name);
        std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(record)?)?;
        Ok(())
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.paths.vms())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().join("vm.json").is_file())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    pub async fn list(&self) -> Vec<VmStatus> {
        let mut out = Vec::new();
        for name in self.names() {
            if let Ok(s) = self.status(&name).await {
                out.push(s);
            }
        }
        out
    }

    pub async fn status(&self, name: &str) -> Result<VmStatus> {
        let record = self.record(name)?;
        let dir = self.paths.vm_dir(name);
        let running = Launch::load(&dir).is_some_and(|l| l.alive());
        let live = self.live.lock().await;
        let entry = live.get(name);
        let state = match entry.and_then(|l| l.state) {
            Some(VmState::Starting) => VmState::Starting,
            Some(VmState::Stopping) => VmState::Stopping,
            _ if running => VmState::Running,
            _ => VmState::Stopped,
        };
        Ok(VmStatus {
            name: record.spec.name.clone(),
            state,
            image: record.spec.image.clone(),
            backend: record.spec.backend.clone(),
            cpus: record.spec.cpus,
            memory_mb: record.spec.memory_mb,
            disk_gb: record.spec.disk_gb,
            mounts: record.spec.mounts.clone(),
            ephemeral: record.spec.ephemeral,
            tags: record.spec.tags.clone(),
            network: record.spec.network,
            idle_stop_minutes: record.spec.idle_stop_minutes,
            created_at: record.created_at,
            error: entry.and_then(|l| l.error.clone()),
            forwards: entry
                .map(|l| l.forwards.iter().map(|f| f.0.clone()).collect())
                .unwrap_or_default(),
        })
    }

    async fn set_state(&self, name: &str, state: Option<VmState>, error: Option<String>) {
        let mut live = self.live.lock().await;
        let entry = live.entry(name.to_string()).or_default();
        entry.state = state;
        entry.error = error;
    }

    /// Network slots held by running VMs.
    fn net_slots_in_use(&self) -> Vec<u32> {
        self.names()
            .iter()
            .filter_map(|n| Launch::load(&self.paths.vm_dir(n)))
            .filter(|l| l.alive())
            .filter_map(|l| l.net)
            .collect()
    }

    /// The guest agent of a running VM. Counts as activity for idle stop; the idle
    /// check itself uses [`Manager::running_guest`], which doesn't.
    pub fn guest(&self, name: &str) -> Result<GuestAddr> {
        let guest = self.running_guest(name)?;
        self.touch(name);
        Ok(guest)
    }

    fn running_guest(&self, name: &str) -> Result<GuestAddr> {
        self.record(name)?;
        match Launch::load(&self.paths.vm_dir(name)) {
            Some(l) if l.alive() => Ok(l.guest),
            _ => bail!("VM {name:?} is not running"),
        }
    }

    /// Record that `name` was just used.
    pub fn touch(&self, name: &str) {
        let mut activity = self.activity.lock().unwrap();
        activity
            .entry(name.to_string())
            .and_modify(|a| a.last = Instant::now())
            .or_insert(Activity {
                last: Instant::now(),
                streams: 0,
            });
    }

    /// Mark a stream to `name` open until the guard is dropped.
    pub fn stream_opened(self: &Arc<Self>, name: &str) -> StreamGuard {
        self.touch(name);
        if let Some(a) = self.activity.lock().unwrap().get_mut(name) {
            a.streams += 1;
        }
        StreamGuard {
            manager: Arc::downgrade(self),
            name: name.to_string(),
        }
    }

    fn stream_closed(&self, name: &str) {
        if let Some(a) = self.activity.lock().unwrap().get_mut(name) {
            a.streams = a.streams.saturating_sub(1);
            a.last = Instant::now();
        }
    }

    /// Stop running VMs whose idle timer ran out; returns their names. `minute` is
    /// how long a minute of `idle_stop_minutes` lasts (shorter in tests).
    pub async fn stop_idle(self: &Arc<Self>, minute: Duration) -> Vec<String> {
        let mut stopped = Vec::new();
        for name in self.names() {
            let Ok(record) = self.record(&name) else {
                continue;
            };
            let Some(minutes) = record.spec.idle_stop_minutes else {
                continue;
            };
            let Ok(guest) = self.running_guest(&name) else {
                continue;
            };
            // A VM nobody has touched since this server started counts from now.
            let (since, streams) = {
                let mut activity = self.activity.lock().unwrap();
                let a = activity.entry(name.clone()).or_insert(Activity {
                    last: Instant::now(),
                    streams: 0,
                });
                (a.last.elapsed(), a.streams)
            };
            let idle = minute * minutes.min(u32::MAX as u64) as u32;
            if !idle_expired(idle, since, streams, 0) {
                continue;
            }
            // Only ask the guest once everything else says idle. If it doesn't
            // answer, leave the VM alone rather than stop it blind.
            let Ok(procs) = guest
                .call_as::<Vec<cove_proto::ProcInfo>>(&Request::Procs)
                .await
            else {
                continue;
            };
            let running = procs.iter().filter(|p| p.running).count();
            if idle_expired(idle, since, streams, running) && self.stop(&name).await.is_ok() {
                stopped.push(name);
            }
        }
        stopped
    }

    pub async fn create(self: &Arc<Self>, spec: VmSpec, start: bool) -> Result<VmStatus> {
        if !crate::valid_name(&spec.name) {
            bail!("VM names use lowercase letters, digits and dashes (max 40)");
        }
        if spec.idle_stop_minutes == Some(0) {
            bail!("idle stop takes at least 1 minute");
        }
        let backend = backend::resolve(&spec.backend)?;
        backend::check(backend).map_err(|e| anyhow!(e))?;
        if backend == backend::HOST
            && !spec.mounts.is_empty()
            && spec.mounts.iter().any(|m| m.host != m.guest)
        {
            bail!("the host backend can't remap folders; use the same path for host and guest");
        }
        for m in &spec.mounts {
            if !Path::new(&m.host).is_dir() {
                bail!("{} is not a folder on this machine", m.host);
            }
        }
        if backend == backend::CLOUD_HYPERVISOR {
            crate::chv::check_spec(&spec, &crate::chv::Host::detect(&self.paths))?;
        }
        if backend == backend::GVISOR {
            crate::gvisor::check_spec(&spec, &crate::chv::Host::detect(&self.paths))?;
        }
        let lock = self.lock(&spec.name).await;
        let _guard = lock.lock().await;
        let dir = self.paths.vm_dir(&spec.name);
        if dir.join("vm.json").exists() {
            bail!("a VM named {:?} already exists", spec.name);
        }
        if backend == backend::GVISOR {
            let base = image::rootfs_path(&self.paths, &spec.image);
            if !base.is_dir() {
                bail!(
                    "image {:?} is not pulled yet: cove image pull {}",
                    spec.image,
                    spec.image
                );
            }
            std::fs::create_dir_all(&dir)?;
            crate::gvisor::copy_tree(&base, &crate::gvisor::rootfs(&dir))?;
        } else if backend != backend::HOST {
            let base = image::disk_path(&self.paths, &spec.image);
            if !base.is_file() {
                bail!(
                    "image {:?} is not pulled yet: cove image pull {}",
                    spec.image,
                    spec.image
                );
            }
            std::fs::create_dir_all(&dir)?;
            clone_disk(&base, &dir.join("disk.raw"), spec.disk_gb)?;
        } else {
            std::fs::create_dir_all(&dir)?;
        }
        let mut spec = spec;
        spec.backend = backend.to_string();
        let record = VmRecord {
            created_at: crate::now(),
            instance_id: format!("{}-{}", spec.name, crate::random_hex(6)),
            spec,
        };
        self.save_record(&record)?;
        let name = record.spec.name.clone();
        drop(_guard);
        if start {
            self.start(&name).await?;
        }
        self.status(&name).await
    }

    pub async fn start(self: &Arc<Self>, name: &str) -> Result<VmStatus> {
        let lock = self.lock(name).await;
        let _guard = lock.lock().await;
        let record = self.record(name)?;
        let dir = self.paths.vm_dir(name);
        if Launch::load(&dir).is_some_and(|l| l.alive()) {
            drop(_guard);
            return self.status(name).await;
        }
        self.set_state(name, Some(VmState::Starting), None).await;
        let result = self.boot(&record, &dir).await;
        match &result {
            Ok(()) => {
                self.touch(name);
                self.set_state(name, None, None).await
            }
            Err(e) => {
                if let Some(l) = Launch::load(&dir) {
                    backend::signal(l.pid, libc::SIGKILL);
                    l.stop_helpers();
                }
                Launch::clear(&dir);
                self.set_state(name, None, Some(format!("{e:#}"))).await;
            }
        }
        drop(_guard);
        result?;
        self.status(name).await
    }

    async fn boot(&self, record: &VmRecord, dir: &Path) -> Result<()> {
        let mut opts = backend::LaunchOptions::default();
        let mut net_guard = None;
        if record.spec.backend == backend::CLOUD_HYPERVISOR {
            opts.firmware = Some(match crate::chv::firmware(&self.paths) {
                Some(f) => f,
                None => image::pull_firmware(&self.paths).await?,
            });
        }
        if record.spec.backend == backend::GVISOR {
            opts.guest_binary = Some(backend::guest_binary(&self.paths)?);
        }
        let vm = record.spec.backend == backend::CLOUD_HYPERVISOR
            || record.spec.backend == backend::VFKIT;
        let routed = record.spec.backend == backend::CLOUD_HYPERVISOR
            || record.spec.backend == backend::GVISOR;
        if routed && record.spec.network == Network::Full {
            net_guard = Some(self.net_lock.lock().await);
            opts.net = Some(crate::chv::free_slot(&self.net_slots_in_use())?);
        }
        if vm {
            let agent = backend::guest_binary(&self.paths)?;
            let network = opts.net.map(|n| n.network_config());
            seed::build(
                &dir.join("seed.img"),
                &record.instance_id,
                &record.spec.name,
                &agent,
                network.as_deref(),
            )?;
        }
        let launch = backend::launch(record, dir, &opts)?;
        drop(net_guard);
        let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
        loop {
            if !launch.alive() {
                let log = std::fs::read_to_string(dir.join("backend.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(5).collect();
                bail!(
                    "the VM exited while booting: {}",
                    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                );
            }
            if let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_secs(2), launch.guest.ping()).await
            {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "the guest agent did not answer within {}s (see {})",
                    BOOT_TIMEOUT.as_secs(),
                    dir.join("console.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // Real VMs mount their virtio-fs shares; gVisor's are bind mounts already.
        if vm {
            for (i, m) in record.spec.mounts.iter().enumerate() {
                launch
                    .guest
                    .call(&Request::Mount {
                        tag: backend::mount_tag(i),
                        path: m.guest.clone(),
                        read_only: m.read_only,
                    })
                    .await
                    .with_context(|| format!("mounting {} at {}", m.host, m.guest))?;
            }
        }
        Ok(())
    }

    pub async fn stop(self: &Arc<Self>, name: &str) -> Result<VmStatus> {
        let lock = self.lock(name).await;
        let _guard = lock.lock().await;
        let record = self.record(name)?;
        let dir = self.paths.vm_dir(name);
        self.close_forwards(name).await;
        if let Some(launch) = Launch::load(&dir) {
            if launch.alive() {
                self.set_state(name, Some(VmState::Stopping), None).await;
                if record.spec.backend == backend::GVISOR {
                    crate::gvisor::halt(name, &dir, &launch).await;
                } else if record.spec.backend != backend::HOST {
                    let asked = tokio::time::timeout(
                        Duration::from_secs(5),
                        launch.guest.call(&Request::Shutdown),
                    )
                    .await;
                    // The agent didn't answer: press the power button instead.
                    if !matches!(asked, Ok(Ok(_)))
                        && record.spec.backend == backend::CLOUD_HYPERVISOR
                    {
                        let _ = crate::chv::power_button(&dir).await;
                    }
                } else {
                    backend::signal(launch.pid, libc::SIGTERM);
                }
                let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
                while launch.alive() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if launch.alive() {
                    backend::signal(launch.pid, libc::SIGTERM);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                if launch.alive() {
                    backend::signal(launch.pid, libc::SIGKILL);
                }
            }
            launch.stop_helpers();
            Launch::clear(&dir);
        }
        self.activity.lock().unwrap().remove(name);
        self.set_state(name, None, None).await;
        drop(_guard);
        let status = self.status(name).await?;
        if record.spec.ephemeral {
            Box::pin(self.delete(name)).await?;
        }
        Ok(status)
    }

    pub async fn delete(self: &Arc<Self>, name: &str) -> Result<()> {
        self.record(name)?;
        let dir = self.paths.vm_dir(name);
        if Launch::load(&dir).is_some_and(|l| l.alive()) {
            let _ = Box::pin(self.stop_no_delete(name)).await;
        }
        let lock = self.lock(name).await;
        let _guard = lock.lock().await;
        crate::gvisor::release_mounts(&dir);
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        self.live.lock().await.remove(name);
        Ok(())
    }

    async fn stop_no_delete(self: &Arc<Self>, name: &str) -> Result<()> {
        let mut record = self.record(name)?;
        let ephemeral = record.spec.ephemeral;
        record.spec.ephemeral = false;
        self.save_record(&record)?;
        let result = self.stop(name).await;
        if let Ok(mut r) = self.record(name) {
            r.spec.ephemeral = ephemeral;
            let _ = self.save_record(&r);
        }
        result.map(|_| ())
    }

    /// Copy a stopped VM's disk and settings to a new VM.
    pub async fn copy(self: &Arc<Self>, from: &str, to: &str) -> Result<VmStatus> {
        if !crate::valid_name(to) {
            bail!("VM names use lowercase letters, digits and dashes (max 40)");
        }
        let src = self.record(from)?;
        let src_dir = self.paths.vm_dir(from);
        if Launch::load(&src_dir).is_some_and(|l| l.alive()) {
            bail!("stop {from:?} before copying it");
        }
        let dir = self.paths.vm_dir(to);
        if dir.exists() {
            bail!("a VM named {to:?} already exists");
        }
        std::fs::create_dir_all(&dir)?;
        if src_dir.join("disk.raw").is_file() {
            std::fs::copy(src_dir.join("disk.raw"), dir.join("disk.raw"))?;
        }
        let rootfs = crate::gvisor::rootfs(&src_dir);
        if rootfs.is_dir() {
            if let Err(e) = crate::gvisor::copy_tree(&rootfs, &crate::gvisor::rootfs(&dir)) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        }
        let mut record = src.clone();
        record.spec.name = to.to_string();
        record.created_at = crate::now();
        record.instance_id = format!("{to}-{}", crate::random_hex(6));
        self.save_record(&record)?;
        self.status(to).await
    }

    pub async fn ports(&self, name: &str) -> Result<Vec<cove_proto::ListeningPort>> {
        self.guest(name)?.call_as(&Request::Ports).await
    }

    /// Forward a guest port to a host port on 127.0.0.1, reusing an existing forward.
    pub async fn forward(self: &Arc<Self>, name: &str, guest_port: u16) -> Result<Forward> {
        let guest = self.guest(name)?;
        let mut live = self.live.lock().await;
        let entry = live.entry(name.to_string()).or_default();
        if let Some((f, _)) = entry
            .forwards
            .iter()
            .find(|(f, _)| f.guest_port == guest_port)
        {
            return Ok(f.clone());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let host_port = listener.local_addr()?.port();
        let manager = Arc::downgrade(self);
        let vm = name.to_string();
        let task = tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                // Someone looking at a preview is using the VM.
                if let Some(m) = manager.upgrade() {
                    m.touch(&vm);
                }
                let guest = guest.clone();
                tokio::spawn(async move {
                    let _ = conn.set_nodelay(true);
                    if let Ok(mut upstream) = open_tcp(&guest, guest_port).await {
                        let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
                    }
                });
            }
        });
        let forward = Forward {
            guest_port,
            host_port,
            url: format!("http://{}:{host_port}", self.url_host),
        };
        entry.forwards.push((forward.clone(), task));
        Ok(forward)
    }

    pub async fn unforward(&self, name: &str, guest_port: u16) {
        if let Some(entry) = self.live.lock().await.get_mut(name) {
            entry.forwards.retain(|(f, task)| {
                if f.guest_port == guest_port {
                    task.abort();
                    false
                } else {
                    true
                }
            });
        }
    }

    async fn close_forwards(&self, name: &str) {
        if let Some(entry) = self.live.lock().await.get_mut(name) {
            for (_, task) in entry.forwards.drain(..) {
                task.abort();
            }
        }
    }
}

/// Keeps a stream counted as open; see [`Manager::stream_opened`].
pub struct StreamGuard {
    manager: Weak<Manager>,
    name: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(m) = self.manager.upgrade() {
            m.stream_closed(&self.name);
        }
    }
}

/// Open a raw TCP pipe to a guest port.
pub async fn open_tcp(guest: &GuestAddr, port: u16) -> Result<crate::guest::GuestStream> {
    let mut s = guest.open(&Request::Tcp { port }).await?;
    // Read the reply line byte by byte so no piped data is swallowed.
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if s.read(&mut byte).await? == 0 {
            bail!("guest closed the connection");
        }
        if byte[0] == b'\n' {
            break;
        }
        reply.push(byte[0]);
    }
    match serde_json::from_slice::<cove_proto::Frame>(&reply)? {
        cove_proto::Frame::Ok { .. } => {
            s.flush().await?;
            Ok(s)
        }
        cove_proto::Frame::Error { message } => bail!(message),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Clone the base disk (APFS clone on macOS, reflink on btrfs/XFS via
/// `copy_file_range`, a plain copy elsewhere) and grow it to `disk_gb`.
fn clone_disk(base: &Path, target: &Path, disk_gb: u64) -> Result<()> {
    std::fs::copy(base, target).with_context(|| format!("copying {}", base.display()))?;
    crate::paths::restrict(target, 0o600);
    let want = disk_gb * 1024 * 1024 * 1024;
    let file = std::fs::OpenOptions::new().write(true).open(target)?;
    if file.metadata()?.len() < want {
        file.set_len(want)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn idle_needs_quiet_streams_and_procs() {
        assert!(idle_expired(10 * MIN, 10 * MIN, 0, 0));
        assert!(idle_expired(10 * MIN, 11 * MIN, 0, 0));
        assert!(!idle_expired(10 * MIN, 9 * MIN, 0, 0));
        // An open shell or a running agent keeps the VM up however long it's quiet.
        assert!(!idle_expired(10 * MIN, 60 * MIN, 1, 0));
        assert!(!idle_expired(10 * MIN, 60 * MIN, 0, 1));
    }

    #[test]
    fn streams_count_until_dropped() {
        let home = tempfile::tempdir().unwrap();
        let m = Manager::new(Paths::at(home.path())).unwrap();
        let guard = m.stream_opened("a");
        let second = m.stream_opened("a");
        assert_eq!(m.activity.lock().unwrap()["a"].streams, 2);
        drop(guard);
        drop(second);
        assert_eq!(m.activity.lock().unwrap()["a"].streams, 0);
        assert!(m.activity.lock().unwrap()["a"].last.elapsed() < MIN);
    }
}
