//! Saving and restoring a vCPU's whole state, for snapshots and suspend.
//!
//! A saved vCPU is its registers, by WHP's names, and what WHP saves whole: the XSAVE
//! area (x87, SSE, AVX and the rest), the local APIC, and with the enlightenments the
//! synthetic interrupt controller's message and event pages and its timers. Restoring
//! puts them back in an order that keeps each step valid: control registers and MSRs
//! first, then the saved areas (the local APIC needs its base MSR), then the TSC and
//! whatever was pending.

use std::ffi::c_void;

use hv::VcpuState;
use windows::Win32::System::Hypervisor::{
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
};

use crate::api::{WhpError, check};
use crate::regs::Aligned16;
use crate::vcpu::WhpVcpu;

/// What saved states say they are.
pub(crate) const BACKEND: &str = "whp";

/// Restored first, in this order (as `WHV_REGISTER_NAME` numbers, inclusive ranges).
const EARLY: &[(u32, u32)] = &[
    (0x1c, 0x1f),     // CR0, CR2, CR3, CR4 (CR8 is the local APIC's TPR, restored with it)
    (0x2001, 0x2001), // EFER
    (0x27, 0x27),     // XCR0, before the XSAVE area it describes
    (0x12, 0x1b),     // ES, CS, SS, DS, FS, GS, LDTR, TR, IDTR, GDTR
    (0x21, 0x26),     // DR0 to DR3, DR6, DR7
    (0x00, 0x11),     // the general-purpose registers, RIP, RFLAGS
    (0x2002, 0x200b), // KERNEL_GS_BASE, APIC_BASE, PAT, SYSENTER_*, STAR, LSTAR, CSTAR, SFMASK
    (0x200e, 0x200e), // MTRR_DEF_TYPE
    (0x2010, 0x201f), // MTRR_PHYS_BASE0 to F
    (0x2040, 0x204f), // MTRR_PHYS_MASK0 to F
    (0x2070, 0x207a), // the fixed-range MTRRs
    (0x207b, 0x207c), // TSC_AUX, BNDCFGS
    (0x2084, 0x2084), // SPEC_CTRL
    (0x2088, 0x2088), // TSX_CTRL
    (0x208b, 0x2093), // XSS, the CET registers
    (0x2096, 0x2096), // TSC_ADJUST
    (0x2098, 0x209a), // UMWAIT_CONTROL, XFD, XFD_ERR
    (0x5002, 0x5002), // GUEST_OS_ID, which the hypercall page needs set
    (0x5001, 0x5001), // HYPERCALL
    (0x5013, 0x5013), // VP_ASSIST_PAGE
    (0x5017, 0x5017), // REFERENCE_TSC
    (0x4000, 0x4010), // SINT0 to 15, SCONTROL
    (0x4012, 0x4013), // SIEFP, SIMP, before the pages they place
];

/// Restored last, after the saved areas.
const LATE: &[(u32, u32)] = &[
    (0x2000, 0x2000), // TSC
    (0x2095, 0x2095), // TSC_DEADLINE, compared with it
    // PendingInterruption, InterruptState, PendingEvent; DeliverabilityNotifications,
    // InternalActivityState (halted, waiting for a SIPI), PendingDebugException.
    (0x8000_0000, 0x8000_0002),
    (0x8000_0004, 0x8000_0006),
];

/// `WHV_VIRTUAL_PROCESSOR_STATE_TYPE`s saved whole, in the order they're restored:
/// XSAVE, the local APIC, then the SynIC's message page, event flags page and timers.
const XSAVE: u32 = 0x1001;
const LOCAL_APIC: u32 = 0x1000;
const SYNIC: [u32; 3] = [0, 1, 2];

/// `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)`.
const INSUFFICIENT_BUFFER: i32 = 0x8007_007a_u32 as i32;

/// The registers this host's vCPUs have, of [`EARLY`] and [`LATE`].
#[derive(Debug)]
pub(crate) struct Registers {
    early: Vec<WHV_REGISTER_NAME>,
    late: Vec<WHV_REGISTER_NAME>,
}

fn names(ranges: &[(u32, u32)]) -> impl Iterator<Item = WHV_REGISTER_NAME> + '_ {
    ranges
        .iter()
        .flat_map(|&(first, last)| first..=last)
        .map(|name| WHV_REGISTER_NAME(name as i32))
}

pub(crate) fn save_vcpu(vcpu: &WhpVcpu) -> hv::Result<VcpuState> {
    let registers = vcpu.partition.saved_registers.get_or_init(|| Registers {
        early: names(EARLY)
            .filter(|&name| has_register(vcpu, name))
            .collect(),
        late: names(LATE)
            .filter(|&name| has_register(vcpu, name))
            .collect(),
    });
    let mut state = VcpuState {
        backend: BACKEND.into(),
        ..VcpuState::default()
    };
    for list in [&registers.early, &registers.late] {
        let values = get_registers(vcpu, list)?;
        state.registers.extend(
            list.iter()
                .zip(values)
                .map(|(name, value)| (name.0 as u32, to_u128(value))),
        );
    }
    state.blobs.push((XSAVE, get_state(vcpu, XSAVE)?));
    state.blobs.push((LOCAL_APIC, get_state(vcpu, LOCAL_APIC)?));
    if vcpu.partition.enlightenments {
        for kind in SYNIC {
            state.blobs.push((kind, get_state(vcpu, kind)?));
        }
    }
    Ok(state)
}

pub(crate) fn restore_vcpu(vcpu: &mut WhpVcpu, state: &VcpuState) -> hv::Result<()> {
    if state.backend != BACKEND {
        return Err(hv::Error::BadState(format!(
            "it was saved by the {:?} hypervisor backend, not WHP",
            state.backend
        )));
    }
    let late: Vec<u32> = names(LATE).map(|name| name.0 as u32).collect();
    let (late_registers, early_registers): (Vec<_>, Vec<_>) = state
        .registers
        .iter()
        .partition(|(name, _)| late.contains(name));
    set_registers(vcpu, &early_registers)?;
    let mut kinds = vec![XSAVE, LOCAL_APIC];
    if vcpu.partition.enlightenments {
        kinds.extend(SYNIC);
    }
    for kind in kinds {
        let data = state
            .blob(kind)
            .ok_or_else(|| hv::Error::BadState(format!("it has no state of type {kind:#x}")))?;
        set_state(vcpu, kind, data)?;
    }
    set_registers(vcpu, &late_registers)
}

/// Whether this host's WHP has `name` for this vCPU.
fn has_register(vcpu: &WhpVcpu, name: WHV_REGISTER_NAME) -> bool {
    let ok = get_registers(vcpu, &[name]).is_ok();
    if !ok {
        tracing::debug!(
            register = format_args!("{:#x}", name.0),
            "not saved: this host's vCPUs don't have it"
        );
    }
    ok
}

fn get_registers(
    vcpu: &WhpVcpu,
    names: &[WHV_REGISTER_NAME],
) -> hv::Result<Vec<WHV_REGISTER_VALUE>> {
    let mut values = vec![Aligned16(WHV_REGISTER_VALUE::default()); names.len()];
    // SAFETY: `names` and `values` both hold `names.len()` elements, and the values are
    // contiguous 16-byte-aligned WHV_REGISTER_VALUEs, as WHP requires.
    check("WHvGetVirtualProcessorRegisters", unsafe {
        (vcpu.partition.api.WHvGetVirtualProcessorRegisters)(
            vcpu.partition.handle,
            vcpu.index,
            names.as_ptr(),
            names.len() as u32,
            values.as_mut_ptr().cast::<WHV_REGISTER_VALUE>(),
        )
    })?;
    Ok(values.into_iter().map(|value| value.0).collect())
}

/// Sets each register to its saved value: all at once, or, if WHP refuses, one by one
/// to say which it refused.
fn set_registers(vcpu: &WhpVcpu, registers: &[&(u32, u128)]) -> hv::Result<()> {
    let names: Vec<WHV_REGISTER_NAME> = registers
        .iter()
        .map(|&&(name, _)| WHV_REGISTER_NAME(name as i32))
        .collect();
    let values: Vec<Aligned16<WHV_REGISTER_VALUE>> = registers
        .iter()
        .map(|&&(_, value)| Aligned16(from_u128(value)))
        .collect();
    let set = |names: &[WHV_REGISTER_NAME], values: &[Aligned16<WHV_REGISTER_VALUE>]| {
        // SAFETY: `names` and `values` hold the same number of elements, and the values
        // are contiguous 16-byte-aligned WHV_REGISTER_VALUEs, as WHP requires.
        unsafe {
            (vcpu.partition.api.WHvSetVirtualProcessorRegisters)(
                vcpu.partition.handle,
                vcpu.index,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr().cast::<WHV_REGISTER_VALUE>(),
            )
        }
    };
    if set(&names, &values).is_ok() {
        return Ok(());
    }
    for (index, name) in names.iter().enumerate() {
        let hr = set(&names[index..=index], &values[index..=index]);
        if hr.is_err() {
            return Err(hv::Error::BadState(format!(
                "this host won't take register {:#x}: {}",
                name.0,
                WhpError(hr)
            )));
        }
    }
    Ok(())
}

/// A page of memory, aligned as the XSAVE area must be.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

fn get_state(vcpu: &WhpVcpu, kind: u32) -> hv::Result<Vec<u8>> {
    // The XSAVE area grows with the processor's features (AMX takes it past 8 KiB).
    let mut pages = 4;
    loop {
        let mut buffer = vec![Page([0; 4096]); pages];
        let size = pages * size_of::<Page>();
        let mut written = 0u32;
        // SAFETY: `buffer` is `size` writable bytes, and `written` a valid out-pointer.
        let hr = unsafe {
            (vcpu.partition.api.WHvGetVirtualProcessorState)(
                vcpu.partition.handle,
                vcpu.index,
                WHV_VIRTUAL_PROCESSOR_STATE_TYPE(kind as i32),
                buffer.as_mut_ptr().cast::<c_void>(),
                size as u32,
                &mut written,
            )
        };
        if hr.0 == INSUFFICIENT_BUFFER && (written as usize) > size {
            pages = (written as usize).div_ceil(size_of::<Page>());
            continue;
        }
        check("WHvGetVirtualProcessorState", hr)?;
        let bytes: &[u8] = bytemuck_pages(&buffer);
        return Ok(bytes[..written as usize].to_vec());
    }
}

fn set_state(vcpu: &WhpVcpu, kind: u32, data: &[u8]) -> hv::Result<()> {
    let mut buffer = vec![Page([0; 4096]); data.len().div_ceil(size_of::<Page>()).max(1)];
    bytemuck_pages_mut(&mut buffer)[..data.len()].copy_from_slice(data);
    // SAFETY: `buffer` holds at least `data.len()` readable bytes.
    check("WHvSetVirtualProcessorState", unsafe {
        (vcpu.partition.api.WHvSetVirtualProcessorState)(
            vcpu.partition.handle,
            vcpu.index,
            WHV_VIRTUAL_PROCESSOR_STATE_TYPE(kind as i32),
            buffer.as_ptr().cast::<c_void>(),
            data.len() as u32,
        )
    })
}

fn bytemuck_pages(pages: &[Page]) -> &[u8] {
    // SAFETY: `Page` is plain bytes without padding, so the slice is its bytes.
    unsafe { std::slice::from_raw_parts(pages.as_ptr().cast::<u8>(), size_of_val(pages)) }
}

fn bytemuck_pages_mut(pages: &mut [Page]) -> &mut [u8] {
    // SAFETY: `Page` is plain bytes without padding, so the slice is its bytes.
    unsafe { std::slice::from_raw_parts_mut(pages.as_mut_ptr().cast::<u8>(), size_of_val(pages)) }
}

fn to_u128(value: WHV_REGISTER_VALUE) -> u128 {
    // SAFETY: WHV_REGISTER_VALUE is a 16-byte union of plain integers; every bit
    // pattern is a valid [u8; 16].
    u128::from_le_bytes(unsafe { std::mem::transmute::<WHV_REGISTER_VALUE, [u8; 16]>(value) })
}

fn from_u128(value: u128) -> WHV_REGISTER_VALUE {
    // SAFETY: as in `to_u128`, the other way round.
    unsafe { std::mem::transmute::<[u8; 16], WHV_REGISTER_VALUE>(value.to_le_bytes()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_values_survive_the_round_trip() {
        let value = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff_u128;
        assert_eq!(to_u128(from_u128(value)), value);
        let mut register = WHV_REGISTER_VALUE::default();
        register.Reg64 = 0x1234;
        assert_eq!(to_u128(register), 0x1234);
    }

    #[test]
    fn the_lists_name_each_register_once() {
        let all: Vec<i32> = names(EARLY).chain(names(LATE)).map(|name| name.0).collect();
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len());
        assert!(all.contains(&windows::Win32::System::Hypervisor::WHvX64RegisterRip.0));
        assert!(
            all.contains(&windows::Win32::System::Hypervisor::WHvRegisterInternalActivityState.0)
        );
        assert!(!all.contains(&windows::Win32::System::Hypervisor::WHvX64RegisterCr8.0));
    }
}
