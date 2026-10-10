//! The host bridge at 00:00.0, identifying as Intel's Q35 memory controller hub.

use super::{ConfigSpace, Identity, PciDevice};

pub const HOST_BRIDGE_SLOT: u8 = 0;

pub struct HostBridge {
    config: ConfigSpace,
}

impl HostBridge {
    pub fn new() -> Self {
        Self {
            config: ConfigSpace::new(Identity {
                vendor_id: 0x8086,
                device_id: 0x29c0,
                revision: 0,
                // Bridge device, host bridge.
                class: 0x06_00_00,
                subsystem_vendor_id: 0x1af4,
                subsystem_id: 0x1100,
            }),
        }
    }
}

impl Default for HostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl PciDevice for HostBridge {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn bar_read(&mut self, _: u8, _: u64, data: &mut [u8]) {
        data.fill(0xff);
    }

    fn bar_write(&mut self, _: u8, _: u64, _: &[u8]) {}
}
