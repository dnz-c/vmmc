//! WinHvEmulation's instruction emulator. It completes exits that need the faulting
//! instruction decoded (MMIO accesses and INS/OUTS) by calling back into the VMM.

use std::any::Any;
use std::cell::Cell;
use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::sync::Arc;

use hv::CpuIo;
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::System::Hypervisor::{
    WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO, WHV_EMULATOR_MEMORY_ACCESS_INFO,
    WHV_EMULATOR_STATUS, WHV_MEMORY_ACCESS_CONTEXT, WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
    WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT, WHV_TRANSLATE_GVA_RESULT_CODE,
    WHV_VP_EXIT_CONTEXT, WHV_X64_IO_PORT_ACCESS_CONTEXT,
};
use windows::core::HRESULT;

use crate::api::{Api, check};
use crate::partition::PartitionInner;

static CALLBACKS: WHV_EMULATOR_CALLBACKS = WHV_EMULATOR_CALLBACKS {
    Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
    Reserved: 0,
    WHvEmulatorIoPortCallback: Some(io_port_callback),
    WHvEmulatorMemoryCallback: Some(memory_callback),
    WHvEmulatorGetVirtualProcessorRegisters: Some(get_registers_callback),
    WHvEmulatorSetVirtualProcessorRegisters: Some(set_registers_callback),
    WHvEmulatorTranslateGvaPage: Some(translate_gva_callback),
};

/// One emulator instance. Each vCPU owns its own.
pub(crate) struct Emulator {
    api: Arc<Api>,
    handle: *mut c_void,
}

// SAFETY: the handle is only used through `&mut WhpVcpu`, so by one thread at a time,
// and the emulator has no thread affinity.
unsafe impl Send for Emulator {}

impl Emulator {
    pub fn new(api: Arc<Api>) -> hv::Result<Self> {
        let mut handle = ptr::null_mut();
        // SAFETY: CALLBACKS is a valid, 'static callback table and `handle` a valid
        // out-pointer.
        check("WHvEmulatorCreateEmulator", unsafe {
            (api.WHvEmulatorCreateEmulator)(&CALLBACKS, &mut handle)
        })?;
        Ok(Self { api, handle })
    }

    /// Emulates the INS/OUTS instruction behind an I/O port exit.
    pub fn io(
        &self,
        context: &Context<'_>,
        vp: &WHV_VP_EXIT_CONTEXT,
        io: &WHV_X64_IO_PORT_ACCESS_CONTEXT,
    ) -> hv::Result<Result<(), String>> {
        let mut status = WHV_EMULATOR_STATUS::default();
        // SAFETY: all pointers are valid for the call; `context` outlives it and is what
        // the callbacks cast the context pointer back to.
        let hr = unsafe {
            (self.api.WHvEmulatorTryIoEmulation)(
                self.handle,
                ptr::from_ref(context).cast(),
                vp,
                io,
                &mut status,
            )
        };
        context.resume_panic();
        check("WHvEmulatorTryIoEmulation", hr)?;
        Ok(status_result(status))
    }

    /// Emulates the instruction behind a memory access exit.
    pub fn mmio(
        &self,
        context: &Context<'_>,
        vp: &WHV_VP_EXIT_CONTEXT,
        memory: &WHV_MEMORY_ACCESS_CONTEXT,
    ) -> hv::Result<Result<(), String>> {
        let mut status = WHV_EMULATOR_STATUS::default();
        // SAFETY: as in `io`.
        let hr = unsafe {
            (self.api.WHvEmulatorTryMmioEmulation)(
                self.handle,
                ptr::from_ref(context).cast(),
                vp,
                memory,
                &mut status,
            )
        };
        context.resume_panic();
        check("WHvEmulatorTryMmioEmulation", hr)?;
        Ok(status_result(status))
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: the handle came from WHvEmulatorCreateEmulator and isn't used again.
        let _ = unsafe { (self.api.WHvEmulatorDestroyEmulator)(self.handle) };
    }
}

/// What the callbacks need. The emulator hands it back to them as its context pointer.
pub(crate) struct Context<'a> {
    pub partition: &'a PartitionInner,
    pub index: u32,
    pub io: &'a dyn CpuIo,
    /// A panic caught in a callback, re-raised once the emulator call has returned.
    panic: Cell<Option<Box<dyn Any + Send>>>,
}

impl<'a> Context<'a> {
    pub fn new(partition: &'a PartitionInner, index: u32, io: &'a dyn CpuIo) -> Self {
        Self {
            partition,
            index,
            io,
            panic: Cell::new(None),
        }
    }

    /// Runs a callback body. Panics must not unwind into the emulator, so they are
    /// parked here and re-raised by `resume_panic`.
    fn guard(&self, body: impl FnOnce() -> HRESULT) -> HRESULT {
        match panic::catch_unwind(AssertUnwindSafe(body)) {
            Ok(hr) => hr,
            Err(payload) => {
                self.panic.set(Some(payload));
                E_FAIL
            }
        }
    }

    fn resume_panic(&self) {
        if let Some(payload) = self.panic.take() {
            panic::resume_unwind(payload);
        }
    }
}

/// Recovers the `Context` behind a callback's context pointer.
///
/// # Safety
///
/// `context` must be the pointer `Emulator::io` or `Emulator::mmio` passed in, during
/// that call.
unsafe fn context<'a>(context: *const c_void) -> &'a Context<'a> {
    // SAFETY: guaranteed by the caller.
    unsafe { &*context.cast::<Context<'a>>() }
}

unsafe extern "system" fn io_port_callback(
    context_ptr: *const c_void,
    access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: the emulator passes back our context pointer during our call.
    let context = unsafe { context(context_ptr) };
    // SAFETY: the emulator passes a valid, exclusive access record.
    let access = unsafe { &mut *access };
    context.guard(|| {
        let size = usize::from(access.AccessSize);
        if !matches!(size, 1 | 2 | 4) {
            return E_FAIL;
        }
        if access.Direction == 0 {
            let mut bytes = [0u8; 4];
            context.io.io_read(access.Port, &mut bytes[..size]);
            access.Data = u32::from_le_bytes(bytes);
        } else {
            context
                .io
                .io_write(access.Port, &access.Data.to_le_bytes()[..size]);
        }
        S_OK
    })
}

unsafe extern "system" fn memory_callback(
    context_ptr: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: the emulator passes back our context pointer during our call.
    let context = unsafe { context(context_ptr) };
    // SAFETY: the emulator passes a valid, exclusive access record.
    let access = unsafe { &mut *access };
    context.guard(|| {
        let size = usize::from(access.AccessSize);
        if !(1..=8).contains(&size) {
            return E_FAIL;
        }
        if access.Direction == 0 {
            access.Data = [0; 8];
            context
                .io
                .mem_read(access.GpaAddress, &mut access.Data[..size]);
        } else {
            context
                .io
                .mem_write(access.GpaAddress, &access.Data[..size]);
        }
        S_OK
    })
}

unsafe extern "system" fn get_registers_callback(
    context_ptr: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: the emulator passes back our context pointer during our call.
    let context = unsafe { context(context_ptr) };
    // SAFETY: the emulator passes `count` names and room for `count` values.
    unsafe {
        (context.partition.api.WHvGetVirtualProcessorRegisters)(
            context.partition.handle,
            context.index,
            names,
            count,
            values,
        )
    }
}

unsafe extern "system" fn set_registers_callback(
    context_ptr: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: the emulator passes back our context pointer during our call.
    let context = unsafe { context(context_ptr) };
    // SAFETY: the emulator passes `count` names and `count` values.
    unsafe {
        (context.partition.api.WHvSetVirtualProcessorRegisters)(
            context.partition.handle,
            context.index,
            names,
            count,
            values,
        )
    }
}

unsafe extern "system" fn translate_gva_callback(
    context_ptr: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result_code: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: the emulator passes back our context pointer during our call.
    let context = unsafe { context(context_ptr) };
    let mut result = WHV_TRANSLATE_GVA_RESULT {
        ResultCode: WHV_TRANSLATE_GVA_RESULT_CODE(0),
        Reserved: 0,
    };
    // SAFETY: `result` is a valid out-parameter and the emulator passes a valid `gpa`.
    let hr = unsafe {
        (context.partition.api.WHvTranslateGva)(
            context.partition.handle,
            context.index,
            gva,
            flags,
            &mut result,
            gpa,
        )
    };
    if hr.is_ok() {
        // SAFETY: the emulator passes a valid `result_code`.
        unsafe { *result_code = result.ResultCode };
    }
    hr
}

/// Bit names of `WHV_EMULATOR_STATUS`, from bit 0 up.
const STATUS_BITS: [&str; 10] = [
    "EmulationSuccessful",
    "InternalEmulationFailure",
    "IoPortCallbackFailed",
    "MemoryCallbackFailed",
    "TranslateGvaPageCallbackFailed",
    "TranslateGvaPageCallbackGpaIsNotAligned",
    "GetVirtualProcessorRegistersCallbackFailed",
    "SetVirtualProcessorRegistersCallbackFailed",
    "InterruptCausedIntercept",
    "GuestCannotBeFaulted",
];

fn status_result(status: WHV_EMULATOR_STATUS) -> Result<(), String> {
    // SAFETY: AsUINT32 spans the whole union.
    let bits = unsafe { status.AsUINT32 };
    if bits & 1 != 0 {
        return Ok(());
    }
    let failures: Vec<&str> = STATUS_BITS
        .iter()
        .enumerate()
        .skip(1)
        .filter(|&(bit, _)| bits & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect();
    Err(format!(
        "instruction emulation failed: {}",
        failures.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_success_or_names_the_failures() {
        let status = |bits| WHV_EMULATOR_STATUS { AsUINT32: bits };
        assert_eq!(status_result(status(1)), Ok(()));
        assert_eq!(
            status_result(status(0b1000)),
            Err("instruction emulation failed: MemoryCallbackFailed".into())
        );
        assert_eq!(
            status_result(status(0b10_0000_0010)),
            Err(
                "instruction emulation failed: InternalEmulationFailure, GuestCannotBeFaulted"
                    .into()
            )
        );
    }
}
