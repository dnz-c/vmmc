//! virtio-net: an Ethernet NIC with one receive and one transmit queue, connected to a
//! [`net::NetBackend`].
//!
//! - **Transmit:** runs on the vCPU that notifies the queue; frames go straight to the
//!   backend.
//! - **Receive:** runs on whatever thread the backend delivers from. Frames go into the
//!   driver's receive buffers; while it has none, a few wait and the rest are dropped,
//!   as on a real NIC.
//!
//! No offloads are offered: the guest computes its own checksums and segments.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use net::NetBackend;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{
    QueueInterrupt, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_RING_INDIRECT_DESC, VIRTIO_F_VERSION_1,
    VirtioDevice,
};

const RX: u16 = 0;
const TX: u16 = 1;
const QUEUE_SIZE: u16 = 256;
/// virtio_net_hdr for VERSION_1 devices (with num_buffers).
const HEADER_LEN: usize = 12;
const MAX_FRAME: usize = 65_550;
/// Received frames kept while the driver has no buffers posted.
const RX_BACKLOG: usize = 256;

const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
const S_LINK_UP: u16 = 1;

/// The device's queues once the driver is running.
struct Active {
    memory: GuestMemoryMmap,
    rx: Queue,
    tx: Queue,
    interrupt: Arc<dyn QueueInterrupt>,
}

#[derive(Default)]
struct Shared {
    active: Option<Active>,
    /// Frames for the guest that found no buffer yet.
    backlog: VecDeque<Vec<u8>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct VirtioNet {
    mac: [u8; 6],
    shared: Arc<Mutex<Shared>>,
    backend: Option<Arc<dyn NetBackend>>,
}

/// Hands frames from the backend to a [`VirtioNet`]. Holds the device weakly-in-spirit:
/// once the device is gone, frames are dropped.
#[derive(Clone)]
pub struct Receiver(Arc<Mutex<Shared>>);

impl Receiver {
    pub fn deliver(&self, frame: &[u8]) {
        let mut shared = lock(&self.0);
        if shared.backlog.len() < RX_BACKLOG {
            shared.backlog.push_back(frame.to_vec());
        }
        flush_rx(&mut shared);
    }
}

impl VirtioNet {
    /// A NIC with address `mac`. Connect it with [`VirtioNet::set_backend`]; frames
    /// for the guest go to [`VirtioNet::receiver`].
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            shared: Arc::default(),
            backend: None,
        }
    }

    pub fn receiver(&self) -> Receiver {
        Receiver(self.shared.clone())
    }

    pub fn set_backend(&mut self, backend: Arc<dyn NetBackend>) {
        self.backend = Some(backend);
    }

    fn config_bytes(&self) -> [u8; 10] {
        let mut config = [0u8; 10];
        config[..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&S_LINK_UP.to_le_bytes());
        config[8..10].copy_from_slice(&1u16.to_le_bytes()); // one queue pair
        config
    }

    fn transmit(&self) {
        let mut shared = lock(&self.shared);
        let Some(active) = shared.active.as_mut() else {
            return;
        };
        let mut sent = false;
        loop {
            if active.tx.disable_notification(&active.memory).is_err() {
                return;
            }
            while let Some(chain) = active.tx.pop_descriptor_chain(&active.memory) {
                let head = chain.head_index();
                if let Ok(mut reader) = chain.reader(&active.memory) {
                    let len = reader.available_bytes();
                    if (HEADER_LEN..=HEADER_LEN + MAX_FRAME).contains(&len) {
                        let mut packet = vec![0u8; len];
                        if reader.read_exact(&mut packet).is_ok()
                            && let Some(backend) = &self.backend
                        {
                            backend.send(&packet[HEADER_LEN..]);
                        }
                    }
                }
                if active.tx.add_used(&active.memory, head, 0).is_err() {
                    return;
                }
                sent = true;
            }
            match active.tx.enable_notification(&active.memory) {
                Ok(true) => continue,
                _ => break,
            }
        }
        if sent && active.tx.needs_notification(&active.memory).unwrap_or(true) {
            active.interrupt.signal_queue(TX);
        }
    }
}

/// Moves waiting frames into the driver's receive buffers.
fn flush_rx(shared: &mut Shared) {
    let Shared { active, backlog } = shared;
    let Some(active) = active.as_mut() else {
        backlog.clear();
        return;
    };
    let mut delivered = false;
    while let Some(frame) = backlog.front() {
        let Some(chain) = active.rx.pop_descriptor_chain(&active.memory) else {
            // Out of buffers; ask to hear when the driver adds some.
            let _ = active.rx.enable_notification(&active.memory);
            break;
        };
        let head = chain.head_index();
        let mut written = 0;
        if let Ok(mut writer) = chain.writer(&active.memory)
            && writer.available_bytes() >= HEADER_LEN + frame.len()
        {
            let mut header = [0u8; HEADER_LEN];
            header[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers
            if writer.write_all(&header).is_ok() && writer.write_all(frame).is_ok() {
                written = (HEADER_LEN + frame.len()) as u32;
            }
        }
        // A buffer too small for the frame is returned empty; the frame is dropped.
        backlog.pop_front();
        if active.rx.add_used(&active.memory, head, written).is_err() {
            break;
        }
        delivered = true;
    }
    if delivered && active.rx.needs_notification(&active.memory).unwrap_or(true) {
        active.interrupt.signal_queue(RX);
    }
}

impl VirtioDevice for VirtioNet {
    fn device_type(&self) -> u16 {
        virtio_bindings::virtio_ids::VIRTIO_ID_NET as u16
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
            | VIRTIO_F_RING_INDIRECT_DESC
            | VIRTIO_F_RING_EVENT_IDX
            | F_MAC
            | F_STATUS
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.config_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn activate(
        &mut self,
        memory: GuestMemoryMmap,
        _features: u64,
        queues: Vec<Queue>,
        interrupt: Arc<dyn QueueInterrupt>,
    ) -> Result<(), String> {
        let mut queues = queues.into_iter();
        let (Some(rx), Some(tx)) = (queues.next(), queues.next()) else {
            return Err("virtio-net needs a receive and a transmit queue".into());
        };
        let mut shared = lock(&self.shared);
        shared.active = Some(Active {
            memory,
            rx,
            tx,
            interrupt,
        });
        flush_rx(&mut shared);
        Ok(())
    }

    fn queue_notify(&mut self, queue: u16) {
        match queue {
            RX => flush_rx(&mut lock(&self.shared)),
            TX => self.transmit(),
            _ => {}
        }
    }

    fn reset(&mut self) {
        let mut shared = lock(&self.shared);
        shared.active = None;
        shared.backlog.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Vec<u8>>>);

    impl NetBackend for Recorder {
        fn send(&self, frame: &[u8]) {
            self.0.lock().unwrap().push(frame.to_vec());
        }
    }

    #[derive(Default)]
    struct Signals(AtomicU32);

    impl QueueInterrupt for Signals {
        fn signal_queue(&self, queue: u16) {
            self.0.fetch_or(1 << queue, Ordering::Relaxed);
        }
        fn signal_config(&self) {}
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap()
    }

    #[test]
    fn transmits_frames_without_the_header_and_receives_into_buffers() {
        let memory = memory();
        let rx_queue = MockSplitQueue::create(&memory, GuestAddress(0x1_0000), 16);
        let tx_queue = MockSplitQueue::create(&memory, GuestAddress(0x2_0000), 16);
        // One transmit chain: header plus a 60-byte frame.
        let mut packet = vec![0u8; HEADER_LEN];
        packet.extend((0..60).map(|i| i as u8));
        memory
            .write_slice(&packet, GuestAddress(0x10_0000))
            .unwrap();
        tx_queue
            .add_desc_chains(
                &[RawDescriptor::from(Descriptor::new(
                    0x10_0000,
                    packet.len() as u32,
                    0,
                    0,
                ))],
                0,
            )
            .unwrap();
        // Two receive buffers.
        rx_queue
            .add_desc_chains(
                &[
                    RawDescriptor::from(Descriptor::new(
                        0x20_0000,
                        2048,
                        VRING_DESC_F_WRITE as u16,
                        0,
                    )),
                    RawDescriptor::from(Descriptor::new(
                        0x21_0000,
                        2048,
                        VRING_DESC_F_WRITE as u16,
                        0,
                    )),
                ],
                0,
            )
            .unwrap();

        let recorder = Arc::new(Recorder::default());
        let signals = Arc::new(Signals::default());
        let mut nic = VirtioNet::new([0x52, 0x54, 0, 1, 2, 3]);
        nic.set_backend(recorder.clone());
        let receiver = nic.receiver();
        nic.activate(
            memory.clone(),
            VIRTIO_F_VERSION_1,
            vec![
                rx_queue.create_queue().unwrap(),
                tx_queue.create_queue().unwrap(),
            ],
            signals.clone(),
        )
        .unwrap();

        nic.queue_notify(TX);
        assert_eq!(*recorder.0.lock().unwrap(), [packet[HEADER_LEN..].to_vec()]);
        assert_ne!(signals.0.load(Ordering::Relaxed) & (1 << TX), 0);

        receiver.deliver(b"incoming frame");
        let mut buffer = vec![0u8; HEADER_LEN + 14];
        memory
            .read_slice(&mut buffer, GuestAddress(0x20_0000))
            .unwrap();
        assert_eq!(&buffer[10..12], &[1, 0], "num_buffers");
        assert_eq!(&buffer[HEADER_LEN..], b"incoming frame");
        assert_ne!(signals.0.load(Ordering::Relaxed) & (1 << RX), 0);

        let mut config = [0u8; 8];
        nic.read_config(0, &mut config);
        assert_eq!(&config[..6], &[0x52, 0x54, 0, 1, 2, 3]);
        assert_eq!(config[6], 1, "link up");
    }
}
