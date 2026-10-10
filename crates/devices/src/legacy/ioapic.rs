//! 82093AA-style I/O APIC: 24 input pins, each routed to the local APICs by a
//! redirection table entry.
//!
//! Interrupts go out as MSI messages, which is also how the platform delivers them.

use std::sync::Arc;

use vm_core::BusDevice;

use crate::irq::{MSI_ADDRESS_BASE, MsiMessage, MsiSink};

/// Guest-physical base address of the I/O APIC's registers.
pub const IOAPIC_BASE: u64 = 0xfec0_0000;
/// Size of the register window.
pub const IOAPIC_SIZE: u64 = 0x1000;
pub const NUM_PINS: usize = 24;

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
/// Version 0x20 I/O APICs also take EOIs here.
const EOI: u64 = 0x40;

const REG_ID: u8 = 0x00;
const REG_VERSION: u8 = 0x01;
const REG_ARBITRATION: u8 = 0x02;
const REG_REDIRECTION: u8 = 0x10;

const VERSION: u32 = 0x20 | ((NUM_PINS as u32 - 1) << 16);

// Redirection entry bits.
const VECTOR: u64 = 0xff;
const DELIVERY_MODE: u64 = 0b111 << 8;
const DEST_MODE_LOGICAL: u64 = 1 << 11;
const DELIVERY_STATUS: u64 = 1 << 12;
const REMOTE_IRR: u64 = 1 << 14;
const TRIGGER_LEVEL: u64 = 1 << 15;
const MASKED: u64 = 1 << 16;
/// Bits the guest can't write: delivery status and remote IRR.
const READ_ONLY: u64 = DELIVERY_STATUS | REMOTE_IRR;

pub struct IoApic {
    id: u8,
    select: u8,
    redirection: [u64; NUM_PINS],
    /// Current level of each input pin.
    levels: [bool; NUM_PINS],
    sink: Arc<dyn MsiSink>,
}

impl IoApic {
    /// An I/O APIC with the given APIC ID, all pins masked.
    pub fn new(id: u8, sink: Arc<dyn MsiSink>) -> Self {
        Self {
            id,
            select: 0,
            redirection: [MASKED; NUM_PINS],
            levels: [false; NUM_PINS],
            sink,
        }
    }

    /// Drives input `pin` high or low. Edge-triggered pins fire on the rising edge;
    /// level-triggered pins fire while high, once per EOI.
    pub fn set_level(&mut self, pin: u8, high: bool) {
        let pin = usize::from(pin);
        if pin >= NUM_PINS {
            tracing::warn!(pin, "IOAPIC input out of range");
            return;
        }
        let was_high = std::mem::replace(&mut self.levels[pin], high);
        let entry = self.redirection[pin];
        if entry & TRIGGER_LEVEL != 0 {
            self.service_level(pin);
        } else if high && !was_high && entry & MASKED == 0 {
            self.deliver(pin);
        }
    }

    /// A local APIC finished a level-triggered interrupt with `vector`. Clears remote IRR
    /// on the pins using it, and fires them again if they are still high.
    pub fn end_of_interrupt(&mut self, vector: u8) {
        for pin in 0..NUM_PINS {
            let entry = self.redirection[pin];
            if entry & REMOTE_IRR != 0 && entry & VECTOR == u64::from(vector) {
                self.redirection[pin] &= !REMOTE_IRR;
                self.service_level(pin);
            }
        }
    }

    /// Fires a level-triggered pin if it is high, unmasked and not already in service.
    fn service_level(&mut self, pin: usize) {
        let entry = self.redirection[pin];
        if self.levels[pin] && entry & (MASKED | REMOTE_IRR) == 0 {
            self.redirection[pin] |= REMOTE_IRR;
            self.deliver(pin);
        }
    }

    fn deliver(&self, pin: usize) {
        let message = entry_message(self.redirection[pin]);
        tracing::trace!(pin, ?message, "IOAPIC interrupt");
        self.sink.send(message);
    }

    fn read_register(&self) -> u32 {
        match self.select {
            REG_ID => u32::from(self.id) << 24,
            REG_VERSION => VERSION,
            REG_ARBITRATION => u32::from(self.id) << 24,
            index => match redirection_index(index) {
                Some((pin, true)) => (self.redirection[pin] >> 32) as u32,
                Some((pin, false)) => self.redirection[pin] as u32,
                None => 0,
            },
        }
    }

    fn write_register(&mut self, value: u32) {
        match self.select {
            REG_ID => self.id = (value >> 24) as u8 & 0x0f,
            REG_VERSION | REG_ARBITRATION => {}
            index => {
                let Some((pin, high_half)) = redirection_index(index) else {
                    return;
                };
                let old = self.redirection[pin];
                let new = if high_half {
                    (old & 0xffff_ffff) | (u64::from(value) << 32)
                } else {
                    (old & !0xffff_ffff) | u64::from(value)
                };
                self.redirection[pin] = (new & !READ_ONLY) | (old & READ_ONLY);
                // Unmasking (or switching to level) a pin that is already high fires it.
                if self.redirection[pin] & TRIGGER_LEVEL != 0 {
                    self.service_level(pin);
                } else {
                    self.redirection[pin] &= !REMOTE_IRR;
                }
            }
        }
    }
}

/// Maps a register index to (pin, high half) for redirection table entries.
fn redirection_index(index: u8) -> Option<(usize, bool)> {
    let offset = usize::from(index.checked_sub(REG_REDIRECTION)?);
    let pin = offset / 2;
    (pin < NUM_PINS).then_some((pin, offset % 2 == 1))
}

/// The MSI message a redirection entry describes.
fn entry_message(entry: u64) -> MsiMessage {
    let destination = (entry >> 56) & 0xff;
    let mut address = MSI_ADDRESS_BASE | (destination << 12);
    if entry & DEST_MODE_LOGICAL != 0 {
        address |= 1 << 2;
    }
    let mut data = (entry & (VECTOR | DELIVERY_MODE)) as u32;
    if entry & TRIGGER_LEVEL != 0 {
        // Level triggered, asserted.
        data |= (1 << 15) | (1 << 14);
    }
    MsiMessage { address, data }
}

impl BusDevice for IoApic {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = match offset & !3 {
            IOREGSEL => u32::from(self.select),
            IOWIN => self.read_register(),
            _ => 0,
        };
        copy_from_u32(value, offset, data);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let value = u32_from(data);
        match offset {
            IOREGSEL => self.select = value as u8,
            IOWIN => self.write_register(value),
            EOI => self.end_of_interrupt(value as u8),
            _ => tracing::debug!(offset, "IOAPIC write to unknown register"),
        }
    }
}

/// Copies the bytes of a 32-bit register an access at `offset` (any width) covers.
fn copy_from_u32(value: u32, offset: u64, data: &mut [u8]) {
    let bytes = value.to_le_bytes();
    let start = (offset & 3) as usize;
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = bytes.get(start + i).copied().unwrap_or(0);
    }
}

fn u32_from(data: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    let len = data.len().min(4);
    bytes[..len].copy_from_slice(&data[..len]);
    u32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::testing::RecordingSink;

    fn ioapic() -> (IoApic, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        (IoApic::new(0, sink.clone()), sink)
    }

    fn write_reg(ioapic: &mut IoApic, index: u8, value: u32) {
        ioapic.write(IOREGSEL, &u32::from(index).to_le_bytes());
        ioapic.write(IOWIN, &value.to_le_bytes());
    }

    fn read_reg(ioapic: &mut IoApic, index: u8) -> u32 {
        ioapic.write(IOREGSEL, &u32::from(index).to_le_bytes());
        let mut data = [0u8; 4];
        ioapic.read(IOWIN, &mut data);
        u32::from_le_bytes(data)
    }

    /// Programs `pin` with `low` as the low dword and `destination` in the high one.
    fn route(ioapic: &mut IoApic, pin: u8, low: u32, destination: u8) {
        write_reg(
            ioapic,
            REG_REDIRECTION + 2 * pin + 1,
            u32::from(destination) << 24,
        );
        write_reg(ioapic, REG_REDIRECTION + 2 * pin, low);
    }

    #[test]
    fn identification_registers() {
        let (mut ioapic, _) = ioapic();
        write_reg(&mut ioapic, REG_ID, 4 << 24);
        assert_eq!(read_reg(&mut ioapic, REG_ID), 4 << 24);
        assert_eq!(read_reg(&mut ioapic, REG_VERSION), 0x0017_0020);
        assert_eq!(
            read_reg(&mut ioapic, REG_REDIRECTION + 2 * 5),
            MASKED as u32,
            "pins start masked"
        );
    }

    #[test]
    fn edge_pin_fires_on_rising_edge_only_when_unmasked() {
        let (mut ioapic, sink) = ioapic();
        ioapic.set_level(4, true);
        ioapic.set_level(4, false);
        assert!(sink.take().is_empty(), "masked");

        route(&mut ioapic, 4, 0x34, 2);
        ioapic.set_level(4, true);
        ioapic.set_level(4, true);
        ioapic.set_level(4, false);
        assert_eq!(
            sink.take(),
            [MsiMessage {
                address: 0xfee0_2000,
                data: 0x34
            }]
        );
    }

    #[test]
    fn level_pin_fires_once_per_eoi_while_high() {
        let (mut ioapic, sink) = ioapic();
        route(&mut ioapic, 9, 0x8000 | 0x41, 0);
        ioapic.set_level(9, true);
        let fired = sink.take();
        assert_eq!(fired.len(), 1);
        assert!(fired[0].level_triggered());
        assert_ne!(
            read_reg(&mut ioapic, REG_REDIRECTION + 18) & REMOTE_IRR as u32,
            0,
            "remote IRR set while in service"
        );

        ioapic.set_level(9, true);
        assert!(sink.take().is_empty(), "no repeat before EOI");
        ioapic.end_of_interrupt(0x41);
        assert_eq!(sink.take().len(), 1, "still high after EOI: fires again");

        ioapic.set_level(9, false);
        ioapic.end_of_interrupt(0x41);
        assert!(sink.take().is_empty(), "low after EOI: quiet");
    }

    #[test]
    fn unmasking_a_high_level_pin_fires_it() {
        let (mut ioapic, sink) = ioapic();
        ioapic.set_level(9, true);
        route(&mut ioapic, 9, 0x8000 | 0x41, 0);
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn eoi_register_clears_remote_irr() {
        let (mut ioapic, sink) = ioapic();
        route(&mut ioapic, 9, 0x8000 | 0x41, 0);
        ioapic.set_level(9, true);
        sink.take();
        ioapic.write(EOI, &0x41u32.to_le_bytes());
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn redirection_entry_becomes_the_right_msi() {
        // Vector 0x30, lowest priority, logical destination 0x0f.
        let entry = 0x0f00_0000_0000_0000 | DEST_MODE_LOGICAL | (1 << 8) | 0x30;
        assert_eq!(
            entry_message(entry),
            MsiMessage {
                address: 0xfee0_f004,
                data: 0x130
            }
        );
    }

    #[test]
    fn guest_cannot_write_read_only_bits() {
        let (mut ioapic, _) = ioapic();
        write_reg(&mut ioapic, REG_REDIRECTION, 0x30 | (READ_ONLY as u32));
        assert_eq!(read_reg(&mut ioapic, REG_REDIRECTION), 0x30);
    }
}
