//! vCPU state reports for debugging guest hangs: the registers, and the code and stack
//! they point at.

use hv::{SpecialRegisters, StandardRegisters};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const CR0_PG: u64 = 1 << 31;
const CR4_PSE: u64 = 1 << 4;
const CR4_PAE: u64 = 1 << 5;
const CR4_LA57: u64 = 1 << 12;
const EFER_LMA: u64 = 1 << 10;
const PRESENT: u64 = 1;
const PAGE_SIZE: u64 = 1 << 7;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
/// The L (64-bit code) bit in [`hv::Segment::attributes`].
const SEGMENT_L: u16 = 1 << 13;

/// The guest-physical address `linear` maps to in the vCPU's paging mode, or None if
/// it isn't mapped or a page table isn't in RAM.
pub fn linear_to_physical(
    memory: &GuestMemoryMmap,
    sregs: &SpecialRegisters,
    linear: u64,
) -> Option<u64> {
    if sregs.cr0 & CR0_PG == 0 {
        return Some(linear);
    }
    if sregs.efer & EFER_LMA == 0 && sregs.cr4 & CR4_PAE == 0 {
        // 32-bit paging: two levels of 10 bits.
        let read = |gpa: u64| {
            memory
                .read_obj::<u32>(GuestAddress(gpa))
                .ok()
                .map(u64::from)
        };
        let linear = linear & 0xffff_ffff;
        let pde = read((sregs.cr3 & 0xffff_f000) + (linear >> 22) * 4)?;
        if pde & PRESENT == 0 {
            return None;
        }
        if sregs.cr4 & CR4_PSE != 0 && pde & PAGE_SIZE != 0 {
            return Some((pde & 0xffc0_0000) | (linear & 0x3f_ffff));
        }
        let pte = read((pde & 0xffff_f000) + ((linear >> 12) & 0x3ff) * 4)?;
        return (pte & PRESENT != 0).then_some((pte & 0xffff_f000) | (linear & 0xfff));
    }

    // PAE and long mode: levels of 9 bits with 64-bit entries.
    let read = |gpa: u64| memory.read_obj::<u64>(GuestAddress(gpa)).ok();
    let (levels, mut table) = if sregs.efer & EFER_LMA == 0 {
        // PAE: CR3 points at four PDPTEs, picked by bits 31:30.
        let pdpte = read((sregs.cr3 & 0xffff_ffe0) + ((linear >> 30) & 3) * 8)?;
        if pdpte & PRESENT == 0 {
            return None;
        }
        (2, pdpte & ADDRESS)
    } else if sregs.cr4 & CR4_LA57 != 0 {
        (5, sregs.cr3 & ADDRESS)
    } else {
        (4, sregs.cr3 & ADDRESS)
    };
    for level in (1..=levels).rev() {
        let shift = 12 + 9 * (level - 1);
        let entry = read(table + ((linear >> shift) & 0x1ff) * 8)?;
        if entry & PRESENT == 0 {
            return None;
        }
        // Levels 2 and 3 can map 2 MiB and 1 GiB pages.
        if level == 1 || (level <= 3 && entry & PAGE_SIZE != 0) {
            let offset = (1u64 << shift) - 1;
            return Some((entry & ADDRESS & !offset) | (linear & offset));
        }
        table = entry & ADDRESS;
    }
    None
}

/// Reads up to `len` bytes at `linear`, stopping at the first unmapped byte.
fn read_linear(
    memory: &GuestMemoryMmap,
    sregs: &SpecialRegisters,
    linear: u64,
    len: u64,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    for at in 0..len {
        let byte = linear_to_physical(memory, sregs, linear.wrapping_add(at))
            .and_then(|gpa| memory.read_obj::<u8>(GuestAddress(gpa)).ok());
        match byte {
            Some(byte) => bytes.push(byte),
            None => break,
        }
    }
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!(" {byte:02x}")).collect()
}

/// The local APIC registers that matter for hangs, from its xAPIC register page.
fn describe_apic(page: &[u8]) -> Option<String> {
    let reg = |offset: usize| {
        page.get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // ISR and IRR are 256 bits spread over eight registers, 16 bytes apart.
    let highest = |base: usize| -> Option<String> {
        for word in (0..8).rev() {
            let bits = reg(base + word * 0x10)?;
            if bits != 0 {
                return Some(format!(
                    "{:#x}",
                    word * 32 + 31 - bits.leading_zeros() as usize
                ));
            }
        }
        Some("none".into())
    };
    Some(format!(
        "  apic: id={:#x} tpr={:#x} svr={:#x} esr={:#x} icr={:#x}:{:#x} lvt-timer={:#x} \
         timer={:#x}/{:#x} divide={:#x} isr={} irr={}",
        reg(0x20)? >> 24,
        reg(0x80)?,
        reg(0xf0)?,
        reg(0x280)?,
        reg(0x310)?,
        reg(0x300)?,
        reg(0x320)?,
        reg(0x390)?,
        reg(0x380)?,
        reg(0x3e0)?,
        highest(0x100)?,
        highest(0x200)?,
    ))
}

/// A multi-line report of vCPU `index`: its registers (with its local APIC's, given
/// its register page), the code around RIP and the top of its stack.
pub fn describe(
    index: u32,
    regs: &StandardRegisters,
    sregs: &SpecialRegisters,
    apic: Option<&[u8]>,
    memory: &GuestMemoryMmap,
) -> String {
    let long_mode = sregs.efer & EFER_LMA != 0;
    let code_64 = long_mode && sregs.cs.attributes & SEGMENT_L != 0;
    // Segment bases don't apply to 64-bit code (and are zero for its stack).
    let flat = |base: u64, offset: u64| {
        if code_64 {
            offset
        } else {
            base.wrapping_add(offset) & 0xffff_ffff
        }
    };
    let mut out = format!(
        "vCPU {index}: rip={:#x} rflags={:#x} cs={:#x} (base {:#x}, attr {:#x}) \
         cr0={:#x} cr2={:#x} cr3={:#x} cr4={:#x} efer={:#x}\n",
        regs.rip,
        regs.rflags,
        sregs.cs.selector,
        sregs.cs.base,
        sregs.cs.attributes,
        sregs.cr0,
        sregs.cr2,
        sregs.cr3,
        sregs.cr4,
        sregs.efer,
    );
    out += &format!(
        "  rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x} rbp={:#x} rsp={:#x}\n",
        regs.rax, regs.rbx, regs.rcx, regs.rdx, regs.rsi, regs.rdi, regs.rbp, regs.rsp,
    );
    out += &format!(
        "  r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}\n",
        regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
    );
    if let Some(line) = apic.and_then(describe_apic) {
        out += &line;
        out += "\n";
    }
    let rip = flat(sregs.cs.base, regs.rip);
    // Only shown when all of it is mapped, so the last byte is the one before RIP.
    let before = read_linear(memory, sregs, rip.wrapping_sub(32), 32);
    if before.len() == 32 {
        out += &format!("  code before rip:{}\n", hex(&before));
    }
    let code = read_linear(memory, sregs, rip, 32);
    if code.is_empty() {
        out += "  code: (not mapped)\n";
    } else {
        out += &format!("  code:{}\n", hex(&code));
    }
    out += "  stack:";
    let width = if long_mode { 8 } else { 4 };
    let stack = read_linear(memory, sregs, flat(sregs.ss.base, regs.rsp), 8 * width);
    for word in stack.chunks_exact(width as usize) {
        let mut value = [0u8; 8];
        value[..word.len()].copy_from_slice(word);
        out += &format!(" {:#x}", u64::from_le_bytes(value));
    }
    if stack.is_empty() {
        out += " (not mapped)";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * MIB)]).unwrap()
    }

    fn write64(memory: &GuestMemoryMmap, gpa: u64, value: u64) {
        memory.write_obj(value, GuestAddress(gpa)).unwrap();
    }

    fn long_mode(cr3: u64) -> SpecialRegisters {
        SpecialRegisters {
            cr0: CR0_PG | 1,
            cr3,
            cr4: CR4_PAE,
            efer: EFER_LMA | (1 << 8),
            ..SpecialRegisters::default()
        }
    }

    #[test]
    fn without_paging_linear_is_physical() {
        let sregs = SpecialRegisters::default();
        assert_eq!(linear_to_physical(&memory(), &sregs, 0x1234), Some(0x1234));
    }

    #[test]
    fn walks_four_levels_with_4k_and_2m_pages() {
        let memory = memory();
        // PML4 at 0x1000 -> PDPT at 0x2000 -> PD at 0x3000 -> PT at 0x4000.
        write64(&memory, 0x1000, 0x2000 | 3);
        write64(&memory, 0x2000, 0x3000 | 3);
        // PD entry 0 is a page table, entry 1 a 2 MiB page at 0x20_0000.
        write64(&memory, 0x3000, 0x4000 | 3);
        write64(&memory, 0x3008, 0x20_0000 | PAGE_SIZE | 3);
        // Linear page 5 maps to physical page 0x9.
        write64(&memory, 0x4000 + 5 * 8, 0x9000 | 3);
        let sregs = long_mode(0x1000);
        assert_eq!(linear_to_physical(&memory, &sregs, 0x5123), Some(0x9123));
        assert_eq!(linear_to_physical(&memory, &sregs, 0x6000), None);
        assert_eq!(
            linear_to_physical(&memory, &sregs, 0x20_1234),
            Some(0x20_1234)
        );
        // PML4 entry 1 isn't present.
        assert_eq!(linear_to_physical(&memory, &sregs, 1 << 39), None);
    }

    #[test]
    fn describes_registers_code_and_stack() {
        let memory = memory();
        memory
            .write_slice(&[0xeb, 0xfe], GuestAddress(0x8000))
            .unwrap();
        write64(&memory, 0x7ff8, 0xdead_beef);
        let regs = StandardRegisters {
            rip: 0x8000,
            rsp: 0x7ff8,
            ..StandardRegisters::default()
        };
        let mut sregs = SpecialRegisters {
            efer: EFER_LMA,
            ..SpecialRegisters::default()
        };
        sregs.cs.attributes = SEGMENT_L | 0x9b;
        let mut apic = vec![0u8; 4096];
        apic[0x23] = 2; // APIC ID 2
        apic[0x300..0x304].copy_from_slice(&0x4500u32.to_le_bytes()); // INIT
        apic[0x210] = 0x01; // vector 0x20 pending in IRR
        let text = describe(2, &regs, &sregs, Some(&apic), &memory);
        assert!(text.starts_with("vCPU 2: rip=0x8000"), "{text}");
        assert!(text.contains("apic: id=0x2 "), "{text}");
        assert!(text.contains("icr=0x0:0x4500 "), "{text}");
        assert!(text.contains("isr=none irr=0x20"), "{text}");
        assert!(text.contains("code before rip: 00"), "{text}");
        assert!(text.contains("code: eb fe 00"), "{text}");
        assert!(text.contains("stack: 0xdeadbeef 0x"), "{text}");
    }
}
