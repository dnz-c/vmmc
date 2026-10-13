//! An Intel 82574L Gigabit Ethernet controller (e1000e), written from Intel's 82574
//! datasheet. Windows has a driver for it in the box, as does Linux.
//!
//! - **BAR 0:** the 128 KiB register space.
//! - **Configuration:** EEPROM reads through EERD (a minimal image with the MAC address
//!   and a valid checksum), and PHY registers through MDIC (link always up at 1 Gb/s
//!   full duplex).
//! - **Receive:** one queue, legacy or extended descriptors. Frames that find no free
//!   descriptor wait; while more than a few wait, the backend is told the NIC is full
//!   (`FrameSink::has_room`) and woken once the guest posts descriptors again.
//! - **Transmit:** both of the part's queues, with legacy, context and data
//!   descriptors, IP/TCP/UDP checksum insertion and TCP segmentation (TSO) done in
//!   software. Windows' driver spreads packets over both queues, and resets the
//!   adapter (link down and up) when one stops moving.
//! - **Interrupts:** MSI when the guest enables it, otherwise INTA. No moderation:
//!   causes are signalled at once.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use net::{FrameSink, NetBackend, Waker};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::irq::{IrqLine, MsiMessage, MsiSink};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice, config, msi};

pub const REGISTER_BAR: u8 = 0;
pub const REGISTER_BAR_SIZE: u64 = 128 * 1024;
const REGISTERS: usize = (REGISTER_BAR_SIZE / 4) as usize;

// Registers (byte offsets).
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const EERD: usize = 0x0014;
const CTRL_EXT: usize = 0x0018;
const MDIC: usize = 0x0020;
const ICR: usize = 0x00c0;
const ICS: usize = 0x00c8;
const IMS: usize = 0x00d0;
const IMC: usize = 0x00d8;
const IAM: usize = 0x00e0;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TDBAL1: usize = 0x3900;
const TDBAH1: usize = 0x3904;
const TDLEN1: usize = 0x3908;
const TDH1: usize = 0x3910;
const TDT1: usize = 0x3918;
const STATISTICS: std::ops::Range<usize> = 0x4000..0x4100;
const GPRC: usize = 0x4074;
const GPTC: usize = 0x4080;
const TPR: usize = 0x40d0;
const TPT: usize = 0x40d4;
const RFCTL: usize = 0x5008;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;
const SWSM: usize = 0x5b50;
const FWSM: usize = 0x5b54;

const CTRL_RST: u32 = 1 << 26;
const CTRL_PHY_RST: u32 = 1 << 31;
const CTRL_SLU: u32 = 1 << 6;
/// Full duplex, link up, 1000 Mb/s, GIO master enabled, PHY reset asserted done.
const STATUS_LINK: u32 = 1 | (1 << 1) | (0b10 << 6) | (1 << 19) | (1 << 10);
/// EEPROM present, auto-read done.
const EECD_PRESENT: u32 = (1 << 8) | (1 << 9);
const EERD_START: u32 = 1 << 0;
const EERD_DONE: u32 = 1 << 1;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
const CTRL_EXT_IAME: u32 = 1 << 27;
const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_BSEX: u32 = 1 << 25;
const RFCTL_EXSTEN: u32 = 1 << 15;
const TCTL_EN: u32 = 1 << 1;
const RAH_AV: u32 = 1 << 31;
const SWSM_SMBI: u32 = 1 << 0;

// Interrupt causes.
const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXT0: u32 = 1 << 7;
const ICR_RXQ0: u32 = 1 << 20;
const ICR_TXQ0: u32 = 1 << 22;
const ICR_TXQ1: u32 = 1 << 23;
const ICR_INT_ASSERTED: u32 = 1 << 31;

// Descriptor bits.
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_TSE: u8 = 1 << 2;
const TX_CMD_DEXT: u8 = 1 << 5;
const TX_STA_DD: u8 = 1 << 0;
const TX_POPTS_IXSM: u8 = 1 << 0;
const TX_POPTS_TXSM: u8 = 1 << 1;
const RX_STA_DD: u8 = 1 << 0;
const RX_STA_EOP: u8 = 1 << 1;
const RX_STA_IXSM: u8 = 1 << 2;

const DESCRIPTOR: u64 = 16;
/// Largest packet a TSO context may describe.
const MAX_TSO: usize = 256 * 1024;
const MAX_FRAME: usize = 16 * 1024;
const RX_BACKLOG: usize = 128;
/// The backend holds frames back while this many wait for receive descriptors.
const RX_ROOM: usize = 32;

const PHY_ID1: u16 = 0x0141;
const PHY_ID2: u16 = 0x0cb1;

/// A transmit queue's registers, and the interrupt cause that's its own.
struct TxRegisters {
    base_low: usize,
    base_high: usize,
    len: usize,
    head: usize,
    tail: usize,
    cause: u32,
}

const TX_QUEUES: [TxRegisters; 2] = [
    TxRegisters {
        base_low: TDBAL,
        base_high: TDBAH,
        len: TDLEN,
        head: TDH,
        tail: TDT,
        cause: ICR_TXQ0,
    },
    TxRegisters {
        base_low: TDBAL1,
        base_high: TDBAH1,
        len: TDLEN1,
        head: TDH1,
        tail: TDT1,
        cause: ICR_TXQ1,
    },
];

/// A transmit queue's offload context, and the packet its data descriptors are
/// gathering. Each queue has its own: the driver fills them independently.
#[derive(Default)]
struct TxQueue {
    context: Context,
    packet: Vec<u8>,
    packet_tse: bool,
    packet_popts: u8,
}

/// The offload context the last context descriptor set up.
#[derive(Clone, Copy, Default, Debug)]
struct Context {
    ipcss: usize,
    ipcso: usize,
    tucss: usize,
    tucso: usize,
    tucse: usize,
    /// TCP (else UDP), IPv4 (else IPv6), segmentation.
    tcp: bool,
    ipv4: bool,
    hdrlen: usize,
    mss: usize,
}

struct State {
    regs: Vec<u32>,
    eeprom: [u16; 64],
    phy: [u16; 32],
    memory: GuestMemoryMmap,
    tx: [TxQueue; 2],
    backlog: VecDeque<Vec<u8>>,
    /// Wakes the backend, which found the NIC full, once there's room again.
    waker: Option<Waker>,
    backend_waiting: bool,
    sink: Arc<dyn MsiSink>,
    msi: Option<MsiMessage>,
    intx: Option<IrqLine>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One's complement sum of 16-bit big-endian words, not yet folded.
fn sum16(data: &[u8], mut sum: u32) -> u32 {
    let (pairs, rest) = data.as_chunks::<2>();
    for pair in pairs {
        sum += u32::from(u16::from_be_bytes(*pair));
    }
    if let [last] = rest {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

impl State {
    fn reg(&self, offset: usize) -> u32 {
        self.regs[offset / 4]
    }

    fn set(&mut self, offset: usize, value: u32) {
        self.regs[offset / 4] = value;
    }

    fn reset(&mut self) {
        self.regs.fill(0);
        let mac = [
            self.eeprom[0].to_le_bytes(),
            self.eeprom[1].to_le_bytes(),
            self.eeprom[2].to_le_bytes(),
        ];
        let mac: Vec<u8> = mac.iter().flatten().copied().collect();
        self.set(CTRL, CTRL_SLU | (0b10 << 8)); // link up, 1000 Mb/s
        self.set(EECD, EECD_PRESENT);
        self.set(RAL0, u32::from_le_bytes(mac[..4].try_into().unwrap()));
        self.set(
            RAH0,
            u32::from(u16::from_le_bytes([mac[4], mac[5]])) | RAH_AV,
        );
        self.tx = Default::default();
        self.backlog.clear();
        self.wake_backend();
        self.phy = phy_registers();
        // The link comes (back) up, which latches a link status change, as on real
        // hardware. It interrupts once the driver unmasks it, unless the driver
        // clears ICR first.
        self.cause(ICR_LSC);
    }

    /// Raises interrupt causes.
    fn cause(&mut self, causes: u32) {
        let icr = self.reg(ICR) | causes;
        self.set(ICR, icr);
        self.update_interrupt();
    }

    fn update_interrupt(&mut self) {
        let pending = self.reg(ICR) & self.reg(IMS) & !ICR_INT_ASSERTED != 0;
        let icr = if pending {
            self.reg(ICR) | ICR_INT_ASSERTED
        } else {
            self.reg(ICR) & !ICR_INT_ASSERTED
        };
        self.set(ICR, icr);
        match self.msi {
            Some(message) => {
                if pending {
                    tracing::trace!(icr = self.reg(ICR), ims = self.reg(IMS), "e1000e: MSI");
                    self.sink.send(message);
                }
            }
            None => {
                if let Some(intx) = &self.intx {
                    intx.set_level(pending);
                }
            }
        }
    }

    fn read_icr(&mut self) -> u32 {
        let value = self.reg(ICR);
        tracing::trace!(value, "e1000e: ICR read");
        self.set(ICR, 0);
        // Interrupt-acknowledge auto-mask.
        if self.reg(CTRL_EXT) & CTRL_EXT_IAME != 0 && value & ICR_INT_ASSERTED != 0 {
            let ims = self.reg(IMS) & !self.reg(IAM);
            self.set(IMS, ims);
        }
        if let Some(intx) = &self.intx
            && self.msi.is_none()
        {
            intx.set_level(false);
        }
        value
    }

    fn mdic(&mut self, value: u32) {
        let register = ((value >> 16) & 0x1f) as usize;
        let phy = (value >> 21) & 0x1f;
        let op = (value >> 26) & 0b11;
        let mut result = (value & !0xffff) | MDIC_READY;
        if phy != 1 && phy != 2 {
            result |= MDIC_ERROR;
        } else if op == 0b10 {
            result |= u32::from(self.phy[register]);
        } else if op == 0b01 {
            let data = value as u16;
            match register {
                // Control: reset and restart autonegotiation self-clear.
                0 => self.phy[0] = data & !((1 << 15) | (1 << 9)),
                1 | 2 | 3 | 5 | 6 | 10 | 17 => {}
                _ => self.phy[register] = data,
            }
            result |= u32::from(data);
        } else {
            result |= MDIC_ERROR;
        }
        self.set(MDIC, result);
    }

    fn eerd(&mut self, value: u32) {
        if value & EERD_START == 0 {
            self.set(EERD, value);
            return;
        }
        let address = ((value >> 2) & 0x3fff) as usize;
        let data = self.eeprom.get(address).copied().unwrap_or(0xffff);
        self.set(EERD, (value & 0xfffc) | EERD_DONE | (u32::from(data) << 16));
    }

    /// A descriptor ring's base address and size, from its BAL, BAH and LEN registers.
    fn ring(&self, (low, high, len): (usize, usize, usize)) -> (u64, u32) {
        let base = u64::from(self.reg(low) & !0xf) | (u64::from(self.reg(high)) << 32);
        (base, self.reg(len) / DESCRIPTOR as u32)
    }

    /// Runs transmit queue `queue`'s descriptors from its head up to its tail, sending
    /// the packets to `backend`.
    fn transmit(&mut self, queue: usize, backend: Option<&dyn NetBackend>) {
        if self.reg(TCTL) & TCTL_EN == 0 {
            return;
        }
        let registers = &TX_QUEUES[queue];
        let (base, entries) = self.ring((registers.base_low, registers.base_high, registers.len));
        if entries == 0 {
            return;
        }
        let mut head = self.reg(registers.head) % entries;
        let tail = self.reg(registers.tail) % entries;
        let mut written_back = false;
        while head != tail {
            let address = GuestAddress(base + u64::from(head) * DESCRIPTOR);
            let mut descriptor = [0u8; 16];
            if self.memory.read_slice(&mut descriptor, address).is_err() {
                break;
            }
            let dtyp_cmd = descriptor[11];
            let extended = dtyp_cmd & TX_CMD_DEXT != 0;
            let dtyp = (descriptor[10] >> 4) & 0xf;
            let tx = &mut self.tx[queue];
            if extended && dtyp == 0 {
                tx.context = parse_context(&descriptor);
            } else {
                let (len, popts) = if extended {
                    let len = u32::from_le_bytes(descriptor[8..12].try_into().unwrap()) & 0xf_ffff;
                    (len as usize, descriptor[13])
                } else {
                    (
                        usize::from(u16::from_le_bytes([descriptor[8], descriptor[9]])),
                        0,
                    )
                };
                let buffer = u64::from_le_bytes(descriptor[..8].try_into().unwrap());
                if tx.packet.len() + len <= MAX_TSO {
                    let start = tx.packet.len();
                    tx.packet.resize(start + len, 0);
                    if self
                        .memory
                        .read_slice(&mut tx.packet[start..], GuestAddress(buffer))
                        .is_err()
                    {
                        tx.packet.truncate(start);
                    }
                }
                if extended {
                    tx.packet_tse |= dtyp_cmd & TX_CMD_TSE != 0;
                    tx.packet_popts |= popts;
                }
                if dtyp_cmd & TX_CMD_EOP != 0 {
                    let packet = std::mem::take(&mut tx.packet);
                    let (context, tse, popts) = (tx.context, tx.packet_tse, tx.packet_popts);
                    tx.packet_tse = false;
                    tx.packet_popts = 0;
                    self.send_packet(backend, packet, &context, tse, popts);
                }
            }
            // Write back "descriptor done" (the status byte), for every descriptor.
            let status_at = address.0 + 12;
            let _ = self
                .memory
                .write_slice(&[descriptor[12] | TX_STA_DD], GuestAddress(status_at));
            written_back = true;
            head = (head + 1) % entries;
        }
        self.set(registers.head, head);
        if written_back {
            self.cause(ICR_TXDW | registers.cause);
        }
    }

    fn send_packet(
        &mut self,
        backend: Option<&dyn NetBackend>,
        packet: Vec<u8>,
        context: &Context,
        tse: bool,
        popts: u8,
    ) {
        let Some(backend) = backend else {
            return;
        };
        let mut sent = 0;
        if tse && context.mss > 0 && context.hdrlen > 0 && packet.len() > context.hdrlen {
            for segment in segment(&packet, context, popts) {
                backend.send(&segment);
                sent += 1;
            }
        } else {
            let mut packet = packet;
            apply_checksums(&mut packet, context, popts, None);
            backend.send(&packet);
            sent = 1;
        }
        for register in [GPTC, TPT] {
            let count = self.reg(register).wrapping_add(sent);
            self.set(register, count);
        }
    }

    /// Accepts a frame by its destination address, as RCTL and the filters say.
    fn accepts(&self, frame: &[u8]) -> bool {
        let Some(destination) = frame.get(..6) else {
            return false;
        };
        let rctl = self.reg(RCTL);
        if destination == [0xff; 6] {
            return rctl & RCTL_BAM != 0 || rctl & RCTL_MPE != 0;
        }
        if destination[0] & 1 != 0 {
            // Multicast: we don't model the hash table; take them all.
            return true;
        }
        if rctl & RCTL_UPE != 0 {
            return true;
        }
        (0..16).any(|i| {
            let high = self.reg(RAH0 + 8 * i);
            if high & RAH_AV == 0 {
                return false;
            }
            let low = self.reg(RAL0 + 8 * i).to_le_bytes();
            let high = (high as u16).to_le_bytes();
            destination == [low[0], low[1], low[2], low[3], high[0], high[1]]
        })
    }

    /// The receive buffer size RCTL selects.
    fn buffer_size(&self) -> usize {
        let rctl = self.reg(RCTL);
        let size = 2048 >> ((rctl >> 16) & 0b11);
        if rctl & RCTL_BSEX != 0 {
            size * 16
        } else {
            size
        }
    }

    /// Moves waiting frames into the receive ring.
    fn receive(&mut self) {
        if self.reg(RCTL) & RCTL_EN == 0 {
            return;
        }
        let (base, entries) = self.ring((RDBAL, RDBAH, RDLEN));
        if entries == 0 {
            return;
        }
        let extended = self.reg(RFCTL) & RFCTL_EXSTEN != 0;
        let buffer_size = self.buffer_size();
        let mut delivered = false;
        while let Some(frame) = self.backlog.front() {
            let head = self.reg(RDH) % entries;
            let tail = self.reg(RDT) % entries;
            let free = (tail + entries - head) % entries;
            let needed = frame.len().div_ceil(buffer_size).max(1) as u32;
            if free < needed {
                break;
            }
            let frame = self.backlog.pop_front().unwrap();
            let mut at = head;
            for (i, chunk) in frame.chunks(buffer_size).enumerate() {
                let address = base + u64::from(at) * DESCRIPTOR;
                let mut descriptor = [0u8; 16];
                let _ = self
                    .memory
                    .read_slice(&mut descriptor, GuestAddress(address));
                let buffer = u64::from_le_bytes(descriptor[..8].try_into().unwrap());
                let _ = self.memory.write_slice(chunk, GuestAddress(buffer));
                let last = (i + 1) * buffer_size >= frame.len();
                let status = RX_STA_DD | if last { RX_STA_EOP } else { 0 };
                let mut writeback = [0u8; 16];
                if extended {
                    // MRQ, IP id/checksum, status/errors, length, VLAN.
                    writeback[8..12].copy_from_slice(&u32::from(status).to_le_bytes());
                    writeback[12..14].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
                } else {
                    writeback[..8].copy_from_slice(&buffer.to_le_bytes());
                    writeback[8..10].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
                    // Checksums weren't checked: the driver does it.
                    writeback[12] = status | RX_STA_IXSM;
                }
                let _ = self.memory.write_slice(&writeback, GuestAddress(address));
                at = (at + 1) % entries;
            }
            self.set(RDH, at);
            for register in [GPRC, TPR] {
                let count = self.reg(register).wrapping_add(1);
                self.set(register, count);
            }
            delivered = true;
        }
        if delivered {
            self.cause(ICR_RXT0 | ICR_RXQ0);
        }
        self.wake_backend();
    }

    /// Tells the backend there's room again, if it was waiting for that.
    fn wake_backend(&mut self) {
        if self.backend_waiting && self.backlog.len() < RX_ROOM {
            self.backend_waiting = false;
            if let Some(waker) = &self.waker {
                waker();
            }
        }
    }
}

fn parse_context(descriptor: &[u8; 16]) -> Context {
    let tucmd = descriptor[11];
    Context {
        ipcss: usize::from(descriptor[0]),
        ipcso: usize::from(descriptor[1]),
        tucss: usize::from(descriptor[4]),
        tucso: usize::from(descriptor[5]),
        tucse: usize::from(u16::from_le_bytes([descriptor[6], descriptor[7]])),
        tcp: tucmd & 1 != 0,
        ipv4: tucmd & 2 != 0,
        hdrlen: usize::from(descriptor[13]),
        mss: usize::from(u16::from_le_bytes([descriptor[14], descriptor[15]])),
    }
}

/// Inserts the IP and TCP/UDP checksums `popts` asks for. For TSO segments,
/// `l4_length` is the segment's TCP length, which the device adds to the
/// pseudo-header sum the driver left in the checksum field.
///
/// IXSM alone asks for the IP checksum: the context's IPv4 bit only matters for TSO,
/// and Windows' driver leaves it clear when it just offloads checksums.
fn apply_checksums(packet: &mut [u8], context: &Context, popts: u8, l4_length: Option<usize>) {
    if popts & TX_POPTS_IXSM != 0 {
        let start = context.ipcss;
        let header_len = packet.get(start).map_or(0, |b| usize::from(b & 0xf) * 4);
        let at = context.ipcso;
        if start + header_len <= packet.len() && at + 2 <= packet.len() {
            packet[at..at + 2].fill(0);
            let checksum = !fold(sum16(&packet[start..start + header_len], 0));
            packet[at..at + 2].copy_from_slice(&checksum.to_be_bytes());
        }
    }
    if popts & TX_POPTS_TXSM != 0 {
        let start = context.tucss;
        let end = if context.tucse == 0 {
            packet.len()
        } else {
            (context.tucse + 1).min(packet.len())
        };
        let at = context.tucso;
        if start < end && at + 2 <= packet.len() {
            let extra = l4_length.map_or(0, |len| len as u32);
            let mut checksum = !fold(sum16(&packet[start..end], extra));
            if checksum == 0 && !context.tcp {
                checksum = 0xffff;
            }
            packet[at..at + 2].copy_from_slice(&checksum.to_be_bytes());
        }
    }
}

/// Splits a TSO packet into MSS-sized TCP segments with fixed-up headers.
fn segment(packet: &[u8], context: &Context, popts: u8) -> Vec<Vec<u8>> {
    let header = &packet[..context.hdrlen];
    let payload = &packet[context.hdrlen..];
    let tcp_header_len = context.hdrlen.saturating_sub(context.tucss);
    let chunks: Vec<&[u8]> = payload.chunks(context.mss).collect();
    let count = chunks.len();
    let ip = context.ipcss;
    let tcp = context.tucss;
    let first_id = header
        .get(ip + 4..ip + 6)
        .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
    let first_seq = header
        .get(tcp + 4..tcp + 8)
        .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let mut segments = Vec::with_capacity(count);
    for (index, chunk) in chunks.into_iter().enumerate() {
        let mut segment = Vec::with_capacity(header.len() + chunk.len());
        segment.extend_from_slice(header);
        segment.extend_from_slice(chunk);
        let offset = index * context.mss;
        let last = index + 1 == count;
        if context.ipv4 && ip + 20 <= segment.len() {
            let total = (segment.len() - ip) as u16;
            segment[ip + 2..ip + 4].copy_from_slice(&total.to_be_bytes());
            let id = first_id.wrapping_add(index as u16);
            segment[ip + 4..ip + 6].copy_from_slice(&id.to_be_bytes());
        } else if !context.ipv4 && ip + 40 <= segment.len() {
            let payload_length = (segment.len() - ip - 40) as u16;
            segment[ip + 4..ip + 6].copy_from_slice(&payload_length.to_be_bytes());
        }
        if context.tcp && tcp + 14 <= segment.len() {
            let seq = first_seq.wrapping_add(offset as u32);
            segment[tcp + 4..tcp + 8].copy_from_slice(&seq.to_be_bytes());
            // FIN and PSH only on the last segment.
            if !last {
                segment[tcp + 13] &= !0x09;
            }
        }
        let l4_length = tcp_header_len + chunk.len();
        apply_checksums(&mut segment, context, popts, Some(l4_length));
        segments.push(segment);
    }
    segments
}

fn phy_registers() -> [u16; 32] {
    let mut phy = [0u16; 32];
    phy[0] = 0x1140; // autonegotiation on, full duplex, 1000 Mb/s
    phy[1] = 0x796d; // capabilities, autonegotiation complete, link up
    phy[2] = PHY_ID1;
    phy[3] = PHY_ID2;
    phy[4] = 0x0de1; // advertising 10/100 half and full, pause
    phy[5] = 0xcde1; // the partner: the same, acknowledged
    // The partner autonegotiates, with next pages (which 1000BASE-T needs), as do we.
    // Without this, Windows' driver warns of a duplex mismatch at every link-up.
    phy[6] = 0x000d;
    phy[9] = 0x0300; // advertising 1000 full and half
    phy[10] = 0x3c00; // the partner can do 1000 full and half
    phy[15] = 0x3000; // 1000BASE-T full and half capable
    phy[17] = 0xac08; // 1000 Mb/s, full duplex, resolved, link up
    phy
}

/// The EEPROM image: MAC address, IDs and a checksum making words 0-63 sum to 0xbaba.
fn eeprom(mac: [u8; 6]) -> [u16; 64] {
    let mut words = [0u16; 64];
    for i in 0..3 {
        words[i] = u16::from_le_bytes([mac[2 * i], mac[2 * i + 1]]);
    }
    words[0x0a] = 0x4248; // initialization control 1: load device IDs, 64-bit addressing
    words[0x0b] = 0x0000; // subsystem ID
    words[0x0c] = 0x8086; // subsystem vendor
    words[0x0d] = 0x10d3; // device ID
    words[0x0e] = 0x8086; // vendor ID
    words[0x0f] = 0x3f00; // initialization control 2: full duplex, pause
    let sum = words[..63]
        .iter()
        .fold(0u16, |sum, &word| sum.wrapping_add(word));
    words[63] = 0xbabau16.wrapping_sub(sum);
    words
}

/// Hands frames from the backend to an [`E1000e`].
#[derive(Clone)]
pub struct Receiver(Arc<Mutex<State>>);

impl FrameSink for Receiver {
    fn deliver(&self, frame: &[u8]) {
        let mut state = lock(&self.0);
        if frame.len() > MAX_FRAME || !state.accepts(frame) {
            return;
        }
        if state.backlog.len() >= RX_BACKLOG {
            let missed = state.reg(0x4010).wrapping_add(1); // MPC
            state.set(0x4010, missed);
            tracing::debug!(
                head = state.reg(RDH),
                tail = state.reg(RDT),
                "e1000e: receive ring full, frame dropped"
            );
            state.cause(ICR_RXO);
            return;
        }
        state.backlog.push_back(frame.to_vec());
        state.receive();
    }

    fn has_room(&self) -> bool {
        let mut state = lock(&self.0);
        let room = state.backlog.len() < RX_ROOM;
        state.backend_waiting |= !room;
        room
    }

    fn set_waker(&self, waker: Waker) {
        lock(&self.0).waker = Some(waker);
    }
}

pub struct E1000e {
    config: ConfigSpace,
    msi_capability: u16,
    state: Arc<Mutex<State>>,
    /// Kept here, not in `state`: the backend's own thread holds `state` (through a
    /// [`Receiver`]), so a backend in there would keep both alive for good, guest
    /// memory included, every time the machine is rebuilt.
    backend: Option<Arc<dyn NetBackend>>,
}

impl E1000e {
    pub fn new(
        mac: [u8; 6],
        memory: GuestMemoryMmap,
        sink: Arc<dyn MsiSink>,
        intx: Option<IrqLine>,
    ) -> Self {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x8086,
            device_id: 0x10d3,
            revision: 0,
            // Network controller, Ethernet.
            class: 0x02_00_00,
            subsystem_vendor_id: 0x8086,
            subsystem_id: 0x0000,
        });
        config.add_bar(
            REGISTER_BAR.into(),
            BarKind::Memory32,
            REGISTER_BAR_SIZE,
            false,
        );
        config.set_u8(config::INTERRUPT_PIN, 1);
        let msi_capability = msi::add_capability(&mut config);
        let mut state = State {
            regs: vec![0; REGISTERS],
            eeprom: eeprom(mac),
            phy: phy_registers(),
            memory,
            tx: Default::default(),
            backlog: VecDeque::new(),
            waker: None,
            backend_waiting: false,
            sink,
            msi: None,
            intx,
        };
        state.reset();
        Self {
            config,
            msi_capability,
            state: Arc::new(Mutex::new(state)),
            backend: None,
        }
    }

    pub fn receiver(&self) -> Receiver {
        Receiver(self.state.clone())
    }

    pub fn set_backend(&mut self, backend: Arc<dyn NetBackend>) {
        self.backend = Some(backend);
    }

    /// Reads a register. Every access is traced (`VMMC_LOG=devices::nic::e1000e=trace`),
    /// which shows what a driver does with the device.
    fn read_register(&self, offset: usize) -> u32 {
        let value = self.read_register_inner(offset);
        tracing::trace!(
            offset = format_args!("{offset:#06x}"),
            value = format_args!("{value:#010x}"),
            "e1000e: read"
        );
        value
    }

    fn read_register_inner(&self, offset: usize) -> u32 {
        let mut state = lock(&self.state);
        match offset {
            STATUS => STATUS_LINK,
            ICR => state.read_icr(),
            SWSM => {
                // Reading takes the hardware semaphore if it's free.
                let value = state.reg(SWSM);
                state.set(SWSM, value | SWSM_SMBI);
                value
            }
            FWSM => 0,
            o if STATISTICS.contains(&o) => {
                let value = state.reg(o);
                state.set(o, 0);
                value
            }
            o if o < REGISTER_BAR_SIZE as usize => state.reg(o),
            _ => 0,
        }
    }

    fn write_register(&self, offset: usize, value: u32) {
        tracing::trace!(
            offset = format_args!("{offset:#06x}"),
            value = format_args!("{value:#010x}"),
            "e1000e: write"
        );
        let mut state = lock(&self.state);
        match offset {
            CTRL => {
                if value & CTRL_RST != 0 {
                    tracing::debug!("e1000e: the driver resets the device");
                    state.reset();
                } else {
                    state.set(CTRL, value & !(CTRL_RST | CTRL_PHY_RST));
                    if value & CTRL_PHY_RST != 0 {
                        tracing::debug!("e1000e: the driver resets the PHY");
                        state.phy = phy_registers();
                        state.cause(ICR_LSC);
                    }
                }
            }
            STATUS | 0x4000..0x4100 => {}
            EECD => state.set(EECD, (value & !0x300) | EECD_PRESENT),
            EERD => state.eerd(value),
            MDIC => state.mdic(value),
            ICR => {
                let icr = state.reg(ICR) & !value;
                state.set(ICR, icr);
                state.update_interrupt();
            }
            ICS => {
                tracing::trace!(value, "e1000e: ICS");
                state.cause(value);
            }
            IMS => {
                let ims = state.reg(IMS) | value;
                state.set(IMS, ims);
                state.update_interrupt();
            }
            IMC => {
                let ims = state.reg(IMS) & !value;
                state.set(IMS, ims);
                state.update_interrupt();
            }
            RCTL => {
                state.set(RCTL, value);
                state.receive();
            }
            RDT => {
                state.set(RDT, value & 0xffff);
                state.receive();
            }
            RDH | TDH | TDH1 => state.set(offset, value & 0xffff),
            TCTL => {
                state.set(TCTL, value);
                for queue in 0..TX_QUEUES.len() {
                    state.transmit(queue, self.backend.as_deref());
                }
            }
            TDT => {
                state.set(TDT, value & 0xffff);
                state.transmit(0, self.backend.as_deref());
            }
            TDT1 => {
                state.set(TDT1, value & 0xffff);
                state.transmit(1, self.backend.as_deref());
            }
            o if o < REGISTER_BAR_SIZE as usize => state.set(o, value),
            _ => {}
        }
    }
}

impl PciDevice for E1000e {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        if msi::touched(self.msi_capability, offset, data.len()) {
            let mut state = lock(&self.state);
            state.msi = msi::message(&self.config, self.msi_capability);
            if state.msi.is_some()
                && let Some(intx) = &state.intx
            {
                intx.set_level(false);
            }
            state.update_interrupt();
        }
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar != REGISTER_BAR {
            data.fill(0xff);
            return;
        }
        let value = self.read_register((offset & !3) as usize).to_le_bytes();
        let shift = (offset & 3) as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = value.get(shift + i).copied().unwrap_or(0);
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar != REGISTER_BAR || data.is_empty() {
            return;
        }
        // Registers are 32 bits; drivers write them whole.
        let mut bytes = [0u8; 4];
        let n = data.len().min(4);
        bytes[..n].copy_from_slice(&data[..n]);
        self.write_register((offset & !3) as usize, u32::from_le_bytes(bytes));
    }
}

#[cfg(test)]
mod tests;
