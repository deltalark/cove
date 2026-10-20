//! Snapshots: a VM's disk (or root filesystem) saved under a name, that new VMs can
//! start from. They hold installed software and files, not memory or processes.

use crate::backend;
use crate::paths::Paths;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What a snapshot holds, which decides the backends that can start from it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A raw disk: vfkit and cloud-hypervisor boot the same Debian disk.
    Disk,
    /// A root filesystem folder: gVisor.
    Rootfs,
    /// The dev backend's working folder (tests only).
    Host,
}

impl Kind {
    pub fn for_backend(backend: &str) -> Kind {
        match backend {
            backend::GVISOR => Kind::Rootfs,
            backend::HOST => Kind::Host,
            _ => Kind::Disk,
        }
    }

    pub fn fits(self, backend: &str) -> bool {
        Kind::for_backend(backend) == self
    }

    fn describe(self) -> &'static str {
        match self {
            Kind::Disk => "a VM disk (vfkit or cloud-hypervisor)",
            Kind::Rootfs => "a gVisor root filesystem",
            Kind::Host => "a dev-backend folder",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub kind: Kind,
    /// The CPU architecture its disk was made for.
    pub arch: String,
    /// The base image the saved VM started from.
    pub image: String,
    /// The VM it was saved from, which may be gone since.
    pub source_vm: String,
    /// The saved VM's disk size; VMs made from it get at least this much.
    pub disk_gb: u64,
    pub created_at: u64,
    /// Bytes it uses on disk.
    #[serde(default)]
    pub size: u64,
}

pub fn info_path(paths: &Paths, name: &str) -> PathBuf {
    paths.snapshot_dir(name).join("snapshot.json")
}

/// Where a snapshot's data lives: `disk.raw`, `rootfs/` or `root/`.
pub fn data_path(dir: &Path, kind: Kind) -> PathBuf {
    match kind {
        Kind::Disk => dir.join("disk.raw"),
        Kind::Rootfs => dir.join("rootfs"),
        Kind::Host => dir.join("root"),
    }
}

pub fn load(paths: &Paths, name: &str) -> Result<SnapshotInfo> {
    // Names come from URLs: never let one reach outside snapshots/.
    if !crate::valid_name(name) {
        bail!("no snapshot named {name:?}");
    }
    let text = std::fs::read_to_string(info_path(paths, name))
        .map_err(|_| anyhow!("no snapshot named {name:?}"))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn list(paths: &Paths) -> Vec<SnapshotInfo> {
    let mut out: Vec<SnapshotInfo> = std::fs::read_dir(paths.snapshots())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| load(paths, &e.file_name().to_string_lossy()).ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn remove(paths: &Paths, name: &str) -> Result<()> {
    load(paths, name)?;
    let dir = paths.snapshot_dir(name);
    crate::gvisor::release_mounts(&dir);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Check that a VM on `backend` can start from `snap`.
pub fn check_fits(snap: &SnapshotInfo, backend: &str) -> Result<()> {
    if !snap.kind.fits(backend) {
        bail!(
            "snapshot {} holds {}, so a {backend} VM can't start from it",
            snap.name,
            snap.kind.describe()
        );
    }
    if snap.arch != crate::image::rust_arch() {
        bail!(
            "snapshot {} was made on {}, and this machine is {}",
            snap.name,
            snap.arch,
            crate::image::rust_arch()
        );
    }
    Ok(())
}

/// Copy a VM's data folder or disk into place (`to` must not exist).
pub fn copy_data(kind: Kind, from: &Path, to: &Path) -> Result<()> {
    match kind {
        Kind::Disk => crate::disk::copy_sparse(from, to),
        Kind::Rootfs | Kind::Host => {
            if from.is_dir() {
                crate::gvisor::copy_tree(from, to)
            } else {
                std::fs::create_dir_all(to)?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_fit_their_backends() {
        assert!(Kind::Disk.fits(backend::VFKIT));
        assert!(Kind::Disk.fits(backend::CLOUD_HYPERVISOR));
        assert!(!Kind::Disk.fits(backend::GVISOR));
        assert!(Kind::Rootfs.fits(backend::GVISOR));
        assert!(Kind::Host.fits(backend::HOST));
        let snap = SnapshotInfo {
            name: "base".into(),
            description: String::new(),
            kind: Kind::Rootfs,
            arch: crate::image::rust_arch().into(),
            image: "debian-13".into(),
            source_vm: "dev".into(),
            disk_gb: 20,
            created_at: 0,
            size: 0,
        };
        assert!(check_fits(&snap, backend::GVISOR).is_ok());
        let refused = check_fits(&snap, backend::CLOUD_HYPERVISOR).unwrap_err();
        assert!(refused.to_string().contains("gVisor"), "{refused}");
    }

    #[test]
    fn lists_and_removes() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        paths.ensure().unwrap();
        assert!(list(&paths).is_empty());
        let dir = paths.snapshot_dir("base");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            info_path(&paths, "base"),
            r#"{"name":"base","kind":"disk","arch":"x86_64","image":"debian-13",
                "source_vm":"dev","disk_gb":20,"created_at":1}"#,
        )
        .unwrap();
        assert_eq!(list(&paths)[0].name, "base");
        remove(&paths, "base").unwrap();
        assert!(load(&paths, "base").is_err());
        assert!(remove(&paths, "../vms").is_err());
    }
}
