//! The set of VMs on this machine and their lifecycle.

use crate::backend::{self, Launch};
use crate::guest::GuestAddr;
use crate::model::{Forward, VmRecord, VmSpec, VmState, VmStatus};
use crate::paths::Paths;
use crate::{image, seed};
use anyhow::{anyhow, bail, Context, Result};
use cove_proto::Request;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
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

pub struct Manager {
    pub paths: Paths,
    live: Mutex<HashMap<String, Live>>,
    /// Serialises lifecycle changes per VM.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Host name used in forwarded-port URLs.
    pub url_host: String,
}

impl Manager {
    pub fn new(paths: Paths) -> Result<Arc<Manager>> {
        paths.ensure()?;
        Ok(Arc::new(Manager {
            paths,
            live: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
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

    /// The guest agent of a running VM.
    pub fn guest(&self, name: &str) -> Result<GuestAddr> {
        self.record(name)?;
        match Launch::load(&self.paths.vm_dir(name)) {
            Some(l) if l.alive() => Ok(l.guest),
            _ => bail!("VM {name:?} is not running"),
        }
    }

    pub async fn create(self: &Arc<Self>, spec: VmSpec, start: bool) -> Result<VmStatus> {
        if !crate::valid_name(&spec.name) {
            bail!("VM names use lowercase letters, digits and dashes (max 40)");
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
        let lock = self.lock(&spec.name).await;
        let _guard = lock.lock().await;
        let dir = self.paths.vm_dir(&spec.name);
        if dir.join("vm.json").exists() {
            bail!("a VM named {:?} already exists", spec.name);
        }
        if backend != backend::HOST {
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
            Ok(()) => self.set_state(name, None, None).await,
            Err(e) => {
                if let Some(l) = Launch::load(&dir) {
                    backend::signal(l.pid, libc::SIGKILL);
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
        if record.spec.backend != backend::HOST {
            let agent = backend::guest_binary(&self.paths)?;
            seed::build(
                &dir.join("seed.img"),
                &record.instance_id,
                &record.spec.name,
                &agent,
            )?;
        }
        let launch = backend::launch(record, dir)?;
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
        if record.spec.backend != backend::HOST {
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
                if record.spec.backend != backend::HOST {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        launch.guest.call(&Request::Shutdown),
                    )
                    .await;
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
            Launch::clear(&dir);
        }
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
        let task = tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
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
