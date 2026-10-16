//! virtio devices and their PCI transport (virtio 1.2, modern devices only).

pub mod blk;
pub mod net;
pub mod pci;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use virtio_queue::{Queue, QueueState};
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

    /// Where the running device is in each of its queues.
    fn queue_states(&self) -> Vec<QueueState>;

    /// Finishes the requests it has in flight, and takes nothing more from the host
    /// (frames from the network) until [`VirtioDevice::resume`].
    fn quiesce(&mut self) {}

    fn resume(&mut self) {}
}

/// A queue's [`QueueState`], saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQueue {
    max_size: u16,
    next_avail: u16,
    next_used: u16,
    event_idx_enabled: bool,
    size: u16,
    ready: bool,
    desc_table: u64,
    avail_ring: u64,
    used_ring: u64,
}

impl From<QueueState> for SavedQueue {
    fn from(state: QueueState) -> Self {
        Self {
            max_size: state.max_size,
            next_avail: state.next_avail,
            next_used: state.next_used,
            event_idx_enabled: state.event_idx_enabled,
            size: state.size,
            ready: state.ready,
            desc_table: state.desc_table,
            avail_ring: state.avail_ring,
            used_ring: state.used_ring,
        }
    }
}

impl SavedQueue {
    /// The queue, as it was.
    pub fn queue(&self) -> Result<Queue, String> {
        Queue::try_from(QueueState {
            max_size: self.max_size,
            next_avail: self.next_avail,
            next_used: self.next_used,
            event_idx_enabled: self.event_idx_enabled,
            size: self.size,
            ready: self.ready,
            desc_table: self.desc_table,
            avail_ring: self.avail_ring,
            used_ring: self.used_ring,
        })
        .map_err(|e| format!("a saved virtqueue doesn't fit: {e}"))
    }
}
