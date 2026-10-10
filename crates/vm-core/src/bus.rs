use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::{Error, Result};

/// A device that claims a range of I/O ports or MMIO addresses.
pub trait BusDevice: Send {
    /// Handles a guest read. `offset` is relative to the start of the device's range.
    fn read(&mut self, offset: u64, data: &mut [u8]);
    /// Handles a guest write. `offset` is relative to the start of the device's range.
    fn write(&mut self, offset: u64, data: &[u8]);
}

/// A device as the bus holds it.
pub type SharedDevice = Arc<Mutex<dyn BusDevice>>;

struct Entry {
    len: u64,
    device: SharedDevice,
}

/// Routes guest accesses to the device whose range contains the address.
///
/// The bus can change while vCPUs run, for example when the guest moves a PCI BAR. It
/// never holds its own lock while calling into a device, so a device may change the bus
/// from inside an access.
#[derive(Default)]
pub struct Bus {
    /// Keyed by range start. Ranges never overlap.
    entries: RwLock<BTreeMap<u64, Entry>>,
}

impl Bus {
    /// Puts `device` at `base..base + len`. The range must not overlap another device's.
    pub fn insert(&self, base: u64, len: u64, device: SharedDevice) -> Result<()> {
        let end = base
            .checked_add(len)
            .filter(|_| len > 0)
            .ok_or(Error::BusRange { base, len })?;
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        // Ranges are disjoint and sorted, so the last one starting before `end` is the
        // only one that can reach past `base`.
        if let Some((&other, entry)) = entries.range(..end).next_back()
            && other + entry.len > base
        {
            return Err(Error::BusRange { base, len });
        }
        entries.insert(base, Entry { len, device });
        Ok(())
    }

    /// Removes the device whose range starts at `base`, if there is one.
    pub fn remove(&self, base: u64) -> Option<SharedDevice> {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&base)
            .map(|entry| entry.device)
    }

    /// Forwards a read to the device at `addr`. Returns `false` if no device claims it.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        self.dispatch(addr, |device, offset| device.read(offset, data))
    }

    /// Forwards a write to the device at `addr`. Returns `false` if no device claims it.
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.dispatch(addr, |device, offset| device.write(offset, data))
    }

    fn dispatch(&self, addr: u64, access: impl FnOnce(&mut dyn BusDevice, u64)) -> bool {
        let Some((device, offset)) = self.find(addr) else {
            return false;
        };
        let mut device = device
            .lock()
            .expect("a device panicked during a guest access");
        access(&mut *device, offset);
        true
    }

    /// Looks up the device at `addr`, releasing the bus lock before returning.
    fn find(&self, addr: u64) -> Option<(SharedDevice, u64)> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let (&base, entry) = entries.range(..=addr).next_back()?;
        let offset = addr - base;
        (offset < entry.len).then(|| (entry.device.clone(), offset))
    }
}

impl fmt::Debug for Bus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        f.debug_list()
            .entries(
                entries
                    .iter()
                    .map(|(base, e)| format!("{base:#x}..{:#x}", base + e.len)),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records accesses and answers reads with its offset.
    #[derive(Default)]
    struct Recorder {
        writes: Vec<(u64, Vec<u8>)>,
    }

    impl BusDevice for Recorder {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }

        fn write(&mut self, offset: u64, data: &[u8]) {
            self.writes.push((offset, data.to_vec()));
        }
    }

    #[test]
    fn routes_accesses_with_device_relative_offsets() {
        let bus = Bus::default();
        let a = Arc::new(Mutex::new(Recorder::default()));
        let b = Arc::new(Mutex::new(Recorder::default()));
        bus.insert(0x60, 1, a.clone()).unwrap();
        bus.insert(0x3f8, 8, b.clone()).unwrap();

        assert!(bus.write(0x60, &[0xaa]));
        assert!(bus.write(0x3fd, &[1, 2]));
        let mut data = [0u8; 1];
        assert!(bus.read(0x3ff, &mut data));
        assert_eq!(data, [7]);

        assert_eq!(a.lock().unwrap().writes, [(0, vec![0xaa])]);
        assert_eq!(b.lock().unwrap().writes, [(5, vec![1, 2])]);
    }

    #[test]
    fn unclaimed_addresses_are_reported() {
        let bus = Bus::default();
        bus.insert(0x3f8, 8, Arc::new(Mutex::new(Recorder::default())))
            .unwrap();
        let mut data = [0u8; 1];
        assert!(!bus.read(0x3f7, &mut data));
        assert!(!bus.read(0x400, &mut data));
        assert!(!bus.write(0, &data));
    }

    #[test]
    fn rejects_overlapping_empty_and_wrapping_ranges() {
        let bus = Bus::default();
        let dev = || Arc::new(Mutex::new(Recorder::default()));
        bus.insert(0x10, 0x10, dev()).unwrap();
        bus.insert(0x30, 0x10, dev()).unwrap();

        assert!(
            bus.insert(0x18, 0x4, dev()).is_err(),
            "inside an existing range"
        );
        assert!(
            bus.insert(0x08, 0x10, dev()).is_err(),
            "overlaps the start of one"
        );
        assert!(bus.insert(0x1f, 0x20, dev()).is_err(), "spans two ranges");
        assert!(bus.insert(0x50, 0, dev()).is_err(), "empty");
        assert!(bus.insert(u64::MAX, 2, dev()).is_err(), "wraps around");

        bus.insert(0x20, 0x10, dev())
            .expect("exactly fills the gap");
    }

    /// Moves itself to `to` on its first write, the way a PCI device moves a BAR.
    struct Mover {
        bus: Arc<Bus>,
        me: Option<SharedDevice>,
        from: u64,
        to: u64,
    }

    impl BusDevice for Mover {
        fn read(&mut self, _: u64, data: &mut [u8]) {
            data.fill(0x42);
        }

        fn write(&mut self, _: u64, _: &[u8]) {
            if let Some(me) = self.me.take() {
                self.bus.remove(self.from).expect("mapped at `from`");
                self.bus.insert(self.to, 0x10, me).unwrap();
            }
        }
    }

    #[test]
    fn a_device_can_remap_itself_during_an_access() {
        let bus = Arc::new(Bus::default());
        let mover = Arc::new(Mutex::new(Mover {
            bus: bus.clone(),
            me: None,
            from: 0x1000,
            to: 0x2000,
        }));
        mover.lock().unwrap().me = Some(mover.clone());
        bus.insert(0x1000, 0x10, mover.clone()).unwrap();

        assert!(bus.write(0x1000, &[1]));
        let mut data = [0u8; 1];
        assert!(!bus.read(0x1000, &mut data), "gone from the old address");
        assert!(bus.read(0x2004, &mut data), "present at the new one");
        assert_eq!(data, [0x42]);
        // Break the Arc cycle so the test doesn't leak.
        bus.remove(0x2000);
    }
}
