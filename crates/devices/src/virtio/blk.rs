//! virtio-blk: a block device backed by a disk image.
//!
//! Requests are served on a worker thread, which the driver's queue notifications wake
//! up. Every sector number and length comes from the guest and is checked against the
//! disk before any I/O happens.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use disk::BlockBackend;
use virtio_bindings::virtio_blk::{
    VIRTIO_BLK_F_BLK_SIZE, VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_RO, VIRTIO_BLK_F_SEG_MAX,
    VIRTIO_BLK_ID_BYTES, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP,
    VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{
    QueueInterrupt, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_RING_INDIRECT_DESC, VIRTIO_F_VERSION_1,
    VirtioDevice,
};

pub const SECTOR_SIZE: u64 = 512;
const QUEUE_SIZE: u16 = 256;
/// Segments per request we advertise: the queue size minus header and status.
const SEG_MAX: u32 = QUEUE_SIZE as u32 - 2;
/// Requests larger than this fail rather than allocate without bound.
const MAX_REQUEST_BYTES: usize = 64 << 20;
const ID_BYTES: usize = VIRTIO_BLK_ID_BYTES as usize;

enum Message {
    Notify,
    Stop,
}

struct Worker {
    messages: Sender<Message>,
    thread: JoinHandle<()>,
}

pub struct VirtioBlk {
    backend: Arc<dyn BlockBackend>,
    /// What GET_ID returns: up to 20 bytes, NUL-padded.
    serial: [u8; ID_BYTES],
    worker: Option<Worker>,
}

impl VirtioBlk {
    pub fn new(backend: Arc<dyn BlockBackend>, serial: &str) -> Self {
        let mut id = [0u8; ID_BYTES];
        let len = serial.len().min(ID_BYTES);
        id[..len].copy_from_slice(&serial.as_bytes()[..len]);
        Self {
            backend,
            serial: id,
            worker: None,
        }
    }

    /// The device configuration space (`struct virtio_blk_config`), up to `blk_size`.
    fn config_bytes(&self) -> [u8; 24] {
        let mut config = [0u8; 24];
        config[0..8].copy_from_slice(&(self.backend.len() / SECTOR_SIZE).to_le_bytes());
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        config[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        config
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_type(&self) -> u16 {
        virtio_bindings::virtio_ids::VIRTIO_ID_BLOCK as u16
    }

    fn features(&self) -> u64 {
        let mut features = VIRTIO_F_VERSION_1
            | VIRTIO_F_RING_INDIRECT_DESC
            | VIRTIO_F_RING_EVENT_IDX
            | (1 << VIRTIO_BLK_F_SEG_MAX)
            | (1 << VIRTIO_BLK_F_BLK_SIZE)
            | (1 << VIRTIO_BLK_F_FLUSH);
        if self.backend.is_read_only() {
            features |= 1 << VIRTIO_BLK_F_RO;
        }
        features
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
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
        let queue = queues
            .into_iter()
            .next()
            .ok_or("virtio-blk needs a queue")?;
        if !queue.ready() {
            return Err("the driver didn't enable the request queue".into());
        }
        let (messages, inbox) = mpsc::channel();
        let backend = self.backend.clone();
        let serial = self.serial;
        let thread = thread::Builder::new()
            .name("virtio-blk".into())
            .spawn(move || run(inbox, memory, queue, interrupt, &*backend, &serial))
            .map_err(|e| e.to_string())?;
        self.worker = Some(Worker { messages, thread });
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let Some(worker) = &self.worker {
            let _ = worker.messages.send(Message::Notify);
        }
    }

    fn reset(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.messages.send(Message::Stop);
            let _ = worker.thread.join();
        }
    }
}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        self.reset();
    }
}

fn run(
    inbox: Receiver<Message>,
    memory: GuestMemoryMmap,
    mut queue: Queue,
    interrupt: Arc<dyn QueueInterrupt>,
    backend: &dyn BlockBackend,
    serial: &[u8; ID_BYTES],
) {
    while let Ok(Message::Notify) = inbox.recv() {
        if let Err(e) = process_queue(&memory, &mut queue, backend, serial) {
            tracing::error!(%e, "virtio-blk queue is broken; ignoring it until reset");
            return;
        }
        // Coalesce notifications that arrived while we worked: one pass covered them.
        loop {
            match inbox.try_recv() {
                Ok(Message::Notify) => continue,
                Ok(Message::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        match queue.needs_notification(&memory) {
            Ok(false) => {}
            Ok(true) | Err(_) => interrupt.signal_queue(0),
        }
    }
}

/// Serves every request the driver has made available.
pub(crate) fn process_queue(
    memory: &GuestMemoryMmap,
    queue: &mut Queue,
    backend: &dyn BlockBackend,
    serial: &[u8; ID_BYTES],
) -> Result<(), virtio_queue::Error> {
    loop {
        queue.disable_notification(memory)?;
        while let Some(chain) = queue.pop_descriptor_chain(memory) {
            let head = chain.head_index();
            let used = handle_request(memory, chain, backend, serial);
            queue.add_used(memory, head, used)?;
        }
        // Re-enable notifications; if the driver added requests meanwhile, go again.
        if !queue.enable_notification(memory)? {
            return Ok(());
        }
    }
}

/// Executes one request and returns how many bytes were written into the driver's
/// buffers (data plus the status byte). A chain too malformed to report a status on is
/// returned with nothing written.
fn handle_request(
    memory: &GuestMemoryMmap,
    chain: DescriptorChain<&GuestMemoryMmap>,
    backend: &dyn BlockBackend,
    serial: &[u8; ID_BYTES],
) -> u32 {
    let Ok(mut reader) = chain.clone().reader(memory) else {
        return 0;
    };
    let Ok(mut writer) = chain.writer(memory) else {
        return 0;
    };
    // The status is the last writable byte; everything writable before it is data.
    let Some(status_at) = writer.available_bytes().checked_sub(1) else {
        return 0;
    };
    let Ok(mut status_writer) = writer.split_at(status_at) else {
        return 0;
    };
    let (status, data_written) = execute(&mut reader, &mut writer, backend, serial);
    if status_writer.write_all(&[status as u8]).is_err() {
        return 0;
    }
    (data_written + 1) as u32
}

/// The byte offset of a request's data, if `len` bytes there are inside the disk.
fn disk_range(sector: u64, len: usize, disk_len: u64) -> Option<u64> {
    let offset = sector.checked_mul(SECTOR_SIZE)?;
    let end = offset.checked_add(len as u64)?;
    (end <= disk_len).then_some(offset)
}

fn execute(
    reader: &mut impl Read,
    writer: &mut (impl Write + Available),
    backend: &dyn BlockBackend,
    serial: &[u8; ID_BYTES],
) -> (u32, usize) {
    let mut header = [0u8; 16];
    if reader.read_exact(&mut header).is_err() {
        return (VIRTIO_BLK_S_IOERR, 0);
    }
    let request = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());

    match request {
        VIRTIO_BLK_T_IN => {
            let len = writer.available();
            if len > MAX_REQUEST_BYTES {
                return (VIRTIO_BLK_S_IOERR, 0);
            }
            let Some(offset) = disk_range(sector, len, backend.len()) else {
                return (VIRTIO_BLK_S_IOERR, 0);
            };
            let mut data = vec![0u8; len];
            if backend.read_at(&mut data, offset).is_err() || writer.write_all(&data).is_err() {
                return (VIRTIO_BLK_S_IOERR, 0);
            }
            (VIRTIO_BLK_S_OK, len)
        }
        VIRTIO_BLK_T_OUT => {
            if backend.is_read_only() {
                return (VIRTIO_BLK_S_IOERR, 0);
            }
            let mut data = Vec::new();
            match reader
                .take(MAX_REQUEST_BYTES as u64 + 1)
                .read_to_end(&mut data)
            {
                Ok(len) if len <= MAX_REQUEST_BYTES => {}
                _ => return (VIRTIO_BLK_S_IOERR, 0),
            }
            let Some(offset) = disk_range(sector, data.len(), backend.len()) else {
                return (VIRTIO_BLK_S_IOERR, 0);
            };
            match backend.write_at(&data, offset) {
                Ok(()) => (VIRTIO_BLK_S_OK, 0),
                Err(_) => (VIRTIO_BLK_S_IOERR, 0),
            }
        }
        VIRTIO_BLK_T_FLUSH => match backend.flush() {
            Ok(()) => (VIRTIO_BLK_S_OK, 0),
            Err(_) => (VIRTIO_BLK_S_IOERR, 0),
        },
        VIRTIO_BLK_T_GET_ID => {
            let len = writer.available().min(ID_BYTES);
            match writer.write_all(&serial[..len]) {
                Ok(()) => (VIRTIO_BLK_S_OK, len),
                Err(_) => (VIRTIO_BLK_S_IOERR, 0),
            }
        }
        _ => (VIRTIO_BLK_S_UNSUPP, 0),
    }
}

/// How much room a request's data buffers have left.
trait Available {
    fn available(&self) -> usize;
}

impl Available for virtio_queue::Writer<'_> {
    fn available(&self) -> usize {
        self.available_bytes()
    }
}

#[cfg(test)]
mod tests {
    use virtio_bindings::virtio_ring::{VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Bytes, GuestAddress};

    use disk::MemDisk;

    use super::*;

    const HEADER: u64 = 0x10_0000;
    const DATA: u64 = 0x20_0000;
    const STATUS: u64 = 0x30_0000;
    const SERIAL: &[u8; ID_BYTES] = b"vmmc-test-disk\0\0\0\0\0\0";

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap()
    }

    fn desc(addr: u64, len: u32, flags: u32, next: u16) -> RawDescriptor {
        RawDescriptor::from(Descriptor::new(addr, len, flags as u16, next))
    }

    /// Submits one request (header, optional data, status) and runs the queue.
    /// Returns (status byte, used length).
    fn submit(
        disk: &MemDisk,
        request: u32,
        sector: u64,
        data_len: u32,
        data_writable: bool,
    ) -> (GuestMemoryMmap, u8, u32) {
        let memory = memory();
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&request.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        memory.write_slice(&header, GuestAddress(HEADER)).unwrap();
        memory.write_slice(&[0xee], GuestAddress(STATUS)).unwrap();

        let mock = MockSplitQueue::create(&memory, GuestAddress(0), 16);
        let mut descs = vec![desc(HEADER, 16, VRING_DESC_F_NEXT, 1)];
        if data_len > 0 {
            let flags = VRING_DESC_F_NEXT | if data_writable { VRING_DESC_F_WRITE } else { 0 };
            descs.push(desc(DATA, data_len, flags, 2));
        }
        descs.push(desc(STATUS, 1, VRING_DESC_F_WRITE, 0));
        mock.add_desc_chains(&descs, 0).unwrap();
        let mut queue: Queue = mock.create_queue().unwrap();

        process_queue(&memory, &mut queue, disk, SERIAL).unwrap();
        let used_len = mock.used().ring().ref_at(0).unwrap().load().len();
        let status = memory.read_obj::<u8>(GuestAddress(STATUS)).unwrap();
        (memory, status, used_len)
    }

    #[test]
    fn write_then_read_back() {
        let disk = MemDisk::new(16 * SECTOR_SIZE as usize, false);
        let memory = memory();
        memory
            .write_slice(&[0xab; 1024], GuestAddress(DATA))
            .unwrap();
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&VIRTIO_BLK_T_OUT.to_le_bytes());
        header[8..16].copy_from_slice(&3u64.to_le_bytes());
        memory.write_slice(&header, GuestAddress(HEADER)).unwrap();
        let mock = MockSplitQueue::create(&memory, GuestAddress(0), 16);
        mock.add_desc_chains(
            &[
                desc(HEADER, 16, VRING_DESC_F_NEXT, 1),
                desc(DATA, 1024, VRING_DESC_F_NEXT, 2),
                desc(STATUS, 1, VRING_DESC_F_WRITE, 0),
            ],
            0,
        )
        .unwrap();
        let mut queue: Queue = mock.create_queue().unwrap();
        process_queue(&memory, &mut queue, &disk, SERIAL).unwrap();
        assert_eq!(memory.read_obj::<u8>(GuestAddress(STATUS)).unwrap(), 0);
        assert_eq!(&disk.bytes(3 * 512, 2 * 512)[..], &[0xab; 1024][..]);

        let (memory, status, used) = submit(&disk, VIRTIO_BLK_T_IN, 3, 1024, true);
        assert_eq!(status, VIRTIO_BLK_S_OK as u8);
        assert_eq!(used, 1025, "data plus status");
        let mut back = [0u8; 1024];
        memory.read_slice(&mut back, GuestAddress(DATA)).unwrap();
        assert_eq!(back, [0xab; 1024]);
    }

    #[test]
    fn reads_past_the_end_fail_without_touching_memory() {
        let disk = MemDisk::new(4 * SECTOR_SIZE as usize, false);
        let (_, status, used) = submit(&disk, VIRTIO_BLK_T_IN, 3, 1024, true);
        assert_eq!(status, VIRTIO_BLK_S_IOERR as u8);
        assert_eq!(used, 1);
        // A sector number that overflows the byte offset.
        let (_, status, _) = submit(&disk, VIRTIO_BLK_T_IN, u64::MAX / 2, 512, true);
        assert_eq!(status, VIRTIO_BLK_S_IOERR as u8);
    }

    #[test]
    fn read_only_disks_refuse_writes() {
        let disk = MemDisk::new(4 * SECTOR_SIZE as usize, true);
        let (_, status, _) = submit(&disk, VIRTIO_BLK_T_OUT, 0, 512, false);
        assert_eq!(status, VIRTIO_BLK_S_IOERR as u8);
    }

    #[test]
    fn flush_get_id_and_unsupported_requests() {
        let disk = MemDisk::new(4 * SECTOR_SIZE as usize, false);
        let (_, status, _) = submit(&disk, VIRTIO_BLK_T_FLUSH, 0, 0, false);
        assert_eq!(status, VIRTIO_BLK_S_OK as u8);
        assert_eq!(disk.flushes(), 1);

        let (memory, status, used) = submit(&disk, VIRTIO_BLK_T_GET_ID, 0, 20, true);
        assert_eq!(status, VIRTIO_BLK_S_OK as u8);
        assert_eq!(used, 21);
        let mut id = [0u8; 20];
        memory.read_slice(&mut id, GuestAddress(DATA)).unwrap();
        assert_eq!(&id, SERIAL);

        let (_, status, _) = submit(&disk, 0x7777, 0, 0, false);
        assert_eq!(status, VIRTIO_BLK_S_UNSUPP as u8);
    }

    #[test]
    fn config_space_reports_capacity_and_block_size() {
        let blk = VirtioBlk::new(
            Arc::new(MemDisk::new(2048 * SECTOR_SIZE as usize, false)),
            "x",
        );
        let mut capacity = [0u8; 8];
        blk.read_config(0, &mut capacity);
        assert_eq!(u64::from_le_bytes(capacity), 2048);
        let mut block_size = [0u8; 4];
        blk.read_config(20, &mut block_size);
        assert_eq!(u32::from_le_bytes(block_size), 512);
        assert_eq!(blk.features() & (1 << VIRTIO_BLK_F_RO), 0);
    }
}
