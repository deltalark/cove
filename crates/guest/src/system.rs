//! Small guest-side operations: files, ports, mounts, power.

use cove_proto::{DirEntry, ListeningPort};
use std::path::Path;

/// Files bigger than this go through `exec` (e.g. `tar`) instead.
const MAX_FILE: u64 = 64 * 1024 * 1024;

pub fn ping() -> serde_json::Value {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": cove_proto::PROTOCOL_VERSION,
        "hostname": hostname,
    })
}

pub fn read_file(path: &str) -> Result<cove_proto::Bytes, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if meta.len() > MAX_FILE {
        return Err(format!(
            "{path} is larger than {} MB",
            MAX_FILE / 1024 / 1024
        ));
    }
    std::fs::read(path)
        .map(Into::into)
        .map_err(|e| format!("{path}: {e}"))
}

pub fn write_file(path: &str, data: &[u8], mode: Option<u32>) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, data).map_err(|e| format!("{path}: {e}"))?;
    if let Some(mode) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(true)
}

pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)
        .map_err(|e| format!("{path}: {e}"))?
        .flatten()
    {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: kind.into(),
            size: if meta.is_file() { meta.len() } else { 0 },
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// TCP ports in LISTEN state, from `/proc/net/tcp` and `tcp6`.
pub fn listening_ports() -> Vec<ListeningPort> {
    let mut ports: Vec<ListeningPort> = Vec::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for found in parse_proc_net_tcp(&text, v6) {
            if !ports.iter().any(|p| p.port == found.port) {
                ports.push(found);
            }
        }
    }
    ports.sort_by_key(|p| p.port);
    ports
}

fn parse_proc_net_tcp(text: &str, v6: bool) -> Vec<ListeningPort> {
    let mut out = Vec::new();
    for row in text.lines().skip(1) {
        let cols: Vec<&str> = row.split_whitespace().collect();
        // sl local_address rem_address st ...; 0A is LISTEN.
        if cols.len() < 4 || cols[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = cols[1].split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        let address = if v6 {
            if addr.chars().all(|c| c == '0') {
                "::".into()
            } else {
                format!("[{addr}]")
            }
        } else {
            let Ok(raw) = u32::from_str_radix(addr, 16) else {
                continue;
            };
            std::net::Ipv4Addr::from(raw.to_le_bytes()).to_string()
        };
        out.push(ListeningPort { port, address });
    }
    out
}

pub fn mount(tag: &str, path: &str, read_only: bool) -> Result<bool, String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))?;
    let options = if read_only { "ro" } else { "rw" };
    let status = std::process::Command::new("mount")
        .args(["-t", "virtiofs", "-o", options, tag, path])
        .status()
        .map_err(|e| format!("mount: {e}"))?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!("mount -t virtiofs {tag} {path} failed ({status})"))
    }
}

pub fn power_off() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let ok = std::process::Command::new("systemctl")
            .arg("poweroff")
            .status()
            .map(|s| s.success());
        if !matches!(ok, Ok(true)) {
            let _ = std::process::Command::new("poweroff").status();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listening_sockets() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1\n\
   1: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1\n\
   2: 0100007F:0BB8 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 3 1\n";
        let ports = parse_proc_net_tcp(tcp, false);
        assert_eq!(
            ports,
            vec![
                ListeningPort {
                    port: 5173,
                    address: "0.0.0.0".into()
                },
                ListeningPort {
                    port: 3000,
                    address: "127.0.0.1".into()
                },
            ]
        );
    }
}
