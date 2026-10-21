//! The VM side of cove: where things live on disk, images, disks, cloud-init
//! seeds, backends that boot VMs, and the client that talks to the guest agent.

pub mod backend;
pub mod chv;
pub mod disk;
pub mod egress;
pub mod guest;
pub mod gvisor;
pub mod https;
pub mod image;
pub mod keys;
pub mod logins;
pub mod manager;
pub mod model;
pub mod paths;
pub mod seed;
pub mod settings;
pub mod snapshot;

pub use manager::Manager;
pub use model::{Mount, Network, VmRecord, VmSpec, VmState, VmStatus};

/// Random lowercase hex, for ids and API keys.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("the OS random source is available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// VM names become hostnames and URL labels, so keep them DNS-safe.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// How many seconds a minute of the idle timers and timeouts lasts: 60, unless
/// `COVE_IDLE_MINUTE_SECS` shortens it so tests don't wait for real minutes.
pub fn minute_secs() -> u64 {
    std::env::var("COVE_IDLE_MINUTE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(60)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        assert!(super::valid_name("my-app-2"));
        assert!(!super::valid_name("My_App"));
        assert!(!super::valid_name("-x"));
        assert!(!super::valid_name(""));
    }
}
