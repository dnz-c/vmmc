//! The Intel ICH9 LPC bridge at 00:1f.0.
//!
//! Firmware finds the ACPI PM block through it: PMBASE says where the block decodes,
//! and ACPI_CNTL turns decoding on. The PIRQ routing and other chipset registers are
//! plain storage; PCI INTx goes through the IOAPIC as the DSDT's `_PRT` describes.

use std::sync::{Arc, Weak};

use vm_core::{Bus, SharedDevice};

use super::touches;
use crate::pci::{ConfigSpace, Identity, PciDevice};

pub const LPC_SLOT: u8 = 0x1f;
pub const LPC_FUNCTION: u8 = 0;

/// ACPI PM I/O base: bits 15:7 hold the base, bit 0 reads 1 (I/O space).
const PMBASE: u16 = 0x40;
const PMBASE_ADDRESS: u32 = 0xff80;
const PMBASE_IO: u32 = 1;
/// ACPI control: bit 7 enables PM block decoding.
const ACPI_CNTL: u16 = 0x44;
const ACPI_EN: u8 = 1 << 7;
/// PIRQ[A-D] and PIRQ[E-H] routing registers; 0x80 means not routed.
const PIRQA_ROUT: u16 = 0x60;
const PIRQE_ROUT: u16 = 0x68;
const PIRQ_NOT_ROUTED: u32 = 0x8080_8080;

pub struct Ich9Lpc {
    config: ConfigSpace,
    pio: Weak<Bus>,
    pm: SharedDevice,
    pm_len: u16,
    /// Where the PM block decodes now.
    mapped: Option<u16>,
}

impl Ich9Lpc {
    /// `pm` is the ACPI PM block, `pm_len` ports long. With `pm_base` it starts out
    /// decoding there, as if firmware had set it up.
    pub fn new(pio: &Arc<Bus>, pm: SharedDevice, pm_len: u16, pm_base: Option<u16>) -> Self {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x8086,
            device_id: 0x2918,
            revision: 2,
            // Bridge device, ISA bridge.
            class: 0x06_01_00,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        });
        for offset in (0x40..0x100).step_by(4) {
            config.set_wmask_u32(offset, u32::MAX);
        }
        config.set_wmask_u32(PMBASE, PMBASE_ADDRESS);
        config.set_u32(PMBASE, PMBASE_IO);
        config.set_u32(PIRQA_ROUT, PIRQ_NOT_ROUTED);
        config.set_u32(PIRQE_ROUT, PIRQ_NOT_ROUTED);
        if let Some(base) = pm_base {
            config.set_u32(PMBASE, (u32::from(base) & PMBASE_ADDRESS) | PMBASE_IO);
            config.set_u8(ACPI_CNTL, ACPI_EN);
        }
        let mut lpc = Self {
            config,
            pio: Arc::downgrade(pio),
            pm,
            pm_len,
            mapped: None,
        };
        lpc.sync_pm();
        lpc
    }

    /// The I/O base the PM block decodes at, if decoding is on.
    pub fn pm_base(&self) -> Option<u16> {
        (self.config.u8_at(ACPI_CNTL) & ACPI_EN != 0)
            .then(|| (self.config.u32_at(PMBASE) & PMBASE_ADDRESS) as u16)
    }

    fn sync_pm(&mut self) {
        let wanted = self.pm_base();
        if wanted == self.mapped {
            return;
        }
        let Some(pio) = self.pio.upgrade() else {
            return;
        };
        if let Some(base) = self.mapped.take() {
            pio.remove(base.into());
        }
        if let Some(base) = wanted {
            match pio.insert(base.into(), self.pm_len.into(), self.pm.clone()) {
                Ok(()) => self.mapped = wanted,
                Err(e) => tracing::warn!(base, %e, "can't place the ACPI PM block"),
            }
        }
    }
}

impl PciDevice for Ich9Lpc {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        if touches(offset, data.len(), PMBASE, 4) || touches(offset, data.len(), ACPI_CNTL, 1) {
            self.sync_pm();
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
    struct Block;

    impl BusDevice for Block {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn write(&mut self, _: u64, _: &[u8]) {}
    }

    fn lpc(base: Option<u16>) -> (Ich9Lpc, Arc<Bus>) {
        let pio = Arc::new(Bus::default());
        let lpc = Ich9Lpc::new(&pio, Arc::new(Mutex::new(Block)), 0x80, base);
        (lpc, pio)
    }

    fn read8(pio: &Bus, port: u64) -> Option<u8> {
        let mut data = [0u8];
        pio.read(port, &mut data).then_some(data[0])
    }

    #[test]
    fn decodes_nothing_until_firmware_enables_it() {
        let (lpc, pio) = lpc(None);
        assert_eq!(lpc.pm_base(), None);
        assert_eq!(read8(&pio, 0x608), None);
        assert_eq!(lpc.config().u32_at(PMBASE), 1, "I/O space indicator");
    }

    #[test]
    fn ovmf_sequence_maps_the_pm_block() {
        let (mut lpc, pio) = lpc(None);
        // OVMF's AcpiTimerLib: PMBASE = 0x600, then ACPI_EN.
        lpc.config_write(PMBASE, &0x600u32.to_le_bytes());
        assert_eq!(read8(&pio, 0x608), None, "not until ACPI_EN");
        lpc.config_write(ACPI_CNTL, &[ACPI_EN]);
        assert_eq!(lpc.pm_base(), Some(0x600));
        assert_eq!(read8(&pio, 0x608), Some(8));
        assert_eq!(read8(&pio, 0x67f), Some(0x7f));

        // Moving it, then turning decoding off.
        lpc.config_write(PMBASE, &0xb000u32.to_le_bytes());
        assert_eq!(read8(&pio, 0x608), None);
        assert_eq!(read8(&pio, 0xb008), Some(8));
        lpc.config_write(ACPI_CNTL, &[0]);
        assert_eq!(read8(&pio, 0xb008), None);
    }

    #[test]
    fn can_start_enabled_for_direct_boot() {
        let (lpc, pio) = lpc(Some(0x600));
        assert_eq!(lpc.pm_base(), Some(0x600));
        assert_eq!(read8(&pio, 0x600), Some(0));
    }

    #[test]
    fn pirq_routing_defaults_to_not_routed_and_is_writable() {
        let (mut lpc, _) = lpc(None);
        let mut pirq = [0u8; 4];
        lpc.config_read(PIRQA_ROUT, &mut pirq);
        assert_eq!(pirq, [0x80; 4]);
        lpc.config_write(PIRQA_ROUT, &[0x0a, 0x0a, 0x0b, 0x0b]);
        lpc.config_read(PIRQA_ROUT, &mut pirq);
        assert_eq!(pirq, [0x0a, 0x0a, 0x0b, 0x0b]);
    }
}
