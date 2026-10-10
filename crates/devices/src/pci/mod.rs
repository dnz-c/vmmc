//! PCI: configuration space, the root complex on bus 0, and MSI-X.

pub mod config;
pub mod host_bridge;
pub mod msix;
pub mod root;

use std::sync::{Arc, Mutex};

pub use config::{BarKind, BarMapping, ConfigSpace, Identity};

/// A PCI function as the root complex sees it.
pub trait PciDevice: Send {
    fn config(&self) -> &ConfigSpace;
    fn config_mut(&mut self) -> &mut ConfigSpace;

    /// A guest configuration read. The default serves it from config space.
    fn config_read(&mut self, offset: u16, data: &mut [u8]) {
        self.config().read(offset, data);
    }

    /// A guest configuration write. The default applies it through the write masks.
    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config_mut().write(offset, data);
    }

    /// A guest read of one of the device's memory BARs.
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]);
    /// A guest write to one of the device's memory BARs.
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]);
}

pub type SharedPciDevice = Arc<Mutex<dyn PciDevice>>;
