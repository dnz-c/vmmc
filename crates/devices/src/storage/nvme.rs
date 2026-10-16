//! An NVMe 1.4 controller with any number of namespaces, written from the NVM Express
//! base specification.
//!
//! - **BAR 0** (16 KiB): controller registers, then doorbells at 0x1000, the MSI-X table
//!   at 0x2000 and its pending bits at 0x3000.
//! - **Admin commands** run on the vCPU that rings the admin doorbell.
//! - **I/O submission queues** each get a worker thread, woken by their doorbell, which
//!   does the disk I/O and posts completions. A completion queue may serve several
//!   submission queues and interrupts through its own MSI-X vector.
//!
//! Every queue address, size, PRP and LBA comes from the guest and is checked before
//! use. Data moves through PRPs (4 KiB pages); SGLs aren't offered.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use disk::BlockBackend;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::irq::MsiSink;
use crate::pci::msix::{self, MsixState, MsixTable};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice};
use crate::snapshot::{self, Snapshot, StateError};

/// BAR 0 holds everything.
pub const NVME_BAR: u8 = 0;
pub const NVME_BAR_SIZE: u64 = 0x4000;
const DOORBELLS: u64 = 0x1000;
const MSIX_TABLE: u32 = 0x2000;
const MSIX_PBA: u32 = 0x3000;

/// I/O queue pairs the controller offers (as many as vmmc has vCPUs).
pub const MAX_IO_QUEUES: u16 = 32;
/// Entries per queue (CAP.MQES is this minus one).
const MAX_QUEUE_ENTRIES: u32 = 1024;
const PAGE_SIZE: u64 = 4096;
/// Largest transfer: 2^MDTS pages.
const MDTS: u8 = 7;
const MAX_TRANSFER: u64 = PAGE_SIZE << MDTS;
pub const LBA_SIZE: u64 = 512;
const SQ_ENTRY: u64 = 64;
const CQ_ENTRY: u64 = 16;
/// Asynchronous event requests the controller holds (AERL is this minus one).
const AER_LIMIT: usize = 4;

// Controller registers.
const CAP: u64 = 0x00;
const VS: u64 = 0x08;
const INTMS: u64 = 0x0c;
const INTMC: u64 = 0x10;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1c;
const AQA: u64 = 0x24;
const ASQ: u64 = 0x28;
const ACQ: u64 = 0x30;
const REGISTERS_LEN: usize = 0x40;

const VERSION: u32 = 0x0001_0400;
const CC_EN: u32 = 1 << 0;
const CC_SHN_SHIFT: u32 = 14;
const CSTS_RDY: u32 = 1 << 0;
const CSTS_SHST_COMPLETE: u32 = 0b10 << 2;

/// The IDs to report. QEMU's NVMe IDs until vmmc has a PCI vendor ID of its own;
/// drivers bind to the class code, not these.
const VENDOR_ID: u16 = 0x1b36;
const DEVICE_ID: u16 = 0x0010;
const SUBSYSTEM_VENDOR_ID: u16 = 0x1af4;
const MODEL: &str = "vmmc NVMe Disk";
const FIRMWARE: &str = "1.0";

/// A completion status: type in bits 10:8, code in 7:0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(u16);

impl Status {
    pub const SUCCESS: Self = Self(0x000);
    pub const INVALID_OPCODE: Self = Self(0x001);
    pub const INVALID_FIELD: Self = Self(0x002);
    pub const DATA_TRANSFER_ERROR: Self = Self(0x004);
    pub const INTERNAL_ERROR: Self = Self(0x006);
    pub const INVALID_NAMESPACE: Self = Self(0x00b);
    pub const PRP_OFFSET_INVALID: Self = Self(0x013);
    pub const LBA_OUT_OF_RANGE: Self = Self(0x080);
    pub const WRITE_PROTECTED: Self = Self(0x020);
    pub const INVALID_COMPLETION_QUEUE: Self = Self(0x100);
    pub const INVALID_QUEUE_ID: Self = Self(0x101);
    pub const INVALID_QUEUE_SIZE: Self = Self(0x102);
    pub const AER_LIMIT_EXCEEDED: Self = Self(0x105);
    pub const INVALID_VECTOR: Self = Self(0x108);
    pub const INVALID_LOG_PAGE: Self = Self(0x109);
    pub const INVALID_QUEUE_DELETION: Self = Self(0x10c);
    pub const FEATURE_NOT_SAVEABLE: Self = Self(0x10d);

    pub fn code(self) -> u16 {
        self.0
    }
}

type CommandResult = Result<u32, Status>;

/// One namespace: a disk and the IDs the guest sees it by.
pub struct Namespace {
    pub backend: Arc<dyn BlockBackend>,
    /// Stays the same for the life of the disk.
    pub nguid: [u8; 16],
}

/// What both the controller and its queue workers use.
struct Shared {
    memory: GuestMemoryMmap,
    namespaces: Vec<Namespace>,
    msix: Mutex<MsixTable>,
    /// Volatile write cache enabled; when off, every write is flushed.
    write_cache: AtomicBool,
    /// SMART counters: 512-byte units read and written, and commands.
    units_read: AtomicU64,
    units_written: AtomicU64,
    read_commands: AtomicU64,
    write_commands: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A completion queue, shared by the submission queues that complete into it.
struct CompletionQueue {
    id: u16,
    base: u64,
    entries: u32,
    vector: Option<u16>,
    state: Mutex<CqState>,
    /// Signalled when the guest frees entries, or a worker is told to stop.
    space: Condvar,
}

struct CqState {
    head: u32,
    tail: u32,
    phase: bool,
}

impl CompletionQueue {
    fn new(id: u16, base: u64, entries: u32, vector: Option<u16>) -> Self {
        Self {
            id,
            base,
            entries,
            vector,
            state: Mutex::new(CqState {
                head: 0,
                tail: 0,
                phase: true,
            }),
            space: Condvar::new(),
        }
    }

    fn is_full(state: &CqState, entries: u32) -> bool {
        (state.tail + 1) % entries == state.head
    }

    fn has_space(&self) -> bool {
        !Self::is_full(&lock(&self.state), self.entries)
    }

    /// Posts a completion. An I/O queue worker passes its stop flag and waits for room;
    /// the admin queue (whose doorbell runs on the posting thread) passes `None` and
    /// never waits. Returns `false` if it wasn't posted.
    fn post(&self, shared: &Shared, entry: [u32; 4], stop: Option<&AtomicBool>) -> bool {
        let mut state = lock(&self.state);
        while Self::is_full(&state, self.entries) {
            match stop {
                Some(stop) if !stop.load(Ordering::Acquire) => {
                    state = self
                        .space
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                _ => return false,
            }
        }
        let address = GuestAddress(self.base + u64::from(state.tail) * CQ_ENTRY);
        let mut bytes = [0u8; 16];
        let dw3 = entry[3] | (u32::from(state.phase) << 16);
        for (i, dword) in [entry[0], entry[1], entry[2], dw3].iter().enumerate() {
            bytes[4 * i..4 * i + 4].copy_from_slice(&dword.to_le_bytes());
        }
        // The guest polls the phase bit in the last dword, so it goes in last.
        let written = shared.memory.write_slice(&bytes[..12], address).is_ok()
            && shared
                .memory
                .write_slice(&bytes[12..], GuestAddress(address.0 + 12))
                .is_ok();
        if !written {
            tracing::warn!(cq = self.id, "NVMe completion queue isn't in guest RAM");
        }
        state.tail = (state.tail + 1) % self.entries;
        if state.tail == 0 {
            state.phase = !state.phase;
        }
        drop(state);
        if let Some(vector) = self.vector {
            lock(&shared.msix).notify(vector);
        }
        true
    }

    fn set_head(&self, head: u32) {
        lock(&self.state).head = head;
        self.space.notify_all();
    }
}

enum Message {
    Notify,
    /// Answered once everything before it is done.
    Sync(Sender<()>),
    Stop,
}

/// An I/O submission queue and its worker.
struct SubmissionQueue {
    cq: Arc<CompletionQueue>,
    base: u64,
    entries: u32,
    tail: Arc<AtomicU16>,
    /// The worker's head: the next entry it takes, once it completed the one before.
    head: Arc<AtomicU32>,
    messages: Sender<Message>,
    /// Tells the worker to give up waiting for completion queue room.
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SubmissionQueue {
    fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.messages.send(Message::Stop);
        // Take the lock so a worker about to wait sees the flag first.
        drop(lock(&self.cq.state));
        self.cq.space.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SubmissionQueue {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A command as the guest submitted it.
#[derive(Clone, Copy, Debug)]
struct Command {
    opcode: u8,
    cid: u16,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
}

impl Command {
    fn parse(bytes: &[u8; SQ_ENTRY as usize]) -> Self {
        let dword = |i: usize| u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap());
        let qword = |i: usize| u64::from(dword(i)) | (u64::from(dword(i + 1)) << 32);
        Self {
            opcode: bytes[0],
            cid: u16::from_le_bytes([bytes[2], bytes[3]]),
            nsid: dword(1),
            prp1: qword(6),
            prp2: qword(8),
            cdw10: dword(10),
            cdw11: dword(11),
            cdw12: dword(12),
            cdw13: dword(13),
        }
    }
}

/// The completion entry for `command` from queue `sqid` whose head is now `sq_head`.
fn completion(command: &Command, sqid: u16, sq_head: u32, result: CommandResult) -> [u32; 4] {
    let (dw0, status) = match result {
        Ok(dw0) => (dw0, Status::SUCCESS),
        Err(status) => (0, status),
    };
    [
        dw0,
        0,
        sq_head | (u32::from(sqid) << 16),
        u32::from(command.cid) | (u32::from(status.0) << 17),
    ]
}

/// The guest-physical pieces a PRP pair describes for a `len`-byte transfer.
fn prp_segments(
    memory: &GuestMemoryMmap,
    prp1: u64,
    prp2: u64,
    len: u64,
) -> Result<Vec<(u64, u64)>, Status> {
    if len > MAX_TRANSFER {
        return Err(Status::INVALID_FIELD);
    }
    let read_entry = |at: u64| -> Result<u64, Status> {
        let mut raw = [0u8; 8];
        memory
            .read_slice(&mut raw, GuestAddress(at))
            .map_err(|_| Status::DATA_TRANSFER_ERROR)?;
        Ok(u64::from_le_bytes(raw))
    };
    let mut segments = Vec::new();
    let first = len.min(PAGE_SIZE - prp1 % PAGE_SIZE);
    segments.push((prp1, first));
    let mut left = len - first;
    if left == 0 {
        return Ok(segments);
    }
    if left <= PAGE_SIZE {
        if !prp2.is_multiple_of(PAGE_SIZE) {
            return Err(Status::PRP_OFFSET_INVALID);
        }
        segments.push((prp2, left));
        return Ok(segments);
    }
    // PRP2 points at a list of page addresses. When the rest doesn't fit in the list's
    // page, its last entry points at the next list page instead.
    let mut list = prp2;
    // Every hop but a degenerate one adds a page, so this bounds a looping list.
    let mut hops = MAX_TRANSFER / PAGE_SIZE + 1;
    while left > 0 {
        if !list.is_multiple_of(8) || hops == 0 {
            return Err(Status::PRP_OFFSET_INVALID);
        }
        hops -= 1;
        let slots = (PAGE_SIZE - list % PAGE_SIZE) / 8;
        let pages = left.div_ceil(PAGE_SIZE);
        let data_slots = if pages <= slots { pages } else { slots - 1 };
        for slot in 0..data_slots {
            let page = read_entry(list + 8 * slot)?;
            if !page.is_multiple_of(PAGE_SIZE) {
                return Err(Status::PRP_OFFSET_INVALID);
            }
            let now = left.min(PAGE_SIZE);
            segments.push((page, now));
            left -= now;
        }
        if left > 0 {
            list = read_entry(list + 8 * (slots - 1))?;
        }
    }
    Ok(segments)
}

/// Copies `data` to the guest through `command`'s PRPs.
fn to_guest(shared: &Shared, command: &Command, data: &[u8]) -> Result<(), Status> {
    let mut done = 0;
    for (address, len) in prp_segments(
        &shared.memory,
        command.prp1,
        command.prp2,
        data.len() as u64,
    )? {
        let len = len as usize;
        shared
            .memory
            .write_slice(&data[done..done + len], GuestAddress(address))
            .map_err(|_| Status::DATA_TRANSFER_ERROR)?;
        done += len;
    }
    Ok(())
}

/// Copies `data.len()` bytes from the guest through `command`'s PRPs.
fn from_guest(shared: &Shared, command: &Command, data: &mut [u8]) -> Result<(), Status> {
    let mut done = 0;
    for (address, len) in prp_segments(
        &shared.memory,
        command.prp1,
        command.prp2,
        data.len() as u64,
    )? {
        let len = len as usize;
        shared
            .memory
            .read_slice(&mut data[done..done + len], GuestAddress(address))
            .map_err(|_| Status::DATA_TRANSFER_ERROR)?;
        done += len;
    }
    Ok(())
}

impl Shared {
    fn namespace(&self, nsid: u32) -> Result<&Namespace, Status> {
        nsid.checked_sub(1)
            .and_then(|index| self.namespaces.get(index as usize))
            .ok_or(Status::INVALID_NAMESPACE)
    }

    /// Byte offset and length of an LBA range, checked against the namespace.
    fn lba_range(namespace: &Namespace, slba: u64, blocks: u64) -> Result<(u64, u64), Status> {
        let lbas = namespace.backend.len() / LBA_SIZE;
        match slba.checked_add(blocks) {
            Some(end) if end <= lbas => Ok((slba * LBA_SIZE, blocks * LBA_SIZE)),
            _ => Err(Status::LBA_OUT_OF_RANGE),
        }
    }

    /// Runs an NVM command set (I/O) command.
    fn io_command(&self, command: &Command) -> CommandResult {
        const FLUSH: u8 = 0x00;
        const WRITE: u8 = 0x01;
        const READ: u8 = 0x02;
        const WRITE_ZEROES: u8 = 0x08;
        const DATASET_MANAGEMENT: u8 = 0x09;

        if command.opcode == FLUSH {
            let all = command.nsid == u32::MAX;
            for (index, namespace) in self.namespaces.iter().enumerate() {
                if all || command.nsid == index as u32 + 1 {
                    namespace.backend.flush().map_err(io_error)?;
                }
            }
            return if all {
                Ok(0)
            } else {
                self.namespace(command.nsid).map(|_| 0)
            };
        }
        let namespace = self.namespace(command.nsid)?;
        let slba = u64::from(command.cdw10) | (u64::from(command.cdw11) << 32);
        let blocks = u64::from(command.cdw12 & 0xffff) + 1;
        // Transfers are capped at MDTS before anything else is looked at.
        if matches!(command.opcode, READ | WRITE) && blocks * LBA_SIZE > MAX_TRANSFER {
            return Err(Status::INVALID_FIELD);
        }
        match command.opcode {
            READ => {
                let (offset, len) = Self::lba_range(namespace, slba, blocks)?;
                let mut data = vec![0u8; len as usize];
                namespace
                    .backend
                    .read_at(&mut data, offset)
                    .map_err(io_error)?;
                to_guest(self, command, &data)?;
                self.units_read.fetch_add(blocks, Ordering::Relaxed);
                self.read_commands.fetch_add(1, Ordering::Relaxed);
                Ok(0)
            }
            WRITE => {
                let (offset, len) = Self::lba_range(namespace, slba, blocks)?;
                if namespace.backend.is_read_only() {
                    return Err(Status::WRITE_PROTECTED);
                }
                let mut data = vec![0u8; len as usize];
                from_guest(self, command, &mut data)?;
                namespace
                    .backend
                    .write_at(&data, offset)
                    .map_err(io_error)?;
                // Force unit access, or no write cache: the data must be durable now.
                let fua = command.cdw12 & (1 << 30) != 0;
                if fua || !self.write_cache.load(Ordering::Relaxed) {
                    namespace.backend.flush().map_err(io_error)?;
                }
                self.units_written.fetch_add(blocks, Ordering::Relaxed);
                self.write_commands.fetch_add(1, Ordering::Relaxed);
                Ok(0)
            }
            WRITE_ZEROES => {
                let (offset, len) = Self::lba_range(namespace, slba, blocks)?;
                if namespace.backend.is_read_only() {
                    return Err(Status::WRITE_PROTECTED);
                }
                namespace
                    .backend
                    .write_zeroes(offset, len)
                    .map_err(io_error)?;
                Ok(0)
            }
            DATASET_MANAGEMENT => {
                const DEALLOCATE: u32 = 1 << 2;
                let ranges = (command.cdw10 & 0xff) as usize + 1;
                let mut list = vec![0u8; ranges * 16];
                from_guest(self, command, &mut list)?;
                if command.cdw11 & DEALLOCATE == 0 {
                    // Only hints otherwise.
                    return Ok(0);
                }
                if namespace.backend.is_read_only() {
                    return Err(Status::WRITE_PROTECTED);
                }
                for range in list.chunks(16) {
                    let blocks = u64::from(u32::from_le_bytes(range[4..8].try_into().unwrap()));
                    let slba = u64::from_le_bytes(range[8..16].try_into().unwrap());
                    let (offset, len) = Self::lba_range(namespace, slba, blocks)?;
                    namespace.backend.discard(offset, len).map_err(io_error)?;
                }
                Ok(0)
            }
            _ => Err(Status::INVALID_OPCODE),
        }
    }
}

fn io_error(e: std::io::Error) -> Status {
    tracing::warn!(%e, "NVMe disk I/O failed");
    Status::INTERNAL_ERROR
}

/// What an I/O queue worker serves.
struct QueueWork {
    shared: Arc<Shared>,
    sqid: u16,
    base: u64,
    entries: u32,
    tail: Arc<AtomicU16>,
    head: Arc<AtomicU32>,
    cq: Arc<CompletionQueue>,
    stopping: Arc<AtomicBool>,
}

/// Serves one I/O submission queue until told to stop.
fn run_queue(work: QueueWork, messages: Receiver<Message>) {
    loop {
        match messages.recv() {
            Ok(Message::Notify) => {}
            Ok(Message::Sync(done)) => {
                let _ = done.send(());
                continue;
            }
            Ok(Message::Stop) | Err(_) => return,
        }
        loop {
            let head = work.head.load(Ordering::Acquire);
            let tail = u32::from(work.tail.load(Ordering::Acquire));
            if head == tail {
                break;
            }
            let mut raw = [0u8; SQ_ENTRY as usize];
            let address = GuestAddress(work.base + u64::from(head) * SQ_ENTRY);
            let read = work.shared.memory.read_slice(&mut raw, address).is_ok();
            let next = (head + 1) % work.entries;
            let command = Command::parse(&raw);
            let result = if read {
                work.shared.io_command(&command)
            } else {
                Err(Status::DATA_TRANSFER_ERROR)
            };
            let entry = completion(&command, work.sqid, next, result);
            if !work.cq.post(&work.shared, entry, Some(&work.stopping)) {
                return;
            }
            // Only now: a snapshot taken while the completion waits for room runs the
            // command again rather than lose it.
            work.head.store(next, Ordering::Release);
        }
    }
}

pub struct Nvme {
    config: ConfigSpace,
    msix_capability: u16,
    shared: Arc<Shared>,
    serial: String,
    registers: [u8; REGISTERS_LEN],
    /// Admin queue: head of the submission queue, the guest's tail, and its completion
    /// queue (also in `cqs`).
    admin: Option<AdminQueue>,
    cqs: Vec<Option<Arc<CompletionQueue>>>,
    sqs: Vec<Option<SubmissionQueue>>,
    /// Command IDs of the outstanding asynchronous event requests.
    pending_events: Vec<u16>,
    features: Features,
}

struct AdminQueue {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
    cq: Arc<CompletionQueue>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct Features {
    arbitration: u32,
    power_management: u32,
    temperature_threshold: u32,
    error_recovery: u32,
    interrupt_coalescing: u32,
    write_atomicity: u32,
    async_event_config: u32,
    /// Allocated I/O queues, as Number of Queues reports them (0-based, CQs << 16).
    queues: u32,
}

impl Nvme {
    /// A controller whose namespaces are `namespaces`, in order (NSID 1 first).
    pub fn new(
        namespaces: Vec<Namespace>,
        serial: &str,
        memory: GuestMemoryMmap,
        sink: Arc<dyn MsiSink>,
    ) -> Self {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: VENDOR_ID,
            device_id: DEVICE_ID,
            revision: 2,
            // Mass storage, non-volatile memory, NVM Express.
            class: 0x01_08_02,
            subsystem_vendor_id: SUBSYSTEM_VENDOR_ID,
            subsystem_id: 0x1100,
        });
        config.add_bar(NVME_BAR.into(), BarKind::Memory64, NVME_BAR_SIZE, false);
        let vectors = MAX_IO_QUEUES + 1;
        let msix_capability = msix::add_capability(
            &mut config,
            vectors,
            (NVME_BAR, MSIX_TABLE),
            (NVME_BAR, MSIX_PBA),
        );
        let mut nvme = Self {
            config,
            msix_capability,
            shared: Arc::new(Shared {
                memory,
                namespaces,
                msix: Mutex::new(MsixTable::new(vectors, sink)),
                write_cache: AtomicBool::new(true),
                units_read: AtomicU64::new(0),
                units_written: AtomicU64::new(0),
                read_commands: AtomicU64::new(0),
                write_commands: AtomicU64::new(0),
            }),
            serial: serial.to_owned(),
            registers: [0; REGISTERS_LEN],
            admin: None,
            cqs: (0..=MAX_IO_QUEUES).map(|_| None).collect(),
            sqs: (0..=MAX_IO_QUEUES).map(|_| None).collect(),
            pending_events: Vec::new(),
            features: Features::default(),
        };
        nvme.reset();
        nvme
    }

    fn register(&self, offset: u64) -> u32 {
        let at = offset as usize;
        u32::from_le_bytes(self.registers[at..at + 4].try_into().unwrap())
    }

    fn set_register(&mut self, offset: u64, value: u32) {
        let at = offset as usize;
        self.registers[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn register64(&self, offset: u64) -> u64 {
        u64::from(self.register(offset)) | (u64::from(self.register(offset + 4)) << 32)
    }

    /// Controller reset: every queue goes, registers return to their defaults.
    fn reset(&mut self) {
        for sq in &mut self.sqs {
            *sq = None;
        }
        self.cqs.fill(None);
        self.admin = None;
        self.pending_events.clear();
        self.features = Features {
            queues: (u32::from(MAX_IO_QUEUES - 1) << 16) | u32::from(MAX_IO_QUEUES - 1),
            ..Features::default()
        };
        self.shared.write_cache.store(true, Ordering::Relaxed);
        let aqa = self.register(AQA);
        let asq = self.register64(ASQ);
        let acq = self.register64(ACQ);
        self.registers = [0; REGISTERS_LEN];
        // CAP: MQES, contiguous queues required, 4 KiB pages only, NVM command set,
        // doorbell stride 4, ready timeout 5 s (in 500 ms units).
        let cap: u64 = u64::from(MAX_QUEUE_ENTRIES - 1) | (1 << 16) | (10 << 24) | (1 << 37);
        self.set_register(CAP, cap as u32);
        self.set_register(CAP + 4, (cap >> 32) as u32);
        self.set_register(VS, VERSION);
        // The admin queue registers survive a controller reset.
        self.set_register(AQA, aqa);
        self.set_register(ASQ, asq as u32);
        self.set_register(ASQ + 4, (asq >> 32) as u32);
        self.set_register(ACQ, acq as u32);
        self.set_register(ACQ + 4, (acq >> 32) as u32);
    }

    fn enable(&mut self) {
        let aqa = self.register(AQA);
        let sq_entries = (aqa & 0xfff) + 1;
        let cq_entries = ((aqa >> 16) & 0xfff) + 1;
        let asq = self.register64(ASQ) & !(PAGE_SIZE - 1);
        let acq = self.register64(ACQ) & !(PAGE_SIZE - 1);
        if sq_entries < 2 || cq_entries < 2 {
            tracing::warn!(aqa, "NVMe: admin queue too small; staying disabled");
            return;
        }
        let cq = Arc::new(CompletionQueue::new(0, acq, cq_entries, Some(0)));
        self.cqs[0] = Some(cq.clone());
        self.admin = Some(AdminQueue {
            base: asq,
            entries: sq_entries,
            head: 0,
            tail: 0,
            cq,
        });
        let csts = self.register(CSTS) | CSTS_RDY;
        self.set_register(CSTS, csts);
    }

    fn write_cc(&mut self, value: u32) {
        let old = self.register(CC);
        // EN, CSS, MPS, AMS, SHN, IOSQES and IOCQES are writable.
        self.set_register(CC, value & 0x00ff_fff1);
        if value & CC_EN != 0 && old & CC_EN == 0 {
            self.enable();
        } else if value & CC_EN == 0 && old & CC_EN != 0 {
            self.reset();
            self.set_register(CC, value & 0x00ff_fff0);
        }
        // A shutdown notification: write everything out, then report it done.
        if (value >> CC_SHN_SHIFT) & 0b11 != 0 && (old >> CC_SHN_SHIFT) & 0b11 == 0 {
            for namespace in &self.shared.namespaces {
                if let Err(e) = namespace.backend.flush() {
                    tracing::warn!(%e, "NVMe: flush on shutdown failed");
                }
            }
            let csts = self.register(CSTS) | CSTS_SHST_COMPLETE;
            self.set_register(CSTS, csts);
        } else if (value >> CC_SHN_SHIFT) & 0b11 == 0 {
            let csts = self.register(CSTS) & !(0b11 << 2);
            self.set_register(CSTS, csts);
        }
    }

    fn write_register(&mut self, offset: u64, data: &[u8]) {
        // Apply the bytes to a copy of the dword(s) they touch, then act on whole
        // registers.
        let start = (offset & !3) as usize;
        let end = (offset as usize + data.len())
            .next_multiple_of(4)
            .min(REGISTERS_LEN);
        if start >= REGISTERS_LEN {
            return;
        }
        let mut bytes = self.registers[start..end].to_vec();
        let at = offset as usize - start;
        let len = data.len().min(bytes.len() - at);
        bytes[at..at + len].copy_from_slice(&data[..len]);
        for (i, dword) in bytes.chunks(4).enumerate() {
            let register = (start + 4 * i) as u64;
            let value = u32::from_le_bytes(dword.try_into().unwrap());
            match register {
                INTMS => {
                    let mask = self.register(INTMS) | value;
                    self.set_register(INTMS, mask);
                }
                INTMC => {
                    let mask = self.register(INTMS) & !value;
                    self.set_register(INTMS, mask);
                }
                CC => self.write_cc(value),
                AQA => self.set_register(AQA, value & 0x0fff_0fff),
                ASQ | 0x2c | ACQ | 0x34 => self.set_register(register, value),
                _ => {}
            }
        }
    }

    fn read_register(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let at = offset as usize + i;
            *byte = match at {
                // INTMC reads back the mask too.
                0x10..=0x13 => self.registers[at - 4],
                _ => self.registers.get(at).copied().unwrap_or(0),
            };
        }
    }

    fn doorbell(&mut self, offset: u64, value: u32) {
        let index = offset / 4;
        let qid = (index / 2) as usize;
        let is_cq_head = index % 2 == 1;
        if is_cq_head {
            let Some(cq) = self.cqs.get(qid).and_then(Option::as_ref) else {
                tracing::debug!(qid, "NVMe: doorbell for a missing completion queue");
                return;
            };
            if value >= cq.entries {
                tracing::debug!(qid, value, "NVMe: completion queue head out of range");
                return;
            }
            cq.set_head(value);
            // The admin queue may have been waiting for room.
            if qid == 0 {
                self.run_admin();
            }
        } else if qid == 0 {
            match &mut self.admin {
                Some(admin) if value < admin.entries => admin.tail = value,
                _ => return,
            }
            self.run_admin();
        } else if let Some(sq) = self.sqs.get(qid).and_then(Option::as_ref)
            && value < sq.entries
        {
            sq.tail.store(value as u16, Ordering::Release);
            let _ = sq.messages.send(Message::Notify);
        }
    }

    /// Runs admin commands until the queue is empty or its completion queue is full.
    fn run_admin(&mut self) {
        loop {
            let Some(admin) = &self.admin else { return };
            if admin.head == admin.tail || !admin.cq.has_space() {
                return;
            }
            let address = GuestAddress(admin.base + u64::from(admin.head) * SQ_ENTRY);
            let mut raw = [0u8; SQ_ENTRY as usize];
            let read = self.shared.memory.read_slice(&mut raw, address).is_ok();
            let command = Command::parse(&raw);
            let result = if read {
                self.admin_command(&command)
            } else {
                Some(Err(Status::DATA_TRANSFER_ERROR))
            };
            let Some(admin) = &mut self.admin else { return };
            admin.head = (admin.head + 1) % admin.entries;
            // Held asynchronous event requests complete later, if ever.
            if let Some(result) = result {
                let entry = completion(&command, 0, admin.head, result);
                admin.cq.post(&self.shared, entry, None);
            }
        }
    }

    /// Runs an admin command. `None` means it completes later.
    fn admin_command(&mut self, command: &Command) -> Option<CommandResult> {
        const DELETE_SQ: u8 = 0x00;
        const CREATE_SQ: u8 = 0x01;
        const GET_LOG_PAGE: u8 = 0x02;
        const DELETE_CQ: u8 = 0x04;
        const CREATE_CQ: u8 = 0x05;
        const IDENTIFY: u8 = 0x06;
        const ABORT: u8 = 0x08;
        const SET_FEATURES: u8 = 0x09;
        const GET_FEATURES: u8 = 0x0a;
        const ASYNC_EVENT_REQUEST: u8 = 0x0c;

        Some(match command.opcode {
            IDENTIFY => self.identify(command),
            CREATE_CQ => self.create_cq(command),
            CREATE_SQ => self.create_sq(command),
            DELETE_SQ => self.delete_sq(command),
            DELETE_CQ => self.delete_cq(command),
            GET_FEATURES => self.get_features(command),
            SET_FEATURES => self.set_features(command),
            GET_LOG_PAGE => self.get_log_page(command),
            // Never aborts anything: bit 0 of the result says so.
            ABORT => Ok(1),
            ASYNC_EVENT_REQUEST => {
                if self.pending_events.len() >= AER_LIMIT {
                    return Some(Err(Status::AER_LIMIT_EXCEEDED));
                }
                // No events are ever raised, so the request stays outstanding.
                self.pending_events.push(command.cid);
                return None;
            }
            _ => Err(Status::INVALID_OPCODE),
        })
    }

    fn identify(&self, command: &Command) -> CommandResult {
        let mut data = vec![0u8; 4096];
        match command.cdw10 & 0xff {
            // Namespace.
            0x00 => {
                let namespace = self.shared.namespace(command.nsid)?;
                let lbas = namespace.backend.len() / LBA_SIZE;
                data[0..8].copy_from_slice(&lbas.to_le_bytes()); // size
                data[8..16].copy_from_slice(&lbas.to_le_bytes()); // capacity
                data[16..24].copy_from_slice(&lbas.to_le_bytes()); // utilization
                data[25] = 0; // one LBA format (0-based)
                data[26] = 0; // format 0, metadata none
                data[104..120].copy_from_slice(&namespace.nguid);
                // LBA format 0: no metadata, 2^9-byte blocks, best performance.
                data[128..132].copy_from_slice(&(9u32 << 16).to_le_bytes());
            }
            // Controller.
            0x01 => self.identify_controller(&mut data),
            // Active namespace IDs above the one given.
            0x02 => {
                let ids = (1..=self.shared.namespaces.len() as u32).filter(|&id| id > command.nsid);
                for (i, id) in ids.take(1024).enumerate() {
                    data[4 * i..4 * i + 4].copy_from_slice(&id.to_le_bytes());
                }
            }
            // Namespace identification descriptors: the NGUID.
            0x03 => {
                let namespace = self.shared.namespace(command.nsid)?;
                data[0] = 0x02;
                data[1] = 16;
                data[4..20].copy_from_slice(&namespace.nguid);
            }
            _ => return Err(Status::INVALID_FIELD),
        }
        to_guest(&self.shared, command, &data)?;
        Ok(0)
    }

    fn identify_controller(&self, data: &mut [u8]) {
        let text = |data: &mut [u8], at: usize, len: usize, value: &str| {
            let field = &mut data[at..at + len];
            field.fill(b' ');
            let bytes = value.as_bytes();
            let n = bytes.len().min(len);
            field[..n].copy_from_slice(&bytes[..n]);
        };
        data[0..2].copy_from_slice(&VENDOR_ID.to_le_bytes());
        data[2..4].copy_from_slice(&SUBSYSTEM_VENDOR_ID.to_le_bytes());
        text(data, 4, 20, &self.serial);
        text(data, 24, 40, MODEL);
        text(data, 64, 8, FIRMWARE);
        data[72] = 6; // recommended arbitration burst
        data[77] = MDTS;
        data[80..84].copy_from_slice(&VERSION.to_le_bytes());
        data[258] = 3; // abort command limit, 0-based
        data[259] = (AER_LIMIT - 1) as u8;
        data[260] = 0b011; // one firmware slot, slot 1 read-only
        data[261] = 0; // log page attributes
        data[262] = 0; // one error log entry, 0-based
        data[263] = 0; // one power state, 0-based
        data[266..268].copy_from_slice(&343u16.to_le_bytes()); // warning temperature, K
        data[268..270].copy_from_slice(&373u16.to_le_bytes()); // critical temperature, K
        data[512] = 0x66; // submission queue entries: 64 bytes
        data[513] = 0x44; // completion queue entries: 16 bytes
        data[516..520].copy_from_slice(&(self.shared.namespaces.len() as u32).to_le_bytes());
        // Optional NVM commands: Dataset Management and Write Zeroes.
        data[520..522].copy_from_slice(&0x000cu16.to_le_bytes());
        data[525] = 1; // volatile write cache present
        let nqn = format!("nqn.2026-10.dev.vmmc:nvme:{}", self.serial);
        let nqn = nqn.as_bytes();
        data[768..768 + nqn.len().min(255)].copy_from_slice(&nqn[..nqn.len().min(255)]);
        // Power state 0: 25 W.
        data[2048..2050].copy_from_slice(&2500u16.to_le_bytes());
    }

    /// The I/O queue ID in CDW10, checked to be in range.
    fn io_queue_id(command: &Command) -> Result<usize, Status> {
        let qid = command.cdw10 & 0xffff;
        if (1..=u32::from(MAX_IO_QUEUES)).contains(&qid) {
            Ok(qid as usize)
        } else {
            Err(Status::INVALID_QUEUE_ID)
        }
    }

    fn queue_entries(command: &Command) -> Result<u32, Status> {
        let entries = (command.cdw10 >> 16) + 1;
        if (2..=MAX_QUEUE_ENTRIES).contains(&entries) {
            Ok(entries)
        } else {
            Err(Status::INVALID_QUEUE_SIZE)
        }
    }

    fn create_cq(&mut self, command: &Command) -> CommandResult {
        const PHYSICALLY_CONTIGUOUS: u32 = 1 << 0;
        const INTERRUPTS_ENABLED: u32 = 1 << 1;
        let qid = Self::io_queue_id(command)?;
        if self.cqs[qid].is_some() {
            return Err(Status::INVALID_QUEUE_ID);
        }
        let entries = Self::queue_entries(command)?;
        if command.cdw11 & PHYSICALLY_CONTIGUOUS == 0 || !command.prp1.is_multiple_of(PAGE_SIZE) {
            return Err(Status::INVALID_FIELD);
        }
        let vector = (command.cdw11 >> 16) as u16;
        if vector >= lock(&self.shared.msix).len() {
            return Err(Status::INVALID_VECTOR);
        }
        let vector = (command.cdw11 & INTERRUPTS_ENABLED != 0).then_some(vector);
        self.cqs[qid] = Some(Arc::new(CompletionQueue::new(
            qid as u16,
            command.prp1,
            entries,
            vector,
        )));
        Ok(0)
    }

    fn create_sq(&mut self, command: &Command) -> CommandResult {
        const PHYSICALLY_CONTIGUOUS: u32 = 1 << 0;
        let qid = Self::io_queue_id(command)?;
        if self.sqs[qid].is_some() {
            return Err(Status::INVALID_QUEUE_ID);
        }
        let entries = Self::queue_entries(command)?;
        if command.cdw11 & PHYSICALLY_CONTIGUOUS == 0 || !command.prp1.is_multiple_of(PAGE_SIZE) {
            return Err(Status::INVALID_FIELD);
        }
        let cqid = (command.cdw11 >> 16) as usize;
        let Some(cq) = self
            .cqs
            .get(cqid)
            .filter(|_| cqid != 0)
            .and_then(Option::clone)
        else {
            return Err(Status::INVALID_COMPLETION_QUEUE);
        };
        self.sqs[qid] = Some(
            self.start_sq(qid, command.prp1, entries, cq, 0, 0)
                .map_err(|e| {
                    tracing::error!(%e, "NVMe: can't start a queue thread");
                    Status::INTERNAL_ERROR
                })?,
        );
        Ok(0)
    }

    /// An I/O submission queue and its worker, at `head` and `tail`.
    fn start_sq(
        &self,
        qid: usize,
        base: u64,
        entries: u32,
        cq: Arc<CompletionQueue>,
        head: u32,
        tail: u16,
    ) -> std::io::Result<SubmissionQueue> {
        let (messages, receiver) = mpsc::channel();
        let tail = Arc::new(AtomicU16::new(tail));
        let head = Arc::new(AtomicU32::new(head));
        let stopping = Arc::new(AtomicBool::new(false));
        let work = QueueWork {
            shared: self.shared.clone(),
            sqid: qid as u16,
            base,
            entries,
            tail: tail.clone(),
            head: head.clone(),
            cq: cq.clone(),
            stopping: stopping.clone(),
        };
        let thread = thread::Builder::new()
            .name(format!("nvme-sq{qid}"))
            .spawn(move || run_queue(work, receiver))?;
        Ok(SubmissionQueue {
            cq,
            base,
            entries,
            tail,
            head,
            messages,
            stopping,
            thread: Some(thread),
        })
    }

    fn delete_sq(&mut self, command: &Command) -> CommandResult {
        let qid = Self::io_queue_id(command)?;
        match self.sqs[qid].take() {
            // Dropping it stops and joins the worker.
            Some(_) => Ok(0),
            None => Err(Status::INVALID_QUEUE_ID),
        }
    }

    fn delete_cq(&mut self, command: &Command) -> CommandResult {
        let qid = Self::io_queue_id(command)?;
        let Some(cq) = &self.cqs[qid] else {
            return Err(Status::INVALID_QUEUE_ID);
        };
        if self.sqs.iter().flatten().any(|sq| Arc::ptr_eq(&sq.cq, cq)) {
            return Err(Status::INVALID_QUEUE_DELETION);
        }
        self.cqs[qid] = None;
        Ok(0)
    }

    fn feature(&mut self, id: u32) -> Option<&mut u32> {
        let features = &mut self.features;
        Some(match id {
            0x01 => &mut features.arbitration,
            0x02 => &mut features.power_management,
            0x04 => &mut features.temperature_threshold,
            0x05 => &mut features.error_recovery,
            0x08 => &mut features.interrupt_coalescing,
            0x0a => &mut features.write_atomicity,
            0x0b => &mut features.async_event_config,
            _ => return None,
        })
    }

    fn get_features(&mut self, command: &Command) -> CommandResult {
        const VOLATILE_WRITE_CACHE: u32 = 0x06;
        const NUMBER_OF_QUEUES: u32 = 0x07;
        const INTERRUPT_VECTOR_CONFIG: u32 = 0x09;
        let id = command.cdw10 & 0xff;
        // Select 3: what can be saved or changed. Nothing is saveable; all changeable.
        if (command.cdw10 >> 8) & 0b111 == 3 {
            return Ok(0b100);
        }
        match id {
            VOLATILE_WRITE_CACHE => Ok(self.shared.write_cache.load(Ordering::Relaxed).into()),
            NUMBER_OF_QUEUES => Ok(self.features.queues),
            // Coalescing is never applied, so no vector has it disabled.
            INTERRUPT_VECTOR_CONFIG => Ok(command.cdw11 & 0xffff),
            _ => self
                .feature(id)
                .map(|value| *value)
                .ok_or(Status::INVALID_FIELD),
        }
    }

    fn set_features(&mut self, command: &Command) -> CommandResult {
        const SAVE: u32 = 1 << 31;
        const VOLATILE_WRITE_CACHE: u32 = 0x06;
        const NUMBER_OF_QUEUES: u32 = 0x07;
        const INTERRUPT_VECTOR_CONFIG: u32 = 0x09;
        if command.cdw10 & SAVE != 0 {
            return Err(Status::FEATURE_NOT_SAVEABLE);
        }
        let id = command.cdw10 & 0xff;
        match id {
            VOLATILE_WRITE_CACHE => {
                self.shared
                    .write_cache
                    .store(command.cdw11 & 1 != 0, Ordering::Relaxed);
                Ok(0)
            }
            NUMBER_OF_QUEUES => {
                let wanted_sqs = command.cdw11 & 0xffff;
                let wanted_cqs = command.cdw11 >> 16;
                if wanted_sqs == 0xffff || wanted_cqs == 0xffff {
                    return Err(Status::INVALID_FIELD);
                }
                let most = u32::from(MAX_IO_QUEUES - 1);
                self.features.queues = (wanted_cqs.min(most) << 16) | wanted_sqs.min(most);
                Ok(self.features.queues)
            }
            INTERRUPT_VECTOR_CONFIG => Ok(0),
            _ => {
                let value = self.feature(id).ok_or(Status::INVALID_FIELD)?;
                *value = command.cdw11;
                Ok(0)
            }
        }
    }

    fn get_log_page(&self, command: &Command) -> CommandResult {
        const ERROR_INFORMATION: u32 = 0x01;
        const SMART: u32 = 0x02;
        const FIRMWARE_SLOTS: u32 = 0x03;
        let id = command.cdw10 & 0xff;
        let dwords = ((command.cdw10 >> 16) | ((command.cdw11 & 0xffff) << 16)) as u64 + 1;
        let offset = u64::from(command.cdw12) | (u64::from(command.cdw13) << 32);
        let mut page = vec![0u8; 512];
        match id {
            // One entry, empty.
            ERROR_INFORMATION => page.truncate(64),
            SMART => {
                // Composite temperature 315 K, spare 100% (threshold 10%), nothing used.
                page[1..3].copy_from_slice(&315u16.to_le_bytes());
                page[3] = 100;
                page[4] = 10;
                // Data units are thousands of 512-byte units, rounded up.
                let read = self
                    .shared
                    .units_read
                    .load(Ordering::Relaxed)
                    .div_ceil(1000);
                let written = self
                    .shared
                    .units_written
                    .load(Ordering::Relaxed)
                    .div_ceil(1000);
                page[32..40].copy_from_slice(&read.to_le_bytes());
                page[48..56].copy_from_slice(&written.to_le_bytes());
                let reads = self.shared.read_commands.load(Ordering::Relaxed);
                let writes = self.shared.write_commands.load(Ordering::Relaxed);
                page[64..72].copy_from_slice(&reads.to_le_bytes());
                page[80..88].copy_from_slice(&writes.to_le_bytes());
                page[112..120].copy_from_slice(&1u64.to_le_bytes()); // power cycles
            }
            FIRMWARE_SLOTS => {
                page[0] = 1; // slot 1 active
                let firmware = FIRMWARE.as_bytes();
                page[8..8 + firmware.len()].copy_from_slice(firmware);
            }
            _ => return Err(Status::INVALID_LOG_PAGE),
        }
        // Offsets past the end read zeroes, as do bytes beyond the page.
        if dwords * 4 > MAX_TRANSFER {
            return Err(Status::INVALID_FIELD);
        }
        let len = (dwords * 4) as usize;
        let mut data = vec![0u8; len];
        if let Some(rest) = page.get(offset as usize..) {
            let n = rest.len().min(len);
            data[..n].copy_from_slice(&rest[..n]);
        }
        to_guest(&self.shared, command, &data)?;
        Ok(0)
    }
}

#[derive(Serialize, Deserialize)]
struct SavedCq {
    id: u16,
    base: u64,
    entries: u32,
    vector: Option<u16>,
    head: u32,
    tail: u32,
    phase: bool,
}

#[derive(Serialize, Deserialize)]
struct SavedSq {
    qid: usize,
    cqid: u16,
    base: u64,
    entries: u32,
    head: u32,
    tail: u16,
}

#[derive(Serialize, Deserialize)]
struct SavedAdmin {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
}

#[derive(Serialize, Deserialize)]
struct NvmeState {
    config: Vec<u8>,
    msix: MsixState,
    registers: Vec<u8>,
    admin: Option<SavedAdmin>,
    cqs: Vec<SavedCq>,
    sqs: Vec<SavedSq>,
    pending_events: Vec<u16>,
    features: Features,
    write_cache: bool,
    /// Units read and written, read and write commands.
    counters: [u64; 4],
}

/// How long a quiesce waits for a queue worker, which can be stuck waiting for room in
/// a full completion queue the paused guest won't empty.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(5);

impl Snapshot for Nvme {
    fn save(&self) -> Vec<u8> {
        let saved_cq = |cq: &CompletionQueue| {
            let state = lock(&cq.state);
            SavedCq {
                id: cq.id,
                base: cq.base,
                entries: cq.entries,
                vector: cq.vector,
                head: state.head,
                tail: state.tail,
                phase: state.phase,
            }
        };
        let counters = &self.shared;
        snapshot::encode(&NvmeState {
            config: self.config.save(),
            msix: lock(&self.shared.msix).save(),
            registers: self.registers.to_vec(),
            admin: self.admin.as_ref().map(|admin| SavedAdmin {
                base: admin.base,
                entries: admin.entries,
                head: admin.head,
                tail: admin.tail,
            }),
            cqs: self.cqs.iter().flatten().map(|cq| saved_cq(cq)).collect(),
            sqs: self
                .sqs
                .iter()
                .enumerate()
                .filter_map(|(qid, sq)| {
                    let sq = sq.as_ref()?;
                    Some(SavedSq {
                        qid,
                        cqid: sq.cq.id,
                        base: sq.base,
                        entries: sq.entries,
                        head: sq.head.load(Ordering::Acquire),
                        tail: sq.tail.load(Ordering::Acquire),
                    })
                })
                .collect(),
            pending_events: self.pending_events.clone(),
            features: self.features,
            write_cache: self.shared.write_cache.load(Ordering::Relaxed),
            counters: [
                counters.units_read.load(Ordering::Relaxed),
                counters.units_written.load(Ordering::Relaxed),
                counters.read_commands.load(Ordering::Relaxed),
                counters.write_commands.load(Ordering::Relaxed),
            ],
        })
    }

    /// Commands the guest had submitted and the controller hadn't taken run now.
    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: NvmeState = snapshot::decode(state)?;
        let bad = |what: &str| StateError(format!("NVMe: {what}"));
        for sq in &mut self.sqs {
            *sq = None;
        }
        self.cqs.fill(None);
        self.config.restore(&state.config)?;
        lock(&self.shared.msix).restore(state.msix)?;
        self.registers = state
            .registers
            .try_into()
            .map_err(|_| bad("the registers were a different size"))?;
        for saved in state.cqs {
            let cq = CompletionQueue::new(saved.id, saved.base, saved.entries, saved.vector);
            *lock(&cq.state) = CqState {
                head: saved.head,
                tail: saved.tail,
                phase: saved.phase,
            };
            *self
                .cqs
                .get_mut(usize::from(saved.id))
                .ok_or_else(|| bad("a completion queue out of range"))? = Some(Arc::new(cq));
        }
        self.admin = match state.admin {
            Some(saved) => Some(AdminQueue {
                base: saved.base,
                entries: saved.entries,
                head: saved.head,
                tail: saved.tail,
                cq: self.cqs[0]
                    .clone()
                    .ok_or_else(|| bad("an admin queue without its completion queue"))?,
            }),
            None => None,
        };
        for saved in state.sqs {
            let cq = self
                .cqs
                .get(usize::from(saved.cqid))
                .and_then(Option::clone)
                .ok_or_else(|| bad("a submission queue without its completion queue"))?;
            if saved.qid == 0 || saved.qid >= self.sqs.len() {
                return Err(bad("a submission queue out of range"));
            }
            let sq = self
                .start_sq(
                    saved.qid,
                    saved.base,
                    saved.entries,
                    cq,
                    saved.head,
                    saved.tail,
                )
                .map_err(|e| bad(&format!("can't start a queue thread: {e}")))?;
            let _ = sq.messages.send(Message::Notify);
            self.sqs[saved.qid] = Some(sq);
        }
        self.pending_events = state.pending_events;
        self.features = state.features;
        self.shared
            .write_cache
            .store(state.write_cache, Ordering::Relaxed);
        let [read, written, reads, writes] = state.counters;
        self.shared.units_read.store(read, Ordering::Relaxed);
        self.shared.units_written.store(written, Ordering::Relaxed);
        self.shared.read_commands.store(reads, Ordering::Relaxed);
        self.shared.write_commands.store(writes, Ordering::Relaxed);
        Ok(())
    }

    fn quiesce(&mut self) {
        for (qid, sq) in self.sqs.iter().enumerate() {
            let Some(sq) = sq else { continue };
            let (done, wait) = mpsc::channel();
            if sq.messages.send(Message::Sync(done)).is_ok()
                && wait.recv_timeout(QUIESCE_TIMEOUT).is_err()
            {
                tracing::warn!(qid, "NVMe: a queue is waiting for completion queue room");
            }
        }
    }
}

impl Drop for Nvme {
    fn drop(&mut self) {
        // Stop the queue workers before the memory they use goes away.
        for sq in &mut self.sqs {
            *sq = None;
        }
    }
}

impl PciDevice for Nvme {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        let control = self.msix_capability + 2;
        let end = offset + data.len() as u16;
        if offset < control + 2 && end > control {
            let (enabled, masked) = msix::control(&self.config, self.msix_capability);
            lock(&self.shared.msix).set_control(enabled, masked);
        }
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar != NVME_BAR {
            data.fill(0xff);
            return;
        }
        match offset {
            ..DOORBELLS => self.read_register(offset, data),
            o if o >= u64::from(MSIX_PBA) => {
                lock(&self.shared.msix).read_pba(o - u64::from(MSIX_PBA), data);
            }
            o if o >= u64::from(MSIX_TABLE) => {
                lock(&self.shared.msix).read_table(o - u64::from(MSIX_TABLE), data);
            }
            // Doorbells read as zero.
            _ => data.fill(0),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar != NVME_BAR {
            return;
        }
        match offset {
            ..DOORBELLS => self.write_register(offset, data),
            o if o >= u64::from(MSIX_PBA) => {}
            o if o >= u64::from(MSIX_TABLE) => {
                lock(&self.shared.msix).write_table(o - u64::from(MSIX_TABLE), data);
            }
            o if data.len() == 4 && o % 4 == 0 => {
                let value = u32::from_le_bytes(data.try_into().unwrap());
                self.doorbell(o - DOORBELLS, value);
            }
            o => tracing::debug!(o, len = data.len(), "NVMe: odd-sized doorbell write"),
        }
    }
}

#[cfg(test)]
mod tests;
