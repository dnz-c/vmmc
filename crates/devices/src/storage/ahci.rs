//! An Intel ICH9 AHCI SATA controller (AHCI 1.3.1) with six ports, written from the
//! Serial ATA AHCI specification. Each port holds an ATA disk, an ATAPI DVD drive, or
//! nothing.
//!
//! - **ABAR (BAR 5):** generic host control at 0x00, then 0x80 bytes per port at 0x100.
//! - **Commands:** the guest builds them in its command list and sets PxCI. A worker
//!   thread per port runs them in slot order, moves data through the PRDT, posts the
//!   received FISes and raises the port's interrupt.
//! - **Interrupts:** MSI when the guest enables it, otherwise INTA through the IOAPIC.
//! - **Errors:** a failed command sets PxIS.TFES and stops the port, with PxCI still set,
//!   until the guest restarts it (PxCMD.ST 1 → 0 → 1), as real HBAs do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::ata::{
    self, AtaDisk, ERROR_ABRT, STATUS_BSY, STATUS_DRDY, STATUS_DRQ, STATUS_DSC, STATUS_ERR,
    Taskfile, Transfer,
};
use super::atapi::{Cdrom, CdromState, Outcome};
use crate::irq::{IrqLine, MsiMessage, MsiSink};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice, config, msi};
use crate::snapshot::{self, Snapshot, StateError};

pub const AHCI_SLOT: u8 = 0x1f;
pub const AHCI_FUNCTION: u8 = 2;
pub const ABAR: u8 = 5;
pub const ABAR_SIZE: u64 = 0x1000;
pub const PORTS: usize = 6;

const COMMAND_SLOTS: u32 = 32;
/// Largest transfer a command may ask for, so a guest can't make us allocate without
/// bound (the PRDT can describe up to 4 GiB).
const MAX_TRANSFER: usize = 32 << 20;

// Generic host control.
const CAP: u64 = 0x00;
const GHC: u64 = 0x04;
const IS: u64 = 0x08;
const PI: u64 = 0x0c;
const VS: u64 = 0x10;
const CAP2: u64 = 0x24;
const GHC_HR: u32 = 1 << 0;
const GHC_IE: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;
const PORT_BASE: u64 = 0x100;
const PORT_SIZE: u64 = 0x80;

// Port registers.
const PX_CLB: u64 = 0x00;
const PX_CLBU: u64 = 0x04;
const PX_FB: u64 = 0x08;
const PX_FBU: u64 = 0x0c;
const PX_IS: u64 = 0x10;
const PX_IE: u64 = 0x14;
const PX_CMD: u64 = 0x18;
const PX_TFD: u64 = 0x20;
const PX_SIG: u64 = 0x24;
const PX_SSTS: u64 = 0x28;
const PX_SCTL: u64 = 0x2c;
const PX_SERR: u64 = 0x30;
const PX_SACT: u64 = 0x34;
const PX_CI: u64 = 0x38;

// PxIS bits.
const IS_DHRS: u32 = 1 << 0;
const IS_PSS: u32 = 1 << 1;
const IS_DPS: u32 = 1 << 5;
const IS_TFES: u32 = 1 << 30;
/// Bits PxIE can enable.
const IE_MASK: u32 = 0xfdc0_00ff;

// PxCMD bits.
const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_CLO: u32 = 1 << 3;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const CMD_ATAPI: u32 = 1 << 24;
/// Bits software may set in PxCMD.
const CMD_WRITABLE: u32 = CMD_ST | CMD_SUD | CMD_POD | CMD_FRE | CMD_ATAPI | (0xf << 28);

const SIGNATURE_ATA: u32 = 0x0000_0101;
const SIGNATURE_ATAPI: u32 = 0xeb14_0101;
/// Device present, Gen 1 speed, interface active.
const SSTS_PRESENT: u32 = 0x113;
const TFD_IDLE: u32 = (STATUS_DRDY | STATUS_DSC) as u32;

// Received FIS area offsets.
const RFIS_PIO_SETUP: u64 = 0x20;
const RFIS_D2H: u64 = 0x40;

/// What's attached to a port.
pub enum Drive {
    Disk(AtaDisk),
    Cdrom(Cdrom),
}

impl Drive {
    fn signature(&self) -> u32 {
        match self {
            Drive::Disk(_) => SIGNATURE_ATA,
            Drive::Cdrom(_) => SIGNATURE_ATAPI,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct PortRegs {
    clb: u64,
    fb: u64,
    is: u32,
    ie: u32,
    cmd: u32,
    tfd: u32,
    sig: u32,
    ssts: u32,
    sctl: u32,
    serr: u32,
    sact: u32,
    ci: u32,
    /// What's attached, fixed for the life of the controller.
    signature: Option<u32>,
    /// A command failed: run nothing more until the guest restarts the port.
    halted: bool,
}

impl PortRegs {
    fn new(signature: Option<u32>) -> Self {
        let mut port = Self {
            signature,
            ..Self::default()
        };
        port.reset_device();
        port
    }

    /// The device's state after power-on or a COMRESET.
    fn reset_device(&mut self) {
        match self.signature {
            Some(signature) => {
                self.sig = signature;
                self.ssts = SSTS_PRESENT;
                self.tfd = TFD_IDLE;
            }
            None => {
                self.sig = u32::MAX;
                self.ssts = 0;
                self.tfd = 0x7f;
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Registers {
    ghc: u32,
    is: u32,
    ports: [PortRegs; PORTS],
}

/// What the controller and its port workers share.
struct Shared {
    memory: GuestMemoryMmap,
    regs: Mutex<Registers>,
    sink: Arc<dyn MsiSink>,
    /// The MSI message, while the guest has MSI enabled.
    msi: Mutex<Option<MsiMessage>>,
    intx: Option<IrqLine>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Recomputes the HBA interrupt status and signals it, as MSI or INTA.
    fn update_interrupt(&self, regs: &mut Registers) {
        for (index, port) in regs.ports.iter().enumerate() {
            if port.is & port.ie != 0 {
                regs.is |= 1 << index;
            }
        }
        let pending = regs.ghc & GHC_IE != 0 && regs.is != 0;
        match *lock(&self.msi) {
            Some(message) => {
                if pending {
                    self.sink.send(message);
                }
                if let Some(intx) = &self.intx {
                    intx.set_level(false);
                }
            }
            None => {
                if let Some(intx) = &self.intx {
                    intx.set_level(pending);
                }
            }
        }
    }

    /// Writes a FIS into the port's received-FIS area, if FIS receive is on.
    fn post_fis(&self, port: &PortRegs, offset: u64, fis: &[u8]) {
        if port.cmd & CMD_FRE == 0 {
            return;
        }
        if self
            .memory
            .write_slice(fis, GuestAddress(port.fb + offset))
            .is_err()
        {
            tracing::debug!(fb = port.fb, "AHCI: received-FIS area isn't in guest RAM");
        }
    }
}

/// A device-to-host register FIS.
fn d2h_fis(status: u8, error: u8, signature: Option<u32>) -> [u8; 20] {
    let mut fis = [0u8; 20];
    fis[0] = 0x34;
    fis[1] = 0x40; // interrupt
    fis[2] = status;
    fis[3] = error;
    // After a reset or a refused IDENTIFY, the signature rides in count and LBA.
    if let Some(signature) = signature {
        fis[12] = signature as u8; // sector count
        fis[4] = (signature >> 8) as u8; // LBA low
        fis[5] = (signature >> 16) as u8; // LBA mid
        fis[6] = (signature >> 24) as u8; // LBA high
    }
    fis
}

/// A PIO setup FIS for a `len`-byte data-in transfer ending with `status`.
fn pio_setup_fis(len: usize, status: u8) -> [u8; 20] {
    let mut fis = [0u8; 20];
    fis[0] = 0x5f;
    fis[1] = 0x20 | 0x40; // device to host, interrupt
    fis[2] = STATUS_DRDY | STATUS_DRQ;
    fis[15] = status;
    fis[16..18].copy_from_slice(&(len.min(0xffff) as u16).to_le_bytes());
    fis
}

/// A command as the guest put it in the command list and table.
struct Command {
    header_address: u64,
    taskfile: Taskfile,
    cdb: [u8; 16],
    /// PRDT entries: (address, byte count, interrupt on completion).
    prdt: Vec<(u64, u32, bool)>,
}

fn read_command(memory: &GuestMemoryMmap, clb: u64, slot: u32) -> Option<Command> {
    let header_address = clb + u64::from(slot) * 32;
    let mut header = [0u8; 16];
    memory
        .read_slice(&mut header, GuestAddress(header_address))
        .ok()?;
    let dword =
        |bytes: &[u8], i: usize| u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap());
    let flags = dword(&header, 0);
    let prdt_len = (flags >> 16) as usize;
    let table = u64::from(dword(&header, 2) & !0x7f) | (u64::from(dword(&header, 3)) << 32);
    let mut cfis = [0u8; 20];
    memory.read_slice(&mut cfis, GuestAddress(table)).ok()?;
    let mut cdb = [0u8; 16];
    memory
        .read_slice(&mut cdb, GuestAddress(table + 0x40))
        .ok()?;
    let mut prdt = Vec::with_capacity(prdt_len.min(1024));
    for index in 0..prdt_len {
        let mut entry = [0u8; 16];
        memory
            .read_slice(&mut entry, GuestAddress(table + 0x80 + 16 * index as u64))
            .ok()?;
        let address = u64::from(dword(&entry, 0)) | (u64::from(dword(&entry, 1)) << 32);
        let count = (dword(&entry, 3) & 0x3f_ffff) + 1;
        prdt.push((address, count, dword(&entry, 3) & (1 << 31) != 0));
    }
    let taskfile = Taskfile {
        command: cfis[2],
        features: u16::from(cfis[3]) | (u16::from(cfis[11]) << 8),
        lba: u64::from(cfis[4])
            | (u64::from(cfis[5]) << 8)
            | (u64::from(cfis[6]) << 16)
            | (u64::from(cfis[8]) << 24)
            | (u64::from(cfis[9]) << 32)
            | (u64::from(cfis[10]) << 40),
        count: u16::from_le_bytes([cfis[12], cfis[13]]),
        device: cfis[7],
    };
    if cfis[0] != 0x27 {
        tracing::debug!(fis = cfis[0], "AHCI: command FIS isn't a register FIS");
    }
    Some(Command {
        header_address,
        taskfile,
        cdb,
        prdt,
    })
}

/// Copies `data` into the PRDT's buffers. Returns the bytes moved.
fn scatter(memory: &GuestMemoryMmap, prdt: &[(u64, u32, bool)], data: &[u8]) -> Result<usize, ()> {
    let mut done = 0;
    for &(address, count, _) in prdt {
        if done == data.len() {
            break;
        }
        let len = (count as usize).min(data.len() - done);
        memory
            .write_slice(&data[done..done + len], GuestAddress(address))
            .map_err(|_| ())?;
        done += len;
    }
    Ok(done)
}

/// Fills `data` from the PRDT's buffers. Fails if they hold too little.
fn gather(memory: &GuestMemoryMmap, prdt: &[(u64, u32, bool)], data: &mut [u8]) -> Result<(), ()> {
    let mut done = 0;
    for &(address, count, _) in prdt {
        if done == data.len() {
            break;
        }
        let len = (count as usize).min(data.len() - done);
        memory
            .read_slice(&mut data[done..done + len], GuestAddress(address))
            .map_err(|_| ())?;
        done += len;
    }
    if done == data.len() { Ok(()) } else { Err(()) }
}

/// How a command ended.
struct Completion {
    status: u8,
    error: u8,
    /// Bytes moved through the PRDT.
    bytes: usize,
    /// A PIO data-in command: post a PIO setup FIS too.
    pio_in: bool,
    /// Report the signature in the D2H FIS (a refused IDENTIFY DEVICE).
    signature: Option<u32>,
}

impl Completion {
    fn ok(bytes: usize, pio_in: bool) -> Self {
        Self {
            status: STATUS_DRDY | STATUS_DSC,
            error: 0,
            bytes,
            pio_in,
            signature: None,
        }
    }

    fn error(error: u8) -> Self {
        Self {
            status: STATUS_DRDY | STATUS_DSC | STATUS_ERR,
            error,
            bytes: 0,
            pio_in: false,
            signature: None,
        }
    }
}

fn execute(memory: &GuestMemoryMmap, drive: &mut Drive, command: &Command) -> Completion {
    let tf = &command.taskfile;
    let data_in = |data: &[u8], pio: bool| match scatter(memory, &command.prdt, data) {
        Ok(bytes) => Completion::ok(bytes, pio),
        Err(()) => Completion::error(ERROR_ABRT),
    };
    match drive {
        Drive::Disk(disk) => match disk.command(tf) {
            Ok(Transfer::None) => Completion::ok(0, false),
            Ok(Transfer::In { data, pio }) => data_in(&data, pio),
            Ok(Transfer::Out { len }) => {
                if len > MAX_TRANSFER {
                    return Completion::error(ERROR_ABRT);
                }
                let mut data = vec![0u8; len];
                if gather(memory, &command.prdt, &mut data).is_err() {
                    return Completion::error(ERROR_ABRT);
                }
                match disk.write(tf, &data) {
                    Ok(()) => Completion::ok(len, false),
                    Err(error) => Completion::error(error),
                }
            }
            // IDENTIFY PACKET DEVICE on a disk: refused, like any unknown command.
            Err(error) => Completion::error(error),
        },
        Drive::Cdrom(cdrom) => match tf.command {
            // PACKET.
            0xa0 => match cdrom.command(&command.cdb) {
                Outcome::Data(data) => {
                    // PIO unless the features register asks for DMA.
                    data_in(&data, tf.features & 1 == 0 && !data.is_empty())
                }
                Outcome::Error(sense) => Completion::error(sense.key << 4),
            },
            // IDENTIFY PACKET DEVICE.
            0xa1 => data_in(&ata::identify_cdrom(cdrom.serial()), true),
            // IDENTIFY DEVICE: refused, with the ATAPI signature, so drivers know.
            0xec => Completion {
                signature: Some(SIGNATURE_ATAPI),
                ..Completion::error(ERROR_ABRT)
            },
            // DEVICE RESET, SET FEATURES and power management.
            0x08 | 0xef | 0xe0 | 0xe1 | 0xe2 | 0xe3 | 0xe5 | 0xe6 => Completion::ok(0, false),
            _ => Completion::error(ERROR_ABRT),
        },
    }
}

enum Message {
    Notify,
    /// Answered once everything before it is done.
    Sync(Sender<()>),
    Stop,
}

/// Runs a port's commands until told to stop.
fn run_port(
    shared: Arc<Shared>,
    index: usize,
    drive: Arc<Mutex<Drive>>,
    messages: Receiver<Message>,
) {
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
            let (slot, clb) = {
                let regs = lock(&shared.regs);
                let port = &regs.ports[index];
                if port.cmd & CMD_ST == 0 || port.halted || port.ci == 0 {
                    break;
                }
                (port.ci.trailing_zeros(), port.clb)
            };
            let completion = match read_command(&shared.memory, clb, slot) {
                Some(command) => {
                    let completion = execute(&shared.memory, &mut lock(&drive), &command);
                    // PRDBC: bytes transferred.
                    let _ = shared.memory.write_slice(
                        &(completion.bytes as u32).to_le_bytes(),
                        GuestAddress(command.header_address + 4),
                    );
                    let interrupt = command.prdt.iter().any(|&(_, _, i)| i);
                    (completion, interrupt)
                }
                None => (Completion::error(ERROR_ABRT), false),
            };
            let (completion, prd_interrupt) = completion;

            let mut regs = lock(&shared.regs);
            let port = &mut regs.ports[index];
            // The guest stopped the port meanwhile: the command is abandoned.
            if port.cmd & CMD_ST == 0 || port.ci & (1 << slot) == 0 {
                continue;
            }
            let port_copy = *port;
            if completion.pio_in {
                shared.post_fis(
                    &port_copy,
                    RFIS_PIO_SETUP,
                    &pio_setup_fis(completion.bytes, completion.status),
                );
            }
            shared.post_fis(
                &port_copy,
                RFIS_D2H,
                &d2h_fis(completion.status, completion.error, completion.signature),
            );
            port.tfd = u32::from(completion.status) | (u32::from(completion.error) << 8);
            port.is |= IS_DHRS;
            if completion.pio_in {
                port.is |= IS_PSS;
            }
            if completion.status & STATUS_ERR != 0 {
                port.is |= IS_TFES;
                port.halted = true;
            } else {
                port.ci &= !(1 << slot);
                if prd_interrupt {
                    port.is |= IS_DPS;
                }
            }
            shared.update_interrupt(&mut regs);
        }
    }
}

struct Port {
    messages: Sender<Message>,
    thread: Option<JoinHandle<()>>,
    drive: Arc<Mutex<Drive>>,
}

/// A DVD drive of the controller, whose disc the host can change while the guest
/// runs (the guest sees the drive's unit attention and media event).
#[derive(Clone)]
pub struct DiscDrive(Arc<Mutex<Drive>>);

impl DiscDrive {
    /// Takes the disc out (`None`) or puts one in. Returns whether it happened now, or
    /// waits for the guest to eject the disc it holds (see [`Cdrom::change_medium`]).
    pub fn change(&self, medium: Option<Arc<dyn disk::BlockBackend>>) -> bool {
        match &mut *lock(&self.0) {
            Drive::Cdrom(cdrom) => cdrom.change_medium(medium),
            Drive::Disk(_) => true,
        }
    }

    /// Makes a change that's waiting for the guest. Returns whether there was one.
    pub fn finish_change(&self) -> bool {
        match &mut *lock(&self.0) {
            Drive::Cdrom(cdrom) => cdrom.finish_change(),
            Drive::Disk(_) => false,
        }
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        let _ = self.messages.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Ahci {
    config: ConfigSpace,
    msi_capability: u16,
    shared: Arc<Shared>,
    ports: Vec<Option<Port>>,
    /// The signature FIS goes out the first time FIS receive is turned on.
    sent_signature: [AtomicBool; PORTS],
}

impl Ahci {
    /// A controller with `drives` on ports 0 and up (at most six). `intx` is the IOAPIC
    /// line INTA is wired to, used until the guest turns MSI on.
    pub fn new(
        drives: Vec<Drive>,
        memory: GuestMemoryMmap,
        sink: Arc<dyn MsiSink>,
        intx: Option<IrqLine>,
    ) -> std::io::Result<Self> {
        assert!(drives.len() <= PORTS, "AHCI has {PORTS} ports");
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x8086,
            device_id: 0x2922,
            revision: 2,
            // Mass storage, SATA, AHCI 1.0 programming interface.
            class: 0x01_06_01,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        });
        config.add_bar(ABAR.into(), BarKind::Memory32, ABAR_SIZE, false);
        config.set_u8(config::INTERRUPT_PIN, 1);
        let msi_capability = msi::add_capability(&mut config);

        let mut port_regs = [PortRegs::new(None); PORTS];
        for (regs, drive) in port_regs.iter_mut().zip(&drives) {
            *regs = PortRegs::new(Some(drive.signature()));
        }
        let shared = Arc::new(Shared {
            memory,
            regs: Mutex::new(Registers {
                ghc: GHC_AE,
                is: 0,
                ports: port_regs,
            }),
            sink,
            msi: Mutex::new(None),
            intx,
        });
        let mut ports: Vec<Option<Port>> = (0..PORTS).map(|_| None).collect();
        for (index, drive) in drives.into_iter().enumerate() {
            let (messages, receiver) = mpsc::channel();
            let shared = shared.clone();
            let drive = Arc::new(Mutex::new(drive));
            let port_drive = drive.clone();
            let thread = thread::Builder::new()
                .name(format!("ahci-port{index}"))
                .spawn(move || run_port(shared, index, port_drive, receiver))?;
            ports[index] = Some(Port {
                messages,
                thread: Some(thread),
                drive,
            });
        }
        Ok(Self {
            config,
            msi_capability,
            shared,
            ports,
            sent_signature: Default::default(),
        })
    }

    /// The DVD drives, in port order.
    pub fn disc_drives(&self) -> Vec<DiscDrive> {
        self.ports
            .iter()
            .flatten()
            .filter(|port| matches!(*lock(&port.drive), Drive::Cdrom(_)))
            .map(|port| DiscDrive(port.drive.clone()))
            .collect()
    }

    fn read_register(&self, offset: u64) -> u32 {
        let regs = lock(&self.shared.regs);
        match offset {
            // 64-bit addressing, AHCI only, Gen 1-3 speeds, 32 slots, and the implemented
            // ports (0-based). No staggered spin-up: drivers would then probe the ports
            // one at a time.
            CAP => {
                (1 << 31) | (1 << 18) | (3 << 20) | ((COMMAND_SLOTS - 1) << 8) | (PORTS as u32 - 1)
            }
            GHC => regs.ghc,
            IS => regs.is,
            PI => (1 << PORTS) - 1,
            VS => 0x0001_0301,
            CAP2 => 0,
            _ if offset >= PORT_BASE => {
                let index = ((offset - PORT_BASE) / PORT_SIZE) as usize;
                let Some(port) = regs.ports.get(index) else {
                    return 0;
                };
                match (offset - PORT_BASE) % PORT_SIZE {
                    PX_CLB => port.clb as u32,
                    PX_CLBU => (port.clb >> 32) as u32,
                    PX_FB => port.fb as u32,
                    PX_FBU => (port.fb >> 32) as u32,
                    PX_IS => port.is,
                    PX_IE => port.ie,
                    PX_CMD => port.cmd,
                    PX_TFD => port.tfd,
                    PX_SIG => port.sig,
                    PX_SSTS => port.ssts,
                    PX_SCTL => port.sctl,
                    PX_SERR => port.serr,
                    PX_SACT => port.sact,
                    PX_CI => port.ci,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        tracing::trace!(offset, value, "AHCI register write");
        let shared = self.shared.clone();
        let mut regs = lock(&shared.regs);
        match offset {
            GHC => {
                if value & GHC_HR != 0 {
                    // HBA reset: every port back to its power-on state.
                    for port in &mut regs.ports {
                        *port = PortRegs::new(port.signature);
                    }
                    regs.is = 0;
                    regs.ghc = GHC_AE;
                    for sent in &self.sent_signature {
                        sent.store(false, Ordering::Relaxed);
                    }
                } else {
                    regs.ghc = GHC_AE | (value & GHC_IE);
                }
                shared.update_interrupt(&mut regs);
            }
            IS => {
                regs.is &= !value;
                shared.update_interrupt(&mut regs);
            }
            _ if offset >= PORT_BASE => {
                let index = ((offset - PORT_BASE) / PORT_SIZE) as usize;
                if index >= PORTS {
                    return;
                }
                let notify =
                    self.write_port(&mut regs, index, (offset - PORT_BASE) % PORT_SIZE, value);
                drop(regs);
                if notify && let Some(port) = &self.ports[index] {
                    let _ = port.messages.send(Message::Notify);
                }
            }
            _ => {}
        }
    }

    /// Applies a port register write. Returns whether the worker has commands to run.
    fn write_port(&self, regs: &mut Registers, index: usize, register: u64, value: u32) -> bool {
        let shared = &self.shared;
        let port = &mut regs.ports[index];
        match register {
            PX_CLB => port.clb = (port.clb & !0xffff_ffff) | u64::from(value & !0x3ff),
            PX_CLBU => port.clb = (port.clb & 0xffff_ffff) | (u64::from(value) << 32),
            PX_FB => port.fb = (port.fb & !0xffff_ffff) | u64::from(value & !0xff),
            PX_FBU => port.fb = (port.fb & 0xffff_ffff) | (u64::from(value) << 32),
            PX_IS => {
                port.is &= !value;
                shared.update_interrupt(regs);
            }
            PX_IE => {
                port.ie = value & IE_MASK;
                shared.update_interrupt(regs);
            }
            PX_CMD => return self.write_cmd(regs, index, value),
            PX_SCTL => {
                let was_resetting = port.sctl & 0xf == 1;
                port.sctl = value;
                // COMRESET: DET goes 1 then 0. The device comes back with its signature.
                if value & 0xf == 1 {
                    port.ssts = 0;
                } else if was_resetting {
                    port.reset_device();
                    port.halted = false;
                    let signature = port.signature;
                    if let Some(signature) = signature {
                        let copy = *port;
                        shared.post_fis(
                            &copy,
                            RFIS_D2H,
                            &d2h_fis(TFD_IDLE as u8, 1, Some(signature)),
                        );
                        port.is |= IS_DHRS;
                        // "Exchanged": the device changed (SERR.DIAG.X).
                        port.serr |= 1 << 26;
                    }
                    shared.update_interrupt(regs);
                }
            }
            PX_SERR => port.serr &= !value,
            PX_SACT if port.cmd & CMD_ST != 0 => port.sact |= value,
            PX_CI if port.cmd & CMD_ST != 0 => {
                port.ci |= value;
                return port.signature.is_some();
            }
            _ => {}
        }
        false
    }

    fn write_cmd(&self, regs: &mut Registers, index: usize, value: u32) -> bool {
        let shared = &self.shared;
        let port = &mut regs.ports[index];
        let old = port.cmd;
        port.cmd = (old & !CMD_WRITABLE) | (value & CMD_WRITABLE);
        // Command list override: clear BSY and DRQ so the port can start.
        if value & CMD_CLO != 0 {
            port.tfd &= !u32::from(STATUS_BSY | STATUS_DRQ);
        }
        if value & CMD_FRE != 0 {
            port.cmd |= CMD_FR;
            // The device's first D2H FIS, with its signature.
            if let Some(signature) = port.signature
                && !self.sent_signature[index].swap(true, Ordering::Relaxed)
            {
                let copy = *port;
                shared.post_fis(
                    &copy,
                    RFIS_D2H,
                    &d2h_fis(TFD_IDLE as u8, 1, Some(signature)),
                );
            }
        } else {
            port.cmd &= !CMD_FR;
        }
        if value & CMD_ST != 0 {
            port.cmd |= CMD_CR;
            if old & CMD_ST == 0 {
                port.halted = false;
            }
            port.ci != 0 && port.signature.is_some()
        } else {
            // Stopping the port drops every outstanding command.
            port.cmd &= !CMD_CR;
            port.ci = 0;
            port.sact = 0;
            port.halted = false;
            if port.tfd & u32::from(STATUS_ERR) != 0 {
                port.tfd = TFD_IDLE;
            }
            false
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AhciState {
    config: Vec<u8>,
    registers: Registers,
    sent_signature: [bool; PORTS],
    /// Each DVD drive's, by port.
    cdroms: Vec<(usize, CdromState)>,
}

impl Snapshot for Ahci {
    fn save(&self) -> Vec<u8> {
        let cdroms = self
            .ports
            .iter()
            .enumerate()
            .filter_map(|(index, port)| match &*lock(&port.as_ref()?.drive) {
                Drive::Cdrom(cdrom) => Some((index, cdrom.save())),
                Drive::Disk(_) => None,
            })
            .collect();
        snapshot::encode(&AhciState {
            config: self.config.save(),
            registers: lock(&self.shared.regs).clone(),
            sent_signature: std::array::from_fn(|index| {
                self.sent_signature[index].load(Ordering::Relaxed)
            }),
            cdroms,
        })
    }

    /// Interrupts already raised are in the restored interrupt controllers. Commands
    /// the guest had issued and the ports hadn't run yet run now.
    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: AhciState = snapshot::decode(state)?;
        for (index, (saved, port)) in state
            .registers
            .ports
            .iter()
            .zip(&lock(&self.shared.regs).ports)
            .enumerate()
        {
            if saved.signature != port.signature {
                return Err(StateError(format!(
                    "AHCI port {index} had a different drive"
                )));
            }
        }
        self.config.restore(&state.config)?;
        *lock(&self.shared.msi) = msi::message(&self.config, self.msi_capability);
        *lock(&self.shared.regs) = state.registers;
        for (sent, saved) in self.sent_signature.iter().zip(state.sent_signature) {
            sent.store(saved, Ordering::Relaxed);
        }
        for (index, saved) in state.cdroms {
            if let Some(Some(port)) = self.ports.get(index)
                && let Drive::Cdrom(cdrom) = &mut *lock(&port.drive)
            {
                cdrom.restore(saved);
            }
        }
        for port in self.ports.iter().flatten() {
            let _ = port.messages.send(Message::Notify);
        }
        Ok(())
    }

    fn quiesce(&mut self) {
        for port in self.ports.iter().flatten() {
            let (done, wait) = mpsc::channel();
            if port.messages.send(Message::Sync(done)).is_ok() {
                let _ = wait.recv();
            }
        }
    }
}

impl PciDevice for Ahci {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        if msi::touched(self.msi_capability, offset, data.len()) {
            *lock(&self.shared.msi) = msi::message(&self.config, self.msi_capability);
            let mut regs = lock(&self.shared.regs);
            self.shared.update_interrupt(&mut regs);
        }
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar != ABAR {
            data.fill(0xff);
            return;
        }
        let value = self.read_register(offset & !3).to_le_bytes();
        let shift = (offset & 3) as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = value.get(shift + i).copied().unwrap_or(0);
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar != ABAR {
            return;
        }
        // Registers are written as whole dwords; narrower writes merge into the value.
        let aligned = offset & !3;
        let mut bytes = self.read_register(aligned).to_le_bytes();
        let shift = (offset & 3) as usize;
        for (i, &byte) in data.iter().enumerate().take(4 - shift) {
            bytes[shift + i] = byte;
        }
        // Write-1-to-clear registers must only see the bytes actually written.
        let register = aligned.checked_sub(PORT_BASE).map(|o| o % PORT_SIZE);
        let w1c = aligned == IS || matches!(register, Some(PX_IS | PX_SERR));
        if w1c {
            let mut written = [0u8; 4];
            for (i, &byte) in data.iter().enumerate().take(4 - shift) {
                written[shift + i] = byte;
            }
            bytes = written;
        }
        self.write_register(aligned, u32::from_le_bytes(bytes));
    }
}

#[cfg(test)]
mod tests;
