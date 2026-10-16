//! 8259A programmable interrupt controller: register model only.
//!
//! Guests probe, initialize and mask the two 8259s even when they route interrupts
//! through the IOAPIC, which is all Linux does with them. This model gets that right.
//! It does not deliver interrupts yet: on WHP that needs ExtINT injection, which arrives
//! with firmware boot.

use serde::{Deserialize, Serialize};
use vm_core::BusDevice;

use crate::snapshot::{self, Snapshot, StateError};

/// Command/data ports of the master and slave controllers.
pub const MASTER_BASE: u16 = 0x20;
pub const SLAVE_BASE: u16 = 0xa0;
pub const PIC_PORTS: u16 = 2;
/// Edge/level control registers, one byte per controller.
pub const ELCR_BASE: u16 = 0x4d0;
pub const ELCR_PORTS: u16 = 2;

/// Where an initialization sequence is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Init {
    Done,
    Icw2,
    Icw3,
    Icw4,
}

/// One 8259A.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pic8259 {
    imr: u8,
    irr: u8,
    isr: u8,
    vector_base: u8,
    init: Init,
    single: bool,
    icw4_needed: bool,
    auto_eoi: bool,
    read_isr: bool,
    poll: bool,
}

impl Default for Pic8259 {
    fn default() -> Self {
        Self {
            imr: 0xff,
            irr: 0,
            isr: 0,
            vector_base: 0,
            init: Init::Done,
            single: false,
            icw4_needed: false,
            auto_eoi: false,
            read_isr: false,
            poll: false,
        }
    }
}

impl Pic8259 {
    /// The highest-priority pending, unmasked request (IR0 is highest).
    fn pending(&self) -> Option<u8> {
        let ready = self.irr & !self.imr;
        (ready != 0).then(|| ready.trailing_zeros() as u8)
    }

    fn command(&mut self, value: u8) {
        if value & 0x10 != 0 {
            // ICW1 starts initialization: clears the mask and in-service state.
            self.imr = 0;
            self.isr = 0;
            self.irr = 0;
            self.read_isr = false;
            self.icw4_needed = value & 0x01 != 0;
            self.single = value & 0x02 != 0;
            self.init = Init::Icw2;
        } else if value & 0x08 != 0 {
            // OCW3: register select and poll.
            if value & 0x02 != 0 {
                self.read_isr = value & 0x01 != 0;
            }
            self.poll = value & 0x04 != 0;
        } else {
            // OCW2: end-of-interrupt commands. Priority rotation isn't modeled.
            match value >> 5 {
                // Non-specific EOI (with or without rotation).
                0b001 | 0b101 => {
                    if self.isr != 0 {
                        self.isr &= self.isr - 1;
                    }
                }
                // Specific EOI (with or without rotation).
                0b011 | 0b111 => self.isr &= !(1 << (value & 7)),
                _ => {}
            }
        }
    }

    fn data(&mut self, value: u8) {
        self.init = match self.init {
            Init::Icw2 => {
                self.vector_base = value & 0xf8;
                match (self.single, self.icw4_needed) {
                    (false, _) => Init::Icw3,
                    (true, true) => Init::Icw4,
                    (true, false) => Init::Done,
                }
            }
            Init::Icw3 => {
                if self.icw4_needed {
                    Init::Icw4
                } else {
                    Init::Done
                }
            }
            Init::Icw4 => {
                self.auto_eoi = value & 0x02 != 0;
                Init::Done
            }
            Init::Done => {
                // OCW1: the interrupt mask.
                self.imr = value;
                Init::Done
            }
        };
    }
}

impl BusDevice for Pic8259 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = if offset == 0 {
            if std::mem::take(&mut self.poll) {
                self.pending().map_or(0, |irq| 0x80 | irq)
            } else if self.read_isr {
                self.isr
            } else {
                self.irr
            }
        } else {
            self.imr
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 {
            self.command(data[0]);
        } else {
            self.data(data[0]);
        }
    }
}

impl Snapshot for Pic8259 {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(self)
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        *self = snapshot::decode(state)?;
        Ok(())
    }
}

/// Edge/level control registers (ELCR) at 0x4d0/0x4d1.
#[derive(Debug, Default)]
pub struct Elcr([u8; 2]);

impl Snapshot for Elcr {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&self.0)
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        self.0 = snapshot::decode(state)?;
        Ok(())
    }
}

/// Bits that must stay edge-triggered: IRQ0, 1 and 2 on the master; IRQ8 and 13 on the
/// slave.
const ELCR_WRITABLE: [u8; 2] = [0xf8, 0xde];

impl BusDevice for Elcr {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.0.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            let index = offset as usize + i;
            if let Some(reg) = self.0.get_mut(index) {
                *reg = byte & ELCR_WRITABLE[index];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(pic: &mut Pic8259, offset: u64, value: u8) {
        pic.write(offset, &[value]);
    }

    fn inb(pic: &mut Pic8259, offset: u64) -> u8 {
        let mut data = [0];
        pic.read(offset, &mut data);
        data[0]
    }

    #[test]
    fn linux_probe_reads_back_the_mask() {
        // Linux's probe writes a mask and expects to read the same value back.
        let mut master = Pic8259::default();
        out(&mut master, 1, 0xfb);
        assert_eq!(inb(&mut master, 1), 0xfb);
    }

    #[test]
    fn initialization_sequence_then_masking() {
        let mut master = Pic8259::default();
        out(&mut master, 0, 0x11); // ICW1: edge, cascade, ICW4 needed
        assert_eq!(inb(&mut master, 1), 0, "ICW1 clears the mask");
        out(&mut master, 1, 0x30); // ICW2: vector base 0x30
        out(&mut master, 1, 0x04); // ICW3: slave on IR2
        out(&mut master, 1, 0x01); // ICW4: 8086 mode
        assert_eq!(master.vector_base, 0x30);
        assert_eq!(master.init, Init::Done);

        out(&mut master, 1, 0xff); // OCW1: mask everything
        assert_eq!(inb(&mut master, 1), 0xff);
    }

    #[test]
    fn ocw3_selects_irr_or_isr_and_eoi_clears_isr() {
        let mut pic = Pic8259 {
            irr: 0b0001_0000,
            isr: 0b0000_0101,
            imr: 0,
            ..Default::default()
        };
        out(&mut pic, 0, 0x0a); // read IRR
        assert_eq!(inb(&mut pic, 0), 0b0001_0000);
        out(&mut pic, 0, 0x0b); // read ISR
        assert_eq!(inb(&mut pic, 0), 0b0000_0101);

        out(&mut pic, 0, 0x20); // non-specific EOI: clears the highest priority (IR0)
        assert_eq!(inb(&mut pic, 0), 0b0000_0100);
        out(&mut pic, 0, 0x62); // specific EOI for IR2
        assert_eq!(inb(&mut pic, 0), 0);

        out(&mut pic, 0, 0x0c); // poll
        assert_eq!(inb(&mut pic, 0), 0x84, "IR4 pending");
    }

    #[test]
    fn elcr_keeps_fixed_edge_bits_clear() {
        let mut elcr = Elcr::default();
        elcr.write(0, &[0xff, 0xff]);
        let mut data = [0u8; 2];
        elcr.read(0, &mut data);
        assert_eq!(data, [0xf8, 0xde]);
    }
}
