//! `cargo xtask vmware-import-test`: a real VMware VM opens in vmmc and boots, and its
//! files stay as they were.
//!
//! It imports the VM `VMMC_VMWARE_VMX` names (by default `A:\VMs\Windows 10
//! x64\Windows 10 x64.vmx`) with vmmc's files in the test image directory rather
//! than beside the `.vmx`, so nothing is added to the VMware folder either. It boots
//! the VM headless and waits for the guest to ask vmmc's NAT for an address (DHCP),
//! which an OS only does once it's well up; then it presses the power button and
//! expects the guest to power off. Last, it checks every file in the VMware folder
//! still has its size and time, and the small ones their contents.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::firmware::Session;
use crate::{build_vmmc_vmx, workspace};

const DEFAULT_VMX: &str = r"A:\VMs\Windows 10 x64\Windows 10 x64.vmx";
/// First boot on new hardware: Windows finds its devices, and may restart once.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Files up to this size are compared by content, larger ones by size and time.
const HASHED_UP_TO: u64 = 1 << 20;

/// What a folder's files are: size, modification time, and a hash of the small ones.
type Fingerprint = BTreeMap<String, (u64, Option<SystemTime>, Option<Vec<u8>>)>;

fn fingerprint(dir: &Path) -> Result<Fingerprint, String> {
    let mut files = Fingerprint::new();
    let entries = fs::read_dir(dir).map_err(|e| format!("can't read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        if meta.is_dir() {
            files.insert(format!("{name}/"), (0, None, None));
            continue;
        }
        let hash = (meta.len() <= HASHED_UP_TO)
            .then(|| fs::read(&path).map(|bytes| Sha256::digest(&bytes).to_vec()))
            .transpose()
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        files.insert(name, (meta.len(), meta.modified().ok(), hash));
    }
    Ok(files)
}

pub fn test() -> Result<(), String> {
    let vmx = std::env::var_os("VMMC_VMWARE_VMX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_VMX));
    if !vmx.is_file() {
        return Err(format!(
            "{} not found: set VMMC_VMWARE_VMX to a VMware VM's .vmx",
            vmx.display()
        ));
    }
    let folder = vmx.parent().unwrap().to_owned();
    let before = fingerprint(&folder)?;
    let state = crate::image_dir()?.join("vmware-import");
    let _ = fs::remove_dir_all(&state);
    let imported = vmconfig::import::import(&vmx, Some(&state))?;
    eprintln!("xtask: imported {}", imported.vm.path.display());
    for warning in &imported.warnings {
        eprintln!("xtask:   note: {warning}");
    }

    let workspace = workspace();
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join("vmware-import-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let log = dir.join("vmmc-vmx.log");
    let log_file =
        fs::File::create(&log).map_err(|e| format!("can't create {}: {e}", log.display()))?;
    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--vm")
        .arg(&imported.vm.path)
        .arg("--screenshot")
        .arg(dir.join("screens"))
        .args(["--screenshot-every", "30"])
        // The NAT's DHCP server says when the guest asks for an address.
        .env("VMMC_LOG", "net::dhcp=debug")
        .stderr(Stdio::from(log_file));
    eprintln!("$ {command:?}");
    let started = Instant::now();
    let mut vm = Session::spawn(command)?;
    let booted = loop {
        // Lossy: the log holds whatever the firmware and the guest wrote.
        let text = String::from_utf8_lossy(&fs::read(&log).unwrap_or_default()).into_owned();
        // A DHCP acknowledgement (reply type 5).
        if text.contains("reply=5") {
            break Ok(());
        }
        if let Some(status) = vm.exited() {
            break Err(format!(
                "vmmc-vmx ended ({status}) before the guest asked for an address"
            ));
        }
        if started.elapsed() > BOOT_TIMEOUT {
            break Err(format!(
                "the guest didn't ask for an address within {}s",
                BOOT_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(Duration::from_secs(2));
    };
    let result = if booted.is_ok() {
        eprintln!(
            "xtask: the guest took a DHCP address after {}s",
            started.elapsed().as_secs()
        );
        // Let the logon screen come up, then the power button.
        thread::sleep(Duration::from_secs(60));
        let _ = vm.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        let _ = vm.send("\x01p");
        match vm.wait(SHUTDOWN_TIMEOUT) {
            Some(status) if status.success() => {
                eprintln!("xtask: the guest powered off");
                Ok(())
            }
            Some(status) => Err(format!("vmmc-vmx ended with {status}")),
            None => Err("the guest didn't power off after the power button".into()),
        }
    } else {
        booted.map_err(|e| {
            format!(
                "{e}; see the screenshots in {}",
                dir.join("screens").display()
            )
        })
    };
    if result.is_err() {
        let _ = vm.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        vm.kill();
    }
    // Whatever happened, VMware's files are as they were.
    let after = fingerprint(&folder)?;
    if after != before {
        let changed: Vec<_> = before
            .keys()
            .chain(after.keys())
            .filter(|name| before.get(*name) != after.get(*name))
            .collect();
        return Err(format!("the VMware folder changed: {changed:?}"));
    }
    eprintln!("xtask: every file in {} is unchanged", folder.display());
    result?;
    let _ = fs::remove_dir_all(&state);
    eprintln!("vmware-import-test passed");
    Ok(())
}
