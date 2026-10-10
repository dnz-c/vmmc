use std::ffi::c_void;
use std::fmt;
use std::sync::Arc;

use hv::{LocalApicMode, MemoryAccess, PartitionConfig};
use windows::Win32::System::Hypervisor::{
    WHV_MAP_GPA_RANGE_FLAGS, WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE,
    WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
    WHvX64LocalApicEmulationModeX2Apic, WHvX64LocalApicEmulationModeXApic,
};

use crate::api::{Api, check};
use crate::interrupts::WhpInterrupts;
use crate::vcpu::WhpVcpu;

/// Owns the WHP partition handle. The partition and its vCPUs hold it strongly, so the
/// handle stays valid until the last of them is dropped. Kickers and interrupt handles
/// hold it weakly: they must not keep the guest's memory mapping alive.
pub(crate) struct PartitionInner {
    pub api: Arc<Api>,
    pub handle: WHV_PARTITION_HANDLE,
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

        let mut handle = WHV_PARTITION_HANDLE::default();
        // SAFETY: `handle` is a valid out-pointer.
        check("WHvCreatePartition", unsafe {
            (api.WHvCreatePartition)(&mut handle)
        })?;
        // From here on, dropping `inner` deletes the partition, including on error paths.
        let inner = Arc::new(PartitionInner { api, handle });

        inner.set_property(WHvPartitionPropertyCodeProcessorCount, &config.vcpu_count)?;
        let apic_mode = match config.local_apic {
            LocalApicMode::Disabled => None,
            LocalApicMode::XApic => Some(WHvX64LocalApicEmulationModeXApic),
            LocalApicMode::X2Apic => Some(WHvX64LocalApicEmulationModeX2Apic),
        };
        if let Some(mode) = apic_mode {
            inner.set_property(WHvPartitionPropertyCodeLocalApicEmulationMode, &mode)?;
        }

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
        check("WHvMapGpaRange", unsafe {
            (self.inner.api.WHvMapGpaRange)(
                self.inner.handle,
                host.cast_const().cast::<c_void>(),
                gpa,
                size,
                WHV_MAP_GPA_RANGE_FLAGS(flags),
            )
        })
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
}
