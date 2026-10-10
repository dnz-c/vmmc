//! virtio devices and their PCI transport (virtio 1.2, modern devices only).

pub mod blk;
pub mod pci;

use std::sync::Arc;

use virtio_queue::Queue;
use vm_memory::GuestMemoryMmap;

pub const VIRTIO_F_VERSION_1: u64 = 1 << virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
pub const VIRTIO_F_RING_INDIRECT_DESC: u64 =
    1 << virtio_bindings::virtio_ring::VIRTIO_RING_F_INDIRECT_DESC;
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;

/// Lets a running device interrupt the driver.
pub trait QueueInterrupt: Send + Sync {
    /// The device used buffers on `queue`.
    fn signal_queue(&self, queue: u16);
    /// The device configuration changed.
    fn signal_config(&self);
}

/// A virtio device, independent of its transport.
pub trait VirtioDevice: Send {
    /// The virtio device ID (2 = block).
    fn device_type(&self) -> u16;
    /// The features the device offers.
    fn features(&self) -> u64;
    /// The maximum size of each queue; the length is the number of queues.
    fn queue_max_sizes(&self) -> &[u16];
    /// Reads the device-specific configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// Writes the device-specific configuration space.
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        tracing::debug!(offset, ?data, "write to read-only virtio config ignored");
    }
    /// Starts the device after the driver set DRIVER_OK. `queues` has one entry per
    /// queue, set up as the driver programmed it.
    fn activate(
        &mut self,
        memory: GuestMemoryMmap,
        features: u64,
        queues: Vec<Queue>,
        interrupt: Arc<dyn QueueInterrupt>,
    ) -> Result<(), String>;
    /// The driver notified `queue` that it has new buffers.
    fn queue_notify(&mut self, queue: u16);
    /// Stops the device and forgets its queues (the driver reset it).
    fn reset(&mut self);
}
