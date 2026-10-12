//! Where cove keeps its files. Everything lives under one folder:
//! `$COVE_HOME`, else `~/.cove`.
//!
//! ```text
//! api-key                 bearer token for the HTTP API (mode 600)
//! images/<image>/disk.raw base disks, read-only once pulled
//! vms/<name>/vm.json      the VM's record
//! vms/<name>/disk.raw     its own disk (a clone of the base)
//! vms/<name>/seed.img     cloud-init seed, rebuilt on every start
//! vms/<name>/run.json     how to reach the running VM (pid, guest address)
//! vms/<name>/console.log  serial console
//! guest/cove-guest-<arch>  guest agent binaries copied into VMs
//! firmware/CLOUDHV*.fd    cloud-hypervisor's EFI firmware (Linux)
//! ```

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let root = std::env::var_os("COVE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "/".into());
                home.join(".cove")
            });
        Paths { root }
    }

    pub fn at(root: impl Into<PathBuf>) -> Paths {
        Paths { root: root.into() }
    }

    pub fn api_key(&self) -> PathBuf {
        self.root.join("api-key")
    }
    pub fn server_file(&self) -> PathBuf {
        self.root.join("server.json")
    }
    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }
    pub fn image_dir(&self, image: &str) -> PathBuf {
        self.images().join(image)
    }
    pub fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.vms().join(name)
    }
    pub fn guest_dir(&self) -> PathBuf {
        self.root.join("guest")
    }
    pub fn firmware_dir(&self) -> PathBuf {
        self.root.join("firmware")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [
            self.root.clone(),
            self.images(),
            self.vms(),
            self.guest_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        restrict(&self.root, 0o700);
        Ok(())
    }

    /// The API key, created on first use.
    pub fn load_or_create_api_key(&self) -> std::io::Result<String> {
        let path = self.api_key();
        if let Ok(key) = std::fs::read_to_string(&path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(key);
            }
        }
        self.ensure()?;
        let key = format!("cove_{}", crate::random_hex(24));
        std::fs::write(&path, format!("{key}\n"))?;
        restrict(&path, 0o600);
        Ok(key)
    }
}

pub fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
