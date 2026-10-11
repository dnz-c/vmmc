//! The CPUID leaves the VMM answers itself, starting from WHP's own results.
//!
//! - **Topology (leaves 1, 4, 0xB, 0x1F):** one package with every vCPU as a
//!   single-threaded core, as the SMBIOS tables say. WHP's defaults make each vCPU its
//!   own package, and Windows Pro licenses two sockets, so it would use only two vCPUs
//!   (Home only one).
//! - **Leaf 6:** ARAT. WHP's APIC timer keeps running in every idle state, but WHP
//!   reports leaf 6 as all zeroes, so guests would treat the APIC timer as one that
//!   stops in deep C-states and lean on platform timers instead.

/// The leaves [`adjust`] changes. Only these exit to the VMM.
pub(crate) const LEAVES: [u32; 5] = [1, 4, 6, 0xb, 0x1f];

const LEAF1_EDX_HTT: u32 = 1 << 28;
const LEAF6_EAX_ARAT: u32 = 1 << 2;
/// Extended topology level types.
const LEVEL_SMT: u32 = 1;
const LEVEL_CORE: u32 = 2;

/// WHP's result for `leaf`/`subleaf` (`default`, as EAX, EBX, ECX, EDX), corrected as
/// vCPU `apic_id` of `vcpus` should see it.
pub(crate) fn adjust(
    leaf: u32,
    subleaf: u32,
    default: [u32; 4],
    apic_id: u32,
    vcpus: u32,
) -> [u32; 4] {
    let [mut eax, mut ebx, mut ecx, mut edx] = default;
    // The APIC ID bits that number cores within the package.
    let core_bits = vcpus.max(1).next_power_of_two().trailing_zeros();
    let core_ids = (1u32 << core_bits) - 1;
    match leaf {
        1 => {
            // Logical processor IDs the package can address, and HTT when there are
            // several of them.
            ebx = (ebx & !0x00ff_0000) | ((core_ids + 1).min(0xff) << 16);
            if vcpus > 1 {
                edx |= LEAF1_EDX_HTT;
            } else {
                edx &= !LEAF1_EDX_HTT;
            }
        }
        // Subleaves describe caches until one has cache type 0.
        4 if eax & 0x1f != 0 => {
            // Each core has its own L1 and L2; the package shares the rest.
            let level = (eax >> 5) & 0x7;
            let sharing = if level >= 3 { core_ids } else { 0 };
            eax = (eax & 0x3fff) | ((sharing & 0xfff) << 14) | ((core_ids & 0x3f) << 26);
        }
        6 => eax |= LEAF6_EAX_ARAT,
        0xb | 0x1f => {
            (eax, ebx, ecx) = match subleaf {
                0 => (0, 1, LEVEL_SMT << 8),
                1 => (core_bits, vcpus, (LEVEL_CORE << 8) | 1),
                level => (0, 0, level & 0xff),
            };
            edx = apic_id;
        }
        _ => {}
    }
    [eax, ebx, ecx, edx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_1_counts_the_package_and_keeps_the_rest() {
        // APIC ID 2 in EBX[31:24]; WHP said one logical processor and no HTT.
        let [_, ebx, ecx, edx] = adjust(
            1,
            0,
            [
                0x906a0,
                0x0201_0800,
                0xfeda_3203,
                0x178b_fbff & !LEAF1_EDX_HTT,
            ],
            2,
            4,
        );
        assert_eq!(ebx, 0x0204_0800, "APIC ID kept, 4 addressable IDs");
        assert_eq!(ecx, 0xfeda_3203);
        assert_ne!(edx & LEAF1_EDX_HTT, 0);
        let [_, ebx, _, edx] = adjust(1, 0, [0, 0, 0, LEAF1_EDX_HTT], 0, 1);
        assert_eq!((ebx >> 16) & 0xff, 1);
        assert_eq!(edx & LEAF1_EDX_HTT, 0, "one vCPU: no HTT");
    }

    #[test]
    fn extended_topology_is_one_package_of_cores() {
        assert_eq!(adjust(0xb, 0, [0; 4], 3, 4), [0, 1, 0x100, 3]);
        assert_eq!(adjust(0xb, 1, [0; 4], 3, 4), [2, 4, 0x201, 3]);
        assert_eq!(
            adjust(0x1f, 2, [0; 4], 3, 4),
            [0, 0, 2, 3],
            "no more levels"
        );
        // Six vCPUs need three bits of core ID.
        assert_eq!(adjust(0xb, 1, [0; 4], 5, 6)[0], 3);
    }

    #[test]
    fn caches_are_per_core_except_the_last_level() {
        // L1 data cache (type 1, level 1) and L3 (type 3, level 3), as WHP passes them.
        let l1 = adjust(4, 0, [0x1c00_4121, 0x02c0_003f, 0x3f, 0], 0, 4)[0];
        assert_eq!(l1 & 0x3fff, 0x0121, "type and level kept");
        assert_eq!((l1 >> 14) & 0xfff, 0, "one thread per L1");
        assert_eq!(l1 >> 26, 3, "four core IDs");
        let l3 = adjust(4, 3, [0x1c03_c163, 0x03c0_003f, 0x9fff, 6], 0, 4)[0];
        assert_eq!((l3 >> 14) & 0xfff, 3, "shared by all four");
        // The terminating subleaf stays zero.
        assert_eq!(adjust(4, 5, [0; 4], 0, 4), [0; 4]);
    }

    #[test]
    fn leaf_6_says_the_apic_timer_always_runs() {
        assert_eq!(adjust(6, 0, [0; 4], 0, 4)[0], LEAF6_EAX_ARAT);
    }
}
