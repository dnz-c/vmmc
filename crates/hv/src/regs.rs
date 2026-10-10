//! x86-64 vCPU register state in a backend-neutral layout.

/// General-purpose registers, instruction pointer and flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandardRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// A segment register including its hidden descriptor cache.
///
/// `attributes` uses the VMX access-rights layout packed into 16 bits:
/// type `[3:0]`, S `[4]`, DPL `[6:5]`, P `[7]`, AVL `[12]`, L `[13]`, D/B `[14]`, G `[15]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attributes: u16,
}

impl Segment {
    /// A real-mode code segment: base = selector × 16, 64 KiB limit, present,
    /// execute/read, accessed.
    pub const fn real_mode_code(selector: u16) -> Self {
        Self {
            base: (selector as u64) << 4,
            limit: 0xffff,
            selector,
            attributes: 0x9b,
        }
    }

    /// A real-mode data segment: base = selector × 16, 64 KiB limit, present,
    /// read/write, accessed.
    pub const fn real_mode_data(selector: u16) -> Self {
        Self {
            base: (selector as u64) << 4,
            limit: 0xffff,
            selector,
            attributes: 0x93,
        }
    }
}

/// GDTR or IDTR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DescriptorTable {
    pub base: u64,
    pub limit: u16,
}

/// Segment, descriptor-table and control registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecialRegisters {
    pub cs: Segment,
    pub ds: Segment,
    pub es: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub ss: Segment,
    pub tr: Segment,
    pub ldt: Segment,
    pub gdt: DescriptorTable,
    pub idt: DescriptorTable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_mode_segments_use_selector_times_16_as_base() {
        let cs = Segment::real_mode_code(0xf000);
        assert_eq!(cs.base, 0xf_0000);
        assert_eq!(cs.limit, 0xffff);
        // Present, code, readable, accessed.
        assert_eq!(cs.attributes & 0x80, 0x80);
        assert_eq!(cs.attributes & 0x1f, 0x1b);

        let ds = Segment::real_mode_data(0x0010);
        assert_eq!(ds.base, 0x100);
        // Present, data, writable, accessed.
        assert_eq!(ds.attributes & 0x1f, 0x13);
    }
}
