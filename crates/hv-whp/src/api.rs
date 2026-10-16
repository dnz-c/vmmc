//! WinHvPlatform.dll and WinHvEmulation.dll entry points, resolved at runtime.
//!
//! Linking the DLLs at load time would stop every vmmc binary from starting, with a
//! cryptic loader error, on hosts that don't have the Windows Hypervisor Platform feature
//! installed. Resolving the functions here lets `Whp::new` tell the user how to fix it.
//! The DLLs are only ever loaded from System32, never from the application directory or
//! the current directory.

use std::ffi::c_void;
use std::fmt;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Hypervisor::{
    WHV_CAPABILITY_CODE, WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_STATUS, WHV_INTERRUPT_CONTROL,
    WHV_MAP_GPA_RANGE_FLAGS, WHV_MEMORY_ACCESS_CONTEXT, WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_TRANSLATE_GVA_FLAGS,
    WHV_TRANSLATE_GVA_RESULT, WHV_VIRTUAL_PROCESSOR_STATE_TYPE, WHV_VP_EXIT_CONTEXT,
    WHV_X64_IO_PORT_ACCESS_CONTEXT,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HRESULT, PCSTR, PCWSTR, w};

type RawProc = unsafe extern "system" fn() -> isize;

macro_rules! whp_api {
    ($($dll:literal {
        $($name:ident($($arg:ident: $ty:ty),* $(,)?);)*
    })*) => {
        #[allow(non_snake_case)]
        pub(crate) struct Api {
            $($(pub $name: unsafe extern "system" fn($($ty),*) -> HRESULT,)*)*
        }

        impl Api {
            #[allow(non_snake_case)]
            pub fn load() -> Result<Self, String> {
                $(
                    let module = load_system_dll(w!($dll), $dll)?;
                    $(let $name = resolve(module, $dll, concat!(stringify!($name), "\0"))?;)*
                )*
                Ok(Self {
                    $($($name: {
                        // SAFETY: the signature matches the declaration in the Windows SDK
                        // headers (WinHvPlatform.h / WinHvEmulation.h).
                        unsafe {
                            std::mem::transmute::<
                                RawProc,
                                unsafe extern "system" fn($($ty),*) -> HRESULT,
                            >($name)
                        }
                    },)*)*
                })
            }
        }
    };
}

whp_api! {
    "WinHvPlatform.dll" {
        WHvGetCapability(
            code: WHV_CAPABILITY_CODE,
            buffer: *mut c_void,
            buffer_size: u32,
            written_size: *mut u32,
        );
        WHvCreatePartition(partition: *mut WHV_PARTITION_HANDLE);
        WHvSetupPartition(partition: WHV_PARTITION_HANDLE);
        WHvDeletePartition(partition: WHV_PARTITION_HANDLE);
        WHvSetPartitionProperty(
            partition: WHV_PARTITION_HANDLE,
            code: WHV_PARTITION_PROPERTY_CODE,
            buffer: *const c_void,
            buffer_size: u32,
        );
        WHvGetPartitionProperty(
            partition: WHV_PARTITION_HANDLE,
            code: WHV_PARTITION_PROPERTY_CODE,
            buffer: *mut c_void,
            buffer_size: u32,
            written_size: *mut u32,
        );
        WHvSuspendPartitionTime(partition: WHV_PARTITION_HANDLE);
        WHvResumePartitionTime(partition: WHV_PARTITION_HANDLE);
        WHvMapGpaRange(
            partition: WHV_PARTITION_HANDLE,
            source: *const c_void,
            guest_address: u64,
            size: u64,
            flags: WHV_MAP_GPA_RANGE_FLAGS,
        );
        WHvUnmapGpaRange(partition: WHV_PARTITION_HANDLE, guest_address: u64, size: u64);
        WHvCreateVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32, flags: u32);
        WHvDeleteVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32);
        WHvRunVirtualProcessor(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            exit_context: *mut c_void,
            exit_context_size: u32,
        );
        WHvCancelRunVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32, flags: u32);
        WHvGetVirtualProcessorInterruptControllerState2(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            state: *mut c_void,
            state_size: u32,
            written_size: *mut u32,
        );
        WHvGetVirtualProcessorRegisters(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            names: *const WHV_REGISTER_NAME,
            count: u32,
            values: *mut WHV_REGISTER_VALUE,
        );
        WHvSetVirtualProcessorRegisters(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            names: *const WHV_REGISTER_NAME,
            count: u32,
            values: *const WHV_REGISTER_VALUE,
        );
        WHvGetVirtualProcessorState(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            kind: WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
            buffer: *mut c_void,
            buffer_size: u32,
            written_size: *mut u32,
        );
        WHvSetVirtualProcessorState(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            kind: WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
            buffer: *const c_void,
            buffer_size: u32,
        );
        WHvRequestInterrupt(
            partition: WHV_PARTITION_HANDLE,
            interrupt: *const WHV_INTERRUPT_CONTROL,
            interrupt_size: u32,
        );
        WHvTranslateGva(
            partition: WHV_PARTITION_HANDLE,
            index: u32,
            gva: u64,
            flags: WHV_TRANSLATE_GVA_FLAGS,
            result: *mut WHV_TRANSLATE_GVA_RESULT,
            gpa: *mut u64,
        );
    }
    "WinHvEmulation.dll" {
        WHvEmulatorCreateEmulator(
            callbacks: *const WHV_EMULATOR_CALLBACKS,
            emulator: *mut *mut c_void,
        );
        WHvEmulatorDestroyEmulator(emulator: *const c_void);
        WHvEmulatorTryIoEmulation(
            emulator: *const c_void,
            context: *const c_void,
            vp_context: *const WHV_VP_EXIT_CONTEXT,
            io_context: *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
            status: *mut WHV_EMULATOR_STATUS,
        );
        WHvEmulatorTryMmioEmulation(
            emulator: *const c_void,
            context: *const c_void,
            vp_context: *const WHV_VP_EXIT_CONTEXT,
            mmio_context: *const WHV_MEMORY_ACCESS_CONTEXT,
            status: *mut WHV_EMULATOR_STATUS,
        );
    }
}

fn load_system_dll(name: PCWSTR, display: &str) -> Result<HMODULE, String> {
    // SAFETY: loading a system DLL by name, restricted to System32.
    unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|e| format!("{display} could not be loaded: {}", e.message()))
}

fn resolve(module: HMODULE, dll: &str, name: &'static str) -> Result<RawProc, String> {
    debug_assert!(name.ends_with('\0'));
    // SAFETY: `name` is NUL-terminated and `module` is a loaded DLL that is never freed.
    unsafe { GetProcAddress(module, PCSTR::from_raw(name.as_ptr())) }.ok_or_else(|| {
        format!(
            "{dll} has no export {}; Windows is too old",
            name.trim_end_matches('\0')
        )
    })
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Api(WinHvPlatform.dll, WinHvEmulation.dll)")
    }
}

/// A failed WHP call's HRESULT, with the system message for it.
#[derive(Debug)]
pub(crate) struct WhpError(pub HRESULT);

impl fmt::Display for WhpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // HRESULTs print as unsigned hex by convention, e.g. 0x80070057.
        write!(
            f,
            "{} (HRESULT {:#010x})",
            self.0.message(),
            self.0.0 as u32
        )
    }
}

impl std::error::Error for WhpError {}

/// Turns a WHP call's HRESULT into an `hv::Result`, naming the call on failure.
pub(crate) fn check(op: &'static str, hr: HRESULT) -> hv::Result<()> {
    if hr.is_ok() {
        Ok(())
    } else {
        Err(hv::Error::Backend {
            op,
            source: Box::new(WhpError(hr)),
        })
    }
}
