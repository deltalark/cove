//! `scripts/install.sh`: it parses, passes shellcheck when that is installed, and on
//! Linux installs a release tarball into a scratch home, upgrades it in place and
//! uninstalls it again, without touching the network or the real system.

use std::path::{Path, PathBuf};
use std::process::Command;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

#[test]
fn parses() {
    let out = Command::new("sh").arg("-n").arg(script()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = Command::new("sh")
        .arg(script())
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--prefix",
        "--version",
        "--no-service",
        "--from-source",
        "--uninstall",
    ] {
        assert!(text.contains(flag), "{flag} missing from --help");
    }
}

#[test]
fn passes_shellcheck() {
    let Ok(out) = Command::new("shellcheck").arg(script()).output() else {
        eprintln!("shellcheck is not installed; skipping");
        return;
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
#[cfg(target_os = "linux")]
fn installs_upgrades_and_uninstalls_from_a_tarball() {
    let cove = PathBuf::from(env!("CARGO_BIN_EXE_cove"));
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg/cove");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::copy(&cove, pkg.join("cove")).unwrap();
    std::fs::write(pkg.join("cove-guest-linux-x86_64"), "guest").unwrap();
    let tarball = dir.path().join("cove-test-linux.tar.gz");
    let status = Command::new("tar")
        .arg("-C")
        .arg(dir.path().join("pkg"))
        .arg("-czf")
        .arg(&tarball)
        .arg("cove")
        .status()
        .unwrap();
    assert!(status.success());

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("sh")
            .arg(script())
            .args(args)
            .env("HOME", &home)
            .env_remove("COVE_HOME")
            .env_remove("COVE_PREFIX")
            .env_remove("COVE_URL")
            .env_remove("COVE_API_KEY")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "install.sh {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let install = [
        "--from-file",
        tarball.to_str().unwrap(),
        "--no-link",
        "--no-service",
    ];
    let text = run(&install);
    let bin = home.join(".cove/bin");
    assert!(bin.join("cove").is_file());
    assert!(bin.join("cove-guest-linux-x86_64").is_file());
    // Honest about Linux: no VMs here yet, so no service either.
    assert!(text.contains("can't run VMs on Linux yet"), "{text}");
    // Running it again upgrades in place.
    run(&install);
    assert!(bin.join("cove").is_file());

    // Uninstalling keeps data unless asked; --purge removes it.
    std::fs::create_dir_all(home.join(".cove/vms/keep")).unwrap();
    run(&["--uninstall"]);
    assert!(!bin.join("cove").exists());
    assert!(home.join(".cove/vms/keep").is_dir());
    run(&["--uninstall", "--purge"]);
    assert!(!home.join(".cove").exists());
}
