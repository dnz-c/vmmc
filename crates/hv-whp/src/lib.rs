//! Windows Hypervisor Platform (WHP) backend for the vmmc engine.
//!
//! WHP is the user-mode API Windows gives third-party VMMs for running guests on the
//! Hyper-V hypervisor. On Windows 11 with VBS/HVCI enabled, Hyper-V owns VT-x, so this is
//! the only way a desktop VMM can use hardware virtualization there.
#![cfg(windows)]

mod api;
mod cpuid;
mod emulator;
mod interrupts;
mod partition;
mod regs;
mod state;
mod vcpu;

use std::ffi::c_void;
use std::sync::Arc;

use windows::Win32::System::Hypervisor::{WHV_CAPABILITY, WHvCapabilityCodeHypervisorPresent};

use crate::api::{Api, check};
pub use crate::interrupts::WhpInterrupts;
pub use crate::partition::WhpPartition;
pub use crate::vcpu::{WhpKicker, WhpVcpu};

const ENABLE_WHP: &str = "Enable the Windows Hypervisor Platform feature: run \
    `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` as \
    administrator, make sure `bcdedit /enum {current}` shows `hypervisorlaunchtype Auto`, \
    then reboot.";

/// The WHP hypervisor on this host.
#[derive(Debug, Clone)]
pub struct Whp {
    api: Arc<Api>,
}

impl Whp {
    /// Loads WHP and checks that the Windows hypervisor is running.
    ///
    /// Returns [`hv::Error::Unavailable`], with instructions for the user, when it isn't.
    pub fn new() -> hv::Result<Self> {
        let api = Api::load().map_err(|e| hv::Error::Unavailable(format!("{e}. {ENABLE_WHP}")))?;

        let mut capability = WHV_CAPABILITY::default();
        let mut written = 0u32;
        // SAFETY: `capability` is a valid, writable WHV_CAPABILITY of the size passed.
        check("WHvGetCapability", unsafe {
            (api.WHvGetCapability)(
                WHvCapabilityCodeHypervisorPresent,
                (&raw mut capability).cast::<c_void>(),
                size_of::<WHV_CAPABILITY>() as u32,
                &mut written,
            )
        })?;
        // SAFETY: WHvCapabilityCodeHypervisorPresent fills the HypervisorPresent member.
        if !unsafe { capability.HypervisorPresent }.as_bool() {
            return Err(hv::Error::Unavailable(format!(
                "The Windows hypervisor is not running. {ENABLE_WHP}"
            )));
        }

        Ok(Self { api: Arc::new(api) })
    }
}

impl hv::Hypervisor for Whp {
    type Partition = WhpPartition;

    fn create_partition(&self, config: &hv::PartitionConfig) -> hv::Result<WhpPartition> {
        WhpPartition::create(self.api.clone(), config)
    }
}
