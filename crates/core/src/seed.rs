//! The cloud-init seed disk: a small FAT image labelled CIDATA (cloud-init's NoCloud
//! source) that carries the guest agent binary and the boot script that installs it.
//!
//! It is rebuilt on every start, so a VM always runs the agent that matches the host.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

const SEED_SIZE: u64 = 64 * 1024 * 1024;

/// Runs early on every boot (cloud-init `bootcmd`): load vsock, install the agent from
/// the seed disk, start it. Nothing here needs the network.
const BOOT_SCRIPT: &str = r#"set -e
modprobe vmw_vsock_virtio_transport 2>/dev/null || modprobe virtio_vsock 2>/dev/null || true
mkdir -p /run/cove-seed
mount -o ro -L CIDATA /run/cove-seed 2>/dev/null || mount -o ro /dev/disk/by-label/CIDATA /run/cove-seed
install -m 0755 /run/cove-seed/cove-guest /usr/local/bin/cove-guest
umount /run/cove-seed || true
cat > /etc/systemd/system/cove-guest.service <<'UNIT'
[Unit]
Description=cove guest agent
After=local-fs.target

[Service]
ExecStart=/usr/local/bin/cove-guest --listen vsock:1024
Restart=always
RestartSec=1
KillMode=process

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable cove-guest.service
systemctl start --no-block cove-guest.service
"#;

fn user_data() -> String {
    let mut script = String::new();
    for l in BOOT_SCRIPT.lines() {
        script.push_str("      ");
        script.push_str(l);
        script.push('\n');
    }
    format!(
        "#cloud-config\n\
         bootcmd:\n  - |\n{script}\
         growpart:\n  mode: auto\n  devices: ['/']\n\
         resize_rootfs: true\n\
         ssh_pwauth: false\n\
         disable_root: false\n"
    )
}

fn meta_data(instance_id: &str, hostname: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n")
}

/// `network` is a cloud-init `network-config`, for backends without DHCP
/// (cloud-hypervisor); without one the image's default (DHCP) applies.
pub fn build(
    path: &Path,
    instance_id: &str,
    hostname: &str,
    guest_binary: &Path,
    network: Option<&str>,
) -> Result<()> {
    let binary = std::fs::read(guest_binary)
        .with_context(|| format!("reading the guest agent at {}", guest_binary.display()))?;
    let tmp = path.with_extension("img.partial");
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.set_len(SEED_SIZE)?;
        fatfs::format_volume(
            &file,
            fatfs::FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
        )
        .context("formatting the seed disk")?;
        let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new())?;
        {
            let root = fs.root_dir();
            let mut files = vec![
                ("meta-data", meta_data(instance_id, hostname).into_bytes()),
                ("user-data", user_data().into_bytes()),
                ("cove-guest", binary),
            ];
            if let Some(n) = network {
                files.push(("network-config", n.as_bytes().to_vec()));
            }
            for (name, data) in files {
                let mut f = root.create_file(name)?;
                f.truncate()?;
                f.write_all(&data)?;
            }
        }
        fs.unmount()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn seed_has_cloud_init_files() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("guest");
        std::fs::write(&bin, b"\x7fELF fake").unwrap();
        let img = dir.path().join("seed.img");
        build(&img, "id-1", "box", &bin, None).unwrap();

        let file = std::fs::File::open(&img).unwrap();
        let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label().trim(), "CIDATA");
        let mut text = String::new();
        fs.root_dir()
            .open_file("user-data")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.starts_with("#cloud-config\nbootcmd:\n  - |\n      set -e\n"));
        assert!(text.contains("ExecStart=/usr/local/bin/cove-guest --listen vsock:1024"));
        let mut meta = String::new();
        fs.root_dir()
            .open_file("meta-data")
            .unwrap()
            .read_to_string(&mut meta)
            .unwrap();
        assert_eq!(meta, "instance-id: id-1\nlocal-hostname: box\n");
        let mut agent = Vec::new();
        fs.root_dir()
            .open_file("cove-guest")
            .unwrap()
            .read_to_end(&mut agent)
            .unwrap();
        assert_eq!(agent, b"\x7fELF fake");
        assert!(fs.root_dir().open_file("network-config").is_err());
    }

    #[test]
    fn seed_can_carry_a_network_config() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("guest");
        std::fs::write(&bin, b"x").unwrap();
        let img = dir.path().join("seed.img");
        let config = crate::chv::NetSlot(0).network_config();
        build(&img, "id-1", "box", &bin, Some(&config)).unwrap();
        let file = std::fs::File::open(&img).unwrap();
        let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).unwrap();
        let mut text = String::new();
        fs.root_dir()
            .open_file("network-config")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, config);
    }
}
