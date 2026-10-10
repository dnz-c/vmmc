//! Direct Linux boot through the 64-bit boot protocol: load a bzImage and initrd, write
//! the zero page, and start the boot CPU in long mode at the kernel's 64-bit entry.
//! See the kernel's Documentation/arch/x86/boot.rst.

use std::io::Cursor;

use hv::{DescriptorTable, Segment, SpecialRegisters, StandardRegisters};
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bootparam::{boot_e820_entry, boot_params};
use linux_loader::loader::{BzImage, Cmdline, KernelLoader, load_cmdline};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::layout::{HIGH_MEMORY_START, MemoryLayout};
use crate::{Error, Result};

/// Fixed low-memory locations used during boot.
pub const GDT_ADDR: u64 = 0x500;
pub const ZERO_PAGE: u64 = 0x7000;
pub const BOOT_STACK: u64 = 0x8ff0;
pub const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xa000;
/// Four page directories (0xb000..0xf000) cover 4 GiB with 2 MiB pages.
const PD_ADDR: u64 = 0xb000;
pub const CMDLINE_ADDR: u64 = 0x2_0000;
const CMDLINE_MAX: usize = 0x1_0000;
/// Where the protected-mode kernel goes.
pub const KERNEL_ADDR: u64 = HIGH_MEMORY_START;
/// The 64-bit entry point is 0x200 bytes into the protected-mode kernel.
const ENTRY_64_OFFSET: u64 = 0x200;

/// GDT selectors the 64-bit boot protocol asks for.
const BOOT_CS: u16 = 0x10;
const BOOT_DS: u16 = 0x18;
const BOOT_TSS: u16 = 0x20;

/// setup_header.xloadflags: the kernel has a 64-bit entry point.
const XLF_KERNEL_64: u16 = 1 << 0;
/// setup_header.loadflags: the protected-mode code is loaded at 1 MiB.
const LOADED_HIGH: u8 = 1 << 0;
/// "Undefined" boot loader ID.
const LOADER_TYPE_UNDEFINED: u8 = 0xff;

const PAGE_PRESENT_RW: u64 = 0b11;
const PAGE_HUGE: u64 = 1 << 7;

/// Where the boot CPU starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootEntry {
    pub rip: u64,
    pub zero_page: u64,
}

/// Loads `kernel` (a bzImage) and `initrd`, and writes the zero page, command line, page
/// tables and GDT.
pub fn load_linux(
    memory: &GuestMemoryMmap,
    layout: &MemoryLayout,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
    rsdp: u64,
) -> Result<BootEntry> {
    let loaded = BzImage::load(
        memory,
        Some(GuestAddress(KERNEL_ADDR)),
        &mut Cursor::new(kernel),
        Some(GuestAddress(HIGH_MEMORY_START)),
    )
    .map_err(|e| Error::Boot(format!("can't load the kernel: {e}")))?;
    let mut header = loaded
        .setup_header
        .ok_or_else(|| Error::Boot("not a bzImage".into()))?;

    let xloadflags = header.xloadflags;
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(Error::Boot("the kernel has no 64-bit entry point".into()));
    }
    // The decompressor needs `init_size` bytes from the load address.
    let kernel_end = KERNEL_ADDR + u64::from(header.init_size).max(loaded.kernel_end - KERNEL_ADDR);
    if kernel_end > layout.low_ram_end {
        return Err(Error::Boot("not enough memory for the kernel".into()));
    }

    let cmdline_limit = (header.cmdline_size as usize).min(CMDLINE_MAX - 1);
    if cmdline.len() > cmdline_limit {
        return Err(Error::Boot(format!(
            "kernel command line longer than {cmdline_limit} bytes"
        )));
    }
    let mut line = Cmdline::new(CMDLINE_MAX).map_err(|e| Error::Boot(e.to_string()))?;
    if !cmdline.is_empty() {
        line.insert_str(cmdline)
            .map_err(|e| Error::Boot(format!("bad kernel command line: {e}")))?;
    }
    load_cmdline(memory, GuestAddress(CMDLINE_ADDR), &line)
        .map_err(|e| Error::Boot(format!("can't write the command line: {e}")))?;

    header.type_of_loader = LOADER_TYPE_UNDEFINED;
    header.loadflags |= LOADED_HIGH;
    header.cmd_line_ptr = CMDLINE_ADDR as u32;
    header.cmdline_size = cmdline.len() as u32;
    if let Some(initrd) = initrd {
        let address = place_initrd(layout, kernel_end, header.initrd_addr_max, initrd.len())?;
        memory.write_slice(initrd, GuestAddress(address))?;
        header.ramdisk_image = address as u32;
        header.ramdisk_size = initrd.len() as u32;
    }

    let mut params = boot_params {
        hdr: header,
        acpi_rsdp_addr: rsdp,
        ..Default::default()
    };
    if layout.e820.len() > params.e820_table.len() {
        return Err(Error::Boot("too many memory map entries".into()));
    }
    for (slot, entry) in params.e820_table.iter_mut().zip(&layout.e820) {
        *slot = boot_e820_entry {
            addr: entry.address,
            size: entry.size,
            r#type: entry.kind as u32,
        };
    }
    params.e820_entries = layout.e820.len() as u8;
    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE)),
        memory,
    )
    .map_err(|e| Error::Boot(format!("can't write the zero page: {e}")))?;

    write_page_tables(memory)?;
    write_gdt(memory)?;
    Ok(BootEntry {
        rip: KERNEL_ADDR + ENTRY_64_OFFSET,
        zero_page: ZERO_PAGE,
    })
}

/// Puts the initrd as high as allowed below 4 GiB, above the kernel.
fn place_initrd(
    layout: &MemoryLayout,
    kernel_end: u64,
    max_address: u32,
    len: usize,
) -> Result<u64> {
    let ceiling = layout.low_ram_end.min(u64::from(max_address) + 1);
    let address = ceiling
        .checked_sub(len as u64)
        .map(|a| a & !0xfff)
        .filter(|&a| a >= kernel_end)
        .ok_or_else(|| Error::Boot("the initrd doesn't fit in memory".into()))?;
    Ok(address)
}

/// Identity-maps the first 4 GiB with 2 MiB pages.
fn write_page_tables(memory: &GuestMemoryMmap) -> Result<()> {
    memory.write_obj(PDPT_ADDR | PAGE_PRESENT_RW, GuestAddress(PML4_ADDR))?;
    for directory in 0..4u64 {
        let pd = PD_ADDR + directory * 0x1000;
        memory.write_obj(
            pd | PAGE_PRESENT_RW,
            GuestAddress(PDPT_ADDR + directory * 8),
        )?;
        for entry in 0..512u64 {
            let address = ((directory * 512 + entry) << 21) | PAGE_PRESENT_RW | PAGE_HUGE;
            memory.write_obj(address, GuestAddress(pd + entry * 8))?;
        }
    }
    Ok(())
}

/// GDT entries: null, null, 64-bit code (0x10), data (0x18), TSS (0x20).
const GDT: [u64; 5] = [
    0,
    0,
    0x00af_9b00_0000_ffff,
    0x00cf_9300_0000_ffff,
    0x0000_8b00_0000_0067,
];

fn write_gdt(memory: &GuestMemoryMmap) -> Result<()> {
    for (index, entry) in GDT.iter().enumerate() {
        memory.write_obj(*entry, GuestAddress(GDT_ADDR + index as u64 * 8))?;
    }
    Ok(())
}

/// The boot CPU's registers at the 64-bit entry point. `reset` is the vCPU's reset
/// state; only what the boot protocol requires is changed.
pub fn boot_cpu_registers(
    entry: &BootEntry,
    reset: &SpecialRegisters,
) -> (StandardRegisters, SpecialRegisters) {
    let regs = StandardRegisters {
        rip: entry.rip,
        rsi: entry.zero_page,
        rsp: BOOT_STACK,
        rbp: BOOT_STACK,
        rflags: 0x2,
        ..Default::default()
    };
    let code = Segment {
        base: 0,
        limit: 0xffff_ffff,
        selector: BOOT_CS,
        // Present, code, execute/read, accessed; L=1 (64-bit), G=1.
        attributes: 0xa09b,
    };
    let data = Segment {
        base: 0,
        limit: 0xffff_ffff,
        selector: BOOT_DS,
        // Present, data, read/write, accessed; D/B=1, G=1.
        attributes: 0xc093,
    };
    let sregs = SpecialRegisters {
        cs: code,
        ds: data,
        es: data,
        fs: data,
        gs: data,
        ss: data,
        tr: Segment {
            base: 0,
            limit: 0x67,
            selector: BOOT_TSS,
            // Present, 64-bit busy TSS.
            attributes: 0x008b,
        },
        gdt: DescriptorTable {
            base: GDT_ADDR,
            limit: (GDT.len() * 8 - 1) as u16,
        },
        idt: DescriptorTable { base: 0, limit: 0 },
        // PG | NE | ET | PE.
        cr0: 0x8000_0031,
        cr3: PML4_ADDR,
        // PAE.
        cr4: 0x20,
        // LMA | LME.
        efer: 0x500,
        ..*reset
    };
    (regs, sregs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MIB;

    /// A minimal bzImage: a setup header the loader accepts, then `payload`.
    fn fake_bzimage(payload: &[u8], init_size: u32) -> Vec<u8> {
        const SETUP_SECTS: u8 = 4;
        let mut image = vec![0u8; (usize::from(SETUP_SECTS) + 1) * 512];
        let put = |image: &mut Vec<u8>, offset: usize, bytes: &[u8]| {
            image[offset..offset + bytes.len()].copy_from_slice(bytes);
        };
        image[0x1f1] = SETUP_SECTS;
        put(&mut image, 0x1fe, &0xaa55u16.to_le_bytes());
        put(&mut image, 0x202, b"HdrS");
        put(&mut image, 0x206, &0x020fu16.to_le_bytes());
        image[0x211] = LOADED_HIGH;
        put(&mut image, 0x214, &0x10_0000u32.to_le_bytes());
        put(&mut image, 0x22c, &0x7fff_ffffu32.to_le_bytes());
        put(&mut image, 0x230, &0x20_0000u32.to_le_bytes());
        put(&mut image, 0x236, &(XLF_KERNEL_64 | 2).to_le_bytes());
        put(&mut image, 0x238, &2047u32.to_le_bytes());
        put(&mut image, 0x260, &init_size.to_le_bytes());
        image.extend_from_slice(payload);
        image
    }

    fn memory(layout: &MemoryLayout) -> GuestMemoryMmap {
        let ranges: Vec<_> = layout
            .ram
            .iter()
            .map(|&(gpa, size)| (GuestAddress(gpa), size))
            .collect();
        GuestMemoryMmap::from_ranges(&ranges).unwrap()
    }

    fn read<T: vm_memory::ByteValued>(memory: &GuestMemoryMmap, address: u64) -> T {
        memory.read_obj(GuestAddress(address)).unwrap()
    }

    #[test]
    fn loads_kernel_initrd_cmdline_and_zero_page() {
        let layout = MemoryLayout::new(256 * MIB);
        let memory = memory(&layout);
        let image = fake_bzimage(b"KERNEL!", 16 * MIB as u32);
        let initrd = vec![0x5a; 3000];
        let entry = load_linux(
            &memory,
            &layout,
            &image,
            Some(&initrd),
            "console=ttyS0",
            0xe_0000,
        )
        .unwrap();
        assert_eq!(
            entry,
            BootEntry {
                rip: 0x10_0200,
                zero_page: ZERO_PAGE
            }
        );

        let mut kernel = [0u8; 7];
        memory
            .read_slice(&mut kernel, GuestAddress(KERNEL_ADDR))
            .unwrap();
        assert_eq!(&kernel, b"KERNEL!");

        let mut cmdline = [0u8; 14];
        memory
            .read_slice(&mut cmdline, GuestAddress(CMDLINE_ADDR))
            .unwrap();
        assert_eq!(&cmdline, b"console=ttyS0\0");

        let params: boot_params = read(&memory, ZERO_PAGE);
        let header = params.hdr;
        assert_eq!({ header.type_of_loader }, LOADER_TYPE_UNDEFINED);
        assert_eq!({ header.cmd_line_ptr }, CMDLINE_ADDR as u32);
        assert_eq!({ header.cmdline_size }, 13);
        assert_eq!({ params.acpi_rsdp_addr }, 0xe_0000);

        let (initrd_at, initrd_len) = ({ header.ramdisk_image }, { header.ramdisk_size });
        assert_eq!(initrd_len, 3000);
        assert_eq!(initrd_at % 0x1000, 0);
        assert!(
            u64::from(initrd_at) + 3000 <= 256 * MIB,
            "below the end of RAM"
        );
        assert!(
            u64::from(initrd_at) >= KERNEL_ADDR + 16 * MIB,
            "above init_size"
        );
        let mut first = [0u8; 4];
        memory
            .read_slice(&mut first, GuestAddress(initrd_at.into()))
            .unwrap();
        assert_eq!(first, [0x5a; 4]);

        assert_eq!(usize::from(params.e820_entries), layout.e820.len());
        let e820 = params.e820_table;
        let ram = e820[3];
        assert_eq!(
            ({ ram.addr }, { ram.size }, { ram.r#type }),
            (MIB, 255 * MIB, 1)
        );
    }

    #[test]
    fn identity_map_covers_4_gib_with_2_mib_pages() {
        let layout = MemoryLayout::new(64 * MIB);
        let memory = memory(&layout);
        write_page_tables(&memory).unwrap();
        let pml4: u64 = read(&memory, PML4_ADDR);
        assert_eq!(pml4, PDPT_ADDR | PAGE_PRESENT_RW);
        let pdpt3: u64 = read(&memory, PDPT_ADDR + 3 * 8);
        assert_eq!(pdpt3, (PD_ADDR + 0x3000) | PAGE_PRESENT_RW);
        let last: u64 = read(&memory, PD_ADDR + 0x3000 + 511 * 8);
        assert_eq!(last, ((4 << 30) - (2 << 20)) | PAGE_PRESENT_RW | PAGE_HUGE);
    }

    #[test]
    fn rejects_kernels_without_a_64_bit_entry() {
        let layout = MemoryLayout::new(64 * MIB);
        let memory = memory(&layout);
        let mut image = fake_bzimage(b"x", 0x1000);
        image[0x236] = 0;
        let error = load_linux(&memory, &layout, &image, None, "", 0).unwrap_err();
        assert!(error.to_string().contains("64-bit"), "{error}");
    }

    #[test]
    fn boot_cpu_starts_in_long_mode_at_the_entry_point() {
        let entry = BootEntry {
            rip: 0x10_0200,
            zero_page: ZERO_PAGE,
        };
        let reset = SpecialRegisters {
            ldt: Segment {
                attributes: 0x82,
                limit: 0xffff,
                ..Default::default()
            },
            ..Default::default()
        };
        let (regs, sregs) = boot_cpu_registers(&entry, &reset);
        assert_eq!((regs.rip, regs.rsi), (0x10_0200, ZERO_PAGE));
        assert_eq!(regs.rflags & (1 << 9), 0, "interrupts off");
        assert_eq!(sregs.cs.selector, BOOT_CS);
        assert_ne!(sregs.cs.attributes & (1 << 13), 0, "64-bit code segment");
        assert_eq!((sregs.ds.selector, sregs.ss.selector), (BOOT_DS, BOOT_DS));
        assert_eq!(sregs.efer & 0x500, 0x500, "long mode active");
        assert_eq!(sregs.cr3, PML4_ADDR);
        assert_eq!(sregs.ldt, reset.ldt, "untouched");
        assert_eq!(
            GDT[usize::from(BOOT_CS / 8)] >> 53 & 1,
            1,
            "GDT code entry is 64-bit"
        );
    }
}
