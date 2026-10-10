//! Guest-physical memory map of the vmmc PC (Q35-like, so firmware boot fits later).
//!
//! | Range                         | Use                                         |
//! |-------------------------------|---------------------------------------------|
//! | `0..0xA_0000`                 | RAM (top 1 KiB reserved as the EBDA)        |
//! | `0xA_0000..0xE_0000`          | Hole: legacy VGA and option ROM space       |
//! | `0xE_0000..0x10_0000`         | RAM marked reserved: RSDP and ACPI tables   |
//! | `0x10_0000..low RAM end`      | RAM, up to `ECAM_BASE`                      |
//! | `ECAM_BASE` (256 MiB)         | PCIe ECAM                                   |
//! | `PCI_MMIO32` window           | 32-bit PCI BARs                             |
//! | `0xFEC0_0000` / `0xFEE0_0000` | IOAPIC / local APICs                        |
//! | `4 GiB..`                     | The rest of RAM, then the 64-bit PCI window |

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

pub const EBDA_START: u64 = 0x9_fc00;
pub const LOW_RAM_END: u64 = 0xa_0000;
/// The BIOS area: RSDP and ACPI tables live here.
pub const BIOS_AREA_START: u64 = 0xe_0000;
pub const HIGH_MEMORY_START: u64 = MIB;
pub const ECAM_BASE: u64 = 0xb000_0000;
pub const ECAM_SIZE: u64 = 256 * MIB;
pub const PCI_MMIO32_START: u64 = 0xc000_0000;
pub const PCI_MMIO32_END: u64 = 0xfec0_0000;
pub const RAM_ABOVE_4G_START: u64 = 4 * GIB;
/// Size of the 64-bit PCI window placed after the last RAM.
pub const PCI_MMIO64_SIZE: u64 = 32 * GIB;
/// Smallest VM we build.
pub const MIN_MEMORY: u64 = 32 * MIB;

/// E820 memory types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum E820Type {
    Ram = 1,
    Reserved = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct E820Entry {
    pub address: u64,
    pub size: u64,
    pub kind: E820Type,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryLayout {
    /// Page-aligned `(guest-physical address, size)` ranges to back with host memory.
    pub ram: Vec<(u64, usize)>,
    /// The memory map the guest is told about.
    pub e820: Vec<E820Entry>,
    /// End of RAM below 4 GiB (excluding the low 640 KiB).
    pub low_ram_end: u64,
    /// The 64-bit PCI MMIO window.
    pub mmio64: (u64, u64),
}

impl MemoryLayout {
    /// Lays out `memory` bytes of RAM. `memory` must be at least [`MIN_MEMORY`] and a
    /// multiple of 1 MiB.
    pub fn new(memory: u64) -> Self {
        assert!(
            memory >= MIN_MEMORY && memory.is_multiple_of(MIB),
            "memory {memory:#x}"
        );
        let below_4g = memory.min(ECAM_BASE);
        let above_4g = memory - below_4g;

        // The BIOS area is mapped as RAM so the ACPI tables can live there; it runs
        // straight on into the RAM above 1 MiB.
        let mut ram = vec![
            (0, LOW_RAM_END as usize),
            (BIOS_AREA_START, (below_4g - BIOS_AREA_START) as usize),
        ];
        let mut e820 = vec![
            E820Entry {
                address: 0,
                size: EBDA_START,
                kind: E820Type::Ram,
            },
            E820Entry {
                address: EBDA_START,
                size: LOW_RAM_END - EBDA_START,
                kind: E820Type::Reserved,
            },
            E820Entry {
                address: BIOS_AREA_START,
                size: HIGH_MEMORY_START - BIOS_AREA_START,
                kind: E820Type::Reserved,
            },
            E820Entry {
                address: HIGH_MEMORY_START,
                size: below_4g - HIGH_MEMORY_START,
                kind: E820Type::Ram,
            },
            // Linux only uses ECAM if the window is reserved.
            E820Entry {
                address: ECAM_BASE,
                size: ECAM_SIZE,
                kind: E820Type::Reserved,
            },
        ];
        let mut ram_end = RAM_ABOVE_4G_START;
        if above_4g > 0 {
            ram.push((RAM_ABOVE_4G_START, above_4g as usize));
            e820.push(E820Entry {
                address: RAM_ABOVE_4G_START,
                size: above_4g,
                kind: E820Type::Ram,
            });
            ram_end += above_4g;
        }
        let mmio64_start = ram_end.next_multiple_of(GIB);
        Self {
            ram,
            e820,
            low_ram_end: below_4g,
            mmio64: (mmio64_start, mmio64_start + PCI_MMIO64_SIZE),
        }
    }
}

/// Hands out BAR addresses in the 32-bit PCI window.
#[derive(Debug)]
pub struct Mmio32Allocator {
    next: u64,
}

impl Default for Mmio32Allocator {
    fn default() -> Self {
        Self {
            next: PCI_MMIO32_START,
        }
    }
}

impl Mmio32Allocator {
    /// A naturally aligned range of `size` (a power of two) bytes.
    pub fn allocate(&mut self, size: u64) -> Option<u64> {
        let address = self.next.next_multiple_of(size);
        let end = address.checked_add(size)?;
        (end <= PCI_MMIO32_END).then(|| {
            self.next = end;
            address
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_vm_has_everything_below_4g() {
        let layout = MemoryLayout::new(512 * MIB);
        assert_eq!(
            layout.ram,
            [(0, 0xa_0000), (0xe_0000, (512 * MIB - 0xe_0000) as usize)]
        );
        assert_eq!(layout.low_ram_end, 512 * MIB);
        assert_eq!(layout.mmio64, (4 * GIB, 4 * GIB + PCI_MMIO64_SIZE));
        assert!(
            layout
                .e820
                .iter()
                .all(|e| e.address + e.size <= ECAM_BASE + ECAM_SIZE)
        );
    }

    #[test]
    fn large_vm_splits_around_the_pci_hole() {
        let layout = MemoryLayout::new(4 * GIB);
        let above = 4 * GIB - ECAM_BASE;
        assert_eq!(layout.ram[2], (4 * GIB, above as usize));
        let total: u64 = layout
            .e820
            .iter()
            .filter(|e| e.kind == E820Type::Ram)
            .map(|e| e.size)
            .sum();
        // RAM the guest sees: everything except the EBDA and the BIOS area.
        assert_eq!(
            total,
            4 * GIB - (LOW_RAM_END - EBDA_START) - (HIGH_MEMORY_START - LOW_RAM_END)
        );
        assert_eq!(layout.mmio64.0, 6 * GIB, "1 GiB aligned after RAM");
    }

    #[test]
    fn e820_entries_are_sorted_and_disjoint() {
        let layout = MemoryLayout::new(6 * GIB);
        for pair in layout.e820.windows(2) {
            assert!(
                pair[0].address + pair[0].size <= pair[1].address,
                "{pair:?}"
            );
        }
    }

    #[test]
    fn mmio32_allocations_are_aligned_and_bounded() {
        let mut alloc = Mmio32Allocator::default();
        assert_eq!(alloc.allocate(0x1000), Some(0xc000_0000));
        assert_eq!(alloc.allocate(0x4000), Some(0xc000_4000));
        assert_eq!(alloc.allocate(0x2000), Some(0xc000_8000));
        assert_eq!(alloc.allocate(GIB), None, "doesn't fit below the IOAPIC");
    }
}
