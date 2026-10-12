//! Base images. v1 ships one: Debian 13 "generic" cloud images, which boot with EFI
//! on both vfkit and cloud-hypervisor and run cloud-init from our seed disk.

use crate::paths::Paths;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::io::Write;
use std::path::PathBuf;

pub struct Catalog {
    pub name: &'static str,
    pub description: &'static str,
    base_url: &'static str,
    /// `{arch}` becomes `arm64` or `amd64`.
    file: &'static str,
    checksums: &'static str,
}

pub const CATALOG: &[Catalog] = &[Catalog {
    name: "debian-13",
    description: "Debian 13 (trixie), generic cloud image",
    base_url: "https://cloud.debian.org/images/cloud/trixie/latest/",
    file: "debian-13-generic-{arch}.tar.xz",
    checksums: "SHA512SUMS",
}];

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
    pub pulled: bool,
    pub size: u64,
}

pub fn disk_path(paths: &Paths, image: &str) -> PathBuf {
    paths.image_dir(image).join("disk.raw")
}

pub fn list(paths: &Paths) -> Vec<ImageInfo> {
    CATALOG
        .iter()
        .map(|c| {
            let size = std::fs::metadata(disk_path(paths, c.name))
                .map(|m| m.len())
                .ok();
            ImageInfo {
                name: c.name.into(),
                description: c.description.into(),
                pulled: size.is_some(),
                size: size.unwrap_or(0),
            }
        })
        .collect()
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
