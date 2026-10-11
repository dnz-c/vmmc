//! Conversion between the `hv` register structs and WHP register name/value arrays.

use std::ops::{Deref, DerefMut};

use hv::{DescriptorTable, Segment, SpecialRegisters, StandardRegisters};
use windows::Win32::System::Hypervisor::{
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0,
    WHV_X64_TABLE_REGISTER, WHvX64RegisterCr0, WHvX64RegisterCr2, WHvX64RegisterCr3,
    WHvX64RegisterCr4, WHvX64RegisterCr8, WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEfer,
    WHvX64RegisterEs, WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs, WHvX64RegisterIdtr,
    WHvX64RegisterLdtr, WHvX64RegisterR8, WHvX64RegisterR9, WHvX64RegisterR10, WHvX64RegisterR11,
    WHvX64RegisterR12, WHvX64RegisterR13, WHvX64RegisterR14, WHvX64RegisterR15, WHvX64RegisterRax,
    WHvX64RegisterRbp, WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRdi, WHvX64RegisterRdx,
    WHvX64RegisterRflags, WHvX64RegisterRip, WHvX64RegisterRsi, WHvX64RegisterRsp,
    WHvX64RegisterSs, WHvX64RegisterTr,
};

/// Forces 16-byte alignment on a buffer WHP reads or writes.
///
/// WHP copies register values with SSE instructions (`movaps`) that fault on anything
/// less than 16-byte aligned. The Windows SDK declares `WHV_REGISTER_VALUE` with that
/// alignment, but the `windows` crate's definition only has 8, so every register buffer
/// we hand to WHP, and the exit context, goes through this wrapper.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub(crate) struct Aligned16<T>(pub T);

impl<T> Deref for Aligned16<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Aligned16<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

pub(crate) const STANDARD_NAMES: [WHV_REGISTER_NAME; 18] = [
    WHvX64RegisterRax,
    WHvX64RegisterRbx,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
    WHvX64RegisterRip,
    WHvX64RegisterRflags,
];

pub(crate) const SPECIAL_NAMES: [WHV_REGISTER_NAME; 16] = [
    WHvX64RegisterCs,
    WHvX64RegisterDs,
    WHvX64RegisterEs,
    WHvX64RegisterFs,
    WHvX64RegisterGs,
    WHvX64RegisterSs,
    WHvX64RegisterTr,
    WHvX64RegisterLdtr,
    WHvX64RegisterGdtr,
    WHvX64RegisterIdtr,
    WHvX64RegisterCr0,
    WHvX64RegisterCr2,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterCr8,
    WHvX64RegisterEfer,
];

pub(crate) fn reg64(value: u64) -> WHV_REGISTER_VALUE {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Reg64 = value;
    v
}

pub(crate) fn standard_from_values(v: &[WHV_REGISTER_VALUE; 18]) -> StandardRegisters {
    // SAFETY: every name in STANDARD_NAMES is a 64-bit register, so Reg64 is the member
    // WHP filled (or that `standard_to_values` wrote).
    let r = |i: usize| unsafe { v[i].Reg64 };
    StandardRegisters {
        rax: r(0),
        rbx: r(1),
        rcx: r(2),
        rdx: r(3),
        rsi: r(4),
        rdi: r(5),
        rsp: r(6),
        rbp: r(7),
        r8: r(8),
        r9: r(9),
        r10: r(10),
        r11: r(11),
        r12: r(12),
        r13: r(13),
        r14: r(14),
        r15: r(15),
        rip: r(16),
        rflags: r(17),
    }
}

pub(crate) fn standard_to_values(r: &StandardRegisters) -> [WHV_REGISTER_VALUE; 18] {
    [
        r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11, r.r12,
        r.r13, r.r14, r.r15, r.rip, r.rflags,
    ]
    .map(reg64)
}

pub(crate) fn special_from_values(v: &[WHV_REGISTER_VALUE; 16]) -> SpecialRegisters {
    // SPECIAL_NAMES holds segment registers at 0..8, descriptor tables at 8..10 and
    // 64-bit registers at 10..16; each closure below is only called with its own indices.
    // SAFETY: indices 0..8 are segment registers, so WHP filled the Segment member.
    let seg = |i: usize| segment_from_whp(unsafe { &v[i].Segment });
    let table = |i: usize| {
        // SAFETY: indices 8..10 are GDTR/IDTR, so WHP filled the Table member.
        let t = unsafe { v[i].Table };
        DescriptorTable {
            base: t.Base,
            limit: t.Limit,
        }
    };
    // SAFETY: indices 10..16 are 64-bit registers, so WHP filled the Reg64 member.
    let r = |i: usize| unsafe { v[i].Reg64 };
    SpecialRegisters {
        cs: seg(0),
        ds: seg(1),
        es: seg(2),
        fs: seg(3),
        gs: seg(4),
        ss: seg(5),
        tr: seg(6),
        ldt: seg(7),
        gdt: table(8),
        idt: table(9),
        cr0: r(10),
        cr2: r(11),
        cr3: r(12),
        cr4: r(13),
        cr8: r(14),
        efer: r(15),
    }
}

pub(crate) fn special_to_values(s: &SpecialRegisters) -> [WHV_REGISTER_VALUE; 16] {
    let seg = |segment: &Segment| {
        let mut v = WHV_REGISTER_VALUE::default();
        v.Segment = segment_to_whp(segment);
        v
    };
    let table = |t: &DescriptorTable| {
        let mut v = WHV_REGISTER_VALUE::default();
        v.Table = WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: t.limit,
            Base: t.base,
        };
        v
    };
    [
        seg(&s.cs),
        seg(&s.ds),
        seg(&s.es),
        seg(&s.fs),
        seg(&s.gs),
        seg(&s.ss),
        seg(&s.tr),
        seg(&s.ldt),
        table(&s.gdt),
        table(&s.idt),
        reg64(s.cr0),
        reg64(s.cr2),
        reg64(s.cr3),
        reg64(s.cr4),
        reg64(s.cr8),
        reg64(s.efer),
    ]
}

fn segment_from_whp(s: &WHV_X64_SEGMENT_REGISTER) -> Segment {
    Segment {
        base: s.Base,
        limit: s.Limit,
        selector: s.Selector,
        // SAFETY: Attributes spans the whole 16-bit union.
        attributes: unsafe { s.Anonymous.Attributes },
    }
}

fn segment_to_whp(s: &Segment) -> WHV_X64_SEGMENT_REGISTER {
    WHV_X64_SEGMENT_REGISTER {
        Base: s.base,
        Limit: s.limit,
        Selector: s.selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: s.attributes,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_buffers_are_16_byte_aligned() {
        // A u8 first, so an unaligned buffer would actually land unaligned.
        let pair = (0u8, Aligned16([WHV_REGISTER_VALUE::default(); 3]));
        assert_eq!(std::mem::align_of_val(&pair.1), 16);
        assert_eq!((&raw const pair.1).addr() % 16, 0);
    }

    #[test]
    fn standard_registers_round_trip() {
        let regs = StandardRegisters {
            rax: 1,
            rbx: 2,
            rcx: 3,
            rdx: 4,
            rsi: 5,
            rdi: 6,
            rsp: 7,
            rbp: 8,
            r8: 9,
            r9: 10,
            r10: 11,
            r11: 12,
            r12: 13,
            r13: 14,
            r14: 15,
            r15: 16,
            rip: 0x1000,
            rflags: 0x2,
        };
        assert_eq!(standard_from_values(&standard_to_values(&regs)), regs);
    }

    #[test]
    fn special_registers_round_trip() {
        let sregs = SpecialRegisters {
            cs: Segment::real_mode_code(0xf000),
            ds: Segment::real_mode_data(0x10),
            es: Segment::real_mode_data(0x20),
            fs: Segment::real_mode_data(0x30),
            gs: Segment::real_mode_data(0x40),
            ss: Segment::real_mode_data(0x50),
            tr: Segment {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: 0x8b,
            },
            ldt: Segment {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: 0x82,
            },
            gdt: DescriptorTable {
                base: 0x500,
                limit: 0x27,
            },
            idt: DescriptorTable {
                base: 0,
                limit: 0x3ff,
            },
            cr0: 0x6000_0010,
            cr2: 0xdead_b000,
            cr3: 0x1000,
            cr4: 0x20,
            cr8: 0,
            efer: 0x500,
        };
        assert_eq!(special_from_values(&special_to_values(&sregs)), sregs);
    }
}
