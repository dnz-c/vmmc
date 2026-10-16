//! Device models for the vmmc engine: the PC chipset, firmware interfaces, PCI and
//! virtio.
//!
//! Devices sit on `vm-core` buses and never touch the hypervisor directly. They signal
//! interrupts as x86 MSI messages through [`irq::MsiSink`], and ask for power-offs and
//! resets through a [`PowerHandler`].

pub mod chipset;
pub mod clock;
pub mod display;
pub mod firmware;
pub mod irq;
pub mod legacy;
pub mod nic;
pub mod pci;
pub mod snapshot;
pub mod storage;
pub mod timer;
pub mod tpm;
pub mod usb;
pub mod virtio;

use std::sync::Arc;

/// A guest request that ends or restarts the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    PowerOff,
    Reset,
}

/// What a device calls when the guest powers off or resets.
pub type PowerHandler = Arc<dyn Fn(PowerEvent) + Send + Sync>;
