//! Host memory that devices map into the guest while it runs, outside the VM's main RAM:
//! firmware flash, framebuffers.
//!
//! A device owns a [`RamRegion`] and maps it through a [`RamMapper`]. The VM keeps every
//! mapped region alive until it is unmapped, and the partition goes away before the
//! regions do. So a region can never be freed while the guest can still reach it.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use hv::{MemoryAccess, Partition};
use vm_memory::{MmapRegion, VolatileMemory};

use crate::{Error, Result};

/// The page size mappings are made in.
pub const PAGE_SIZE: u64 = 0x1000;

/// Page-aligned host memory a device can map into the guest.
pub struct RamRegion {
    mmap: MmapRegion<()>,
}

impl RamRegion {
    /// `len` bytes of zeroed memory. `len` must be a whole number of pages.
    pub fn new(len: usize) -> io::Result<Arc<Self>> {
        if len == 0 || !(len as u64).is_multiple_of(PAGE_SIZE) {
            return Err(io::Error::other(format!(
                "RAM region size {len:#x} isn't whole pages"
            )));
        }
        let mmap = MmapRegion::new(len).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Arc::new(Self { mmap }))
    }

    pub fn len(&self) -> usize {
        self.mmap.size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies out `data.len()` bytes at `offset`. Returns `false` if that's out of range.
    pub fn read(&self, offset: usize, data: &mut [u8]) -> bool {
        match self.mmap.get_slice(offset, data.len()) {
            Ok(slice) => {
                slice.copy_to(data);
                true
            }
            Err(_) => false,
        }
    }

    /// Copies `data` in at `offset`. Returns `false` if that's out of range.
    pub fn write(&self, offset: usize, data: &[u8]) -> bool {
        match self.mmap.get_slice(offset, data.len()) {
            Ok(slice) => {
                slice.copy_from(data);
                true
            }
            Err(_) => false,
        }
    }

    fn host_ptr(&self) -> *mut u8 {
        self.mmap.as_ptr()
    }
}

impl std::fmt::Debug for RamRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamRegion")
            .field("len", &self.len())
            .finish()
    }
}

/// What a [`RamMapper`] talks to. Implemented by the VM.
pub(crate) trait MapRegions: Send + Sync {
    fn map(&self, gpa: u64, region: Arc<RamRegion>, access: MemoryAccess) -> Result<()>;
    fn unmap(&self, gpa: u64) -> Result<()>;
}

/// The VM's partition, plus the runtime regions mapped into it.
pub(crate) struct Shared<P: Partition> {
    // Field order matters: the partition is dropped first, which removes every mapping,
    // and only then are the regions freed.
    pub partition: P,
    regions: Mutex<BTreeMap<u64, Arc<RamRegion>>>,
}

impl<P: Partition> Shared<P> {
    pub fn new(partition: P) -> Self {
        Self {
            partition,
            regions: Mutex::default(),
        }
    }
}

impl<P: Partition> MapRegions for Shared<P> {
    fn map(&self, gpa: u64, region: Arc<RamRegion>, access: MemoryAccess) -> Result<()> {
        let len = region.len() as u64;
        let end = gpa
            .checked_add(len)
            .ok_or(Error::BusRange { base: gpa, len })?;
        if !gpa.is_multiple_of(PAGE_SIZE) {
            return Err(Error::BusRange { base: gpa, len });
        }
        let mut regions = self.regions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((&other, existing)) = regions.range(..end).next_back()
            && other + existing.len() as u64 > gpa
        {
            return Err(Error::BusRange { base: gpa, len });
        }
        // SAFETY: the region stays in `regions` (and so allocated) until `unmap` removes
        // its mapping first, or until the partition, dropped before `regions`, is gone.
        unsafe {
            self.partition
                .map_memory(gpa, region.host_ptr(), len, access)?;
        }
        regions.insert(gpa, region);
        Ok(())
    }

    fn unmap(&self, gpa: u64) -> Result<()> {
        let mut regions = self.regions.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(region) = regions.get(&gpa) else {
            return Ok(());
        };
        self.partition.unmap_memory(gpa, region.len() as u64)?;
        // Only now that the guest can't reach it may the region be released.
        regions.remove(&gpa);
        Ok(())
    }
}

/// Maps device-owned host memory into the guest at runtime. Doesn't keep the VM alive.
#[derive(Clone)]
pub struct RamMapper(Weak<dyn MapRegions>);

impl RamMapper {
    pub(crate) fn new(target: Weak<dyn MapRegions>) -> Self {
        Self(target)
    }

    /// A mapper for a VM that no longer exists (or never did). Useful in tests of
    /// devices that don't need real mappings.
    pub fn detached() -> Self {
        Self(Weak::<Detached>::new())
    }

    /// Maps `region` at the page-aligned `gpa`. The range must not overlap another
    /// runtime mapping.
    pub fn map(&self, gpa: u64, region: Arc<RamRegion>, access: MemoryAccess) -> Result<()> {
        match self.0.upgrade() {
            Some(target) => target.map(gpa, region, access),
            None => Err(Error::VmGone),
        }
    }

    /// Unmaps the region mapped at exactly `gpa`, if any.
    pub fn unmap(&self, gpa: u64) -> Result<()> {
        match self.0.upgrade() {
            Some(target) => target.unmap(gpa),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for RamMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RamMapper")
    }
}

/// Stands in for a VM in `RamMapper::detached`; never instantiated.
struct Detached;

impl MapRegions for Detached {
    fn map(&self, _: u64, _: Arc<RamRegion>, _: MemoryAccess) -> Result<()> {
        Err(Error::VmGone)
    }

    fn unmap(&self, _: u64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_whole_pages_and_copy_in_and_out() {
        assert!(RamRegion::new(0x1800).is_err());
        let region = RamRegion::new(0x2000).unwrap();
        assert!(region.write(0x1ffc, b"vmmc"));
        let mut back = [0u8; 4];
        assert!(region.read(0x1ffc, &mut back));
        assert_eq!(&back, b"vmmc");
        assert!(!region.read(0x1ffe, &mut back), "out of range");
    }

    #[test]
    fn a_detached_mapper_refuses_to_map() {
        let mapper = RamMapper::detached();
        let region = RamRegion::new(0x1000).unwrap();
        assert!(matches!(
            mapper.map(0x1000, region, MemoryAccess::READ),
            Err(Error::VmGone)
        ));
        assert!(mapper.unmap(0x1000).is_ok());
    }
}
