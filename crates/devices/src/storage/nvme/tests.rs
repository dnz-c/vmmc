//! NVMe controller tests, driven the way a guest driver does: queues in guest memory,
//! commands through doorbells, completions found by their phase bit.

use std::time::{Duration, Instant};

use disk::MemDisk;

use super::*;
use crate::irq::testing::RecordingSink;

const MEMORY: usize = 4 << 20;
const ASQ_ADDR: u64 = 0x1_0000;
const ACQ_ADDR: u64 = 0x2_0000;
const IOSQ_ADDR: u64 = 0x3_0000;
const IOCQ_ADDR: u64 = 0x4_0000;
const PRP_LIST: u64 = 0x8_0000;
const DATA: u64 = 0x10_0000;
const READ_BACK: u64 = 0x20_0000;
const ADMIN_ENTRIES: u32 = 32;
const IO_ENTRIES: u32 = 64;
const IO_VECTOR: u16 = 1;

// Admin opcodes.
const DELETE_SQ: u8 = 0x00;
const CREATE_SQ: u8 = 0x01;
const GET_LOG_PAGE: u8 = 0x02;
const DELETE_CQ: u8 = 0x04;
const CREATE_CQ: u8 = 0x05;
const IDENTIFY: u8 = 0x06;
const SET_FEATURES: u8 = 0x09;
const GET_FEATURES: u8 = 0x0a;
const ASYNC_EVENT_REQUEST: u8 = 0x0c;
// I/O opcodes.
const FLUSH: u8 = 0x00;
const WRITE: u8 = 0x01;
const READ: u8 = 0x02;
const WRITE_ZEROES: u8 = 0x08;
const DATASET_MANAGEMENT: u8 = 0x09;

/// A submission queue entry, built field by field.
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw: [u32; 6],
}

impl Sqe {
    fn new(opcode: u8) -> Self {
        Self {
            opcode,
            ..Self::default()
        }
    }

    fn nsid(mut self, nsid: u32) -> Self {
        self.nsid = nsid;
        self
    }

    fn prp(mut self, prp1: u64, prp2: u64) -> Self {
        self.prp1 = prp1;
        self.prp2 = prp2;
        self
    }

    /// Sets CDW10 onwards.
    fn cdw(mut self, values: &[u32]) -> Self {
        self.cdw[..values.len()].copy_from_slice(values);
        self
    }

    fn bytes(&self, cid: u16) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[0] = self.opcode;
        bytes[2..4].copy_from_slice(&cid.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.nsid.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.prp1.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.prp2.to_le_bytes());
        for (i, value) in self.cdw.iter().enumerate() {
            bytes[40 + 4 * i..44 + 4 * i].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// One queue pair as the driver tracks it.
struct Pair {
    qid: u16,
    sq: u64,
    cq: u64,
    entries: u32,
    tail: u32,
    head: u32,
    phase: bool,
}

struct Driver {
    nvme: Nvme,
    memory: GuestMemoryMmap,
    disks: Vec<Arc<MemDisk>>,
    sink: Arc<RecordingSink>,
    admin: Pair,
    io: Pair,
    next_cid: u16,
}

/// What came back: status and DW0.
#[derive(Debug, PartialEq)]
struct Completion {
    status: Status,
    dw0: u32,
    cid: u16,
}

impl Driver {
    fn new(disks: &[(usize, bool)]) -> Self {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEMORY)]).unwrap();
        let disks: Vec<Arc<MemDisk>> = disks
            .iter()
            .map(|&(len, read_only)| Arc::new(MemDisk::new(len, read_only)))
            .collect();
        let namespaces = disks
            .iter()
            .enumerate()
            .map(|(i, disk)| Namespace {
                backend: disk.clone() as Arc<dyn BlockBackend>,
                nguid: [i as u8 + 1; 16],
            })
            .collect();
        let sink = Arc::new(RecordingSink::default());
        let nvme = Nvme::new(namespaces, "vmmc-test", memory.clone(), sink.clone());
        let mut driver = Self {
            nvme,
            memory,
            disks,
            sink,
            admin: Pair {
                qid: 0,
                sq: ASQ_ADDR,
                cq: ACQ_ADDR,
                entries: ADMIN_ENTRIES,
                tail: 0,
                head: 0,
                phase: true,
            },
            io: Pair {
                qid: 1,
                sq: IOSQ_ADDR,
                cq: IOCQ_ADDR,
                entries: IO_ENTRIES,
                tail: 0,
                head: 0,
                phase: true,
            },
            next_cid: 1,
        };
        driver.enable();
        driver
    }

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.nvme.bar_read(NVME_BAR, offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.nvme.bar_write(NVME_BAR, offset, &value.to_le_bytes());
    }

    fn write64(&mut self, offset: u64, value: u64) {
        self.nvme.bar_write(NVME_BAR, offset, &value.to_le_bytes());
    }

    fn enable(&mut self) {
        self.write32(AQA, (ADMIN_ENTRIES - 1) | ((ADMIN_ENTRIES - 1) << 16));
        self.write64(ASQ, ASQ_ADDR);
        self.write64(ACQ, ACQ_ADDR);
        // EN, 64-byte SQ entries, 16-byte CQ entries.
        self.write32(CC, 1 | (6 << 16) | (4 << 20));
        assert_eq!(self.read32(CSTS) & CSTS_RDY, CSTS_RDY);
        self.admin.tail = 0;
        self.admin.head = 0;
        self.admin.phase = true;
    }

    fn submit(&mut self, io: bool, sqe: &Sqe) -> u16 {
        let cid = self.next_cid;
        self.next_cid += 1;
        let pair = if io { &mut self.io } else { &mut self.admin };
        let address = GuestAddress(pair.sq + u64::from(pair.tail) * 64);
        self.memory.write_slice(&sqe.bytes(cid), address).unwrap();
        pair.tail = (pair.tail + 1) % pair.entries;
        let (qid, tail) = (pair.qid, pair.tail);
        self.write32(DOORBELLS + 8 * u64::from(qid), tail);
        cid
    }

    /// The next completion on `pair`, if its phase bit says it's there.
    fn poll(&mut self, io: bool) -> Option<Completion> {
        let pair = if io { &mut self.io } else { &mut self.admin };
        let address = GuestAddress(pair.cq + u64::from(pair.head) * 16);
        let mut raw = [0u8; 16];
        self.memory.read_slice(&mut raw, address).unwrap();
        let dw3 = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        if (dw3 >> 16) & 1 != u32::from(pair.phase) {
            return None;
        }
        pair.head = (pair.head + 1) % pair.entries;
        if pair.head == 0 {
            pair.phase = !pair.phase;
        }
        let (qid, head) = (pair.qid, pair.head);
        self.write32(DOORBELLS + 8 * u64::from(qid) + 4, head);
        Some(Completion {
            status: Status((dw3 >> 17) as u16 & 0x7ff),
            dw0: u32::from_le_bytes(raw[0..4].try_into().unwrap()),
            cid: dw3 as u16,
        })
    }

    fn admin(&mut self, sqe: Sqe) -> Completion {
        let cid = self.submit(false, &sqe);
        let completion = self.poll(false).expect("admin commands complete at once");
        assert_eq!(completion.cid, cid);
        completion
    }

    fn io(&mut self, sqe: Sqe) -> Completion {
        let cid = self.submit(true, &sqe);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(completion) = self.poll(true) {
                assert_eq!(completion.cid, cid);
                return completion;
            }
            assert!(Instant::now() < deadline, "I/O command never completed");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Turns on MSI-X with every vector unmasked, vector N sending 0x40 + N.
    fn enable_msix(&mut self) {
        for vector in 0..=MAX_IO_QUEUES {
            let entry = u64::from(MSIX_TABLE) + u64::from(vector) * 16;
            self.write32(entry, 0xfee0_0000);
            self.write32(entry + 4, 0);
            self.write32(entry + 8, 0x40 + u32::from(vector));
            self.write32(entry + 12, 0);
        }
        let control = self.nvme.msix_capability + 2;
        self.nvme.config_write(control, &0x8000u16.to_le_bytes());
    }

    fn create_io_queues(&mut self) {
        let entries = IO_ENTRIES - 1;
        let cq = Sqe::new(CREATE_CQ)
            .prp(IOCQ_ADDR, 0)
            .cdw(&[(entries << 16) | 1, (u32::from(IO_VECTOR) << 16) | 0b11]);
        assert_eq!(self.admin(cq).status, Status::SUCCESS);
        let sq = Sqe::new(CREATE_SQ)
            .prp(IOSQ_ADDR, 0)
            .cdw(&[(entries << 16) | 1, (1 << 16) | 1]);
        assert_eq!(self.admin(sq).status, Status::SUCCESS);
    }

    fn identify(&mut self, cns: u32, nsid: u32) -> (Status, Vec<u8>) {
        let completion = self.admin(Sqe::new(IDENTIFY).nsid(nsid).prp(DATA, 0).cdw(&[cns]));
        let mut data = vec![0u8; 4096];
        self.memory
            .read_slice(&mut data, GuestAddress(DATA))
            .unwrap();
        (completion.status, data)
    }

    fn fill(&self, address: u64, data: &[u8]) {
        self.memory
            .write_slice(data, GuestAddress(address))
            .unwrap();
    }

    fn bytes(&self, address: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        self.memory
            .read_slice(&mut data, GuestAddress(address))
            .unwrap();
        data
    }
}

fn rw(opcode: u8, nsid: u32, slba: u64, blocks: u32, prp1: u64, prp2: u64) -> Sqe {
    Sqe::new(opcode)
        .nsid(nsid)
        .prp(prp1, prp2)
        .cdw(&[slba as u32, (slba >> 32) as u32, blocks - 1])
}

#[test]
fn registers_describe_the_controller_and_follow_enable_and_shutdown() {
    let mut d = Driver::new(&[(1 << 20, false)]);
    let cap = u64::from(d.read32(CAP)) | (u64::from(d.read32(CAP + 4)) << 32);
    assert_eq!(cap & 0xffff, u64::from(MAX_QUEUE_ENTRIES - 1), "MQES");
    assert_ne!(cap & (1 << 16), 0, "contiguous queues required");
    assert_ne!(cap & (1 << 37), 0, "NVM command set");
    assert_eq!((cap >> 48) & 0xf, 0, "4 KiB minimum page size");
    assert_eq!(d.read32(VS), 0x0001_0400);

    // Shutdown notification, then disable and re-enable.
    let cc = d.read32(CC);
    d.write32(CC, cc | (1 << CC_SHN_SHIFT));
    assert_eq!((d.read32(CSTS) >> 2) & 0b11, 0b10, "shutdown complete");
    d.write32(CC, 0);
    assert_eq!(d.read32(CSTS) & CSTS_RDY, 0);
    assert_eq!(
        d.read32(AQA),
        (ADMIN_ENTRIES - 1) * 0x1_0001,
        "admin queue kept"
    );
    d.enable();
    let (status, _) = d.identify(1, 0);
    assert_eq!(status, Status::SUCCESS);
}

#[test]
fn identify_reports_the_controller_and_each_namespace() {
    let mut d = Driver::new(&[(1 << 20, false), (2 << 20, true)]);
    let (status, controller) = d.identify(1, 0);
    assert_eq!(status, Status::SUCCESS);
    assert_eq!(
        u16::from_le_bytes([controller[0], controller[1]]),
        VENDOR_ID
    );
    assert_eq!(&controller[4..13], b"vmmc-test");
    assert_eq!(&controller[24..38], MODEL.as_bytes());
    assert_eq!(controller[77], MDTS);
    assert_eq!((controller[512], controller[513]), (0x66, 0x44));
    assert_eq!(
        u32::from_le_bytes(controller[516..520].try_into().unwrap()),
        2,
        "NN"
    );
    assert!(String::from_utf8_lossy(&controller[768..1024]).starts_with("nqn."));

    let (_, namespace) = d.identify(0, 2);
    let lbas = u64::from_le_bytes(namespace[0..8].try_into().unwrap());
    assert_eq!(lbas, (2 << 20) / LBA_SIZE);
    assert_eq!((namespace[130..132]), [9, 0], "512-byte LBAs");
    assert_eq!(namespace[104..120], [2; 16], "NGUID");

    let (_, list) = d.identify(2, 0);
    assert_eq!(list[0..12], [1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    let (_, list) = d.identify(2, 1);
    assert_eq!(
        list[0..8],
        [2, 0, 0, 0, 0, 0, 0, 0],
        "only IDs above the one given"
    );

    let (_, descriptors) = d.identify(3, 1);
    assert_eq!(descriptors[0..2], [0x02, 16]);
    assert_eq!(descriptors[4..20], [1; 16]);

    assert_eq!(d.identify(0, 3).0, Status::INVALID_NAMESPACE);
    assert_eq!(d.identify(0x55, 0).0, Status::INVALID_FIELD);
}

#[test]
fn reads_and_writes_go_through_prp_lists_and_interrupt() {
    let mut d = Driver::new(&[(1 << 20, false)]);
    d.enable_msix();
    d.create_io_queues();
    d.sink.take();

    // 16 KiB starting 512 bytes into a page: a partial first page, then a PRP list.
    let pattern: Vec<u8> = (0..16 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let first = DATA + 0x200;
    let pages: Vec<u64> = (1..=4).map(|i| DATA + i * PAGE_SIZE).collect();
    let mut offset = 0;
    let first_len = (PAGE_SIZE - 0x200) as usize;
    d.fill(first, &pattern[..first_len]);
    offset += first_len;
    for &page in &pages {
        let len = (pattern.len() - offset).min(PAGE_SIZE as usize);
        d.fill(page, &pattern[offset..offset + len]);
        offset += len;
    }
    let list: Vec<u8> = pages.iter().flat_map(|page| page.to_le_bytes()).collect();
    d.fill(PRP_LIST, &list);
    let completion = d.io(rw(WRITE, 1, 8, 32, first, PRP_LIST));
    assert_eq!(completion.status, Status::SUCCESS);
    assert_eq!(d.disks[0].bytes(8 * 512, pattern.len()), pattern);
    let interrupts = d.sink.take();
    assert!(
        interrupts
            .iter()
            .any(|m| m.vector() == 0x40 + IO_VECTOR as u8),
        "{interrupts:?}"
    );

    // Read it back into one contiguous buffer: four whole pages through a list.
    let pages: Vec<u64> = (1..4).map(|i| READ_BACK + i * PAGE_SIZE).collect();
    let list: Vec<u8> = pages.iter().flat_map(|page| page.to_le_bytes()).collect();
    d.fill(PRP_LIST, &list);
    let completion = d.io(rw(READ, 1, 8, 32, READ_BACK, PRP_LIST));
    assert_eq!(completion.status, Status::SUCCESS);
    assert_eq!(d.bytes(READ_BACK, pattern.len()), pattern);

    // Exactly two pages use PRP2 as the second page itself.
    let completion = d.io(rw(READ, 1, 0, 16, DATA, DATA + PAGE_SIZE));
    assert_eq!(completion.status, Status::SUCCESS);
}

#[test]
fn bad_io_commands_fail_with_the_right_status() {
    let mut d = Driver::new(&[(64 * 1024, false), (64 * 1024, true)]);
    d.create_io_queues();
    let lbas = (64 * 1024 / LBA_SIZE) as u32;
    assert_eq!(
        d.io(rw(READ, 1, u64::from(lbas) - 1, 2, DATA, DATA + PAGE_SIZE))
            .status,
        Status::LBA_OUT_OF_RANGE
    );
    assert_eq!(
        d.io(rw(READ, 1, u64::MAX, 1, DATA, 0)).status,
        Status::LBA_OUT_OF_RANGE,
        "no overflow"
    );
    assert_eq!(
        d.io(rw(READ, 3, 0, 1, DATA, 0)).status,
        Status::INVALID_NAMESPACE
    );
    assert_eq!(
        d.io(rw(WRITE, 2, 0, 1, DATA, 0)).status,
        Status::WRITE_PROTECTED
    );
    assert_eq!(d.io(Sqe::new(0x7f).nsid(1)).status, Status::INVALID_OPCODE);
    // More than MDTS allows.
    assert_eq!(
        d.io(rw(
            READ,
            1,
            0,
            (MAX_TRANSFER / LBA_SIZE) as u32 + 1,
            DATA,
            PRP_LIST
        ))
        .status,
        Status::INVALID_FIELD
    );
    // A second page that isn't page-aligned.
    assert_eq!(
        d.io(rw(READ, 1, 0, 16, DATA, DATA + 0x10)).status,
        Status::PRP_OFFSET_INVALID
    );
}

#[test]
fn flush_write_zeroes_and_deallocate() {
    let mut d = Driver::new(&[(64 * 1024, false)]);
    d.create_io_queues();
    d.fill(DATA, &[0xaa; 4096]);
    assert_eq!(d.io(rw(WRITE, 1, 0, 8, DATA, 0)).status, Status::SUCCESS);

    let flushes = d.disks[0].flushes();
    assert_eq!(d.io(Sqe::new(FLUSH).nsid(1)).status, Status::SUCCESS);
    assert_eq!(d.io(Sqe::new(FLUSH).nsid(u32::MAX)).status, Status::SUCCESS);
    assert_eq!(d.disks[0].flushes(), flushes + 2);

    let zeroes = Sqe::new(WRITE_ZEROES).nsid(1).cdw(&[2, 0, 3]);
    assert_eq!(d.io(zeroes).status, Status::SUCCESS);
    assert_eq!(
        d.disks[0].bytes(512, 1536),
        [[0xaa; 512], [0; 512], [0; 512]].concat()
    );

    // One range: 4 blocks at LBA 0, then one past the end.
    let mut range = [0u8; 16];
    range[4..8].copy_from_slice(&4u32.to_le_bytes());
    d.fill(PRP_LIST, &range);
    let deallocate = Sqe::new(DATASET_MANAGEMENT)
        .nsid(1)
        .prp(PRP_LIST, 0)
        .cdw(&[0, 1 << 2]);
    assert_eq!(d.io(deallocate).status, Status::SUCCESS);
    range[8..16].copy_from_slice(&127u64.to_le_bytes());
    d.fill(PRP_LIST, &range);
    assert_eq!(d.io(deallocate).status, Status::LBA_OUT_OF_RANGE);
}

#[test]
fn queue_creation_and_deletion_are_checked() {
    let mut d = Driver::new(&[(64 * 1024, false)]);
    let sq_without_cq = Sqe::new(CREATE_SQ)
        .prp(IOSQ_ADDR, 0)
        .cdw(&[(63 << 16) | 1, (1 << 16) | 1]);
    assert_eq!(
        d.admin(sq_without_cq).status,
        Status::INVALID_COMPLETION_QUEUE
    );
    let too_big = Sqe::new(CREATE_CQ)
        .prp(IOCQ_ADDR, 0)
        .cdw(&[(4096 << 16) | 1, 1]);
    assert_eq!(d.admin(too_big).status, Status::INVALID_QUEUE_SIZE);
    let bad_vector = Sqe::new(CREATE_CQ)
        .prp(IOCQ_ADDR, 0)
        .cdw(&[(63 << 16) | 1, (99 << 16) | 3]);
    assert_eq!(d.admin(bad_vector).status, Status::INVALID_VECTOR);
    let qid_zero = Sqe::new(CREATE_CQ).prp(IOCQ_ADDR, 0).cdw(&[63 << 16, 1]);
    assert_eq!(d.admin(qid_zero).status, Status::INVALID_QUEUE_ID);

    d.create_io_queues();
    let again = Sqe::new(CREATE_CQ)
        .prp(IOCQ_ADDR, 0)
        .cdw(&[(63 << 16) | 1, 1]);
    assert_eq!(d.admin(again).status, Status::INVALID_QUEUE_ID);
    let delete_cq = Sqe::new(DELETE_CQ).cdw(&[1]);
    assert_eq!(d.admin(delete_cq).status, Status::INVALID_QUEUE_DELETION);
    assert_eq!(
        d.admin(Sqe::new(DELETE_SQ).cdw(&[1])).status,
        Status::SUCCESS
    );
    assert_eq!(d.admin(delete_cq).status, Status::SUCCESS);
    assert_eq!(
        d.admin(Sqe::new(DELETE_SQ).cdw(&[1])).status,
        Status::INVALID_QUEUE_ID
    );
}

#[test]
fn features_and_log_pages() {
    let mut d = Driver::new(&[(64 * 1024, false)]);
    // Number of queues: ask for 64 pairs, get what there is.
    let queues = d.admin(Sqe::new(SET_FEATURES).cdw(&[0x07, (63 << 16) | 63]));
    let most = u32::from(MAX_IO_QUEUES - 1);
    assert_eq!(queues.dw0, (most << 16) | most);
    assert_eq!(
        d.admin(Sqe::new(GET_FEATURES).cdw(&[0x06])).dw0,
        1,
        "write cache on"
    );
    d.admin(Sqe::new(SET_FEATURES).cdw(&[0x06, 0]));
    assert_eq!(d.admin(Sqe::new(GET_FEATURES).cdw(&[0x06])).dw0, 0);
    assert_eq!(
        d.admin(Sqe::new(SET_FEATURES).cdw(&[0x8000_0004, 0x157]))
            .status,
        Status::FEATURE_NOT_SAVEABLE
    );
    d.admin(Sqe::new(SET_FEATURES).cdw(&[0x04, 0x157]));
    assert_eq!(d.admin(Sqe::new(GET_FEATURES).cdw(&[0x04])).dw0, 0x157);
    assert_eq!(
        d.admin(Sqe::new(GET_FEATURES).cdw(&[0x0c])).status,
        Status::INVALID_FIELD
    );

    // SMART log: 512 bytes (128 dwords).
    let smart = Sqe::new(GET_LOG_PAGE)
        .prp(DATA, 0)
        .cdw(&[0x02 | (127 << 16)]);
    assert_eq!(d.admin(smart).status, Status::SUCCESS);
    let page = d.bytes(DATA, 512);
    assert_eq!(u16::from_le_bytes([page[1], page[2]]), 315, "temperature");
    assert_eq!(page[3], 100, "spare");
    let firmware = Sqe::new(GET_LOG_PAGE)
        .prp(DATA, 0)
        .cdw(&[0x03 | (127 << 16)]);
    assert_eq!(d.admin(firmware).status, Status::SUCCESS);
    assert_eq!(&d.bytes(DATA + 8, 3), FIRMWARE.as_bytes());
    let unknown = Sqe::new(GET_LOG_PAGE)
        .prp(DATA, 0)
        .cdw(&[0x70 | (127 << 16)]);
    assert_eq!(d.admin(unknown).status, Status::INVALID_LOG_PAGE);
}

#[test]
fn async_event_requests_stay_outstanding_without_blocking_the_queue() {
    let mut d = Driver::new(&[(64 * 1024, false)]);
    for _ in 0..AER_LIMIT {
        d.submit(false, &Sqe::new(ASYNC_EVENT_REQUEST));
        assert!(d.poll(false).is_none());
    }
    assert_eq!(
        d.admin(Sqe::new(ASYNC_EVENT_REQUEST)).status,
        Status::AER_LIMIT_EXCEEDED
    );
    assert_eq!(d.identify(1, 0).0, Status::SUCCESS);
}

#[test]
fn disabling_the_controller_stops_io_queues() {
    let mut d = Driver::new(&[(64 * 1024, false)]);
    d.create_io_queues();
    d.fill(DATA, &[1; 512]);
    assert_eq!(d.io(rw(WRITE, 1, 0, 1, DATA, 0)).status, Status::SUCCESS);
    d.write32(CC, 0);
    assert!(d.nvme.sqs.iter().all(Option::is_none));
    d.enable();
    d.io.tail = 0;
    d.io.head = 0;
    d.io.phase = true;
    d.memory
        .write_slice(&[0u8; 64 * 16], GuestAddress(IOCQ_ADDR))
        .unwrap();
    d.create_io_queues();
    assert_eq!(
        d.io(rw(READ, 1, 0, 1, READ_BACK, 0)).status,
        Status::SUCCESS
    );
    assert_eq!(d.bytes(READ_BACK, 512), [1; 512]);
}

#[test]
fn prp_lists_chain_across_pages_and_reject_loops() {
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEMORY)]).unwrap();
    // A list whose page has room for just one entry: it must chain straight away.
    let list = PRP_LIST + PAGE_SIZE - 8;
    let next = PRP_LIST + 2 * PAGE_SIZE;
    memory
        .write_slice(&next.to_le_bytes(), GuestAddress(list))
        .unwrap();
    let pages: Vec<u8> = [DATA + PAGE_SIZE, DATA + 2 * PAGE_SIZE]
        .iter()
        .flat_map(|page| page.to_le_bytes())
        .collect();
    memory.write_slice(&pages, GuestAddress(next)).unwrap();
    let segments = prp_segments(&memory, DATA, list, 3 * PAGE_SIZE).unwrap();
    assert_eq!(
        segments,
        [
            (DATA, PAGE_SIZE),
            (DATA + PAGE_SIZE, PAGE_SIZE),
            (DATA + 2 * PAGE_SIZE, PAGE_SIZE)
        ]
    );

    // A list that chains to itself forever.
    memory
        .write_slice(&list.to_le_bytes(), GuestAddress(list))
        .unwrap();
    assert_eq!(
        prp_segments(&memory, DATA, list, 3 * PAGE_SIZE),
        Err(Status::PRP_OFFSET_INVALID)
    );
}
