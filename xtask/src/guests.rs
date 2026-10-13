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

/// The same release's initrd, built for that kernel. Only its modules are used: Ubuntu
/// builds the NVMe driver as one.
const INITRD: Artifact = Artifact {
    file: "initrd",
    url: "https://cloud-images.ubuntu.com/releases/26.04/release-20260927/unpacked/ubuntu-26.04-server-cloudimg-amd64-initrd-generic",
    sha256: "c9f23b4ac34a20df6a1d344b44bf2597faa48799d814cbc206b3974c6ead5215",
};
/// Modules the test init loads, with what they depend on. Built-in ones are skipped.
const MODULES: [&str; 8] = [
    "nvme",
    "ahci",
    "isofs",
    "e1000e",
    "tpm_crb",
    "xhci_pci",
    "usbhid",
    "hid_generic",
];

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

    let modules = modules(&dir)?;

    let busybox_bytes = read(&busybox)?;
    let init = INIT_SCRIPT.replace("\r\n", "\n");
    let paths: Vec<String> = modules
        .iter()
        .map(|(name, _)| format!("lib/modules/{name}"))
        .collect();
    let load_order: String = modules
        .iter()
        .map(|(name, _)| format!("{name}\n"))
        .collect();
    let mut entries = vec![
        Entry::dir("bin"),
        Entry::dir("dev"),
        Entry::dir("lib"),
        Entry::dir("lib/modules"),
        Entry::dir("proc"),
        Entry::dir("sys"),
        Entry::dir("mnt"),
        Entry::dir("tmp"),
        // The kernel opens /dev/console before init runs, so it has to exist already.
        Entry::char_device("dev/console", 0o600, 5, 1),
        Entry::file("bin/busybox", 0o755, &busybox_bytes),
        Entry::file("init", 0o755, init.as_bytes()),
        Entry::file("lib/modules/order", 0o644, load_order.as_bytes()),
    ];
    for (path, (_, data)) in paths.iter().zip(&modules) {
        entries.push(Entry::file(path, 0o644, data));
    }
    let archive = cpio::archive(&entries);
    let initramfs = dir.join("initramfs.cpio");
    write(&initramfs, &archive)?;

    let disk = crate::image_dir()?.join("linux-test-disk.raw");
    let _ = fs::remove_file(&disk);
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

/// The kernel modules the test init loads, in load order. Taken out of Ubuntu's initrd
/// once and kept in `modules/` next to it.
fn modules(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let cache = dir.join("modules");
    let order_file = cache.join("order");
    // The cache records which modules were asked for, so changing the list refreshes it.
    let wanted = MODULES.join(" ");
    if let Ok(order) = fs::read_to_string(&order_file)
        && order.lines().next() == Some(wanted.as_str())
    {
        let cached: Result<Vec<_>, _> = order
            .lines()
            .skip(1)
            .map(|name| read(&cache.join(name)).map(|data| (name.to_owned(), data)))
            .collect();
        if let Ok(cached) = cached {
            return Ok(cached);
        }
    }
    let initrd = fetch(dir, &INITRD)?;
    eprintln!("extracting kernel modules from {}", initrd.display());
    let modules = crate::modules::extract(&read(&initrd)?, &MODULES)?;
    fs::create_dir_all(&cache).map_err(|e| format!("can't create {}: {e}", cache.display()))?;
    for (name, data) in &modules {
        write(&cache.join(name), data)?;
    }
    let order: String = modules
        .iter()
        .map(|(name, _)| format!("{name}\n"))
        .collect();
    write(&order_file, format!("{wanted}\n{order}").as_bytes())?;
    Ok(modules)
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
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
pub(crate) fn verify(name: &str, bytes: &[u8], expected: &str) -> Result<(), String> {
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
