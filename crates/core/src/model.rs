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
    /// Whether the VM gets a network device. Records saved before this existed load
    /// as `full`.
    #[serde(default)]
    pub network: Network,
    /// Stop the VM after this many minutes without exec, stream or named-process
    /// activity. `None` keeps it running until stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_minutes: Option<u64>,
    /// Where a VM may connect through the egress proxy: `example.com`,
    /// `*.example.com` (its subdomains), `*` (anywhere) or a preset such as `@dev`.
    /// Enforced when `network` is `allowlist`; the hosts of `secrets` are always
    /// allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Secrets the egress proxy adds to requests, so their values never enter the VM.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<Secret>,
}

/// A secret kept on the host. Inside the VM, the variable `name` holds a placeholder;
/// the egress proxy swaps it for `value` in the headers of HTTPS requests to `hosts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    pub value: String,
    /// Where the secret may be sent. Empty uses the usual hosts for well-known names
    /// (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GH_TOKEN`…).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// What the VM sees instead of the value. Made by the server.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placeholder: String,
}

/// A secret as the API reports it: never its value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub hosts: Vec<String>,
}

impl VmSpec {
    /// Whether the VM's traffic goes through the egress proxy.
    pub fn egress(&self) -> bool {
        self.network == Network::Allowlist || !self.secrets.is_empty()
    }
}

/// A VM's network policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// NAT to the host's network and the internet.
    #[default]
    Full,
    /// No network device at all. The guest agent still works over vsock.
    Off,
    /// No network device either: programs reach only the hosts on the VM's
    /// allowlist, through the egress proxy on the host.
    Allowlist,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::Full => "full",
            Network::Off => "off",
            Network::Allowlist => "allowlist",
        }
    }

    /// Whether the VM gets a network device of its own.
    pub fn device(self) -> bool {
        self == Network::Full
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Network {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Network::Full),
            "off" => Ok(Network::Off),
            "allowlist" => Ok(Network::Allowlist),
            other => Err(format!(
                "unknown network policy {other:?} (use full, off or allowlist)"
            )),
        }
    }
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
    #[serde(default)]
    pub network: Network,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretInfo>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_records_load_with_full_network() {
        let old = r#"{"name":"a","image":"debian-13","cpus":2,"memory_mb":4096,"disk_gb":20,
            "mounts":[],"ephemeral":false,"tags":[],"backend":"vfkit",
            "created_at":1,"instance_id":"a-1"}"#;
        let record: VmRecord = serde_json::from_str(old).unwrap();
        assert_eq!(record.spec.network, Network::Full);
        assert_eq!(record.spec.idle_stop_minutes, None);
    }

    #[test]
    fn network_policy_text() {
        assert_eq!("off".parse::<Network>(), Ok(Network::Off));
        assert!("none".parse::<Network>().is_err());
        assert_eq!(serde_json::to_string(&Network::Full).unwrap(), r#""full""#);
        let spec: VmSpec =
            serde_json::from_str(r#"{"name":"a","network":"off","idle_stop_minutes":15}"#).unwrap();
        assert_eq!(spec.network, Network::Off);
        assert_eq!(spec.idle_stop_minutes, Some(15));
    }
}
