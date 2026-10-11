//! A PCI function's configuration space: the type 0 header, BARs and the capability
//! list.
//!
//! Writes go through per-byte masks: `wmask` holds the bits the guest may change and
//! `w1cmask` the bits it clears by writing 1. Capabilities register their writable
//! fields when added.

/// PCIe extended configuration space size.
pub const CONFIG_SIZE: usize = 4096;

pub const VENDOR_ID: u16 = 0x00;
pub const DEVICE_ID: u16 = 0x02;
pub const COMMAND: u16 = 0x04;
pub const STATUS: u16 = 0x06;
pub const REVISION: u16 = 0x08;
pub const CLASS_CODE: u16 = 0x09;
pub const HEADER_TYPE: u16 = 0x0e;
pub const BAR0: u16 = 0x10;
pub const SUBSYSTEM_VENDOR_ID: u16 = 0x2c;
pub const SUBSYSTEM_ID: u16 = 0x2e;
pub const CAPABILITIES_POINTER: u16 = 0x34;
pub const INTERRUPT_LINE: u16 = 0x3c;
pub const INTERRUPT_PIN: u16 = 0x3d;

pub const COMMAND_IO: u16 = 1 << 0;
pub const COMMAND_MEMORY: u16 = 1 << 1;
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
pub const COMMAND_INTX_DISABLE: u16 = 1 << 10;
pub const STATUS_CAPABILITIES: u16 = 1 << 4;

/// Capabilities start after the standard header.
const FIRST_CAPABILITY: u16 = 0x40;
const LAST_STANDARD_OFFSET: u16 = 0xff;

pub const NUM_BARS: usize = 6;

/// What a BAR maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Memory32,
    /// Takes two BAR slots; the next one holds the high 32 bits.
    Memory64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bar {
    kind: BarKind,
    size: u64,
    prefetchable: bool,
}

/// A BAR the guest has mapped: memory decode is on and the address is assigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarMapping {
    pub bar: u8,
    pub address: u64,
    pub size: u64,
}

pub struct ConfigSpace {
    regs: Box<[u8; CONFIG_SIZE]>,
    wmask: Box<[u8; CONFIG_SIZE]>,
    w1cmask: Box<[u8; CONFIG_SIZE]>,
    bars: [Option<Bar>; NUM_BARS],
    /// Where the next capability goes, and the pointer to link it from.
    next_capability: u16,
    last_capability_link: u16,
}

/// What identifies a PCI function.
#[derive(Clone, Copy, Debug)]
pub struct Identity {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    /// Base class, subclass and programming interface.
    pub class: u32,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
}

impl ConfigSpace {
    pub fn new(identity: Identity) -> Self {
        let mut config = Self {
            regs: Box::new([0; CONFIG_SIZE]),
            wmask: Box::new([0; CONFIG_SIZE]),
            w1cmask: Box::new([0; CONFIG_SIZE]),
            bars: [None; NUM_BARS],
            next_capability: FIRST_CAPABILITY,
            last_capability_link: CAPABILITIES_POINTER,
        };
        config.set_u16(VENDOR_ID, identity.vendor_id);
        config.set_u16(DEVICE_ID, identity.device_id);
        config.regs[usize::from(REVISION)] = identity.revision;
        let class = identity.class.to_le_bytes();
        config.regs[usize::from(CLASS_CODE)..usize::from(CLASS_CODE) + 3]
            .copy_from_slice(&class[..3]);
        config.set_u16(SUBSYSTEM_VENDOR_ID, identity.subsystem_vendor_id);
        config.set_u16(SUBSYSTEM_ID, identity.subsystem_id);

        config.set_wmask_u16(
            COMMAND,
            COMMAND_IO | COMMAND_MEMORY | COMMAND_BUS_MASTER | COMMAND_INTX_DISABLE,
        );
        // Cache line size, latency timer and interrupt line are plain read/write.
        config.wmask[0x0c] = 0xff;
        config.wmask[0x0d] = 0xff;
        config.wmask[usize::from(INTERRUPT_LINE)] = 0xff;
        config
    }

    pub fn u8_at(&self, offset: u16) -> u8 {
        self.regs[usize::from(offset)]
    }

    pub fn u16_at(&self, offset: u16) -> u16 {
        let i = usize::from(offset);
        u16::from_le_bytes([self.regs[i], self.regs[i + 1]])
    }

    pub fn u32_at(&self, offset: u16) -> u32 {
        let i = usize::from(offset);
        u32::from_le_bytes(self.regs[i..i + 4].try_into().unwrap())
    }

    /// Sets a register's value directly, ignoring write masks (for device code).
    pub fn set_u8(&mut self, offset: u16, value: u8) {
        self.regs[usize::from(offset)] = value;
    }

    pub fn set_u16(&mut self, offset: u16, value: u16) {
        let i = usize::from(offset);
        self.regs[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }

    pub fn set_u32(&mut self, offset: u16, value: u32) {
        let i = usize::from(offset);
        self.regs[i..i + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Lets the guest change `mask`'s bits of the 16-bit register at `offset`.
    pub fn set_wmask_u16(&mut self, offset: u16, mask: u16) {
        let i = usize::from(offset);
        self.wmask[i..i + 2].copy_from_slice(&mask.to_le_bytes());
    }

    pub fn set_wmask_u32(&mut self, offset: u16, mask: u32) {
        let i = usize::from(offset);
        self.wmask[i..i + 4].copy_from_slice(&mask.to_le_bytes());
    }

    pub fn command(&self) -> u16 {
        self.u16_at(COMMAND)
    }

    /// Declares a BAR. `size` must be a power of two of at least 16 bytes.
    pub fn add_bar(&mut self, index: usize, kind: BarKind, size: u64, prefetchable: bool) {
        assert!(size.is_power_of_two() && size >= 16, "BAR size {size:#x}");
        assert!(index < NUM_BARS && (kind == BarKind::Memory32 || index + 1 < NUM_BARS));
        self.bars[index] = Some(Bar {
            kind,
            size,
            prefetchable,
        });
        let low = BAR0 + 4 * index as u16;
        let mut flags = if prefetchable { 1 << 3 } else { 0 };
        if kind == BarKind::Memory64 {
            flags |= 0b10 << 1;
        }
        self.set_u32(low, flags);
        // The address bits above the size are writable; the low ones read as zero.
        self.set_wmask_u32(low, !(size - 1) as u32 & !0xf);
        if kind == BarKind::Memory64 {
            self.set_wmask_u32(low + 4, (!(size - 1) >> 32) as u32);
        }
    }

    /// Assigns a BAR's address (as firmware would).
    pub fn set_bar_address(&mut self, index: usize, address: u64) {
        let bar = self.bars[index].expect("BAR declared");
        let low = BAR0 + 4 * index as u16;
        let flags = self.u32_at(low) & 0xf;
        self.set_u32(low, (address as u32 & !0xf) | flags);
        if bar.kind == BarKind::Memory64 {
            self.set_u32(low + 4, (address >> 32) as u32);
        }
    }

    pub fn bar_address(&self, index: usize) -> Option<u64> {
        let bar = self.bars[index]?;
        let low = BAR0 + 4 * index as u16;
        let mut address = u64::from(self.u32_at(low) & !0xf);
        if bar.kind == BarKind::Memory64 {
            address |= u64::from(self.u32_at(low + 4)) << 32;
        }
        Some(address)
    }

    /// The BARs the guest can reach right now: memory decode on, address assigned, and
    /// not a sizing probe in progress.
    pub fn bar_mappings(&self) -> Vec<BarMapping> {
        if self.command() & COMMAND_MEMORY == 0 {
            return Vec::new();
        }
        self.assigned_bars()
    }

    /// The BARs that have an address assigned (and aren't mid-probe), whether or not
    /// memory decode is on.
    pub fn assigned_bars(&self) -> Vec<BarMapping> {
        (0..NUM_BARS)
            .filter_map(|index| {
                let bar = self.bars[index]?;
                let address = self.bar_address(index)?;
                let (limit, address_bits) = match bar.kind {
                    BarKind::Memory32 => (1 << 32, u64::from(u32::MAX)),
                    BarKind::Memory64 => (u64::MAX, u64::MAX),
                };
                let fits = address
                    .checked_add(bar.size)
                    .is_some_and(|end| end <= limit);
                // All address bits set is what a sizing write leaves behind.
                let probing = address == !(bar.size - 1) & address_bits;
                (address != 0 && fits && !probing).then_some(BarMapping {
                    bar: index as u8,
                    address,
                    size: bar.size,
                })
            })
            .collect()
    }

    /// Adds a capability with `body` (everything after the ID and next-pointer bytes).
    /// Returns its offset. Capabilities are dword-aligned and linked in order.
    pub fn add_capability(&mut self, id: u8, body: &[u8]) -> u16 {
        let offset = self.next_capability;
        let len = 2 + body.len() as u16;
        assert!(
            offset + len - 1 <= LAST_STANDARD_OFFSET,
            "out of capability space"
        );
        let i = usize::from(offset);
        self.regs[i] = id;
        self.regs[i + 1] = 0;
        self.regs[i + 2..i + 2 + body.len()].copy_from_slice(body);
        self.regs[usize::from(self.last_capability_link)] = offset as u8;
        self.last_capability_link = offset + 1;
        self.next_capability = (offset + len).next_multiple_of(4);
        let status = self.u16_at(STATUS) | STATUS_CAPABILITIES;
        self.set_u16(STATUS, status);
        offset
    }

    /// Reads `data.len()` bytes at `offset`. Out-of-range bytes read as zero.
    pub fn read(&self, offset: u16, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.regs.get(usize::from(offset) + i).copied().unwrap_or(0);
        }
    }

    /// Applies a guest write through the write masks.
    pub fn write(&mut self, offset: u16, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            let at = usize::from(offset) + i;
            if at >= CONFIG_SIZE {
                break;
            }
            let wmask = self.wmask[at];
            let w1c = self.w1cmask[at];
            self.regs[at] = (self.regs[at] & !wmask) | (byte & wmask);
            self.regs[at] &= !(byte & w1c);
        }
    }
}

impl std::fmt::Debug for ConfigSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigSpace")
            .field(
                "vendor_id",
                &format_args!("{:#06x}", self.u16_at(VENDOR_ID)),
            )
            .field(
                "device_id",
                &format_args!("{:#06x}", self.u16_at(DEVICE_ID)),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConfigSpace {
        ConfigSpace::new(Identity {
            vendor_id: 0x1af4,
            device_id: 0x1042,
            revision: 1,
            class: 0x01_00_00,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        })
    }

    fn read_u32(config: &ConfigSpace, offset: u16) -> u32 {
        let mut data = [0u8; 4];
        config.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    #[test]
    fn identity_registers_are_read_only() {
        let mut config = config();
        assert_eq!(read_u32(&config, 0), 0x1042_1af4);
        config.write(0, &[0xff; 4]);
        assert_eq!(read_u32(&config, 0), 0x1042_1af4);
        assert_eq!(
            read_u32(&config, 0x08),
            0x0100_0001,
            "class 01:00:00, revision 1"
        );
    }

    #[test]
    fn command_register_keeps_only_writable_bits() {
        let mut config = config();
        config.write(COMMAND, &0xffffu16.to_le_bytes());
        assert_eq!(
            config.command(),
            COMMAND_IO | COMMAND_MEMORY | COMMAND_BUS_MASTER | COMMAND_INTX_DISABLE
        );
    }

    #[test]
    fn sizing_a_64_bit_bar() {
        let mut config = config();
        config.add_bar(0, BarKind::Memory64, 0x4000, false);
        // The OS writes all ones and reads back the size mask, flags included.
        config.write(BAR0, &[0xff; 4]);
        config.write(BAR0 + 4, &[0xff; 4]);
        assert_eq!(read_u32(&config, BAR0), 0xffff_c004);
        assert_eq!(read_u32(&config, BAR0 + 4), 0xffff_ffff);

        config.write(BAR0, &0xc000_0000u32.to_le_bytes());
        config.write(BAR0 + 4, &0x1u32.to_le_bytes());
        assert_eq!(config.bar_address(0), Some(0x1_c000_0000));
    }

    #[test]
    fn bars_map_only_with_memory_decode_and_a_sane_address() {
        let mut config = config();
        config.add_bar(0, BarKind::Memory64, 0x4000, false);
        config.add_bar(2, BarKind::Memory32, 0x1000, false);
        config.set_bar_address(0, 0x8_0000_0000);
        config.set_bar_address(2, 0xc000_0000);
        assert!(config.bar_mappings().is_empty(), "decode off");

        config.write(COMMAND, &COMMAND_MEMORY.to_le_bytes());
        assert_eq!(
            config.bar_mappings(),
            [
                BarMapping {
                    bar: 0,
                    address: 0x8_0000_0000,
                    size: 0x4000
                },
                BarMapping {
                    bar: 2,
                    address: 0xc000_0000,
                    size: 0x1000
                },
            ]
        );

        // A sizing probe left in a 32-bit BAR doesn't map it.
        config.write(BAR0 + 8, &[0xff; 4]);
        assert_eq!(config.bar_mappings().len(), 1);
    }

    #[test]
    fn capabilities_are_linked_in_order() {
        let mut config = config();
        let first = config.add_capability(0x09, &[0; 14]);
        let second = config.add_capability(0x11, &[0; 10]);
        assert_eq!((first, second), (0x40, 0x50));
        assert_eq!(config.u8_at(CAPABILITIES_POINTER), 0x40);
        assert_eq!(config.u8_at(first + 1), 0x50);
        assert_eq!(config.u8_at(second + 1), 0);
        assert_ne!(config.u16_at(STATUS) & STATUS_CAPABILITIES, 0);
    }
}
