//! Opening a VMware VM in place.
//!
//! vmmc reads the `.vmx` and makes its own config beside it, in a `vmmc` folder in the
//! VM's folder (or wherever it's told). Each of VMware's disks gets a delta disk there,
//! which takes every write: VMware's disks, its `.vmx` and its `.nvram` are never
//! written, and deleting the `vmmc` folder leaves the VMware VM as it was. A VMware disk
//! that changes after the import (because the VM ran in VMware again) makes vmmc refuse
//! its delta, as VMware would.

use std::fs;
use std::path::{Path, PathBuf};

use disk::BlockBackend;

use crate::vmx::{Vmx, parse_bios_uuid};
use crate::{
    Bus, Cdrom, Disk, FORMAT, Firmware, Nic, NicModel, Vm, VmConfig, VmwareOrigin, format_mac,
    format_uuid, parse_mac,
};

/// What an import made, and what the user should know about it.
#[derive(Debug)]
pub struct Imported {
    pub vm: Vm,
    pub warnings: Vec<String>,
}

/// Disk controllers VMware has, in the order vmmc attaches their disks.
const CONTROLLERS: [(&str, u32, u32); 4] = [
    ("nvme", 4, 64),
    ("sata", 4, 30),
    ("scsi", 4, 16),
    ("ide", 2, 2),
];
const MAX_NICS: u32 = 10;

/// What a guest OS id means for vmmc's presets.
fn os_from_guest(guest: &str) -> &'static str {
    let guest = guest.to_ascii_lowercase();
    if guest.starts_with("windows11") {
        "windows11"
    } else if guest.starts_with("windows") {
        // VMware calls Windows 10 "windows9".
        "windows10"
    } else if guest.contains("ubuntu") {
        "ubuntu"
    } else if [
        "linux", "debian", "rhel", "centos", "fedora", "suse", "oracle", "rocky", "alma", "other5",
        "other6",
    ]
    .iter()
    .any(|name| guest.contains(name))
    {
        "linux"
    } else {
        "other"
    }
}

/// A disk or disc of the VM, in vmmc's terms.
struct Device {
    bus: Bus,
    /// Where VMware has it (absolute).
    path: PathBuf,
    cdrom: bool,
    connected: bool,
    name: String,
}

fn devices(vmx: &Vmx, dir: &Path, warnings: &mut Vec<String>) -> Vec<Device> {
    let mut devices = Vec::new();
    for (controller, count, units) in CONTROLLERS {
        for index in 0..count {
            let prefix = format!("{controller}{index}");
            if vmx.flag(&format!("{prefix}.present")) != Some(true) {
                continue;
            }
            for unit in 0..units {
                let key = |name: &str| format!("{prefix}:{unit}.{name}");
                if vmx.flag(&key("present")) != Some(true) {
                    continue;
                }
                let Some(file) = vmx.get(&key("fileName")) else {
                    continue;
                };
                let kind = vmx
                    .get(&key("deviceType"))
                    .unwrap_or("disk")
                    .to_ascii_lowercase();
                let name = format!("{prefix}:{unit}");
                let cdrom = kind.contains("cdrom");
                if cdrom && kind != "cdrom-image" {
                    warnings.push(format!(
                        "{name}: the host's own DVD drive isn't supported yet; the VM gets an empty drive"
                    ));
                }
                let bus = match controller {
                    "nvme" => Bus::Nvme,
                    "sata" => Bus::Sata,
                    _ => {
                        if !cdrom {
                            warnings.push(format!(
                                "{name}: a {} disk is attached to SATA here; Windows may not boot from it (\"INACCESSIBLE_BOOT_DEVICE\") until its storahci driver is enabled",
                                controller.to_ascii_uppercase()
                            ));
                        }
                        Bus::Sata
                    }
                };
                if let Some(mode) = vmx.get(&key("mode"))
                    && mode.contains("nonpersistent")
                {
                    warnings.push(format!(
                        "{name}: an independent-nonpersistent disk; vmmc keeps its changes in its delta"
                    ));
                }
                devices.push(Device {
                    bus,
                    path: dir.join(file),
                    cdrom,
                    connected: kind == "cdrom-image"
                        && vmx.flag(&key("startConnected")) != Some(false),
                    name,
                });
            }
        }
    }
    devices
}

/// Whether a disk holds a BitLocker volume: a partition (GPT or MBR) whose boot sector
/// says `-FVE-FS-`.
fn has_bitlocker(disk: &dyn BlockBackend) -> bool {
    let sector = |lba: u64| -> Option<[u8; 512]> {
        let mut buf = [0u8; 512];
        let offset = lba.checked_mul(512)?;
        (offset + 512 <= disk.len() && disk.read_at(&mut buf, offset).is_ok()).then_some(buf)
    };
    let bitlocker = |lba: u64| sector(lba).is_some_and(|s| &s[3..11] == b"-FVE-FS-");
    let Some(mbr) = sector(0) else {
        return false;
    };
    let mut starts = Vec::new();
    if let Some(gpt) = sector(1).filter(|s| &s[..8] == b"EFI PART") {
        let u32_at = |at: usize| u32::from_le_bytes(gpt[at..at + 4].try_into().unwrap());
        let table = u64::from_le_bytes(gpt[72..80].try_into().unwrap());
        let count = u32_at(80).min(256);
        let size = u32_at(84);
        if (128..=4096).contains(&size) {
            for index in 0..u64::from(count) {
                let at = index * u64::from(size);
                let Some(entry_sector) = sector(table + at / 512) else {
                    break;
                };
                let within = (at % 512) as usize;
                if within + 48 > 512 {
                    continue;
                }
                let entry = &entry_sector[within..within + 48];
                if entry[..16].iter().any(|&b| b != 0) {
                    starts.push(u64::from_le_bytes(entry[32..40].try_into().unwrap()));
                }
            }
        }
    } else if mbr[510..512] == [0x55, 0xaa] {
        for index in 0..4 {
            let entry = &mbr[446 + index * 16..462 + index * 16];
            if entry[4] != 0 {
                starts.push(u64::from(u32::from_le_bytes(
                    entry[8..12].try_into().unwrap(),
                )));
            }
        }
    }
    starts.into_iter().any(bitlocker)
}

/// A file name for a delta disk in `dir`, unused so far.
fn delta_name(dir: &Path, stem: &str, taken: &[String]) -> String {
    let mut name = format!("{stem}-vmmc.vmdk");
    let mut number = 2;
    while taken.contains(&name) || dir.join(&name).exists() {
        name = format!("{stem}-vmmc{number}.vmdk");
        number += 1;
    }
    name
}

/// Imports the VMware VM at `vmx_path`. Its vmmc files go in `state_dir`, by default a
/// `vmmc` folder beside the `.vmx`.
pub fn import(vmx_path: &Path, state_dir: Option<&Path>) -> Result<Imported, String> {
    let vmx_path = std::path::absolute(vmx_path).map_err(|e| e.to_string())?;
    let text =
        fs::read(&vmx_path).map_err(|e| format!("can't read {}: {e}", vmx_path.display()))?;
    let vmx = Vmx::parse(&String::from_utf8_lossy(&text))
        .map_err(|e| format!("{}: {e}", vmx_path.display()))?;
    let dir = vmx_path.parent().unwrap_or(Path::new(".")).to_owned();

    // What vmmc can't take.
    if vmx.get("encryption.keySafe").is_some() || vmx.get("encryption.data").is_some() {
        return Err("this VM is encrypted: remove its encryption in VMware first (VM > Settings > Options > Access Control)".into());
    }
    if !vmx
        .get("firmware")
        .is_some_and(|f| f.eq_ignore_ascii_case("efi"))
    {
        return Err("this VM boots with BIOS; vmmc only has UEFI firmware so far".into());
    }
    if vmx.get("checkpoint.vmState").is_some_and(|s| !s.is_empty()) {
        return Err(
            "this VM is suspended in VMware: resume it there and shut it down first".into(),
        );
    }
    let mut lock = vmx_path.as_os_str().to_owned();
    lock.push(".lck");
    if Path::new(&lock).exists() {
        return Err("VMware has this VM open: close it in VMware first".into());
    }

    let state_dir = match state_dir {
        Some(dir) => std::path::absolute(dir).map_err(|e| e.to_string())?,
        None => dir.join("vmmc"),
    };
    if let Ok(entries) = fs::read_dir(&state_dir) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|e| e == crate::EXTENSION)
            {
                return Err(format!(
                    "already imported: {} (delete {} to import again)",
                    entry.path().display(),
                    state_dir.display()
                ));
            }
        }
    }

    let mut warnings = Vec::new();
    let name = vmx
        .get("displayName")
        .filter(|n| !n.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            vmx_path
                .file_stem()
                .map_or("Imported VM".into(), |s| s.to_string_lossy().into_owned())
        });
    let file_name: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c < ' ' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let guest = vmx.get("guestOS").unwrap_or("");
    let os = os_from_guest(guest);
    let memory = vmx
        .number("memsize")
        .ok_or("the .vmx doesn't say how much memory the VM has")?;
    let cpus = vmx
        .number("numvcpus")
        .unwrap_or(1)
        .clamp(1, u64::from(crate::MAX_CPUS)) as u8;
    // VMware's uuid.bios is in SMBIOS order; vmmc keeps UUIDs in RFC order.
    let uuid = vmx
        .get("uuid.bios")
        .and_then(parse_bios_uuid)
        .map(|mut raw| {
            raw[0..4].reverse();
            raw[4..6].reverse();
            raw[6..8].reverse();
            raw
        });
    let mut config = VmConfig::new(&name, os, cpus, memory);
    if let Some(uuid) = uuid {
        config.uuid = format_uuid(&uuid);
    }
    config.format = FORMAT;
    config.firmware = Firmware {
        secure_boot: vmx.flag("uefi.secureBoot.enabled") == Some(true),
        tpm: vmx.flag("vtpm.present") == Some(true),
    };
    if config.firmware.tpm {
        warnings.push("the VM gets a new TPM: what VMware's TPM held (BitLocker keys, Windows Hello) doesn't come along".into());
    }

    // Network adapters, on vmmc's NAT.
    config.nics.clear();
    for index in 0..MAX_NICS {
        let key = |name: &str| format!("ethernet{index}.{name}");
        if vmx.flag(&key("present")) != Some(true) {
            continue;
        }
        let address = match vmx
            .get(&key("addressType"))
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("static") => vmx.get(&key("address")),
            _ => vmx.get(&key("generatedAddress")),
        };
        let mac = address.and_then(parse_mac).unwrap_or_else(|| {
            let uuid = config.uuid_bytes();
            [
                0x52,
                0x54,
                0x00,
                uuid[13],
                uuid[14],
                uuid[15].wrapping_add(index as u8),
            ]
        });
        let model = vmx
            .get(&key("virtualDev"))
            .unwrap_or("e1000")
            .to_ascii_lowercase();
        if model != "e1000e" {
            warnings.push(format!(
                "ethernet{index}: VMware's {model} becomes an e1000e; the guest sees a new network adapter (settings like a static IP don't carry over)"
            ));
        }
        if let Some(kind) = vmx.get(&key("connectionType"))
            && !kind.eq_ignore_ascii_case("nat")
        {
            warnings.push(format!(
                "ethernet{index}: {kind} networking isn't supported yet; it's on NAT"
            ));
        }
        config.nics.push(Nic {
            model: NicModel::E1000e,
            mac: format_mac(&mac),
        });
    }

    // What doesn't come along.
    if vmx.flag("sound.present") == Some(true) {
        warnings.push("no sound card yet".into());
    }
    if vmx.flag("mks.enable3d") == Some(true) {
        warnings.push("no 3D acceleration: Windows uses vmmc's 2D display".into());
    }
    if vmx.flag("vhv.enable") == Some(true) {
        warnings.push("nested virtualization isn't available".into());
    }
    if vmx
        .keys_with_prefix("sharedfolder")
        .any(|k| k.ends_with(".present"))
    {
        warnings.push("shared folders aren't supported yet".into());
    }
    if (0..4).any(|i| vmx.flag(&format!("serial{i}.present")) == Some(true)) {
        warnings.push("serial ports aren't connected to anything but vmmc's console".into());
    }
    if vmx.flag("cleanShutdown") == Some(false) {
        warnings
            .push("VMware didn't shut this VM down cleanly; the guest may check its disks".into());
    }
    if os.starts_with("windows") {
        warnings.push("Windows sees new hardware: it may ask to be activated again".into());
        if vmx.get("tools.syncTime").is_some()
            || vmx.get("toolsInstallManager.updateCounter").is_some()
        {
            warnings.push("VMware Tools reports an error at logon under vmmc (it needs VMware's hypervisor); uninstall it once you've moved over".into());
        }
    }

    let devices = devices(&vmx, &dir, &mut warnings);
    if !devices.iter().any(|d| !d.cdrom) {
        return Err("the .vmx names no disks".into());
    }
    let made_dir = !state_dir.exists();
    fs::create_dir_all(&state_dir)
        .map_err(|e| format!("can't create {}: {e}", state_dir.display()))?;
    let result = (|| {
        let mut deltas: Vec<String> = Vec::new();
        let mut bases = Vec::new();
        for device in &devices {
            if device.cdrom {
                let path = (device.connected && device.path.is_file())
                    .then(|| device.path.display().to_string());
                if device.connected && path.is_none() {
                    warnings.push(format!(
                        "{}: {} isn't there; the drive is empty",
                        device.name,
                        device.path.display()
                    ));
                }
                config.cdroms.push(Cdrom { path });
                continue;
            }
            let base = disk::vmdk::Vmdk::open(&device.path, true)
                .map_err(|e| format!("{}: {e}", device.name))?;
            if has_bitlocker(&base) {
                warnings.push(format!(
                    "{}: holds a BitLocker volume; with a new TPM, Windows will ask for its recovery key",
                    device.name
                ));
            }
            drop(base);
            let stem = device
                .path
                .file_stem()
                .map_or("disk".into(), |s| s.to_string_lossy().into_owned());
            let delta = delta_name(&state_dir, &stem, &deltas);
            disk::create_delta(&state_dir.join(&delta), &device.path)
                .map_err(|e| format!("{}: can't make vmmc's delta disk: {e}", device.name))?;
            config.disks.push(Disk {
                path: delta.clone(),
                bus: device.bus,
                read_only: false,
            });
            deltas.push(delta);
            bases.push(device.path.display().to_string());
        }
        config.vmware = Some(VmwareOrigin {
            vmx: vmx_path.display().to_string(),
            base_disks: bases,
        });
        config.notes = format!(
            "Imported from VMware ({}) on {}.",
            vmx_path.display(),
            crate::now()
        );
        config.validate()?;
        let vm = Vm {
            path: state_dir.join(format!("{file_name}.{}", crate::EXTENSION)),
            config: config.clone(),
        };
        vm.save().map_err(|e| e.to_string())?;
        Ok(vm)
    })();
    match result {
        Ok(vm) => Ok(Imported { vm, warnings }),
        Err(e) => {
            // Leave nothing half made.
            if made_dir {
                let _ = fs::remove_dir_all(&state_dir);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vmware_vm(dir: &Path, extra: &str) -> PathBuf {
        disk::create_sparse(&dir.join("Win.vmdk"), 4 << 20).unwrap();
        disk::create_sparse(&dir.join("Data.vmdk"), 1 << 20).unwrap();
        fs::write(dir.join("Win.iso"), b"").unwrap();
        let vmx = dir.join("Win.vmx");
        fs::write(
            &vmx,
            format!(
                ".encoding = \"UTF-8\"\ndisplayName = \"Win 10\"\nfirmware = \"efi\"\nguestOS = \"windows9-64\"\nnumvcpus = \"4\"\nmemsize = \"4096\"\nnvme0.present = \"TRUE\"\nnvme0:0.present = \"TRUE\"\nnvme0:0.fileName = \"Win.vmdk\"\nsata0.present = \"TRUE\"\nsata0:0.present = \"TRUE\"\nsata0:0.fileName = \"Data.vmdk\"\nsata0:1.present = \"TRUE\"\nsata0:1.deviceType = \"cdrom-image\"\nsata0:1.fileName = \"Win.iso\"\nethernet0.present = \"TRUE\"\nethernet0.virtualDev = \"e1000e\"\nethernet0.connectionType = \"nat\"\nethernet0.addressType = \"generated\"\nethernet0.generatedAddress = \"00:0c:29:df:26:c1\"\nuuid.bios = \"56 4d b7 47 33 14 81 8d-e5 c4 23 db 1c df 26 c1\"\nsound.present = \"TRUE\"\n{extra}"
            ),
        )
        .unwrap();
        vmx
    }

    fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn imports_in_place_without_touching_vmware_s_files() {
        let dir = tempfile::tempdir().unwrap();
        let vmx = vmware_vm(dir.path(), "");
        let before = files(dir.path());
        let imported = import(&vmx, None).unwrap();
        let vm = &imported.vm;
        assert_eq!(vm.path, dir.path().join("vmmc").join("Win 10.vmmc"));
        let config = &Vm::load(&vm.path).unwrap().config;
        assert_eq!(
            (config.cpus, config.memory_mib, config.os.as_str()),
            (4, 4096, "windows10")
        );
        assert_eq!(config.uuid, "47b74d56-1433-8d81-e5c4-23db1cdf26c1");
        assert_eq!(config.nics.len(), 1);
        assert_eq!(config.nics[0].mac, "00:0c:29:df:26:c1");
        assert_eq!(config.disks.len(), 2);
        assert_eq!(config.disks[0].bus, Bus::Nvme);
        assert_eq!(config.disks[1].bus, Bus::Sata);
        assert_eq!(config.cdroms.len(), 1);
        assert!(!config.firmware.secure_boot && !config.firmware.tpm);
        assert!(imported.warnings.iter().any(|w| w.contains("sound")));
        // Writes go to the delta; VMware's files stay as they were.
        {
            let disk = disk::open(&vm.resolve(&config.disks[0].path), false).unwrap();
            disk.write_at(&[0xaa; 4096], 0).unwrap();
        }
        assert_eq!(files(dir.path()), before);
        assert!(import(&vmx, None).unwrap_err().contains("already imported"));
    }

    #[test]
    fn refuses_what_it_cannot_run() {
        let cases = [
            ("encryption.keySafe = \"x\"\n", "encrypted"),
            ("checkpoint.vmState = \"Win.vmss\"\n", "suspended"),
        ];
        for (extra, why) in cases {
            let dir = tempfile::tempdir().unwrap();
            let vmx = vmware_vm(dir.path(), extra);
            let error = import(&vmx, None).unwrap_err();
            assert!(error.contains(why), "{error}");
            assert!(!dir.path().join("vmmc").exists(), "nothing left behind");
        }
        let dir = tempfile::tempdir().unwrap();
        let vmx = vmware_vm(dir.path(), "");
        let text = fs::read_to_string(&vmx)
            .unwrap()
            .replace("firmware = \"efi\"\n", "");
        fs::write(&vmx, text).unwrap();
        assert!(import(&vmx, None).unwrap_err().contains("BIOS"));
        let dir = tempfile::tempdir().unwrap();
        let vmx = vmware_vm(dir.path(), "");
        fs::create_dir(dir.path().join("Win.vmx.lck")).unwrap();
        assert!(import(&vmx, None).unwrap_err().contains("open"));
    }

    #[test]
    fn a_state_dir_elsewhere_keeps_the_vmware_folder_clean() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let vmx = vmware_vm(dir.path(), "");
        let state = elsewhere.path().join("state");
        let imported = import(&vmx, Some(&state)).unwrap();
        assert!(imported.vm.path.starts_with(&state));
        assert!(!dir.path().join("vmmc").exists());
        let config = &imported.vm.config;
        assert_eq!(
            config.vmware.as_ref().unwrap().base_disks[0],
            dir.path().join("Win.vmdk").display().to_string()
        );
    }

    #[test]
    fn finds_bitlocker_volumes() {
        let disk = disk::MemDisk::new(1 << 20, false);
        // GPT: header at LBA 1, entries at LBA 2, one partition at LBA 34.
        let mut header = [0u8; 512];
        header[..8].copy_from_slice(b"EFI PART");
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&4u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        disk.write_at(&header, 512).unwrap();
        let mut entry = [0u8; 128];
        entry[0] = 0xa2; // any type
        entry[32..40].copy_from_slice(&34u64.to_le_bytes());
        disk.write_at(&entry, 1024).unwrap();
        assert!(!has_bitlocker(&disk));
        let mut boot = [0u8; 512];
        boot[3..11].copy_from_slice(b"-FVE-FS-");
        disk.write_at(&boot, 34 * 512).unwrap();
        assert!(has_bitlocker(&disk));
    }

    #[test]
    fn guest_ids_map_to_presets() {
        assert_eq!(os_from_guest("windows9-64"), "windows10");
        assert_eq!(os_from_guest("windows11-64"), "windows11");
        assert_eq!(os_from_guest("ubuntu-64"), "ubuntu");
        assert_eq!(os_from_guest("debian12-64"), "linux");
        assert_eq!(os_from_guest("freebsd14-64"), "other");
    }
}
