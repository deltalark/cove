//! Base images. v1 ships one, Debian 13, in two forms:
//!
//! - a disk: Debian's "generic" cloud image, which boots with EFI on both vfkit and
//!   cloud-hypervisor and runs cloud-init from our seed disk;
//! - a root filesystem for gVisor sandboxes: Debian's official container image
//!   (`debian:trixie` on Docker Hub), fetched straight from the registry with every
//!   digest checked.

use crate::paths::Paths;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Catalog {
    pub name: &'static str,
    pub description: &'static str,
    base_url: &'static str,
    /// `{arch}` becomes `arm64` or `amd64`.
    file: &'static str,
    checksums: &'static str,
    /// The same system as an OCI image on Docker Hub, for the rootfs.
    oci_repo: &'static str,
    oci_tag: &'static str,
}

pub const CATALOG: &[Catalog] = &[Catalog {
    name: "debian-13",
    description: "Debian 13 (trixie), generic cloud image",
    base_url: "https://cloud.debian.org/images/cloud/trixie/latest/",
    file: "debian-13-generic-{arch}.tar.xz",
    checksums: "SHA512SUMS",
    oci_repo: "library/debian",
    oci_tag: "trixie",
}];

const REGISTRY: &str = "https://registry-1.docker.io";
const REGISTRY_AUTH: &str = "https://auth.docker.io/token?service=registry.docker.io";

/// Debian's name for the host's architecture (guests match the host).
pub fn debian_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// Rust's name, used for guest agent binaries.
pub fn rust_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImageInfo {
    pub name: String,
    pub description: String,
    /// Either form is here.
    pub pulled: bool,
    /// Size of the disk, when pulled.
    pub size: u64,
    /// The rootfs gVisor sandboxes start from is here.
    #[serde(default)]
    pub rootfs: bool,
}

pub fn disk_path(paths: &Paths, image: &str) -> PathBuf {
    paths.image_dir(image).join("disk.raw")
}

pub fn rootfs_path(paths: &Paths, image: &str) -> PathBuf {
    paths.image_dir(image).join("rootfs")
}

pub fn list(paths: &Paths) -> Vec<ImageInfo> {
    CATALOG
        .iter()
        .map(|c| {
            let size = std::fs::metadata(disk_path(paths, c.name))
                .map(|m| m.len())
                .ok();
            let rootfs = rootfs_path(paths, c.name).is_dir();
            ImageInfo {
                name: c.name.into(),
                description: c.description.into(),
                pulled: size.is_some() || rootfs,
                size: size.unwrap_or(0),
                rootfs,
            }
        })
        .collect()
}

/// Fetch what `backend` boots from: the rootfs for gVisor, else the disk (and the
/// firmware for cloud-hypervisor).
pub async fn pull_for(paths: &Paths, name: &str, backend: &str) -> Result<PathBuf> {
    if backend == crate::backend::GVISOR {
        pull_rootfs(paths, name).await
    } else {
        pull(paths, name, &|_, _| {}).await
    }
}

/// Download, verify and unpack an image. `progress` gets (bytes done, total).
pub async fn pull(
    paths: &Paths,
    name: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<PathBuf> {
    let entry = CATALOG
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("unknown image {name:?}"))?;
    let file = entry.file.replace("{arch}", debian_arch());
    let dir = paths.image_dir(name);
    std::fs::create_dir_all(&dir)?;
    let client = reqwest::Client::new();

    let sums = client
        .get(format!("{}{}", entry.base_url, entry.checksums))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = sums
        .lines()
        .find_map(|l| {
            let (sum, f) = l.split_once(char::is_whitespace)?;
            (f.trim().trim_start_matches('*') == file).then(|| sum.to_lowercase())
        })
        .ok_or_else(|| anyhow!("{file} is not listed in {}", entry.checksums))?;

    let archive = dir.join(&file);
    let partial = dir.join(format!("{file}.partial"));
    let response = client
        .get(format!("{}{}", entry.base_url, file))
        .send()
        .await?
        .error_for_status()?;
    let total = response.content_length().unwrap_or(0);
    let mut out = std::fs::File::create(&partial)?;
    let mut hasher = Sha512::new();
    let mut done = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        out.write_all(&chunk)?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    out.sync_all()?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        let _ = std::fs::remove_file(&partial);
        bail!("checksum mismatch for {file}");
    }
    std::fs::rename(&partial, &archive)?;

    let target = disk_path(paths, name);
    let unpack_dir = dir.clone();
    let archive_for_unpack = archive.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let reader = xz2::read::XzDecoder::new(std::fs::File::open(&archive_for_unpack)?);
        let mut tar = tar::Archive::new(reader);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.extension().is_some_and(|e| e == "raw") {
                let tmp = unpack_dir.join("disk.raw.partial");
                entry.unpack(&tmp).context("unpacking disk.raw")?;
                std::fs::rename(&tmp, unpack_dir.join("disk.raw"))?;
                return Ok(());
            }
        }
        bail!("no .raw disk in the archive")
    })
    .await??;
    let _ = std::fs::remove_file(&archive);
    crate::paths::restrict(&target, 0o444);
    // cloud-hypervisor boots the same disk with its own EFI firmware.
    if cfg!(target_os = "linux") && crate::chv::firmware(paths).is_none() {
        pull_firmware(paths).await?;
    }
    Ok(target)
}

/// Download cloud-hypervisor's EDK2 firmware for this architecture into
/// `~/.cove/firmware`.
pub async fn pull_firmware(paths: &Paths) -> Result<PathBuf> {
    let name = crate::chv::firmware_name();
    let dir = paths.firmware_dir();
    std::fs::create_dir_all(&dir)?;
    let url = format!("{}{name}", crate::chv::FIRMWARE_URL);
    let data = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {url}"))?
        .bytes()
        .await?;
    // A firmware volume is a few MB; anything tiny is an error page.
    if data.len() < 512 * 1024 {
        bail!("{url} doesn't look like firmware ({} bytes)", data.len());
    }
    let target = dir.join(name);
    let partial = dir.join(format!("{name}.partial"));
    std::fs::write(&partial, &data)?;
    std::fs::rename(&partial, &target)?;
    crate::paths::restrict(&target, 0o444);
    Ok(target)
}

/// Docker Hub's platform names for this machine.
fn oci_arch() -> (&'static str, Option<&'static str>) {
    if cfg!(target_arch = "aarch64") {
        ("arm64", Some("v8"))
    } else {
        ("amd64", None)
    }
}

/// Pick this machine's manifest out of an OCI image index.
pub fn pick_manifest(
    index: &serde_json::Value,
    arch: &str,
    variant: Option<&str>,
) -> Option<String> {
    let manifests = index["manifests"].as_array()?;
    let matches = |m: &&serde_json::Value, with_variant: bool| {
        let p = &m["platform"];
        p["os"] == "linux"
            && p["architecture"] == arch
            && (!with_variant || variant.is_none() || p["variant"].as_str() == variant)
    };
    manifests
        .iter()
        .find(|m| matches(m, true))
        .or_else(|| manifests.iter().find(|m| matches(m, false)))
        .and_then(|m| m["digest"].as_str().map(String::from))
}

/// Download the image's root filesystem from its OCI registry and unpack it into
/// `images/<name>/rootfs`, checking every blob against its digest.
pub async fn pull_rootfs(paths: &Paths, name: &str) -> Result<PathBuf> {
    let entry = CATALOG
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("unknown image {name:?}"))?;
    let repo = entry.oci_repo;
    let client = reqwest::Client::new();
    let token: serde_json::Value = client
        .get(format!("{REGISTRY_AUTH}&scope=repository:{repo}:pull"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = token["token"].as_str().unwrap_or_default().to_string();
    let get = |url: String, accept: &'static str| {
        client
            .get(url)
            .bearer_auth(&token)
            .header("Accept", accept)
            .send()
    };
    let index: serde_json::Value = get(
        format!("{REGISTRY}/v2/{repo}/manifests/{}", entry.oci_tag),
        "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json",
    )
    .await?
    .error_for_status()?
    .json()
    .await?;
    let (arch, variant) = oci_arch();
    let digest = pick_manifest(&index, arch, variant)
        .ok_or_else(|| anyhow!("{repo}:{} has no linux/{arch} image", entry.oci_tag))?;
    let manifest: serde_json::Value = get(
        format!("{REGISTRY}/v2/{repo}/manifests/{digest}"),
        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
    )
    .await?
    .error_for_status()?
    .json()
    .await?;
    let layers: Vec<String> = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow!("the image manifest lists no layers"))?
        .iter()
        .filter_map(|l| l["digest"].as_str().map(String::from))
        .collect();

    let dir = paths.image_dir(name);
    std::fs::create_dir_all(&dir)?;
    let partial = dir.join("rootfs.partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;
    for layer in layers {
        let want = layer
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow!("unsupported digest {layer}"))?
            .to_string();
        let data = get(
            format!("{REGISTRY}/v2/{repo}/blobs/{layer}"),
            "application/octet-stream",
        )
        .await?
        .error_for_status()?
        .bytes()
        .await?;
        let got: String = sha2::Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != want {
            bail!("checksum mismatch for layer {layer}");
        }
        let target = partial.clone();
        tokio::task::spawn_blocking(move || unpack_layer(&data, &target)).await??;
    }
    let rootfs = rootfs_path(paths, name);
    if rootfs.exists() {
        std::fs::remove_dir_all(&rootfs)?;
    }
    std::fs::rename(&partial, &rootfs)?;
    Ok(rootfs)
}

/// Unpack one gzipped layer over `root`, applying OCI whiteouts (`.wh.<name>` deletes
/// a file from the layers below, `.wh..wh..opq` empties its folder).
pub fn unpack_layer(gzipped: &[u8], root: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(gzipped));
    tar.set_preserve_permissions(true);
    // Only root can give files to other users; others unpack everything as themselves.
    tar.set_preserve_ownerships(unsafe { libc::geteuid() } == 0);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(true);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(hidden) = file_name.strip_prefix(".wh.") {
            let parent = root.join(path.parent().unwrap_or(Path::new("")));
            if hidden == ".wh..opq" {
                for child in std::fs::read_dir(&parent).into_iter().flatten().flatten() {
                    remove_any(&child.path());
                }
            } else {
                remove_any(&parent.join(hidden));
            }
            continue;
        }
        // `unpack_in` refuses paths that would escape `root`.
        entry
            .unpack_in(root)
            .with_context(|| format!("unpacking {}", path.display()))?;
    }
    Ok(())
}

fn remove_any(path: &Path) {
    if path.is_dir() && !path.is_symlink() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_this_platform_from_an_index() {
        let index = serde_json::json!({ "manifests": [
            { "digest": "sha256:arm32", "platform": { "os": "linux", "architecture": "arm", "variant": "v7" } },
            { "digest": "sha256:amd", "platform": { "os": "linux", "architecture": "amd64" } },
            { "digest": "sha256:arm", "platform": { "os": "linux", "architecture": "arm64", "variant": "v8" } },
            { "digest": "sha256:att", "platform": { "os": "unknown", "architecture": "unknown" } }
        ]});
        assert_eq!(
            pick_manifest(&index, "amd64", None).as_deref(),
            Some("sha256:amd")
        );
        assert_eq!(
            pick_manifest(&index, "arm64", Some("v8")).as_deref(),
            Some("sha256:arm")
        );
        assert_eq!(pick_manifest(&index, "riscv64", None), None);
    }

    fn layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn layers_unpack_with_whiteouts() {
        let root = tempfile::tempdir().unwrap();
        unpack_layer(
            &layer(&[
                ("etc/os-release", b"debian"),
                ("etc/gone", b"x"),
                ("var/cache/a", b"1"),
                ("var/cache/b", b"2"),
            ]),
            root.path(),
        )
        .unwrap();
        unpack_layer(
            &layer(&[
                ("etc/.wh.gone", b""),
                ("var/cache/.wh..wh..opq", b""),
                ("var/cache/c", b"3"),
            ]),
            root.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("etc/os-release")).unwrap(),
            b"debian"
        );
        assert!(!root.path().join("etc/gone").exists());
        assert!(!root.path().join("var/cache/a").exists());
        assert!(root.path().join("var/cache/c").exists());
    }
}
