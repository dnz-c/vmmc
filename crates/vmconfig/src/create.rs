//! Making a new VM: its folder, config and disk.

use std::fs;
use std::path::{Path, PathBuf};

use crate::presets::Preset;
use crate::{Cdrom, Disk, EjectOn, Install, InstallStep, NVRAM_FILE, TPM_STATE_FILE, Vm, VmConfig};

/// The answer disc Easy Install makes, in the VM's folder.
pub const ANSWER_DISC: &str = "easy-install.iso";

/// Easy Install: the guest OS installs from its ISO with no questions asked.
#[derive(Clone, Debug)]
pub enum EasyInstall {
    Windows {
        /// The edition, as Setup names it: `Windows 11 Pro`.
        edition: String,
        user: String,
        password: String,
        /// Setup's language; by default the ISO's.
        language: Option<String>,
        /// Windows' name for it, like `W. Europe Standard Time`.
        time_zone: String,
    },
    Ubuntu {
        user: String,
        real_name: String,
        password: String,
        /// An X keyboard layout, like `us`.
        keyboard: String,
        /// Like `en_US.UTF-8`.
        locale: String,
        /// Like `Europe/Berlin`.
        time_zone: String,
    },
}

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
    /// Install the guest unattended from `iso`.
    pub easy_install: Option<EasyInstall>,
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
            install_media: new.easy_install.is_some(),
        });
    }
    if let Some(install) = &new.easy_install {
        let iso = new.iso.ok_or("Easy Install needs the OS's ISO")?;
        let (disc, install) = answer_disc(new.name, iso, install)?;
        fs::write(dir.join(ANSWER_DISC), disc)
            .map_err(|e| format!("can't write the answer disc: {e}"))?;
        config.cdroms.push(Cdrom {
            path: Some(ANSWER_DISC.into()),
            install_media: true,
        });
        config.install = Some(install);
    }
    config.validate()?;
    let vm = Vm {
        path: dir.join(format!("{}.{}", new.name, crate::EXTENSION)),
        config,
    };
    vm.save().map_err(|e| e.to_string())?;
    Ok(vm)
}

/// The answer disc for `install`, and what the first boot does with it.
fn answer_disc(
    name: &str,
    iso: &Path,
    install: &EasyInstall,
) -> Result<(Vec<u8>, Install), String> {
    match install {
        EasyInstall::Windows {
            edition,
            user,
            password,
            language,
            time_zone,
        } => {
            let key = media::windows::generic_key(edition)
                .ok_or_else(|| format!("no generic key for {edition}"))?;
            let language = match language {
                Some(language) => language.clone(),
                None => media::windows::iso_language(iso)?,
            };
            let xml = media::windows::answer_file(&media::windows::Setup {
                image: edition,
                product_key: key,
                language: &language,
                computer_name: &media::windows::computer_name(name),
                time_zone,
                user,
                password,
                auto_logons: 1,
                specialize: &[],
                first_logon: &[],
            });
            let xml = xml.replace('\n', "\r\n");
            let disc =
                media::iso::build("VMMC_ANSWER", &[("autounattend.xml", xml.as_bytes())], None);
            // The disc's boot loader waits a moment for a key before giving up on it.
            let install = Install {
                steps: vec![InstallStep {
                    expect: media::windows::CD_PROMPT.into(),
                    send: "\r".into(),
                }],
                eject_on: Some(EjectOn::Reset),
            };
            Ok((disc, install))
        }
        EasyInstall::Ubuntu {
            user,
            real_name,
            password,
            keyboard,
            locale,
            time_zone,
        } => {
            let user_data = media::ubuntu::user_data(&media::ubuntu::Setup {
                user,
                real_name,
                password,
                hostname: &media::ubuntu::hostname(name),
                locale,
                keyboard,
                time_zone,
            });
            let meta_data =
                media::ubuntu::meta_data(&format!("vmmc-{}", media::ubuntu::hostname(name)));
            let disc = media::iso::build(
                "CIDATA",
                &[
                    ("user-data", user_data.as_bytes()),
                    ("meta-data", meta_data.as_bytes()),
                ],
                None,
            );
            let install = Install {
                steps: media::ubuntu::install_steps()
                    .into_iter()
                    .map(|(expect, send)| InstallStep { expect, send })
                    .collect(),
                eject_on: Some(EjectOn::PowerOff),
            };
            Ok((disc, install))
        }
    }
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
            easy_install: None,
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
            easy_install: None,
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
            easy_install: None,
        })
        .unwrap();
        assert_eq!(vm.config.uuid, "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(vm.config.nics[0].mac, "52:54:00:dd:ee:ff");
        assert_eq!(fs::read(vm.nvram()).unwrap(), [7u8; 16]);
        assert!(!vm.tpm_state().exists());
        assert_eq!(vm.resolve(&vm.config.disks[0].path), disk);
    }

    #[test]
    fn easy_install_adds_an_answer_disc_and_first_boot_steps() {
        let root = tempfile::tempdir().unwrap();
        let iso = root.path().join("os.iso");
        fs::write(&iso, b"").unwrap();
        let new = |name, os, install| NewVm {
            name,
            parent_dir: root.path(),
            preset: presets::find(os).unwrap(),
            cpus: None,
            memory_mib: None,
            disk_bytes: Some(1 << 20),
            iso: Some(&iso),
            adopt: None,
            existing_disk: None,
            easy_install: Some(install),
        };
        let ubuntu = create_vm(&new(
            "Ubuntu",
            "ubuntu",
            EasyInstall::Ubuntu {
                user: "anna".into(),
                real_name: "Anna".into(),
                password: "pw".into(),
                keyboard: "us".into(),
                locale: "en_US.UTF-8".into(),
                time_zone: "Etc/UTC".into(),
            },
        ))
        .unwrap();
        let config = &Vm::load(&ubuntu.path).unwrap().config;
        assert_eq!(config.cdroms.len(), 2);
        assert!(config.cdroms.iter().all(|cd| cd.install_media));
        assert_eq!(config.cdroms[1].path.as_deref(), Some(ANSWER_DISC));
        assert!(ubuntu.dir().join(ANSWER_DISC).is_file());
        let install = config.install.as_ref().unwrap();
        assert_eq!(install.eject_on, Some(EjectOn::PowerOff));
        assert_eq!(install.steps[0].send, "c");

        let windows = create_vm(&new(
            "Windows",
            "windows11",
            EasyInstall::Windows {
                edition: "Windows 11 Pro".into(),
                user: "anna".into(),
                password: "pw".into(),
                language: Some("en-US".into()),
                time_zone: "UTC".into(),
            },
        ))
        .unwrap();
        let install = windows.config.install.as_ref().unwrap();
        assert_eq!(install.eject_on, Some(EjectOn::Reset), "Windows restarts");
        assert_eq!(install.steps[0].expect, media::windows::CD_PROMPT);
        let disc = fs::read(windows.dir().join(ANSWER_DISC)).unwrap();
        let text = String::from_utf8_lossy(&disc);
        assert!(text.contains("<Name>anna</Name>") && text.contains("\r\n"));
    }

    #[test]
    fn names_must_be_file_names() {
        assert!(check_name("Ubuntu 26.04").is_ok());
        for bad in ["", " x", "x.", "a/b", "a:b", "CON", "com1", "a\tb"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
