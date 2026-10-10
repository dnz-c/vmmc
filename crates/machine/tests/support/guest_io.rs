//! Guest-side access helpers for fake guest programs: ports, memory, PCI config space,
//! and a small virtio-pci driver.
#![allow(dead_code)]

use std::thread;
use std::time::{Duration, Instant};

use hv::CpuIo;
use machine::layout::ECAM_BASE;

pub struct Guest<'a>(pub &'a dyn CpuIo);

impl Guest<'_> {
    pub fn outb(&self, port: u16, value: u8) {
        self.0.io_write(port, &[value]);
    }

    pub fn outw(&self, port: u16, value: u16) {
        self.0.io_write(port, &value.to_le_bytes());
    }

    pub fn outl(&self, port: u16, value: u32) {
        self.0.io_write(port, &value.to_le_bytes());
    }

    pub fn inb(&self, port: u16) -> u8 {
        let mut data = [0u8];
        self.0.io_read(port, &mut data);
        data[0]
    }

    pub fn inl(&self, port: u16) -> u32 {
        let mut data = [0u8; 4];
        self.0.io_read(port, &mut data);
        u32::from_le_bytes(data)
    }

    pub fn read8(&self, gpa: u64) -> u8 {
        let mut data = [0u8];
        self.0.mem_read(gpa, &mut data);
        data[0]
    }

    pub fn read16(&self, gpa: u64) -> u16 {
        let mut data = [0u8; 2];
        self.0.mem_read(gpa, &mut data);
        u16::from_le_bytes(data)
    }

    pub fn read32(&self, gpa: u64) -> u32 {
        let mut data = [0u8; 4];
        self.0.mem_read(gpa, &mut data);
        u32::from_le_bytes(data)
    }

    pub fn write8(&self, gpa: u64, value: u8) {
        self.0.mem_write(gpa, &[value]);
    }

    pub fn write16(&self, gpa: u64, value: u16) {
        self.0.mem_write(gpa, &value.to_le_bytes());
    }

    pub fn write32(&self, gpa: u64, value: u32) {
        self.0.mem_write(gpa, &value.to_le_bytes());
    }

    pub fn write64(&self, gpa: u64, value: u64) {
        self.0.mem_write(gpa, &value.to_le_bytes());
    }

    /// Writes `bytes` to guest RAM, 8 bytes at a time (as an emulated `rep movs` would).
    pub fn write_bytes(&self, gpa: u64, bytes: &[u8]) {
        for (i, chunk) in bytes.chunks(8).enumerate() {
            self.0.mem_write(gpa + 8 * i as u64, chunk);
        }
    }

    pub fn read_bytes(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, chunk) in out.chunks_mut(8).enumerate() {
            self.0.mem_read(gpa + 8 * i as u64, chunk);
        }
        out
    }

    /// Address of a config register of bus 0 `slot` in the ECAM window.
    pub fn ecam(slot: u8, register: u16) -> u64 {
        ECAM_BASE + (u64::from(slot) << 15) + u64::from(register)
    }

    /// Reads a config dword through ports 0xcf8/0xcfc.
    pub fn config_read32_io(&self, slot: u8, register: u8) -> u32 {
        self.outl(
            0xcf8,
            0x8000_0000 | (u32::from(slot) << 11) | u32::from(register & 0xfc),
        );
        self.inl(0xcfc)
    }

    /// Powers the machine off through ACPI (S5 in PM1a_CNT).
    pub fn acpi_power_off(&self) {
        self.outw(0x604, (5 << 10) | (1 << 13));
    }

    /// Polls `condition` until it holds or `timeout` passes.
    pub fn wait_until(&self, timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        condition()
    }
}

/// Where a virtio-pci device's structures are, found by walking its capabilities.
#[derive(Debug, Default)]
pub struct VirtioLayout {
    pub common: u64,
    pub notify: u64,
    pub notify_multiplier: u32,
    pub device: u64,
    pub msix_capability: u16,
    pub msix_table: u64,
}

impl Guest<'_> {
    /// Enables memory decode and bus mastering, and finds the virtio structures.
    pub fn probe_virtio(&self, slot: u8) -> VirtioLayout {
        // Command: memory space + bus master.
        self.write16(Self::ecam(slot, 0x04), 0x6);
        let bar = |index: u8| -> u64 {
            let low = self.read32(Self::ecam(slot, 0x10 + 4 * u16::from(index)));
            let high = self.read32(Self::ecam(slot, 0x14 + 4 * u16::from(index)));
            u64::from(low & !0xf) | (u64::from(high) << 32)
        };
        let mut layout = VirtioLayout::default();
        let mut cap = u16::from(self.read8(Self::ecam(slot, 0x34)));
        while cap != 0 {
            let id = self.read8(Self::ecam(slot, cap));
            let next = u16::from(self.read8(Self::ecam(slot, cap + 1)));
            if id == 0x09 {
                let cfg_type = self.read8(Self::ecam(slot, cap + 3));
                let base = bar(self.read8(Self::ecam(slot, cap + 4)));
                let offset = u64::from(self.read32(Self::ecam(slot, cap + 8)));
                match cfg_type {
                    1 => layout.common = base + offset,
                    2 => {
                        layout.notify = base + offset;
                        layout.notify_multiplier = self.read32(Self::ecam(slot, cap + 16));
                    }
                    4 => layout.device = base + offset,
                    _ => {}
                }
            } else if id == 0x11 {
                layout.msix_capability = cap;
                let table = self.read32(Self::ecam(slot, cap + 4));
                layout.msix_table = bar((table & 7) as u8) + u64::from(table & !7);
            }
            cap = next;
        }
        layout
    }
}
