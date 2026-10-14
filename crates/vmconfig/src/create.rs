//! Making a new VM: its folder, config and disk.

use std::fs;
use std::path::{Path, PathBuf};

use crate::presets::Preset;
use crate::{Cdrom, Disk, NVRAM_FILE, TPM_STATE_FILE, Vm, VmConfig};

pub struct NewVm<'a> {
    pub name: &'a str,
    /// The VM's folder is made in here, named after the VM.
    pub parent_dir: &'a Path,
    pub preset: &'static Preset,
    /// What the preset says, unless given.
    pub cpus: Option<u8>,
    pub memory_mib: Option<u64>,
    pub disk_bytes: Option<u64>,
    /// An installation disc for the VM's DVD drive.
    pub iso: Option<&'a Path>,
    /// The state directory of a VM run with `vmmc-vmx run --vm-dir`: its UUID, MAC,
    /// UEFI variables and TPM move into the new VM, which is then the same machine to
    /// its guest.
    pub adopt: Option<&'a Path>,
    /// An existing disk to use, rather than a new one (it stays where it is).
    pub existing_disk: Option<&'a Path>,
}

/// A name that works as a file name on Windows.
pub fn check_name(name: &str) -> Result<(), String> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if name.trim().is_empty() {
        return Err("the VM needs a name".into());
    }
    if name != name.trim() || name.ends_with('.') {
        return Err("a VM's name can't start or end with a space, or end with a dot".into());
    }
    if let Some(bad) = name
        .chars()
        .find(|&c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c < ' ')
    {
        return Err(format!("a VM's name can't contain {bad:?}"));
    }
    if RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(format!("{name} is reserved by Windows"));
    }
    if name.chars().count() > 100 {
        return Err("that name is too long".into());
    }
    Ok(())
}

/// Makes the VM's folder, config and (unless one's given) its disk. Nothing is
/// overwritten: an existing folder must be empty.
pub fn create_vm(new: &NewVm) -> Result<Vm, String> {
    check_name(new.name)?;
    let dir = new.parent_dir.join(new.name);
    if dir.exists()
        && fs::read_dir(&dir)
            .map_err(|e| format!("can't read {}: {e}", dir.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} exists and isn't empty", dir.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let preset = new.preset;
    let mut config = VmConfig::new(
        new.name,
        preset.id,
        new.cpus.unwrap_or(preset.cpus),
        new.memory_mib.unwrap_or(preset.memory_mib),
    );
    config.firmware.secure_boot = preset.secure_boot;
    config.firmware.tpm = preset.tpm;
    config.nics[0].model = preset.nic;
    if let Some(old) = new.adopt {
        adopt(&mut config, old, &dir)?;
    }
    let disk_path = match new.existing_disk {
        Some(existing) => absolute(existing)?,
        None => {
            let file = format!("{}.vmdk", new.name);
            let bytes = new.disk_bytes.unwrap_or(preset.disk_gib << 30);
            disk::create_sparse(&dir.join(&file), bytes)
                .map_err(|e| format!("can't create the disk: {e}"))?;
            PathBuf::from(file)
        }
    };
    config.disks.push(Disk {
        path: disk_path.display().to_string(),
        bus: preset.bus,
        read_only: false,
    });
    if let Some(iso) = new.iso {
        config.cdroms.push(Cdrom {
            path: Some(absolute(iso)?.display().to_string()),
        });
    }
    config.validate()?;
    let vm = Vm {
        path: dir.join(format!("{}.{}", new.name, crate::EXTENSION)),
        config,
    };
    vm.save().map_err(|e| e.to_string())?;
    Ok(vm)
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Takes over an old `--vm-dir`: its `uuid` and `mac` files, and copies of its UEFI
/// variables and TPM state.
fn adopt(config: &mut VmConfig, old: &Path, dir: &Path) -> Result<(), String> {
    let read = |name: &str| fs::read_to_string(old.join(name)).ok();
    if let Some(uuid) = read("uuid") {
        let uuid = crate::parse_uuid(uuid.trim())
            .ok_or_else(|| format!("{} doesn't hold a UUID", old.join("uuid").display()))?;
        config.uuid = crate::format_uuid(&uuid);
    }
    if let Some(mac) = read("mac") {
        let mac = crate::parse_mac(mac.trim())
            .ok_or_else(|| format!("{} doesn't hold a MAC", old.join("mac").display()))?;
        config.nics[0].mac = crate::format_mac(&mac);
    }
    for name in [NVRAM_FILE, TPM_STATE_FILE] {
        let from = old.join(name);
        if from.is_file() {
            fs::copy(&from, dir.join(name))
                .map_err(|e| format!("can't copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;

    #[test]
    fn makes_a_folder_with_config_and_disk() {
        let root = tempfile::tempdir().unwrap();
        let iso = root.path().join("install.iso");
        fs::write(&iso, b"").unwrap();
        let vm = create_vm(&NewVm {
            name: "Windows 11",
            parent_dir: root.path(),
            preset: presets::find("windows11").unwrap(),
            cpus: None,
            memory_mib: Some(6144),
            disk_bytes: Some(1 << 30),
            iso: Some(&iso),
            adopt: None,
            existing_disk: None,
        })
        .unwrap();
        let dir = root.path().join("Windows 11");
        assert_eq!(vm.path, dir.join("Windows 11.vmmc"));
        let back = Vm::load(&vm.path).unwrap();
        assert_eq!((back.config.cpus, back.config.memory_mib), (4, 6144));
        assert!(back.config.firmware.tpm && back.config.firmware.secure_boot);
        assert_eq!(back.config.disks[0].path, "Windows 11.vmdk");
        assert!(dir.join("Windows 11.vmdk").is_file());
        assert_eq!(
            back.config.cdroms[0].path.as_deref(),
            Some(&*iso.display().to_string())
        );
        assert!(back.config.created.is_some());
        // Never over an existing VM.
        let again = create_vm(&NewVm {
            name: "Windows 11",
            parent_dir: root.path(),
            preset: presets::find("windows11").unwrap(),
            cpus: None,
            memory_mib: None,
            disk_bytes: Some(1 << 30),
            iso: None,
            adopt: None,
            existing_disk: None,
        });
        assert!(again.unwrap_err().contains("isn't empty"));
    }

    #[test]
    fn adopts_an_old_vm_dir() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("uuid"), "00112233-4455-6677-8899-aabbccddeeff\n").unwrap();
        fs::write(old.join("mac"), "52:54:00:dd:ee:ff\n").unwrap();
        fs::write(old.join("nvram.fd"), [7u8; 16]).unwrap();
        let disk = root.path().join("disk.raw");
        fs::write(&disk, [0u8; 512]).unwrap();
        let vm = create_vm(&NewVm {
            name: "Adopted",
            parent_dir: root.path(),
            preset: presets::find("linux").unwrap(),
            cpus: None,
            memory_mib: None,
            disk_bytes: None,
            iso: None,
            adopt: Some(&old),
            existing_disk: Some(&disk),
        })
        .unwrap();
        assert_eq!(vm.config.uuid, "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(vm.config.nics[0].mac, "52:54:00:dd:ee:ff");
        assert_eq!(fs::read(vm.nvram()).unwrap(), [7u8; 16]);
        assert!(!vm.tpm_state().exists());
        assert_eq!(vm.resolve(&vm.config.disks[0].path), disk);
    }

    #[test]
    fn names_must_be_file_names() {
        assert!(check_name("Ubuntu 26.04").is_ok());
        for bad in ["", " x", "x.", "a/b", "a:b", "CON", "com1", "a\tb"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
