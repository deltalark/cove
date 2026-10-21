use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Pause the VM (memory and processes kept) after this many minutes without
    /// activity. Using it again resumes it. Checked before `idle_stop_minutes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_pause_minutes: Option<u64>,
    /// `key=value` metadata, for finding VMs later (`?label=task=review`). Not for
    /// secrets: it is stored and shown in plain text.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The snapshot to start from instead of `image`. Kept on the record, so a VM
    /// says where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Pause or delete the VM at a set time. Used once: it is cleared when it fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Timeout>,
    /// Keep the agents' logins saved on this server (`cove agents`) in step with the
    /// VM: it gets them when it starts and hands refreshed ones back.
    #[serde(default = "yes")]
    pub logins: bool,
}

fn yes() -> bool {
    true
}

/// A one-off deadline for a VM.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timeout {
    /// Unix time in seconds.
    pub at: u64,
    #[serde(default)]
    pub action: TimeoutAction,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TimeoutAction {
    /// Keep its memory and processes, like an idle pause; it waits for `resume`.
    #[default]
    Pause,
    /// Delete the VM and its disk.
    Delete,
}

impl std::str::FromStr for TimeoutAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pause" | "suspend" => Ok(TimeoutAction::Pause),
            "delete" | "kill" => Ok(TimeoutAction::Delete),
            other => Err(format!(
                "unknown timeout action {other:?} (use pause or delete)"
            )),
        }
    }
}

/// Why a VM is paused. Only an idle pause wakes up by itself when used.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PauseReason {
    /// Someone asked (`cove pause`): it waits for `resume`.
    User,
    /// Its auto-pause timer: exec, streams, files and port visits resume it.
    Idle,
    /// Its timeout fired: it waits for `resume`.
    Timeout,
}

/// A change to a VM's settings (`PATCH /v1/vms/{name}`). Fields left out stay as
/// they are; `null` clears a timer or the timeout.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmUpdate {
    /// Replaces every label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_pause_minutes: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_stop_minutes: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Option<TimeoutRequest>>,
}

/// "In this many minutes, do this."
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeoutRequest {
    pub minutes: u64,
    #[serde(default)]
    pub action: TimeoutAction,
}

/// The longest timeout, as with Instabox: 24 hours.
pub const MAX_TIMEOUT_MINUTES: u64 = 24 * 60;

impl TimeoutRequest {
    /// When it fires, counting `minute` seconds to the minute (60, but shorter in
    /// tests: see [`crate::minute_secs`]).
    pub fn resolve(self, now: u64, minute: u64) -> Result<Timeout, String> {
        if self.minutes == 0 || self.minutes > MAX_TIMEOUT_MINUTES {
            return Err(format!(
                "a timeout is 1 minute to 24 hours ({} minutes); null clears it",
                MAX_TIMEOUT_MINUTES
            ));
        }
        Ok(Timeout {
            at: now + self.minutes * minute,
            action: self.action,
        })
    }
}

/// Tells a field that is present (even as `null`) from one that is left out.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// The most labels a VM may have, as with Instabox.
pub const MAX_LABELS: usize = 32;

/// Check a VM's labels: keys of letters, digits and `-_./`, short values.
pub fn check_labels(labels: &BTreeMap<String, String>) -> Result<(), String> {
    if labels.len() > MAX_LABELS {
        return Err(format!("a VM has at most {MAX_LABELS} labels"));
    }
    for (k, v) in labels {
        let key_ok = !k.is_empty()
            && k.len() <= 63
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
        if !key_ok {
            return Err(format!(
                "label keys use letters, digits and - _ . / (max 63): {k:?}"
            ));
        }
        if v.len() > 256 || v.chars().any(|c| c.is_control()) {
            return Err(format!("label {k} is too long or has control characters"));
        }
    }
    Ok(())
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

impl Default for VmSpec {
    fn default() -> Self {
        VmSpec {
            name: String::new(),
            image: default_image(),
            cpus: default_cpus(),
            memory_mb: default_memory(),
            disk_gb: default_disk(),
            mounts: Vec::new(),
            ephemeral: false,
            tags: Vec::new(),
            backend: String::new(),
            network: Network::Full,
            idle_stop_minutes: None,
            allow: Vec::new(),
            secrets: Vec::new(),
            auto_pause_minutes: None,
            labels: BTreeMap::new(),
            snapshot: None,
            timeout: None,
            logins: true,
        }
    }
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
    /// Running, but frozen: memory and processes are kept, it uses no CPU.
    Paused,
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
    pub auto_pause_minutes: Option<u64>,
    /// Set while `state` is `paused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_by: Option<PauseReason>,
    /// Paused with its memory written to disk: it holds no memory or CPU at all.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused_to_disk: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Timeout>,
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
    fn labels_are_checked() {
        let mut labels = BTreeMap::new();
        labels.insert("task".to_string(), "review".to_string());
        labels.insert("runo/session".to_string(), "abc-1".to_string());
        assert!(check_labels(&labels).is_ok());
        labels.insert("bad key".to_string(), "x".to_string());
        assert!(check_labels(&labels).is_err());
        let many: BTreeMap<String, String> =
            (0..33).map(|i| (format!("k{i}"), String::new())).collect();
        assert!(check_labels(&many).is_err());
    }

    #[test]
    fn new_fields_round_trip() {
        let spec: VmSpec = serde_json::from_str(
            r#"{"name":"a","auto_pause_minutes":30,"labels":{"task":"review"},
                "snapshot":"base","timeout":{"at":100,"action":"delete"}}"#,
        )
        .unwrap();
        assert_eq!(spec.auto_pause_minutes, Some(30));
        assert_eq!(spec.labels["task"], "review");
        assert_eq!(spec.snapshot.as_deref(), Some("base"));
        assert_eq!(spec.timeout.unwrap().action, TimeoutAction::Delete);
        assert_eq!("kill".parse::<TimeoutAction>(), Ok(TimeoutAction::Delete));
        assert_eq!(
            serde_json::to_string(&VmState::Paused).unwrap(),
            r#""paused""#
        );
    }

    #[test]
    fn updates_tell_null_from_missing() {
        let u: VmUpdate = serde_json::from_str(r#"{"auto_pause_minutes":null}"#).unwrap();
        assert_eq!(u.auto_pause_minutes, Some(None));
        assert_eq!(u.idle_stop_minutes, None);
        let u: VmUpdate =
            serde_json::from_str(r#"{"timeout":{"minutes":30,"action":"delete"}}"#).unwrap();
        let t = u.timeout.unwrap().unwrap().resolve(1000, 60).unwrap();
        assert_eq!((t.at, t.action), (1000 + 1800, TimeoutAction::Delete));
        assert!(TimeoutRequest {
            minutes: 0,
            action: TimeoutAction::Pause
        }
        .resolve(0, 60)
        .is_err());
        assert!(serde_json::from_str::<VmUpdate>(r#"{"cpus":4}"#).is_err());
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
