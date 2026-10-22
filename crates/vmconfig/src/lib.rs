//! vmmc's VM files.
//!
//! A VM is a folder with a `.vmmc` file (TOML, [`VmConfig`]) that says what the machine
//! is, beside the state that has to survive restarts: `nvram.fd` (UEFI variables),
//! `tpm-state.bin` and its disks. Paths in the config are relative to the folder, so a
//! VM moves as a whole. The [`library`] lists the VMs the app shows.
//!
//! ```toml
//! format = 1
//! name = "Windows 11"
//! uuid = "6f1c2a4e-8d2b-4c11-9a3e-1b2c3d4e5f60"
//! os = "windows11"
//! cpus = 4
//! memory_mib = 8192
//!
//! [firmware]
//! secure_boot = true
//! tpm = true
//!
//! [[disk]]
//! path = "Windows 11.vmdk"
//! bus = "nvme"
//!
//! [[nic]]
//! model = "e1000e"
//! mac = "52:54:00:4e:5f:60"
//! ```

pub mod clone;
pub mod create;
pub mod import;
pub mod library;
pub mod presets;
pub mod snapshots;
mod time;
pub mod vmx;

use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use time::now;

/// A VM file's extension.
pub const EXTENSION: &str = "vmmc";
/// The config format this vmmc reads and writes.
pub const FORMAT: u32 = 1;
/// The UEFI variable store, in the VM's folder.
pub const NVRAM_FILE: &str = "nvram.fd";
/// The TPM's state, in the VM's folder.
pub const TPM_STATE_FILE: &str = "tpm-state.bin";
pub const MAX_CPUS: u8 = 64;
pub const MIN_MEMORY_MIB: u64 = 32;
pub const MAX_MEMORY_MIB: u64 = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("can't write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// What a VM is, as its `.vmmc` file says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub format: u32,
    pub name: String,
    pub uuid: String,
    /// The OS preset it was made with (`windows11`, `ubuntu`, ...); informational.
    #[serde(default)]
    pub os: String,
    pub cpus: u8,
    pub memory_mib: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// When the VM was made, and last started (RFC 3339, UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_powered_on: Option<String>,
    #[serde(default)]
    pub firmware: Firmware,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub advanced: Advanced,
    /// Where an imported VMware VM came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmware: Option<VmwareOrigin>,
    /// Easy Install's steps for the first boot, until they're done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<Install>,
    #[serde(default, rename = "disk", skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<Disk>,
    #[serde(default, rename = "cdrom", skip_serializing_if = "Vec::is_empty")]
    pub cdroms: Vec<Cdrom>,
    #[serde(default, rename = "nic", skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<Nic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firmware {
    /// UEFI Secure Boot, with Microsoft's keys enrolled.
    #[serde(default = "yes")]
    pub secure_boot: bool,
    /// A TPM 2.0, its state in `tpm-state.bin`.
    #[serde(default = "yes")]
    pub tpm: bool,
}

impl Default for Firmware {
    fn default() -> Self {
        Self {
            secure_boot: true,
            tpm: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Display {
    /// `WxH`: the resolution the monitor prefers, which the firmware boots in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advanced {
    /// Hyper-V enlightenments for the guest.
    #[serde(default = "yes")]
    pub enlightenments: bool,
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            enlightenments: true,
        }
    }
}

fn yes() -> bool {
    true
}

/// What Easy Install does at the first boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Install {
    /// Typed on the serial console, in order: wait for `expect`, then type `send`.
    /// Gone once done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<InstallStep>,
    /// When to take out the install discs (and delete the answer disc, which holds
    /// the password) once the steps are done, and start the guest again from its disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eject_on: Option<EjectOn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EjectOn {
    /// The installer powers off when it's done (Ubuntu's autoinstall).
    PowerOff,
    /// The guest restarts once the install media were read (Windows Setup, which
    /// copies what it needs to the disk before its first restart).
    Reset,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallStep {
    pub expect: String,
    pub send: String,
}

/// A VMware VM opened in place: vmmc writes to delta disks on top of VMware's, and
/// never to VMware's files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmwareOrigin {
    /// The `.vmx` it was imported from.
    pub vmx: String,
    /// VMware's disks beneath vmmc's deltas, in the order of the config's disks.
    pub base_disks: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    Nvme,
    Sata,
    Virtio,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    /// A `.vmdk` (or a raw image), relative to the VM's folder or absolute.
    pub path: String,
    pub bus: Bus,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cdrom {
    /// An ISO; none for an empty drive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// An install disc, which Easy Install takes out once it's done.
    #[serde(default, skip_serializing_if = "is_false")]
    pub install_media: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicModel {
    E1000e,
    Virtio,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nic {
    pub model: NicModel,
    pub mac: String,
}

impl VmConfig {
    /// A new VM's config: a fresh UUID, and one e1000e NIC with a MAC derived from it.
    pub fn new(name: &str, os: &str, cpus: u8, memory_mib: u64) -> Self {
        let uuid = random_uuid();
        let mac = [0x52, 0x54, 0x00, uuid[13], uuid[14], uuid[15]];
        Self {
            format: FORMAT,
            name: name.to_owned(),
            uuid: format_uuid(&uuid),
            os: os.to_owned(),
            cpus,
            memory_mib,
            notes: String::new(),
            created: Some(now()),
            last_powered_on: None,
            firmware: Firmware::default(),
            display: Display::default(),
            advanced: Advanced::default(),
            vmware: None,
            install: None,
            disks: Vec::new(),
            cdroms: Vec::new(),
            nics: vec![Nic {
                model: NicModel::E1000e,
                mac: format_mac(&mac),
            }],
        }
    }

    /// Checks what the type system can't; the message says what's wrong.
    pub fn validate(&self) -> Result<(), String> {
        if self.format > FORMAT {
            return Err(format!(
                "made by a newer vmmc (format {}; this one reads {FORMAT})",
                self.format
            ));
        }
        if self.name.trim().is_empty() {
            return Err("the VM has no name".into());
        }
        parse_uuid(&self.uuid).ok_or_else(|| format!("uuid {:?} isn't a UUID", self.uuid))?;
        if !(1..=MAX_CPUS).contains(&self.cpus) {
            return Err(format!("cpus must be 1 to {MAX_CPUS}"));
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return Err(format!(
                "memory_mib must be {MIN_MEMORY_MIB} to {MAX_MEMORY_MIB}"
            ));
        }
        if let Some(resolution) = &self.display.resolution {
            parse_resolution(resolution)
                .ok_or_else(|| format!("resolution {resolution:?} isn't WIDTHxHEIGHT"))?;
        }
        for disk in &self.disks {
            if disk.path.trim().is_empty() {
                return Err("a disk has no path".into());
            }
        }
        for nic in &self.nics {
            parse_mac(&nic.mac)
                .ok_or_else(|| format!("mac {:?} isn't a unicast MAC address", nic.mac))?;
        }
        Ok(())
    }

    pub fn uuid_bytes(&self) -> [u8; 16] {
        parse_uuid(&self.uuid).unwrap_or_default()
    }

    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.display
            .resolution
            .as_deref()
            .and_then(parse_resolution)
    }
}

/// A VM: its config, and where its file is.
#[derive(Clone, Debug)]
pub struct Vm {
    /// The `.vmmc` file.
    pub path: PathBuf,
    pub config: VmConfig,
}

impl Vm {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        let invalid = |message: String| Error::Invalid {
            path: path.to_owned(),
            message,
        };
        let config: VmConfig = toml::from_str(&text).map_err(|e| invalid(e.to_string()))?;
        config.validate().map_err(invalid)?;
        Ok(Self {
            path: path.to_owned(),
            config,
        })
    }

    /// Writes the config, replacing the file only once the new one is complete.
    pub fn save(&self) -> Result<(), Error> {
        let write = |source| Error::Write {
            path: self.path.clone(),
            source,
        };
        let text = toml::to_string(&self.config)
            .map_err(|e| write(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        let partial = self.path.with_extension("vmmc-partial");
        fs::write(&partial, text).map_err(write)?;
        fs::rename(&partial, &self.path).map_err(write)
    }

    /// The VM's folder.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// A path from the config: relative to the VM's folder, unless absolute.
    pub fn resolve(&self, path: &str) -> PathBuf {
        self.dir().join(path)
    }

    pub fn nvram(&self) -> PathBuf {
        self.dir().join(NVRAM_FILE)
    }

    pub fn tpm_state(&self) -> PathBuf {
        self.dir().join(TPM_STATE_FILE)
    }

    /// Notes that the VM was started now.
    pub fn mark_powered_on(&mut self) -> Result<(), Error> {
        self.config.last_powered_on = Some(now());
        self.save()
    }
}

/// A version 4 (random) UUID. The randomness comes from the standard library's
/// per-process hash keys, which is plenty for telling VMs apart.
pub fn random_uuid() -> [u8; 16] {
    let state = RandomState::new();
    let mut uuid = [0u8; 16];
    uuid[..8].copy_from_slice(&state.hash_one(0u8).to_le_bytes());
    uuid[8..].copy_from_slice(&state.hash_one(1u8).to_le_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    uuid
}

pub fn format_uuid(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(uuid)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A unicast MAC address, with `:` or `-` between its bytes.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let bytes: Vec<u8> = text
        .split([':', '-'])
        .map(|part| {
            (part.len() == 2)
                .then(|| u8::from_str_radix(part, 16).ok())
                .flatten()
        })
        .collect::<Option<_>>()?;
    let mac: [u8; 6] = bytes.try_into().ok()?;
    (mac[0] & 1 == 0).then_some(mac)
}

/// A disk size like `64G`, `500M` or `1T` (powers of 1024), in whole sectors.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let invalid = || format!("{text:?} isn't a size like 64G or 500M");
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let unit = unit.trim().to_ascii_uppercase();
    let unit = unit
        .strip_suffix("IB")
        .or_else(|| unit.strip_suffix('B'))
        .unwrap_or(&unit);
    let shift = match unit {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(invalid()),
    };
    let size = digits
        .parse::<u64>()
        .map_err(|_| invalid())?
        .checked_mul(1 << shift)
        .ok_or_else(invalid)?;
    if size == 0 || !size.is_multiple_of(512) {
        return Err(format!(
            "{text:?}: a disk is a whole number of 512-byte sectors"
        ));
    }
    Ok(size)
}

/// `WIDTHxHEIGHT`.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_config_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = VmConfig::new("Windows 11", "windows11", 4, 8192);
        config.disks.push(Disk {
            path: "Windows 11.vmdk".into(),
            bus: Bus::Nvme,
            read_only: false,
        });
        config.cdroms.push(Cdrom {
            path: Some(r"C:\ISOs\Win11.iso".into()),
            install_media: true,
        });
        config.install = Some(Install {
            steps: vec![InstallStep {
                expect: "Press any key".into(),
                send: "\r".into(),
            }],
            eject_on: Some(EjectOn::PowerOff),
        });
        config.display.resolution = Some("1920x1080".into());
        config.notes = "line one\nline two".into();
        let vm = Vm {
            path: dir.path().join("Windows 11.vmmc"),
            config,
        };
        vm.save().unwrap();
        let back = Vm::load(&vm.path).unwrap();
        assert_eq!(back.config, vm.config);
        assert_eq!(back.config.resolution(), Some((1920, 1080)));
        assert_eq!(back.nvram(), dir.path().join("nvram.fd"));
        assert_eq!(
            back.resolve("Windows 11.vmdk"),
            dir.path().join("Windows 11.vmdk")
        );
        // The MAC comes from the UUID, like the old --vm-dir's.
        let uuid = back.config.uuid_bytes();
        assert_eq!(
            parse_mac(&back.config.nics[0].mac).unwrap()[3..],
            uuid[13..]
        );
    }

    #[test]
    fn a_minimal_hand_written_file_gets_the_defaults() {
        let config: VmConfig = toml::from_str(
            "format = 1\nname = \"x\"\nuuid = \"00112233-4455-6677-8899-aabbccddeeff\"\ncpus = 2\nmemory_mib = 2048\n\n[[disk]]\npath = \"x.vmdk\"\nbus = \"sata\"\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.firmware.secure_boot && config.firmware.tpm);
        assert!(config.advanced.enlightenments);
        assert_eq!(config.disks[0].bus, Bus::Sata);
        assert!(config.nics.is_empty());
    }

    #[test]
    fn validation_says_what_is_wrong() {
        let good = VmConfig::new("x", "", 2, 2048);
        let check = |change: fn(&mut VmConfig)| {
            let mut config = good.clone();
            change(&mut config);
            config.validate().unwrap_err()
        };
        assert!(check(|c| c.format = 2).contains("newer vmmc"));
        assert!(check(|c| c.cpus = 0).contains("cpus"));
        assert!(check(|c| c.memory_mib = 1).contains("memory"));
        assert!(check(|c| c.uuid = "nope".into()).contains("UUID"));
        assert!(check(|c| c.nics[0].mac = "01:00:00:00:00:00".into()).contains("unicast"));
        assert!(check(|c| c.display.resolution = Some("big".into())).contains("WIDTHxHEIGHT"));
        assert!(check(|c| c.name = " ".into()).contains("name"));
    }

    #[test]
    fn disk_sizes_parse_in_powers_of_1024() {
        assert_eq!(parse_size("64G"), Ok(64 << 30));
        assert_eq!(parse_size("64GiB"), Ok(64 << 30));
        assert_eq!(parse_size("500m"), Ok(500 << 20));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert!(parse_size("64X").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("0").is_err());
        assert!(parse_size("1000").is_err(), "not whole sectors");
    }

    #[test]
    fn uuids_and_macs_round_trip() {
        let uuid = random_uuid();
        assert_eq!(parse_uuid(&format_uuid(&uuid)), Some(uuid));
        assert_eq!(uuid[6] >> 4, 4, "version 4");
        let mac = [0x52, 0x54, 0, 1, 2, 3];
        assert_eq!(parse_mac(&format_mac(&mac)), Some(mac));
        assert_eq!(parse_mac("52-54-00-01-02-03"), Some(mac));
        assert_eq!(parse_mac("52:54:00:01:02"), None);
        assert_eq!(parse_mac("5:54:00:01:02:03"), None);
    }
}
