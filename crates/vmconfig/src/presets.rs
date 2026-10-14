//! What a new VM gets for each kind of guest, as the New VM wizard and `vmmc-cli
//! create` offer them.

use crate::{Bus, NicModel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    /// What `--os` and the config's `os` say.
    pub id: &'static str,
    pub label: &'static str,
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub bus: Bus,
    pub nic: NicModel,
    pub secure_boot: bool,
    pub tpm: bool,
}

/// Windows needs no extra drivers for NVMe and e1000e; Linux has virtio too, but
/// the same devices keep the presets alike.
pub const PRESETS: [Preset; 5] = [
    Preset {
        id: "windows11",
        label: "Windows 11",
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 64,
        bus: Bus::Nvme,
        nic: NicModel::E1000e,
        secure_boot: true,
        tpm: true,
    },
    Preset {
        id: "windows10",
        label: "Windows 10",
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        bus: Bus::Nvme,
        nic: NicModel::E1000e,
        secure_boot: true,
        tpm: true,
    },
    Preset {
        id: "ubuntu",
        label: "Ubuntu",
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 32,
        bus: Bus::Nvme,
        nic: NicModel::E1000e,
        secure_boot: true,
        tpm: true,
    },
    Preset {
        id: "linux",
        label: "Other Linux",
        cpus: 2,
        memory_mib: 2048,
        disk_gib: 20,
        bus: Bus::Nvme,
        nic: NicModel::E1000e,
        secure_boot: false,
        tpm: false,
    },
    Preset {
        id: "other",
        label: "Other",
        cpus: 1,
        memory_mib: 2048,
        disk_gib: 20,
        bus: Bus::Sata,
        nic: NicModel::E1000e,
        secure_boot: false,
        tpm: false,
    },
];

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id))
}
