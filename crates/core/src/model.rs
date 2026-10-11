use serde::{Deserialize, Serialize};

/// A host folder shared into the VM with virtio-fs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mount {
    pub host: String,
    /// Where it appears in the guest.
    pub guest: String,
    #[serde(default)]
    pub read_only: bool,
}

/// What a client asks for when creating a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default = "default_disk")]
    pub disk_gb: u64,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Deleted when stopped.
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// `vfkit` (macOS), `cloud-hypervisor` (Linux) or `host` (development only: no
    /// isolation at all). Empty picks the platform default.
    #[serde(default)]
    pub backend: String,
}

pub fn default_image() -> String {
    "debian-13".into()
}
fn default_cpus() -> u32 {
    2
}
fn default_memory() -> u64 {
    4096
}
fn default_disk() -> u64 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    /// `spec.backend` holds the resolved backend.
    #[serde(flatten)]
    pub spec: VmSpec,
    pub created_at: u64,
    /// Changes on every disk clone, so cloud-init treats a copied VM as new.
    pub instance_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

/// A VM as the API reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmStatus {
    pub name: String,
    pub state: VmState,
    pub image: String,
    pub backend: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub mounts: Vec<Mount>,
    pub ephemeral: bool,
    pub tags: Vec<String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Ports forwarded to the host: guest port -> host URL.
    #[serde(default)]
    pub forwards: Vec<Forward>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Forward {
    pub guest_port: u16,
    pub host_port: u16,
    pub url: String,
}
