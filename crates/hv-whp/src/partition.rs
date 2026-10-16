use std::ffi::c_void;
use std::fmt;
use std::sync::Arc;

use hv::{LocalApicMode, MemoryAccess, PartitionConfig};
use windows::Win32::System::Hypervisor::{
    WHV_MAP_GPA_RANGE_FLAGS, WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE,
    WHvCapabilityCodeSyntheticProcessorFeaturesBanks, WHvMapGpaRangeFlagExecute,
    WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite, WHvPartitionPropertyCodeCpuidExitList,
    WHvPartitionPropertyCodeExtendedVmExits, WHvPartitionPropertyCodeLocalApicEmulationMode,
    WHvPartitionPropertyCodeProcessorCount, WHvPartitionPropertyCodeReferenceTime,
    WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks, WHvX64LocalApicEmulationModeX2Apic,
    WHvX64LocalApicEmulationModeXApic,
};

use crate::api::{Api, check};
use crate::cpuid;
use crate::interrupts::WhpInterrupts;
use crate::vcpu::WhpVcpu;

/// `WHV_EXTENDED_VM_EXITS` with only `X64CpuidExit` (bit 0) set.
const CPUID_EXIT: u64 = 1;

/// The Hyper-V enlightenments a Windows guest gets, as bits of the first
/// `WHV_SYNTHETIC_PROCESSOR_FEATURES` bank: the hypervisor's CPUID leaves,
/// hypercalls, VP index and runtime, the reference counter and TSC page, frequency
/// MSRs, the guest idle MSR, the synthetic interrupt controller with direct-mode
/// synthetic timers, the APIC-access MSRs and VP assist page, and the TLB-flush and
/// IPI hypercalls. Left out: what needs a VMBus host (event signalling), nested
/// virtualization, and device-interrupt retargeting.
///
/// Windows needs the guest idle MSR whenever it sees the hypervisor: without it,
/// Windows 10 22H2 stops with KMODE_EXCEPTION_NOT_HANDLED during boot.
const ENLIGHTENMENTS: u64 = (1 << 0) // HypervisorPresent
    | (1 << 1) // Hv1
    | (1 << 2) // AccessVpRunTimeReg
    | (1 << 3) // AccessPartitionReferenceCounter
    | (1 << 4) // AccessSynicRegs
    | (1 << 5) // AccessSyntheticTimerRegs
    | (1 << 6) // AccessIntrCtrlRegs
    | (1 << 7) // AccessHypercallRegs
    | (1 << 8) // AccessVpIndex
    | (1 << 9) // AccessPartitionReferenceTsc
    | (1 << 10) // AccessGuestIdleReg
    | (1 << 11) // AccessFrequencyRegs
    | (1 << 15) // EnableExtendedGvaRangesForFlushVirtualAddressList
    | (1 << 18) // FastHypercallOutput
    | (1 << 22) // DirectSyntheticTimers
    | (1 << 24) // ExtendedProcessorMasks
    | (1 << 25) // TbFlushHypercalls
    | (1 << 26) // SyntheticClusterIpi
    | (1 << 27); // NotifyLongSpinWait

/// `WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS` with one bank.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FeatureBanks {
    count: u32,
    reserved: u32,
    bank0: u64,
}

/// What `WHvMapGpaRange` returns when another partition in this process already has
/// guest memory mapped: only one may at a time. (The status's own text, about a VID
/// partition name that already exists, is misleading.)
const ANOTHER_PARTITION_HAS_MEMORY: windows::core::HRESULT =
    windows::core::HRESULT(0xc037_0008_u32 as i32);

/// Owns the WHP partition handle. The partition and its vCPUs hold it strongly, so the
/// handle stays valid until the last of them is dropped. Kickers and interrupt handles
/// hold it weakly: they must not keep the guest's memory mapping alive.
pub(crate) struct PartitionInner {
    pub api: Arc<Api>,
    pub handle: WHV_PARTITION_HANDLE,
    /// How many vCPUs the partition has.
    pub vcpus: u32,
    /// Whether the guest has the Hyper-V enlightenments, whose state a vCPU's includes.
    pub enlightenments: bool,
    /// The registers this host's vCPUs have, of those a saved state is made of: found
    /// out at the first save.
    pub saved_registers: std::sync::OnceLock<crate::state::Registers>,
}

impl PartitionInner {
    fn set_property<T: Copy>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        value: &T,
    ) -> hv::Result<()> {
        // SAFETY: `value` points to a `T` of the size passed, which is the buffer type WHP
        // expects for `code` at every call site.
        check("WHvSetPartitionProperty", unsafe {
            (self.api.WHvSetPartitionProperty)(
                self.handle,
                code,
                (value as *const T).cast::<c_void>(),
                size_of::<T>() as u32,
            )
        })
    }
}

impl PartitionInner {
    /// The synthetic processor features (first bank) this host's WHP can expose.
    fn supported_features(&self) -> hv::Result<u64> {
        let mut banks = FeatureBanks::default();
        let mut written = 0u32;
        // SAFETY: `banks` is a writable buffer of the size passed.
        check("WHvGetCapability", unsafe {
            (self.api.WHvGetCapability)(
                WHvCapabilityCodeSyntheticProcessorFeaturesBanks,
                (&raw mut banks).cast::<c_void>(),
                size_of::<FeatureBanks>() as u32,
                &mut written,
            )
        })?;
        Ok(if banks.count >= 1 { banks.bank0 } else { 0 })
    }
}

impl Drop for PartitionInner {
    fn drop(&mut self) {
        // SAFETY: the handle came from WHvCreatePartition and nothing else uses it now.
        // Deleting also unmaps all guest memory and deletes any remaining vCPUs.
        let _ = unsafe { (self.api.WHvDeletePartition)(self.handle) };
    }
}

impl fmt::Debug for PartitionInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartitionInner")
            .field("handle", &self.handle.0)
            .finish()
    }
}

/// A WHP partition: one VM's guest-physical address space and vCPUs.
#[derive(Debug)]
pub struct WhpPartition {
    inner: Arc<PartitionInner>,
}

impl WhpPartition {
    pub(crate) fn create(api: Arc<Api>, config: &PartitionConfig) -> hv::Result<Self> {
        if config.vcpu_count == 0 {
            return Err(hv::Error::InvalidArgument(
                "a VM needs at least one vCPU".into(),
            ));
        }
        // WHvSetupPartition would only say "invalid parameter".
        if config.enlightenments && config.local_apic == LocalApicMode::Disabled {
            return Err(hv::Error::InvalidArgument(
                "Hyper-V enlightenments need a local APIC".into(),
            ));
        }

        let mut handle = WHV_PARTITION_HANDLE::default();
        // SAFETY: `handle` is a valid out-pointer.
        check("WHvCreatePartition", unsafe {
            (api.WHvCreatePartition)(&mut handle)
        })?;
        // From here on, dropping `inner` deletes the partition, including on error paths.
        let inner = Arc::new(PartitionInner {
            api,
            handle,
            vcpus: config.vcpu_count,
            enlightenments: config.enlightenments,
            saved_registers: std::sync::OnceLock::new(),
        });

        inner.set_property(WHvPartitionPropertyCodeProcessorCount, &config.vcpu_count)?;
        let apic_mode = match config.local_apic {
            LocalApicMode::Disabled => None,
            LocalApicMode::XApic => Some(WHvX64LocalApicEmulationModeXApic),
            LocalApicMode::X2Apic => Some(WHvX64LocalApicEmulationModeX2Apic),
        };
        if let Some(mode) = apic_mode {
            inner.set_property(WHvPartitionPropertyCodeLocalApicEmulationMode, &mode)?;
        }

        if config.enlightenments {
            // Windows before 10 2004 has no synthetic processor features: the
            // capability query fails, and so does the VM.
            let banks = FeatureBanks {
                count: 1,
                reserved: 0,
                bank0: ENLIGHTENMENTS & inner.supported_features()?,
            };
            inner.set_property(
                WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
                &banks,
            )?;
        }

        // The VMM answers a few CPUID leaves itself (see `cpuid`).
        inner.set_property(WHvPartitionPropertyCodeExtendedVmExits, &CPUID_EXIT)?;
        inner.set_property(WHvPartitionPropertyCodeCpuidExitList, &cpuid::LEAVES)?;

        // SAFETY: the handle is valid and configured.
        check("WHvSetupPartition", unsafe {
            (inner.api.WHvSetupPartition)(inner.handle)
        })?;
        Ok(Self { inner })
    }
}

impl hv::Partition for WhpPartition {
    type Vcpu = WhpVcpu;
    type Interrupts = WhpInterrupts;

    unsafe fn map_memory(
        &self,
        gpa: u64,
        host: *mut u8,
        size: u64,
        access: MemoryAccess,
    ) -> hv::Result<()> {
        let mut flags = 0;
        for (wanted, flag) in [
            (MemoryAccess::READ, WHvMapGpaRangeFlagRead),
            (MemoryAccess::WRITE, WHvMapGpaRangeFlagWrite),
            (MemoryAccess::EXECUTE, WHvMapGpaRangeFlagExecute),
        ] {
            if access.contains(wanted) {
                flags |= flag.0;
            }
        }
        // SAFETY: the caller guarantees `host` points to `size` bytes that outlive the
        // mapping (see `hv::Partition::map_memory`).
        let hr = unsafe {
            (self.inner.api.WHvMapGpaRange)(
                self.inner.handle,
                host.cast_const().cast::<c_void>(),
                gpa,
                size,
                WHV_MAP_GPA_RANGE_FLAGS(flags),
            )
        };
        if hr == ANOTHER_PARTITION_HAS_MEMORY {
            return Err(hv::Error::Backend {
                op: "WHvMapGpaRange",
                source: "another VM in this process already has guest memory; Windows allows \
                         one per process, so run each VM in its own process"
                    .into(),
            });
        }
        check("WHvMapGpaRange", hr)
    }

    fn unmap_memory(&self, gpa: u64, size: u64) -> hv::Result<()> {
        // SAFETY: plain call on a valid partition handle.
        check("WHvUnmapGpaRange", unsafe {
            (self.inner.api.WHvUnmapGpaRange)(self.inner.handle, gpa, size)
        })
    }

    fn create_vcpu(&self, index: u32) -> hv::Result<WhpVcpu> {
        WhpVcpu::create(self.inner.clone(), index)
    }

    fn interrupts(&self) -> WhpInterrupts {
        WhpInterrupts::new(Arc::downgrade(&self.inner))
    }

    fn suspend_time(&self) -> hv::Result<()> {
        // SAFETY: plain call on a valid partition handle.
        check("WHvSuspendPartitionTime", unsafe {
            (self.inner.api.WHvSuspendPartitionTime)(self.inner.handle)
        })
    }

    fn resume_time(&self) -> hv::Result<()> {
        // SAFETY: plain call on a valid partition handle.
        check("WHvResumePartitionTime", unsafe {
            (self.inner.api.WHvResumePartitionTime)(self.inner.handle)
        })
    }

    fn save_state(&self) -> hv::Result<hv::PartitionState> {
        let mut reference_time = 0u64;
        let mut written = 0u32;
        // SAFETY: `reference_time` is a writable u64, the property's type.
        check("WHvGetPartitionProperty", unsafe {
            (self.inner.api.WHvGetPartitionProperty)(
                self.inner.handle,
                WHvPartitionPropertyCodeReferenceTime,
                (&raw mut reference_time).cast::<c_void>(),
                size_of::<u64>() as u32,
                &mut written,
            )
        })?;
        Ok(hv::PartitionState { reference_time })
    }

    fn restore_state(&self, state: &hv::PartitionState) -> hv::Result<()> {
        self.inner
            .set_property(WHvPartitionPropertyCodeReferenceTime, &state.reference_time)
    }
}
