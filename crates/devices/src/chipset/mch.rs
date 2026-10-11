//! The Intel Q35 memory controller hub at 00:00.0.
//!
//! PCIEXBAR is live: it places the PCIe ECAM window on the MMIO bus. The other chipset
//! registers (PAM, SMRAM, TOLUD and so on) are plain storage. Nothing needs them: the
//! low megabyte is always RAM, and there is no SMM.

use std::sync::{Arc, Weak};

use vm_core::{Bus, SharedDevice};

use super::touches;
use crate::pci::{ConfigSpace, Identity, PciDevice};

pub const MCH_SLOT: u8 = 0;
pub const MCH_DEVICE_ID: u16 = 0x29c0;

/// PCIe extended configuration base address register (64-bit).
const PCIEXBAR: u16 = 0x60;
const PCIEXBAR_EN: u64 = 1 << 0;
/// Base address bits 35:26; the length field masks off more of them.
const PCIEXBAR_BASE: u64 = 0xf_fc00_0000;
/// The length field, bits 2:1.
const PCIEXBAR_LENGTH: u64 = 0b110;

pub struct Mch {
    config: ConfigSpace,
    mmio: Weak<Bus>,
    ecam: SharedDevice,
    /// Where the ECAM window sits on the MMIO bus now, as `(base, size)`.
    mapped: Option<(u64, u64)>,
}

impl Mch {
    /// `ecam` is the configuration window device. With `ecam_base`, PCIEXBAR starts out
    /// enabled there for 256 buses, as if firmware had already set it up.
    pub fn new(mmio: &Arc<Bus>, ecam: SharedDevice, ecam_base: Option<u64>) -> Self {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x8086,
            device_id: MCH_DEVICE_ID,
            revision: 0,
            // Bridge device, host bridge.
            class: 0x06_00_00,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        });
        for offset in (0x40..0x100).step_by(4) {
            config.set_wmask_u32(offset, u32::MAX);
        }
        let writable = PCIEXBAR_BASE | PCIEXBAR_LENGTH | PCIEXBAR_EN;
        config.set_wmask_u32(PCIEXBAR, writable as u32);
        config.set_wmask_u32(PCIEXBAR + 4, (writable >> 32) as u32);
        if let Some(base) = ecam_base {
            assert_eq!(base & !PCIEXBAR_BASE, 0, "ECAM base {base:#x}");
            let value = base | PCIEXBAR_EN;
            config.set_u32(PCIEXBAR, value as u32);
            config.set_u32(PCIEXBAR + 4, (value >> 32) as u32);
        }
        let mut mch = Self {
            config,
            mmio: Arc::downgrade(mmio),
            ecam,
            mapped: None,
        };
        mch.sync_ecam();
        mch
    }

    /// The ECAM window as `(base, size)`, if PCIEXBAR enables one.
    pub fn ecam_window(&self) -> Option<(u64, u64)> {
        let value = u64::from(self.config.u32_at(PCIEXBAR))
            | (u64::from(self.config.u32_at(PCIEXBAR + 4)) << 32);
        if value & PCIEXBAR_EN == 0 {
            return None;
        }
        // 256, 128 or 64 buses of 1 MiB each.
        let size: u64 = match (value & PCIEXBAR_LENGTH) >> 1 {
            0 => 256 << 20,
            1 => 128 << 20,
            2 => 64 << 20,
            _ => return None,
        };
        Some((value & PCIEXBAR_BASE & !(size - 1), size))
    }

    fn sync_ecam(&mut self) {
        let wanted = self.ecam_window();
        if wanted == self.mapped {
            return;
        }
        let Some(mmio) = self.mmio.upgrade() else {
            return;
        };
        if let Some((base, _)) = self.mapped.take() {
            mmio.remove(base);
        }
        if let Some((base, size)) = wanted {
            match mmio.insert(base, size, self.ecam.clone()) {
                Ok(()) => self.mapped = wanted,
                Err(e) => tracing::warn!(base, size, %e, "can't place the ECAM window"),
            }
        }
    }
}

impl PciDevice for Mch {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        if touches(offset, data.len(), PCIEXBAR, 8) {
            self.sync_ecam();
        }
    }

    fn bar_read(&mut self, _: u8, _: u64, data: &mut [u8]) {
        data.fill(0xff);
    }

    fn bar_write(&mut self, _: u8, _: u64, _: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use vm_core::BusDevice;

    use super::*;

    /// Reads return the offset's low byte.
    struct Window;

    impl BusDevice for Window {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn write(&mut self, _: u64, _: &[u8]) {}
    }

    fn mch(base: Option<u64>) -> (Mch, Arc<Bus>) {
        let mmio = Arc::new(Bus::default());
        let mch = Mch::new(&mmio, Arc::new(Mutex::new(Window)), base);
        (mch, mmio)
    }

    fn mapped(mmio: &Bus, address: u64) -> bool {
        mmio.read(address, &mut [0u8])
    }

    #[test]
    fn identifies_as_the_q35_host_bridge() {
        let (mut mch, _) = mch(None);
        let mut id = [0u8; 4];
        mch.config_read(0, &mut id);
        assert_eq!(u32::from_le_bytes(id), 0x29c0_8086);
    }

    #[test]
    fn starts_mapped_only_when_asked() {
        let (_, mmio) = mch(None);
        assert!(!mapped(&mmio, 0xb000_0000));
        let (mch, mmio) = mch(Some(0xb000_0000));
        assert_eq!(mch.ecam_window(), Some((0xb000_0000, 256 << 20)));
        assert!(mapped(&mmio, 0xb000_0000));
        assert!(mapped(&mmio, 0xbfff_ffff));
    }

    #[test]
    fn firmware_moves_resizes_and_disables_the_window() {
        let (mut mch, mmio) = mch(Some(0xb000_0000));
        // What OVMF does: the high dword, then the low dword with EN.
        mch.config_write(PCIEXBAR + 4, &0u32.to_le_bytes());
        mch.config_write(PCIEXBAR, &(0xe000_0000u32 | 1).to_le_bytes());
        assert!(!mapped(&mmio, 0xb000_0000));
        assert!(mapped(&mmio, 0xe000_0000));

        // 64 buses: 64 MiB.
        mch.config_write(PCIEXBAR, &(0xe000_0000u32 | 0b101).to_le_bytes());
        assert_eq!(mch.ecam_window(), Some((0xe000_0000, 64 << 20)));
        assert!(!mapped(&mmio, 0xe400_0000));

        mch.config_write(PCIEXBAR, &0xe000_0000u32.to_le_bytes());
        assert_eq!(mch.ecam_window(), None);
        assert!(!mapped(&mmio, 0xe000_0000));
    }

    #[test]
    fn chipset_registers_are_storage() {
        let (mut mch, _) = mch(None);
        // PAM0..2 at 0x90.
        mch.config_write(0x90, &[0x30, 0x33, 0x33]);
        let mut pam = [0u8; 3];
        mch.config_read(0x90, &mut pam);
        assert_eq!(pam, [0x30, 0x33, 0x33]);
    }
}
