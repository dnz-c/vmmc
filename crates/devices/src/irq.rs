//! Interrupt plumbing between devices and the platform.

use std::sync::{Arc, Mutex};

use crate::legacy::ioapic::IoApic;

/// Base of the x86 MSI address window (the local APICs).
pub const MSI_ADDRESS_BASE: u64 = 0xfee0_0000;

/// An x86 MSI message: what a PCI device writes to signal an interrupt, and what the
/// IOAPIC sends to the local APICs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

impl MsiMessage {
    /// Destination APIC ID (or logical destination), address bits 19:12.
    pub fn destination(&self) -> u8 {
        (self.address >> 12) as u8
    }

    /// Logical rather than physical destination mode, address bit 2.
    pub fn logical(&self) -> bool {
        self.address & (1 << 2) != 0
    }

    pub fn vector(&self) -> u8 {
        self.data as u8
    }

    /// Delivery mode, data bits 10:8: 0 fixed, 1 lowest priority, 4 NMI, 5 INIT, 7 ExtINT.
    pub fn delivery_mode(&self) -> u8 {
        ((self.data >> 8) & 0b111) as u8
    }

    /// Level rather than edge triggered, data bit 15.
    pub fn level_triggered(&self) -> bool {
        self.data & (1 << 15) != 0
    }
}

/// Where devices send interrupt messages. The platform turns them into interrupts.
pub trait MsiSink: Send + Sync {
    fn send(&self, message: MsiMessage);
}

/// One IOAPIC input pin, as a device holds it.
#[derive(Clone)]
pub struct IrqLine {
    ioapic: Arc<Mutex<IoApic>>,
    pin: u8,
}

impl IrqLine {
    pub fn new(ioapic: Arc<Mutex<IoApic>>, pin: u8) -> Self {
        Self { ioapic, pin }
    }

    /// Drives the pin high (asserted) or low.
    pub fn set_level(&self, high: bool) {
        self.lock().set_level(self.pin, high);
    }

    /// Signals one edge-triggered interrupt: high, then low.
    pub fn pulse(&self) {
        let mut ioapic = self.lock();
        ioapic.set_level(self.pin, true);
        ioapic.set_level(self.pin, false);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, IoApic> {
        self.ioapic.lock().expect("the IOAPIC panicked")
    }
}

impl std::fmt::Debug for IrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrqLine").field("pin", &self.pin).finish()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Records every message it is sent.
    #[derive(Default)]
    pub struct RecordingSink(pub Mutex<Vec<MsiMessage>>);

    impl RecordingSink {
        pub fn take(&self) -> Vec<MsiMessage> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    impl MsiSink for RecordingSink {
        fn send(&self, message: MsiMessage) {
            self.0.lock().unwrap().push(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_msi_fields() {
        let message = MsiMessage {
            address: MSI_ADDRESS_BASE | (3 << 12) | (1 << 2),
            data: 0x8130,
        };
        assert_eq!(message.destination(), 3);
        assert!(message.logical());
        assert_eq!(message.vector(), 0x30);
        assert_eq!(message.delivery_mode(), 1);
        assert!(message.level_triggered());
    }
}
