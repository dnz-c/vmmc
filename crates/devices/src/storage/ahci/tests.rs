//! AHCI tests, driven the way a guest driver does: command lists and tables in guest
//! memory, PxCI to issue, completion seen through PxCI, PxIS and the received FISes.

use std::time::{Duration, Instant};

use disk::{BlockBackend, MemDisk};

use super::*;
use crate::irq::testing::RecordingSink;
use crate::storage::atapi::{self, SECTOR_SIZE as CD_SECTOR};

const MEMORY: usize = 4 << 20;
const DATA: u64 = 0x10_0000;
const READ_BACK: u64 = 0x20_0000;
const DISK: usize = 0;
const CD: usize = 1;
const EMPTY: usize = 2;

fn clb(port: usize) -> u64 {
    0x1_0000 + port as u64 * 0x1000
}

fn fb(port: usize) -> u64 {
    0x2_0000 + port as u64 * 0x100
}

fn table(port: usize, slot: u32) -> u64 {
    0x3_0000 + port as u64 * 0x8000 + u64::from(slot) * 0x400
}

struct Driver {
    ahci: Ahci,
    memory: GuestMemoryMmap,
    disk: Arc<MemDisk>,
    sink: Arc<RecordingSink>,
}

/// What a command left behind.
#[derive(Debug)]
struct Result {
    tfd: u32,
    is: u32,
    ci: u32,
    prdbc: u32,
}

impl Result {
    fn ok(&self) -> bool {
        self.tfd & u32::from(STATUS_ERR) == 0 && self.is & IS_TFES == 0
    }
}

impl Driver {
    fn new() -> Self {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEMORY)]).unwrap();
        let disk = Arc::new(MemDisk::new(1 << 20, false));
        let iso = MemDisk::new(64 * CD_SECTOR as usize, true);
        iso.write_at(b"CD001", 16 * CD_SECTOR + 1).unwrap();
        let drives = vec![
            Drive::Disk(AtaDisk {
                backend: disk.clone(),
                serial: "VMMCDISK".into(),
            }),
            Drive::Cdrom(atapi::Cdrom::new(Some(Arc::new(iso)), "VMMCCD")),
        ];
        let sink = Arc::new(RecordingSink::default());
        let ahci = Ahci::new(drives, memory.clone(), sink.clone(), None).unwrap();
        Self {
            ahci,
            memory,
            disk,
            sink,
        }
    }

    fn read(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.ahci.bar_read(ABAR, offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write(&mut self, offset: u64, value: u32) {
        self.ahci.bar_write(ABAR, offset, &value.to_le_bytes());
    }

    fn port_read(&mut self, port: usize, register: u64) -> u32 {
        self.read(PORT_BASE + port as u64 * PORT_SIZE + register)
    }

    fn port_write(&mut self, port: usize, register: u64, value: u32) {
        self.write(PORT_BASE + port as u64 * PORT_SIZE + register, value);
    }

    fn start(&mut self, port: usize) {
        self.port_write(port, PX_CLB, clb(port) as u32);
        self.port_write(port, PX_FB, fb(port) as u32);
        self.port_write(port, PX_CMD, CMD_FRE);
        self.port_write(port, PX_CMD, CMD_FRE | CMD_ST);
    }

    /// Issues one command in `slot` and waits for it to finish (or fail).
    fn issue(
        &mut self,
        port: usize,
        slot: u32,
        cfis: &[u8; 20],
        cdb: Option<&[u8]>,
        prdt: &[(u64, u32)],
        write: bool,
    ) -> Result {
        let table = table(port, slot);
        self.memory.write_slice(cfis, GuestAddress(table)).unwrap();
        if let Some(cdb) = cdb {
            let mut padded = [0u8; 16];
            padded[..cdb.len()].copy_from_slice(cdb);
            self.memory
                .write_slice(&padded, GuestAddress(table + 0x40))
                .unwrap();
        }
        for (index, &(address, len)) in prdt.iter().enumerate() {
            let mut entry = [0u8; 16];
            entry[0..8].copy_from_slice(&address.to_le_bytes());
            entry[12..16].copy_from_slice(&((len - 1) | (1 << 31)).to_le_bytes());
            self.memory
                .write_slice(&entry, GuestAddress(table + 0x80 + 16 * index as u64))
                .unwrap();
        }
        let flags: u32 = 5 // CFIS length in dwords
            | (u32::from(cdb.is_some()) << 5)
            | (u32::from(write) << 6)
            | ((prdt.len() as u32) << 16);
        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&table.to_le_bytes());
        let header_address = clb(port) + u64::from(slot) * 32;
        self.memory
            .write_slice(&header, GuestAddress(header_address))
            .unwrap();

        self.port_write(port, PX_CI, 1 << slot);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ci = self.port_read(port, PX_CI);
            let is = self.port_read(port, PX_IS);
            if ci & (1 << slot) == 0 || is & IS_TFES != 0 {
                let mut prdbc = [0u8; 4];
                self.memory
                    .read_slice(&mut prdbc, GuestAddress(header_address + 4))
                    .unwrap();
                let result = Result {
                    tfd: self.port_read(port, PX_TFD),
                    is,
                    ci,
                    prdbc: u32::from_le_bytes(prdbc),
                };
                self.port_write(port, PX_IS, is);
                return result;
            }
            assert!(Instant::now() < deadline, "command never finished");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn bytes(&self, address: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        self.memory
            .read_slice(&mut data, GuestAddress(address))
            .unwrap();
        data
    }

    fn restart(&mut self, port: usize) {
        self.port_write(port, PX_CMD, CMD_FRE);
        self.port_write(port, PX_CMD, CMD_FRE | CMD_ST);
    }
}

/// A host-to-device register FIS.
fn h2d(command: u8, lba: u64, count: u16, features: u16) -> [u8; 20] {
    let mut fis = [0u8; 20];
    fis[0] = 0x27;
    fis[1] = 0x80;
    fis[2] = command;
    fis[3] = features as u8;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    fis[7] = 0x40; // LBA mode
    fis[8] = (lba >> 24) as u8;
    fis[9] = (lba >> 32) as u8;
    fis[10] = (lba >> 40) as u8;
    fis[11] = (features >> 8) as u8;
    fis[12..14].copy_from_slice(&count.to_le_bytes());
    fis
}

#[test]
fn reports_capabilities_ports_and_signatures() {
    let mut d = Driver::new();
    let cap = d.read(CAP);
    assert_eq!(cap & 0x1f, PORTS as u32 - 1, "ports");
    assert_eq!((cap >> 8) & 0x1f, 31, "32 command slots");
    assert_ne!(cap & (1 << 31), 0, "64-bit addressing");
    assert_eq!(d.read(PI), 0x3f);
    assert_eq!(d.read(VS), 0x0001_0301);
    assert_ne!(d.read(GHC) & GHC_AE, 0, "AHCI mode");
    assert_eq!(d.port_read(DISK, PX_SIG), SIGNATURE_ATA);
    assert_eq!(d.port_read(CD, PX_SIG), SIGNATURE_ATAPI);
    assert_eq!(d.port_read(DISK, PX_SSTS) & 0xf, 3, "device present");
    assert_eq!(d.port_read(EMPTY, PX_SSTS), 0, "nothing on port 2");
}

#[test]
fn disk_identify_write_and_read_back() {
    let mut d = Driver::new();
    d.start(DISK);
    // The signature FIS arrived when FIS receive came on.
    assert_eq!(d.bytes(fb(DISK) + RFIS_D2H, 1), [0x34]);

    let result = d.issue(DISK, 0, &h2d(0xec, 0, 0, 0), None, &[(DATA, 512)], false);
    assert!(result.ok(), "{result:?}");
    assert_ne!(result.is & IS_PSS, 0, "PIO setup FIS for IDENTIFY");
    assert_eq!(result.prdbc, 512);
    let identify = d.bytes(DATA, 512);
    assert_eq!(&identify[54..58], b"mvcm", "model, byte-swapped");

    // Write 16 sectors from two PRD entries, then read them back through one.
    let pattern: Vec<u8> = (0..8192).map(|i| (i % 253) as u8).collect();
    d.memory.write_slice(&pattern, GuestAddress(DATA)).unwrap();
    let prdt = [(DATA, 4096), (DATA + 4096, 4096)];
    let result = d.issue(DISK, 3, &h2d(0x35, 100, 16, 0), None, &prdt, true);
    assert!(result.ok(), "{result:?}");
    assert_eq!(result.prdbc, 8192);
    let mut on_disk = vec![0u8; 8192];
    d.disk.read_at(&mut on_disk, 100 * 512).unwrap();
    assert_eq!(on_disk, pattern);

    let result = d.issue(
        DISK,
        1,
        &h2d(0x25, 100, 16, 0),
        None,
        &[(READ_BACK, 8192)],
        false,
    );
    assert!(result.ok(), "{result:?}");
    assert_ne!(result.is & IS_DPS, 0, "the PRD asked for an interrupt");
    assert_eq!(d.bytes(READ_BACK, 8192), pattern);
    assert_eq!(d.bytes(fb(DISK) + RFIS_D2H, 3), [0x34, 0x40, 0x50]);

    let flush = d.issue(DISK, 2, &h2d(0xea, 0, 0, 0), None, &[], false);
    assert!(flush.ok());
    assert_eq!(d.disk.flushes(), 1);
}

#[test]
fn atapi_drive_answers_packets_and_refuses_ata_identify() {
    let mut d = Driver::new();
    d.start(CD);
    let refused = d.issue(CD, 0, &h2d(0xec, 0, 0, 0), None, &[(DATA, 512)], false);
    assert_ne!(refused.is & IS_TFES, 0);
    let fis = d.bytes(fb(CD) + RFIS_D2H, 20);
    assert_eq!((fis[5], fis[6]), (0x14, 0xeb), "ATAPI signature in the FIS");
    d.restart(CD);

    let identify = d.issue(CD, 0, &h2d(0xa1, 0, 0, 0), None, &[(DATA, 512)], false);
    assert!(identify.ok(), "{identify:?}");
    assert_eq!(d.bytes(DATA + 1, 1), [0x85], "ATAPI device");

    let inquiry = d.issue(
        CD,
        1,
        &h2d(0xa0, 0, 0, 0),
        Some(&[0x12, 0, 0, 0, 36]),
        &[(DATA, 512)],
        false,
    );
    assert!(inquiry.ok(), "{inquiry:?}");
    assert_eq!(inquiry.prdbc, 36);
    assert_eq!(d.bytes(DATA, 1), [0x05]);

    // READ(10) of the volume descriptor sector, by DMA.
    let read = d.issue(
        CD,
        2,
        &h2d(0xa0, 0, 0, 1),
        Some(&[0x28, 0, 0, 0, 0, 16, 0, 0, 1]),
        &[(READ_BACK, 2048)],
        false,
    );
    assert!(read.ok(), "{read:?}");
    assert_eq!(read.is & IS_PSS, 0, "DMA, no PIO setup");
    assert_eq!(d.bytes(READ_BACK + 1, 5), b"CD001");
}

#[test]
fn errors_halt_the_port_until_the_guest_restarts_it() {
    let mut d = Driver::new();
    d.start(CD);
    let bad = d.issue(
        CD,
        4,
        &h2d(0xa0, 0, 0, 0),
        Some(&[0xff]),
        &[(DATA, 512)],
        false,
    );
    assert_ne!(bad.is & IS_TFES, 0);
    assert_ne!(bad.ci & (1 << 4), 0, "the failed command stays issued");
    assert_eq!(bad.tfd & 0xff, 0x51, "DRDY, DSC, ERR");
    assert_eq!(
        (bad.tfd >> 12) & 0xf,
        u32::from(atapi::ILLEGAL_REQUEST),
        "sense key"
    );

    // Nothing more runs until the port is restarted, which drops the failed command.
    d.port_write(CD, PX_CI, 1 << 5);
    thread::sleep(Duration::from_millis(20));
    assert_ne!(d.port_read(CD, PX_CI) & (1 << 5), 0);
    d.restart(CD);
    assert_eq!(d.port_read(CD, PX_CI), 0);
    assert_eq!(d.port_read(CD, PX_TFD) & 0xff, TFD_IDLE);

    let sense = d.issue(
        CD,
        0,
        &h2d(0xa0, 0, 0, 0),
        Some(&[0x03, 0, 0, 0, 18]),
        &[(DATA, 512)],
        false,
    );
    assert!(sense.ok(), "{sense:?}");
    assert_eq!(
        d.bytes(DATA + 12, 1),
        [0x20],
        "invalid command operation code"
    );
}

#[test]
fn disk_errors_report_the_failing_lba_range() {
    let mut d = Driver::new();
    d.start(DISK);
    let sectors = (1 << 20) / 512;
    let result = d.issue(
        DISK,
        0,
        &h2d(0x25, sectors - 1, 2, 0),
        None,
        &[(DATA, 1024)],
        false,
    );
    assert_ne!(result.is & IS_TFES, 0);
    assert_eq!(
        (result.tfd >> 8) & 0xff,
        u32::from(ata::ERROR_IDNF | ERROR_ABRT)
    );
}

#[test]
fn completions_interrupt_through_msi_once_enabled() {
    let mut d = Driver::new();
    d.start(DISK);
    let msi = d.ahci.msi_capability;
    d.ahci.config_write(msi + 4, &0xfee0_0000u32.to_le_bytes());
    d.ahci.config_write(msi + 12, &0x0045u16.to_le_bytes());
    d.ahci.config_write(msi + 2, &1u16.to_le_bytes());
    d.write(GHC, GHC_IE);
    d.port_write(DISK, PX_IE, IS_DHRS);
    d.sink.take();

    let result = d.issue(DISK, 0, &h2d(0xe7, 0, 0, 0), None, &[], false);
    assert!(result.ok());
    let messages = d.sink.take();
    assert!(messages.iter().any(|m| m.vector() == 0x45), "{messages:?}");
    assert_ne!(
        d.read(IS) & (1 << DISK),
        0,
        "port interrupt pending at the HBA"
    );
    d.write(IS, 1 << DISK);
    assert_eq!(d.read(IS), 0);
}

#[test]
fn comreset_and_hba_reset_bring_ports_back() {
    let mut d = Driver::new();
    d.start(DISK);
    d.port_write(DISK, PX_SCTL, 1);
    assert_eq!(d.port_read(DISK, PX_SSTS), 0, "link down during COMRESET");
    d.port_write(DISK, PX_SCTL, 0);
    assert_eq!(d.port_read(DISK, PX_SSTS) & 0xf, 3);
    assert_ne!(
        d.port_read(DISK, PX_IS) & IS_DHRS,
        0,
        "signature FIS posted"
    );

    d.write(GHC, GHC_HR);
    assert_eq!(d.read(GHC) & GHC_HR, 0, "reset done at once");
    assert_eq!(d.port_read(DISK, PX_CMD) & CMD_ST, 0);
    assert_eq!(d.port_read(DISK, PX_CLB), 0);
    assert_eq!(d.port_read(DISK, PX_SIG), SIGNATURE_ATA);
}
