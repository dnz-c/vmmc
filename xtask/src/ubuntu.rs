//! `cargo xtask ubuntu-desktop-test`: installs Ubuntu 26.04 Desktop from its ISO with
//! autoinstall, boots the installed system, and checks what it reports.
//!
//! - **ISO:** downloaded from releases.ubuntu.com into the test image directory and
//!   checked against a pinned SHA-256; `VMMC_UBUNTU_ISO` names a copy of the same file.
//! - **Autoinstall:** a disc labelled CIDATA, which cloud-init reads as a NoCloud data
//!   source, carries the configuration. GRUB is driven over the serial console to add
//!   `autoinstall` (no confirmation prompt) and a serial console to the kernel's
//!   command line.
//! - **Install:** offline, onto a 64 GiB NVMe disk, with Secure Boot on. Its late
//!   commands add a first-boot service, then the installer powers off.
//! - **First boot:** from the disk alone, now with an e1000e network adapter. The
//!   service writes `VMMC-UBUNTU:` markers to COM1 and powers off.
//! - **Progress:** a screenshot every 30 s in `target/ubuntu-desktop-test/screens`.
//!
//! It takes 15 to 60 minutes.

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::firmware::Session;
use crate::{build_vmmc_vmx, native, workspace};
use media::iso;

const ISO_VAR: &str = "VMMC_UBUNTU_ISO";
const ISO_FILE: &str = "ubuntu-26.04.1-desktop-amd64.iso";
const ISO_URL: &str = "https://releases.ubuntu.com/26.04/ubuntu-26.04.1-desktop-amd64.iso";
/// From the release directory's SHA256SUMS.
const ISO_SHA256: &str = "601e30fbf5d97759367c632e2c33630665039b7e2158fd068403da3ccf1bda1f";
const USER_DATA: &str = include_str!("../guest/ubuntu-user-data.yaml");
const DISK_SIZE: u64 = 64 << 30;
const MEMORY_MIB: &str = "8192";
const CPUS: &str = "4";
/// The first entry of the ISO's GRUB menu.
const GRUB_MENU: &str = "Try or Install Ubuntu";
/// What GRUB's command line runs instead of the menu entry: the same kernel, with
/// autoinstall and the serial console added.
const GRUB_COMMANDS: [&str; 4] = [
    "set gfxpayload=keep",
    "linux /casper/vmlinuz autoinstall console=tty0 console=ttyS0,115200 ---",
    "initrd /casper/initrd",
    "boot",
];
const MENU_TIMEOUT: Duration = Duration::from_secs(180);
const LIVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);
/// What the first-boot service must report, in order.
const MARKERS: [&str; 6] = [
    "VMMC-UBUNTU: booted",
    "VMMC-UBUNTU: cpus=4",
    "VMMC-UBUNTU: secureboot=1",
    "VMMC-UBUNTU: desktop=active",
    "VMMC-UBUNTU: net=ok",
    "VMMC-UBUNTU: done",
];

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn check_iso(path: &Path) -> Result<(), String> {
    eprintln!("xtask: checking {}", path.display());
    let actual = sha256_file(path)?;
    if actual == ISO_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "{}: SHA-256 is {actual}, expected {ISO_SHA256} ({ISO_FILE})",
            path.display()
        ))
    }
}

/// The checked ISO: `VMMC_UBUNTU_ISO`, or a download kept in the test image directory.
pub(crate) fn fetch_iso() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os(ISO_VAR) {
        let path = PathBuf::from(path);
        check_iso(&path)?;
        return Ok(path);
    }
    let path = crate::image_dir()?.join(ISO_FILE);
    if path.is_file() && check_iso(&path).is_ok() {
        return Ok(path);
    }
    let partial = path.with_extension("iso.partial");
    eprintln!("downloading {ISO_URL} (about 6.5 GB)");
    // curl.exe ships with Windows 10 and later. An interrupted download resumes.
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--continue-at",
            "-",
            "--output",
        ])
        .arg(&partial)
        .arg(ISO_URL)
        .status()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if !status.success() {
        return Err(format!("downloading {ISO_URL} failed ({status})"));
    }
    if let Err(e) = check_iso(&partial) {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &path).map_err(|e| format!("can't move {}: {e}", partial.display()))?;
    Ok(path)
}

/// The cloud-init user data, with the host's test web server filled in.
fn user_data(http_port: u16) -> String {
    USER_DATA
        .replace("\r\n", "\n")
        .replace("@@HTTP_PORT@@", &http_port.to_string())
        .replace("@@HTTP_BODY@@", crate::HTTP_BODY)
}

/// Prints the markers the guest wrote so far.
fn report(vm: &Session) {
    let output = vm.text(0, vm.len());
    for line in output.lines().filter(|line| line.contains("VMMC-UBUNTU:")) {
        eprintln!("{}", line.trim());
    }
}

/// Waits for the guest to power the VM off.
fn powered_off(vm: &mut Session, what: &str) -> Result<(), String> {
    match vm.wait(SHUTDOWN_TIMEOUT) {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("vmmc-vmx exited with {status} after {what}")),
        None => Err(format!("the guest didn't power off after {what}")),
    }
}

/// The command that boots test `name`'s Ubuntu from its disk (the image directory's
/// `NAME-disk.vmdk`), its state in `target/NAME/vm`, screenshots every 30 s in
/// `target/NAME/SCREENS`.
pub(crate) fn machine(name: &str, screens: &str) -> Result<Command, String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join(name);
    let disk = crate::image_dir()?.join(format!("{name}-disk.vmdk"));
    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.fd"))
        .arg("--vars-template")
        .arg(native.join("OVMF_VARS.secboot.fd"))
        .arg("--vm-dir")
        .arg(dir.join("vm"))
        .arg("--disk")
        .arg(format!("{},nvme", disk.display()))
        .args(["--cpus", CPUS, "--memory", MEMORY_MIB])
        .arg("--screenshot")
        .arg(dir.join(screens))
        .args(["--screenshot-every", "30"]);
    Ok(command)
}

/// Installs Ubuntu Desktop for test `name` onto a new disk, with `user_data` (cloud-init
/// autoinstall) on its CIDATA disc, whose early and late commands write `started` and
/// `installed` to COM1. Passes once the installer powers the VM off.
pub(crate) fn install(
    name: &str,
    user_data: &str,
    started: &str,
    installed: &str,
) -> Result<(), String> {
    let ubuntu_iso = fetch_iso()?;
    let dir = workspace().join("target").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let seed = dir.join("cidata.iso");
    let meta = format!("instance-id: vmmc-{name}\n");
    let disc = iso::build(
        "CIDATA",
        &[
            ("user-data", user_data.as_bytes()),
            ("meta-data", meta.as_bytes()),
        ],
        None,
    );
    fs::write(&seed, disc).map_err(|e| format!("can't write {}: {e}", seed.display()))?;
    let disk = crate::image_dir()?.join(format!("{name}-disk.vmdk"));
    let _ = fs::remove_file(&disk);
    disk::create_sparse(&disk, DISK_SIZE)
        .map_err(|e| format!("can't create {}: {e}", disk.display()))?;

    let mut install = machine(name, "screens")?;
    install
        .arg("--cdrom")
        .arg(&ubuntu_iso)
        .arg("--cdrom")
        .arg(&seed);
    eprintln!("$ {install:?}");
    eprintln!(
        "installing Ubuntu; progress screenshots in {}",
        dir.join("screens").display()
    );
    let mut vm = Session::spawn(install)?;
    let result = (|| {
        vm.expect(GRUB_MENU, 0, MENU_TIMEOUT)?;
        // GRUB's command line rather than the menu entry, so the kernel gets ours.
        vm.send("c")?;
        let mut at = vm.expect("grub>", 0, Duration::from_secs(30))?;
        for command in GRUB_COMMANDS {
            vm.send(&format!("{command}\r"))?;
            if command != "boot" {
                at = vm.expect("grub>", at, Duration::from_secs(60))?;
            }
        }
        at = vm.expect(started, at, LIVE_TIMEOUT)?;
        vm.expect(installed, at, INSTALL_TIMEOUT)?;
        powered_off(&mut vm, "installing")
    })();
    report(&vm);
    if result.is_err() {
        vm.dump_state();
        vm.kill();
    }
    result
}

pub fn test() -> Result<(), String> {
    const NAME: &str = "ubuntu-desktop-test";
    // The guest fetches a page from this through the NAT's gateway on its first boot.
    let http = crate::serve_http()?;
    install(
        NAME,
        &user_data(http),
        "VMMC-UBUNTU: autoinstall",
        "VMMC-UBUNTU: installed",
    )?;

    let mut first_boot = machine(NAME, "screens-first-boot")?;
    first_boot.args(["--nic", "e1000e"]);
    eprintln!("$ {first_boot:?}");
    let mut vm = Session::spawn(first_boot)?;
    let booted = (|| {
        let mut at = 0;
        for marker in MARKERS {
            at = vm.expect(marker, at, BOOT_TIMEOUT)?;
        }
        powered_off(&mut vm, "the first-boot checks")
    })();
    report(&vm);
    if let Err(e) = booted {
        vm.dump_state();
        vm.kill();
        return Err(e);
    }
    eprintln!("Ubuntu Desktop install test passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_is_autoinstall_cloud_config_with_the_server_filled_in() {
        let data = user_data(8080);
        assert!(data.starts_with("#cloud-config\n"));
        assert!(data.contains("\nautoinstall:\n  version: 1\n"));
        assert!(data.contains("http://10.0.2.2:8080/"));
        assert!(!data.contains("@@"));
        assert!(!data.contains('\t'), "YAML indents with spaces");
        assert!(data.contains("shutdown: poweroff"));
        // A command list item with ": " in it must be quoted, or YAML reads a map.
        for line in data.lines() {
            let item = line.trim_start().strip_prefix("- ");
            if let Some(item) = item.filter(|item| !item.starts_with(['"', '\'', '|'])) {
                assert!(!item.contains(": "), "unquoted mapping: {line}");
            }
        }
    }
}
