//! Copying VM disks without filling their holes.
//!
//! A VM disk is a raw file grown to its full size with `set_len`, so most of it is a
//! hole. A plain copy on a filesystem without reflinks (ext4, which most VPS images
//! use) would write every one of those zeros. These copies share blocks where the
//! filesystem can (APFS clones, btrfs and XFS reflinks) and otherwise copy only the
//! parts that hold data.

use anyhow::{Context, Result};
use std::path::Path;

/// Copy `from` to `to` (which must not exist yet), keeping holes.
pub fn copy_sparse(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        anyhow::bail!("{} already exists", to.display());
    }
    let result = copy_inner(from, to);
    if result.is_err() {
        let _ = std::fs::remove_file(to);
    }
    result.with_context(|| format!("copying {} to {}", from.display(), to.display()))
}

#[cfg(target_os = "macos")]
fn copy_inner(from: &Path, to: &Path) -> Result<()> {
    // std's copy clones the file on APFS, which shares every block.
    std::fs::copy(from, to)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn copy_inner(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let src = std::fs::File::open(from)?;
    let dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    // FICLONE: share every block (btrfs, XFS with reflinks).
    const FICLONE: libc::c_ulong = 0x4004_9409;
    if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let len = src.metadata()?.len();
    let (s, d) = (src.as_raw_fd(), dst.as_raw_fd());
    let mut pos: i64 = 0;
    while (pos as u64) < len {
        let data = unsafe { libc::lseek(s, pos, libc::SEEK_DATA) };
        if data < 0 {
            // ENXIO: only a hole from here to the end.
            break;
        }
        let hole = unsafe { libc::lseek(s, data, libc::SEEK_HOLE) };
        let end = if hole < 0 { len as i64 } else { hole };
        let mut off_in = data;
        let mut off_out = data;
        while off_in < end {
            let n = unsafe {
                libc::copy_file_range(s, &mut off_in, d, &mut off_out, (end - off_in) as usize, 0)
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if n == 0 {
                break;
            }
        }
        pos = end;
    }
    dst.set_len(len)?;
    use std::os::unix::fs::PermissionsExt;
    let mode = src.metadata()?.permissions().mode();
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Bytes a file really uses on disk (its holes don't count).
pub fn allocated(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.blocks() * 512)
        .unwrap_or(0)
}

/// Bytes a folder's files really use, for snapshot sizes.
pub fn allocated_tree(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        use std::os::unix::fs::MetadataExt;
        return meta.blocks() * 512;
    }
    std::fs::read_dir(path)
        .map(|rd| rd.flatten().map(|e| allocated_tree(&e.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn copies_data_and_keeps_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.raw");
        let mut f = std::fs::File::create(&from).unwrap();
        f.write_all(b"start").unwrap();
        f.seek(SeekFrom::Start(64 << 20)).unwrap();
        f.write_all(b"middle").unwrap();
        f.set_len(256 << 20).unwrap();
        drop(f);
        let to = dir.path().join("b.raw");
        copy_sparse(&from, &to).unwrap();
        let data = std::fs::read(&to).unwrap();
        assert_eq!(data.len(), 256 << 20);
        assert_eq!(&data[..5], b"start");
        assert_eq!(&data[64 << 20..(64 << 20) + 6], b"middle");
        assert!(data[5..64 << 20].iter().all(|&b| b == 0));
        // The holes stay holes: far less than the full size is allocated.
        assert!(allocated(&to) < 64 << 20, "{} allocated", allocated(&to));
        // It refuses to overwrite.
        assert!(copy_sparse(&from, &to).is_err());
        assert_eq!(std::fs::read(&to).unwrap().len(), 256 << 20);
    }
}
