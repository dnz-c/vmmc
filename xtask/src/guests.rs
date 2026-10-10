//! Test guest artifacts: a pinned Linux kernel and BusyBox, plus an initramfs and a
//! blank disk built from them. Everything lands in `target/guests/`.
//!
//! These are test inputs only. They are never shipped (BusyBox is GPL).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::cpio::{self, Entry};

struct Artifact {
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
}

/// Ubuntu 26.04's generic kernel: virtio, ext4 and the 8250 UART are built in.
/// The hash is from Ubuntu's SHA256SUMS for this release directory.
const KERNEL: Artifact = Artifact {
    file: "vmlinuz",
    url: "https://cloud-images.ubuntu.com/releases/26.04/release-20260927/unpacked/ubuntu-26.04-server-cloudimg-amd64-vmlinuz-generic",
    sha256: "7efd88a7facf80874d781ccb2c2421f0aaaa575e3b94760430619e826f490117",
};

/// A static x86-64 BusyBox. busybox.net publishes no checksums for these builds; the
/// hash pins the file as first downloaded.
const BUSYBOX: Artifact = Artifact {
    file: "busybox",
    url: "https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox",
    sha256: "6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348",
};

const INIT_SCRIPT: &str = include_str!("../guest/init.sh");
const DISK_SIZE: u64 = 64 << 20;

/// Paths of a prepared guest.
pub struct Guest {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub disk: PathBuf,
}

pub fn directory(workspace: &Path) -> PathBuf {
    workspace.join("target").join("guests")
}

/// Downloads (or reuses) the artifacts and builds the initramfs and a fresh disk.
pub fn prepare(workspace: &Path) -> Result<Guest, String> {
    let dir = directory(workspace);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let kernel = fetch(&dir, &KERNEL)?;
    let busybox = fetch(&dir, &BUSYBOX)?;

    let busybox_bytes = read(&busybox)?;
    let init = INIT_SCRIPT.replace("\r\n", "\n");
    let archive = cpio::archive(&[
        Entry::dir("bin"),
        Entry::dir("dev"),
        Entry::dir("proc"),
        Entry::dir("sys"),
        Entry::dir("mnt"),
        Entry::dir("tmp"),
        // The kernel opens /dev/console before init runs, so it has to exist already.
        Entry::char_device("dev/console", 0o600, 5, 1),
        Entry::file("bin/busybox", 0o755, &busybox_bytes),
        Entry::file("init", 0o755, init.as_bytes()),
    ]);
    let initramfs = dir.join("initramfs.cpio");
    write(&initramfs, &archive)?;

    let disk = dir.join("disk.raw");
    let file =
        fs::File::create(&disk).map_err(|e| format!("can't create {}: {e}", disk.display()))?;
    file.set_len(DISK_SIZE)
        .map_err(|e| format!("can't size {}: {e}", disk.display()))?;

    Ok(Guest {
        kernel,
        initramfs,
        disk,
    })
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Checks `bytes` against a pinned SHA-256.
fn verify(name: &str, bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{name}: SHA-256 is {actual}, expected {expected}"))
    }
}

/// Returns the artifact's local path, downloading it if it's missing or doesn't match.
fn fetch(dir: &Path, artifact: &Artifact) -> Result<PathBuf, String> {
    let path = dir.join(artifact.file);
    if let Ok(bytes) = fs::read(&path)
        && verify(artifact.file, &bytes, artifact.sha256).is_ok()
    {
        return Ok(path);
    }
    eprintln!("downloading {}", artifact.url);
    let partial = dir.join(format!("{}.partial", artifact.file));
    // curl.exe ships with Windows 10 and later.
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(&partial)
        .arg(artifact.url)
        .status()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if !status.success() {
        return Err(format!("downloading {} failed ({status})", artifact.url));
    }
    let bytes = read(&partial)?;
    if let Err(e) = verify(artifact.file, &bytes, artifact.sha256) {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &path).map_err(|e| format!("can't move {}: {e}", partial.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors_and_rejects_others() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify("x", b"abc", good).is_ok());
        let error = verify("x", b"abd", good).unwrap_err();
        assert!(error.contains("expected"), "{error}");
    }

    #[test]
    fn init_script_is_a_busybox_shell_script_that_powers_off() {
        assert!(INIT_SCRIPT.starts_with("#!/bin/busybox sh"));
        assert!(INIT_SCRIPT.contains("VMMC-TEST: done"));
        assert!(INIT_SCRIPT.contains("poweroff -f"));
    }
}
