//! An xHCI USB host controller, written from Intel's eXtensible Host Controller
//! Interface specification (revision 1.2).
//!
//! - **BAR 0** (64 KiB): capability registers, operational registers (port registers
//!   at +0x400), runtime registers at 0x1000 (one interrupter), doorbells at 0x2000,
//!   the MSI-X table at 0x3000 and its pending bits at 0x3800, and the extended
//!   capabilities (supported protocols) at 0x8000.
//! - **Ports:** 1 and 2 are USB 2, with a device each (when given); 3 and 4 are empty
//!   USB 3 ports.
//! - **Rings:** the command ring, the event ring (through its segment table) and each
//!   endpoint's transfer ring are walked in guest memory, following Link TRBs and cycle
//!   bits. Every TRB and context comes from the guest and is read with bounds checks.
//! - **Work** happens on the vCPU that rings a doorbell, and on the host thread that
//!   delivers input ([`Xhci::service`]); devices' interrupt endpoints only complete a
//!   transfer when they have a report.
//!
//! Interrupts are MSI-X only, which Windows, Linux and OVMF (which polls) all handle.

use std::sync::Arc;
use std::time::Instant;

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::{Setup, UsbDevice};
use crate::irq::{IrqLine, MsiSink};
use crate::pci::msix::{self, MsixTable};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice, config};

pub const XHCI_BAR: u8 = 0;
pub const XHCI_BAR_SIZE: u64 = 0x10000;

/// QEMU's xHCI identity (Red Hat's vendor ID); guests bind by class.
const VENDOR_ID: u16 = 0x1b36;
const DEVICE_ID: u16 = 0x000d;

const CAP_LENGTH: u64 = 0x40;
const OPERATIONAL: u64 = CAP_LENGTH;
const PORTS: u64 = OPERATIONAL + 0x400;
const RUNTIME: u64 = 0x1000;
const INTERRUPTER_0: u64 = RUNTIME + 0x20;
const DOORBELLS: u64 = 0x2000;
const MSIX_TABLE: u32 = 0x3000;
const MSIX_PBA: u32 = 0x3800;
const EXTENDED_CAPS: u64 = 0x8000;

const MAX_SLOTS: u8 = 16;
const MAX_PORTS: u8 = 4;
const USB2_PORTS: u8 = 2;
/// Event ring segment table entries the controller reads (ERST Max is log2 of this).
const ERST_MAX_LOG2: u32 = 4;

// Capability registers.
const HCIVERSION: u32 = 0x0100;
const HCSPARAMS1: u32 = MAX_SLOTS as u32 | (1 << 8) | ((MAX_PORTS as u32) << 24);
const HCSPARAMS2: u32 = ERST_MAX_LOG2 << 4;
/// 64-bit addressing, 32-byte contexts, extended capabilities at EXTENDED_CAPS.
const HCCPARAMS1: u32 = 1 | (((EXTENDED_CAPS / 4) as u32) << 16);

// Operational registers (offsets from OPERATIONAL).
const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const PAGESIZE: u64 = 0x08;
const DNCTRL: u64 = 0x14;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const STS_HALTED: u32 = 1 << 0;
const STS_EINT: u32 = 1 << 3;
const STS_PCD: u32 = 1 << 4;
/// USBSTS bits software clears by writing 1.
const STS_RW1C: u32 = (1 << 2) | STS_EINT | STS_PCD | (1 << 10);
const CRCR_RCS: u64 = 1 << 0;
const CRCR_CS: u64 = 1 << 1;
const CRCR_CA: u64 = 1 << 2;
const CRCR_CRR: u64 = 1 << 3;

// PORTSC.
const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PLS_SHIFT: u32 = 5;
const PORT_PLS: u32 = 0xf << PORT_PLS_SHIFT;
const PORT_PP: u32 = 1 << 9;
const PORT_SPEED_SHIFT: u32 = 10;
const PORT_LWS: u32 = 1 << 16;
const PORT_CSC: u32 = 1 << 17;
const PORT_PRC: u32 = 1 << 21;
/// Change bits software clears by writing 1.
const PORT_CHANGES: u32 = 0x7f << 17;
/// Wake-on bits software reads back.
const PORT_WAKE: u32 = 0x7 << 25;
const PORT_WPR: u32 = 1 << 31;
const PLS_U0: u32 = 0;
const PLS_U3: u32 = 3;
const PLS_RX_DETECT: u32 = 5;
const PLS_POLLING: u32 = 7;
const PLS_RESUME: u32 = 15;

// Interrupter registers (offsets from INTERRUPTER_0).
const IMAN: u64 = 0x00;
const IMOD: u64 = 0x04;
const ERSTSZ: u64 = 0x08;
const ERSTBA: u64 = 0x10;
const ERDP: u64 = 0x18;
const IMAN_IP: u32 = 1 << 0;
const IMAN_IE: u32 = 1 << 1;
const ERDP_EHB: u64 = 1 << 3;

// TRB types.
const TRB_NORMAL: u8 = 1;
const TRB_SETUP: u8 = 2;
const TRB_DATA: u8 = 3;
const TRB_STATUS: u8 = 4;
const TRB_LINK: u8 = 6;
const TRB_EVENT_DATA: u8 = 7;
const TRB_ENABLE_SLOT: u8 = 9;
const TRB_DISABLE_SLOT: u8 = 10;
const TRB_ADDRESS_DEVICE: u8 = 11;
const TRB_CONFIGURE_ENDPOINT: u8 = 12;
const TRB_EVALUATE_CONTEXT: u8 = 13;
const TRB_RESET_ENDPOINT: u8 = 14;
const TRB_STOP_ENDPOINT: u8 = 15;
const TRB_SET_TR_DEQUEUE: u8 = 16;
const TRB_RESET_DEVICE: u8 = 17;
const TRB_NO_OP_COMMAND: u8 = 23;
const TRB_TRANSFER_EVENT: u8 = 32;
const TRB_COMMAND_COMPLETION: u8 = 33;
const TRB_PORT_STATUS_CHANGE: u8 = 34;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
const TRB_CHAIN: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_BSR: u32 = 1 << 9;
const TRB_EVENT_DATA_FLAG: u32 = 1 << 2;

// Completion codes.
const SUCCESS: u8 = 1;
const TRB_ERROR: u8 = 5;
const STALL: u8 = 6;
const NO_SLOTS: u8 = 9;
const SLOT_NOT_ENABLED: u8 = 11;
const SHORT_PACKET: u8 = 13;
const PARAMETER_ERROR: u8 = 17;
const CONTEXT_STATE_ERROR: u8 = 19;
const COMMAND_RING_STOPPED: u8 = 24;
const STOPPED_LENGTH_INVALID: u8 = 27;

// Contexts (32 bytes each).
const CONTEXT: u64 = 32;
const SLOT_STATE_DEFAULT: u32 = 1;
const SLOT_STATE_ADDRESSED: u32 = 2;
const SLOT_STATE_CONFIGURED: u32 = 3;
const EP_DISABLED: u32 = 0;
const EP_RUNNING: u32 = 1;
const EP_HALTED: u32 = 2;
const EP_STOPPED: u32 = 3;
const EP_TYPE_INTERRUPT_IN: u32 = 7;

/// Link TRBs followed in a row before a ring counts as broken.
const MAX_LINKS: usize = 32;
/// TRBs one TD may have before it counts as broken.
const MAX_TD: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u8 {
        ((self.control >> 10) & 0x3f) as u8
    }

    fn cycle(&self) -> bool {
        self.control & TRB_CYCLE != 0
    }

    fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }

    fn endpoint(&self) -> u8 {
        ((self.control >> 16) & 0x1f) as u8
    }

    fn length(&self) -> u32 {
        self.status & 0x1_ffff
    }

    fn to_bytes(self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.parameter.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.status.to_le_bytes());
        bytes[12..].copy_from_slice(&self.control.to_le_bytes());
        bytes
    }
}

/// Where the controller reads a ring next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ring {
    dequeue: u64,
    cycle: bool,
}

/// Reads the TRB at the ring's dequeue pointer, if software has handed it over, and
/// moves past it (and past any Link TRBs on the way).
fn next_trb(memory: &GuestMemoryMmap, ring: &mut Ring) -> Option<(u64, Trb)> {
    for _ in 0..MAX_LINKS {
        let mut bytes = [0u8; 16];
        memory
            .read_slice(&mut bytes, GuestAddress(ring.dequeue))
            .ok()?;
        let trb = Trb {
            parameter: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            status: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            control: u32::from_le_bytes(bytes[12..].try_into().unwrap()),
        };
        if trb.cycle() != ring.cycle {
            return None;
        }
        if trb.kind() == TRB_LINK {
            if trb.control & TRB_TOGGLE_CYCLE != 0 {
                ring.cycle = !ring.cycle;
            }
            ring.dequeue = trb.parameter & !0xf;
            continue;
        }
        let address = ring.dequeue;
        ring.dequeue += 16;
        return Some((address, trb));
    }
    tracing::debug!(dequeue = ring.dequeue, "xHCI: a ring of Link TRBs");
    None
}

#[derive(Clone, Copy, Debug, Default)]
struct Endpoint {
    state: u32,
    kind: u32,
    ring: Ring,
}

struct Slot {
    enabled: bool,
    /// The root port (1-based) its device is on.
    port: u8,
    endpoints: [Endpoint; 32],
}

impl Slot {
    fn new() -> Self {
        Self {
            enabled: false,
            port: 0,
            endpoints: [Endpoint::default(); 32],
        }
    }
}

struct Port {
    usb3: bool,
    portsc: u32,
    device: Option<Box<dyn UsbDevice>>,
}

#[derive(Default)]
struct Interrupter {
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    /// The event ring's producer: segment index, enqueue pointer, segment end, cycle.
    segment: u32,
    enqueue: u64,
    segment_end: u64,
    cycle: bool,
}

pub struct Xhci {
    config: ConfigSpace,
    msix_capability: u16,
    msix: MsixTable,
    /// INTA, for drivers that don't enable MSI-X (Windows' doesn't).
    intx: Option<IrqLine>,
    memory: GuestMemoryMmap,
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    dcbaap: u64,
    config_register: u32,
    command_ring: Ring,
    command_ring_running: bool,
    interrupter: Interrupter,
    ports: Vec<Port>,
    slots: Vec<Slot>,
    started: Instant,
    /// Events written since the last interrupt.
    events_pending: bool,
}

impl Xhci {
    /// A controller with `devices` on its USB 2 ports, in order (at most two).
    /// `intx` is the IOAPIC line INTA is routed to, if any.
    pub fn new(
        memory: GuestMemoryMmap,
        sink: Arc<dyn MsiSink>,
        intx: Option<IrqLine>,
        devices: Vec<Box<dyn UsbDevice>>,
    ) -> Self {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: VENDOR_ID,
            device_id: DEVICE_ID,
            revision: 1,
            // Serial bus controller, USB, xHCI.
            class: 0x0c_03_30,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        });
        config.add_bar(XHCI_BAR.into(), BarKind::Memory64, XHCI_BAR_SIZE, false);
        let msix_capability =
            msix::add_capability(&mut config, 1, (XHCI_BAR, MSIX_TABLE), (XHCI_BAR, MSIX_PBA));
        // Serial bus release 3.0, and the default frame length adjustment.
        config.set_u8(0x60, 0x30);
        config.set_u8(0x61, 0x20);
        if intx.is_some() {
            config.set_u8(config::INTERRUPT_PIN, 1);
        }
        let mut devices = devices.into_iter();
        let ports = (1..=MAX_PORTS)
            .map(|number| Port {
                usb3: number > USB2_PORTS,
                portsc: 0,
                device: if number <= USB2_PORTS {
                    devices.next()
                } else {
                    None
                },
            })
            .collect();
        let mut xhci = Self {
            config,
            msix_capability,
            msix: MsixTable::new(1, sink),
            intx,
            memory,
            usbcmd: 0,
            usbsts: 0,
            dnctrl: 0,
            dcbaap: 0,
            config_register: 0,
            command_ring: Ring::default(),
            command_ring_running: false,
            interrupter: Interrupter::default(),
            ports,
            slots: (0..=MAX_SLOTS).map(|_| Slot::new()).collect(),
            started: Instant::now(),
            events_pending: false,
        };
        xhci.reset();
        xhci
    }

    fn reset(&mut self) {
        self.usbcmd = 0;
        self.usbsts = STS_HALTED;
        self.dnctrl = 0;
        self.dcbaap = 0;
        self.config_register = 0;
        self.command_ring = Ring::default();
        self.command_ring_running = false;
        self.interrupter = Interrupter::default();
        for slot in &mut self.slots {
            *slot = Slot::new();
        }
        for port in &mut self.ports {
            port.portsc = match &mut port.device {
                Some(device) => {
                    device.reset();
                    PORT_PP
                        | PORT_CCS
                        | PORT_CSC
                        | (PLS_POLLING << PORT_PLS_SHIFT)
                        | (u32::from(device.speed()) << PORT_SPEED_SHIFT)
                }
                None => PORT_PP | (PLS_RX_DETECT << PORT_PLS_SHIFT),
            };
        }
        // MSI-X is PCI state, which a controller reset leaves alone: Windows sets it up
        // first and resets the controller after.
        self.update_intx();
    }

    fn running(&self) -> bool {
        self.usbcmd & CMD_RUN != 0
    }

    /// Completes whatever interrupt transfers have reports waiting: the host calls
    /// this after giving a device new input.
    pub fn service(&mut self) {
        if !self.running() {
            return;
        }
        for slot_id in 1..=MAX_SLOTS {
            if !self.slots[usize::from(slot_id)].enabled {
                continue;
            }
            for dci in (3..32u8).step_by(2) {
                let endpoint = self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
                if endpoint.state == EP_RUNNING && endpoint.kind == EP_TYPE_INTERRUPT_IN {
                    self.interrupt_in(slot_id, dci);
                }
            }
        }
        self.interrupt();
    }

    // ---- Events ----------------------------------------------------------------

    /// Loads event ring segment `index` from the segment table.
    fn load_segment(&mut self, index: u32) -> bool {
        let entry = self.interrupter.erstba + u64::from(index) * 16;
        let (Ok(base), Ok(size)) = (
            self.memory.read_obj::<u64>(GuestAddress(entry)),
            self.memory.read_obj::<u32>(GuestAddress(entry + 8)),
        ) else {
            return false;
        };
        let size = u64::from(size & 0xffff);
        if size == 0 {
            return false;
        }
        self.interrupter.segment = index;
        self.interrupter.enqueue = base & !0x3f;
        self.interrupter.segment_end = self.interrupter.enqueue + size * 16;
        true
    }

    fn push_event(&mut self, mut trb: Trb) {
        let interrupter = &self.interrupter;
        if interrupter.erstsz == 0 || interrupter.enqueue == 0 {
            return;
        }
        // Full when the slot after this one is where software reads next.
        let dequeue = interrupter.erdp & !0xf;
        let next = if interrupter.enqueue + 16 == interrupter.segment_end {
            None
        } else {
            Some(interrupter.enqueue + 16)
        };
        if next == Some(dequeue) {
            tracing::warn!("xHCI: event ring full; dropping an event");
            return;
        }
        trb.control = (trb.control & !TRB_CYCLE) | u32::from(interrupter.cycle);
        let at = GuestAddress(interrupter.enqueue);
        if self.memory.write_slice(&trb.to_bytes(), at).is_err() {
            return;
        }
        tracing::trace!(
            kind = trb.kind(),
            at = format_args!("{:#x}", at.0),
            cycle = interrupter.cycle,
            "xHCI: event"
        );
        self.interrupter.enqueue += 16;
        if self.interrupter.enqueue == self.interrupter.segment_end {
            let next = (self.interrupter.segment + 1) % self.interrupter.erstsz.max(1);
            if next == 0 {
                self.interrupter.cycle = !self.interrupter.cycle;
            }
            self.load_segment(next);
        }
        self.events_pending = true;
    }

    /// Tells software about the events written since the last time.
    fn interrupt(&mut self) {
        if !std::mem::take(&mut self.events_pending) {
            return;
        }
        tracing::trace!(
            iman = self.interrupter.iman,
            usbcmd = self.usbcmd,
            "xHCI: interrupt"
        );
        self.usbsts |= STS_EINT;
        self.interrupter.erdp |= ERDP_EHB;
        let enabled = self.interrupter.iman & IMAN_IE != 0 && self.usbcmd & CMD_INTE != 0;
        if enabled && self.msix.enabled() {
            // With MSI-X, IP clears once the message is sent.
            self.msix.notify(0);
        } else {
            self.interrupter.iman |= IMAN_IP;
            self.update_intx();
        }
    }

    /// INTA is asserted while the interrupter has an interrupt pending and enabled,
    /// and MSI-X is off. Software lowers it by clearing IP.
    fn update_intx(&self) {
        if let Some(intx) = &self.intx {
            intx.set_level(
                !self.msix.enabled()
                    && self.interrupter.iman & (IMAN_IP | IMAN_IE) == IMAN_IP | IMAN_IE
                    && self.usbcmd & CMD_INTE != 0,
            );
        }
    }

    fn command_completion(&mut self, command: u64, code: u8, slot: u8) {
        self.push_event(Trb {
            parameter: command,
            status: u32::from(code) << 24,
            control: (u32::from(TRB_COMMAND_COMPLETION) << 10) | (u32::from(slot) << 24),
        });
    }

    fn transfer_event(&mut self, trb: u64, residual: u32, code: u8, slot: u8, dci: u8, data: bool) {
        self.push_event(Trb {
            parameter: trb,
            status: (u32::from(code) << 24) | (residual & 0xff_ffff),
            control: (u32::from(TRB_TRANSFER_EVENT) << 10)
                | (u32::from(dci) << 16)
                | (u32::from(slot) << 24)
                | if data { TRB_EVENT_DATA_FLAG } else { 0 },
        });
    }

    fn port_status_change(&mut self, port: u8) {
        self.usbsts |= STS_PCD;
        self.push_event(Trb {
            parameter: u64::from(port) << 24,
            status: u32::from(SUCCESS) << 24,
            control: u32::from(TRB_PORT_STATUS_CHANGE) << 10,
        });
    }

    // ---- Contexts ----------------------------------------------------------------

    /// The output device context for `slot`, from the DCBAA.
    fn device_context(&self, slot: u8) -> Option<u64> {
        let entry = self.dcbaap + u64::from(slot) * 8;
        let address = self.memory.read_obj::<u64>(GuestAddress(entry)).ok()? & !0x3f;
        (address != 0).then_some(address)
    }

    fn read_context(&self, address: u64) -> Option<[u32; 8]> {
        let mut words = [0u32; 8];
        for (i, word) in words.iter_mut().enumerate() {
            *word = self
                .memory
                .read_obj(GuestAddress(address + 4 * i as u64))
                .ok()?;
        }
        Some(words)
    }

    fn write_context(&self, address: u64, words: &[u32; 8]) {
        for (i, word) in words.iter().enumerate() {
            let _ = self
                .memory
                .write_obj(*word, GuestAddress(address + 4 * i as u64));
        }
    }

    /// Rewrites endpoint `dci`'s state and dequeue pointer in the output context.
    fn sync_endpoint_context(&self, slot_id: u8, dci: u8) {
        let Some(output) = self.device_context(slot_id) else {
            return;
        };
        let address = output + u64::from(dci) * CONTEXT;
        let Some(mut words) = self.read_context(address) else {
            return;
        };
        let endpoint = self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
        words[0] = (words[0] & !0x7) | endpoint.state;
        let dequeue = endpoint.ring.dequeue | u64::from(endpoint.ring.cycle);
        words[2] = dequeue as u32;
        words[3] = (dequeue >> 32) as u32;
        self.write_context(address, &words);
    }

    /// Sets the slot state, device address and context entries in the output context.
    fn sync_slot_context(&self, slot_id: u8, state: u32, address: u8) {
        let Some(output) = self.device_context(slot_id) else {
            return;
        };
        let Some(mut words) = self.read_context(output) else {
            return;
        };
        let endpoints = &self.slots[usize::from(slot_id)].endpoints;
        let entries = (1..32u32)
            .rev()
            .find(|&dci| endpoints[dci as usize].state != EP_DISABLED)
            .unwrap_or(1);
        words[0] = (words[0] & !(0x1f << 27)) | (entries << 27);
        words[3] = (state << 27) | u32::from(address);
        self.write_context(output, &words);
    }

    fn endpoint_from_context(words: &[u32; 8], state: u32) -> Endpoint {
        let dequeue = u64::from(words[2]) | (u64::from(words[3]) << 32);
        Endpoint {
            state,
            kind: (words[1] >> 3) & 0x7,
            ring: Ring {
                dequeue: dequeue & !0xf,
                cycle: dequeue & 1 != 0,
            },
        }
    }

    // ---- Commands ----------------------------------------------------------------

    fn run_commands(&mut self) {
        if !self.running() {
            return;
        }
        self.command_ring_running = true;
        let mut ring = self.command_ring;
        while let Some((address, trb)) = next_trb(&self.memory, &mut ring) {
            self.command_ring = ring;
            let (code, slot) = self.command(&trb);
            tracing::trace!(kind = trb.kind(), slot, code, "xHCI: command");
            self.command_completion(address, code, slot);
            ring = self.command_ring;
        }
        self.command_ring = ring;
        self.interrupt();
    }

    fn command(&mut self, trb: &Trb) -> (u8, u8) {
        let slot_id = trb.slot();
        let slot_ok =
            (1..=MAX_SLOTS).contains(&slot_id) && self.slots[usize::from(slot_id)].enabled;
        match trb.kind() {
            TRB_ENABLE_SLOT => {
                let limit = (self.config_register & 0xff).clamp(1, u32::from(MAX_SLOTS)) as u8;
                match (1..=limit).find(|&id| !self.slots[usize::from(id)].enabled) {
                    Some(id) => {
                        self.slots[usize::from(id)] = Slot::new();
                        self.slots[usize::from(id)].enabled = true;
                        (SUCCESS, id)
                    }
                    None => (NO_SLOTS, 0),
                }
            }
            _ if !slot_ok && trb.kind() != TRB_NO_OP_COMMAND => (SLOT_NOT_ENABLED, slot_id),
            TRB_DISABLE_SLOT => {
                if let Some(device) = self.slot_device(slot_id) {
                    device.reset();
                }
                self.slots[usize::from(slot_id)] = Slot::new();
                (SUCCESS, slot_id)
            }
            TRB_ADDRESS_DEVICE => (self.address_device(slot_id, trb), slot_id),
            TRB_CONFIGURE_ENDPOINT => (self.configure_endpoint(slot_id, trb), slot_id),
            TRB_EVALUATE_CONTEXT => (self.evaluate_context(slot_id, trb), slot_id),
            TRB_RESET_ENDPOINT => {
                let dci = trb.endpoint();
                let endpoint = &mut self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
                if endpoint.state != EP_HALTED {
                    return (CONTEXT_STATE_ERROR, slot_id);
                }
                endpoint.state = EP_STOPPED;
                self.sync_endpoint_context(slot_id, dci);
                (SUCCESS, slot_id)
            }
            TRB_STOP_ENDPOINT => {
                let dci = trb.endpoint();
                let endpoint = self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
                if endpoint.state == EP_DISABLED || dci == 0 {
                    return (CONTEXT_STATE_ERROR, slot_id);
                }
                // A transfer waiting on the stopped endpoint ends with nothing moved.
                let mut ring = endpoint.ring;
                if endpoint.state == EP_RUNNING
                    && let Some((address, _)) = next_trb(&self.memory, &mut ring)
                {
                    self.transfer_event(address, 0, STOPPED_LENGTH_INVALID, slot_id, dci, false);
                }
                self.slots[usize::from(slot_id)].endpoints[usize::from(dci)].state = EP_STOPPED;
                self.sync_endpoint_context(slot_id, dci);
                (SUCCESS, slot_id)
            }
            TRB_SET_TR_DEQUEUE => {
                let dci = trb.endpoint();
                let endpoint = &mut self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
                if !matches!(endpoint.state, EP_STOPPED | 4) {
                    return (CONTEXT_STATE_ERROR, slot_id);
                }
                endpoint.ring = Ring {
                    dequeue: trb.parameter & !0xf,
                    cycle: trb.parameter & 1 != 0,
                };
                self.sync_endpoint_context(slot_id, dci);
                (SUCCESS, slot_id)
            }
            TRB_RESET_DEVICE => {
                let slot = &mut self.slots[usize::from(slot_id)];
                for endpoint in &mut slot.endpoints[2..] {
                    *endpoint = Endpoint::default();
                }
                if let Some(device) = self.slot_device(slot_id) {
                    device.reset();
                }
                self.sync_slot_context(slot_id, SLOT_STATE_DEFAULT, 0);
                (SUCCESS, slot_id)
            }
            TRB_NO_OP_COMMAND => (SUCCESS, 0),
            other => {
                tracing::debug!(other, "xHCI: unsupported command");
                (TRB_ERROR, slot_id)
            }
        }
    }

    fn slot_device(&mut self, slot_id: u8) -> Option<&mut Box<dyn UsbDevice>> {
        let port = self.slots[usize::from(slot_id)].port;
        let port = self.ports.get_mut(usize::from(port).checked_sub(1)?)?;
        port.device.as_mut()
    }

    fn address_device(&mut self, slot_id: u8, trb: &Trb) -> u8 {
        let input = trb.parameter & !0xf;
        let (Some(slot_words), Some(ep0_words)) = (
            self.read_context(input + CONTEXT),
            self.read_context(input + 2 * CONTEXT),
        ) else {
            return PARAMETER_ERROR;
        };
        let port = ((slot_words[1] >> 16) & 0xff) as u8;
        let has_device = self
            .ports
            .get(usize::from(port).wrapping_sub(1))
            .is_some_and(|port| port.device.is_some() && port.portsc & PORT_PED != 0);
        if !has_device {
            return CONTEXT_STATE_ERROR;
        }
        let Some(output) = self.device_context(slot_id) else {
            return PARAMETER_ERROR;
        };
        let block = trb.control & TRB_BSR != 0;
        let slot = &mut self.slots[usize::from(slot_id)];
        slot.port = port;
        slot.endpoints[1] = Self::endpoint_from_context(&ep0_words, EP_RUNNING);
        let (state, address) = if block {
            (SLOT_STATE_DEFAULT, 0)
        } else {
            (SLOT_STATE_ADDRESSED, slot_id)
        };
        let mut slot_out = slot_words;
        slot_out[3] = (state << 27) | u32::from(address);
        slot_out[0] = (slot_out[0] & !(0x1f << 27)) | (1 << 27);
        self.write_context(output, &slot_out);
        let mut ep0_out = ep0_words;
        ep0_out[0] = (ep0_out[0] & !0x7) | EP_RUNNING;
        self.write_context(output + CONTEXT, &ep0_out);
        SUCCESS
    }

    fn configure_endpoint(&mut self, slot_id: u8, trb: &Trb) -> u8 {
        const DECONFIGURE: u32 = 1 << 9;
        if trb.control & DECONFIGURE != 0 {
            for dci in 2..32u8 {
                self.slots[usize::from(slot_id)].endpoints[usize::from(dci)] = Endpoint::default();
                self.sync_endpoint_context(slot_id, dci);
            }
            self.sync_slot_context(slot_id, SLOT_STATE_ADDRESSED, slot_id);
            return SUCCESS;
        }
        let input = trb.parameter & !0xf;
        let Some(control) = self.read_context(input) else {
            return PARAMETER_ERROR;
        };
        let Some(output) = self.device_context(slot_id) else {
            return PARAMETER_ERROR;
        };
        let (drop, add) = (control[0], control[1]);
        for dci in 2..32u8 {
            let bit = 1u32 << dci;
            if drop & bit != 0 {
                self.slots[usize::from(slot_id)].endpoints[usize::from(dci)] = Endpoint::default();
                self.sync_endpoint_context(slot_id, dci);
            }
            if add & bit != 0 {
                let Some(words) = self.read_context(input + (u64::from(dci) + 1) * CONTEXT) else {
                    return PARAMETER_ERROR;
                };
                self.slots[usize::from(slot_id)].endpoints[usize::from(dci)] =
                    Self::endpoint_from_context(&words, EP_RUNNING);
                let mut out = words;
                out[0] = (out[0] & !0x7) | EP_RUNNING;
                self.write_context(output + u64::from(dci) * CONTEXT, &out);
            }
        }
        let configured = self.slots[usize::from(slot_id)].endpoints[2..]
            .iter()
            .any(|endpoint| endpoint.state != EP_DISABLED);
        let state = if configured {
            SLOT_STATE_CONFIGURED
        } else {
            SLOT_STATE_ADDRESSED
        };
        self.sync_slot_context(slot_id, state, slot_id);
        SUCCESS
    }

    fn evaluate_context(&mut self, slot_id: u8, trb: &Trb) -> u8 {
        let input = trb.parameter & !0xf;
        let (Some(control), Some(output)) =
            (self.read_context(input), self.device_context(slot_id))
        else {
            return PARAMETER_ERROR;
        };
        let add = control[1];
        if add & 1 != 0
            && let (Some(new), Some(mut old)) = (
                self.read_context(input + CONTEXT),
                self.read_context(output),
            )
        {
            // Max exit latency and the interrupter target.
            old[1] = (old[1] & !0xffff) | (new[1] & 0xffff);
            old[2] = (old[2] & 0x003f_ffff) | (new[2] & !0x003f_ffff);
            self.write_context(output, &old);
        }
        if add & 2 != 0
            && let (Some(new), Some(mut old)) = (
                self.read_context(input + 2 * CONTEXT),
                self.read_context(output + CONTEXT),
            )
        {
            // Endpoint 0's max packet size.
            old[1] = (old[1] & 0xffff) | (new[1] & 0xffff_0000);
            self.write_context(output + CONTEXT, &old);
        }
        SUCCESS
    }

    // ---- Transfers ----------------------------------------------------------------

    fn ring_endpoint(&mut self, slot_id: u8, dci: u8) {
        if !(1..=MAX_SLOTS).contains(&slot_id) || !(1..32).contains(&dci) {
            return;
        }
        let slot = &mut self.slots[usize::from(slot_id)];
        if !slot.enabled {
            return;
        }
        let endpoint = &mut slot.endpoints[usize::from(dci)];
        // A doorbell restarts a stopped endpoint.
        if endpoint.state == EP_STOPPED {
            endpoint.state = EP_RUNNING;
        }
        if endpoint.state != EP_RUNNING {
            return;
        }
        if dci == 1 {
            self.control_transfers(slot_id);
        } else if endpoint.kind == EP_TYPE_INTERRUPT_IN {
            self.interrupt_in(slot_id, dci);
        } else {
            self.discard_transfers(slot_id, dci);
        }
        self.interrupt();
    }

    /// Takes one whole TD off a ring: every TRB up to one that doesn't chain (for a
    /// control transfer, up to the Status stage and any Event Data chained after it).
    /// Leaves the ring alone if software hasn't finished queueing it.
    fn take_td(&self, ring: &mut Ring, control: bool) -> Option<Vec<(u64, Trb)>> {
        let mut peek = *ring;
        let mut td = Vec::new();
        let mut status_seen = !control;
        while td.len() < MAX_TD {
            let (address, trb) = next_trb(&self.memory, &mut peek)?;
            if trb.kind() == TRB_STATUS {
                status_seen = true;
            }
            let chained = trb.control & TRB_CHAIN != 0;
            td.push((address, trb));
            // A control TD's Setup and Data stages don't chain to what follows.
            if status_seen && !chained {
                *ring = peek;
                return Some(td);
            }
        }
        tracing::debug!("xHCI: a TD too long to be real");
        *ring = peek;
        Some(td)
    }

    fn control_transfers(&mut self, slot_id: u8) {
        loop {
            let endpoint = self.slots[usize::from(slot_id)].endpoints[1];
            if endpoint.state != EP_RUNNING {
                return;
            }
            let mut ring = endpoint.ring;
            let Some(td) = self.take_td(&mut ring, true) else {
                return;
            };
            self.slots[usize::from(slot_id)].endpoints[1].ring = ring;
            let Some(&(_, setup_trb)) = td.first().filter(|(_, trb)| trb.kind() == TRB_SETUP)
            else {
                // Not a control TD: fail its last TRB.
                if let Some(&(address, _)) = td.last() {
                    self.transfer_event(address, 0, TRB_ERROR, slot_id, 1, false);
                }
                continue;
            };
            if setup_trb.control & TRB_IDT == 0 {
                tracing::debug!("xHCI: Setup TRB without immediate data");
            }
            let setup = Setup::from_bytes(setup_trb.parameter.to_le_bytes());
            tracing::trace!(slot_id, ?setup, "xHCI: control request");
            let data_trbs: Vec<&(u64, Trb)> = td
                .iter()
                .filter(|(_, trb)| matches!(trb.kind(), TRB_DATA | TRB_NORMAL))
                .collect();
            let out_data = if setup.is_in() {
                Vec::new()
            } else {
                let mut data = Vec::new();
                for (_, trb) in &data_trbs {
                    let mut chunk = vec![0u8; trb.length() as usize];
                    if self.read_buffer(trb, &mut chunk) {
                        data.extend_from_slice(&chunk);
                    }
                }
                data.truncate(usize::from(setup.length));
                data
            };
            let answer = self
                .slot_device(slot_id)
                .and_then(|device| device.control(&setup, &out_data));
            let Some(mut answer) = answer else {
                // Stall: on the Data stage if there is one, else the Status stage.
                let stalled = data_trbs
                    .first()
                    .map(|(address, _)| *address)
                    .or_else(|| td.last().map(|(address, _)| *address))
                    .unwrap_or(0);
                self.slots[usize::from(slot_id)].endpoints[1].state = EP_HALTED;
                self.sync_endpoint_context(slot_id, 1);
                self.transfer_event(stalled, 0, STALL, slot_id, 1, false);
                return;
            };
            if setup.is_in() {
                answer.truncate(usize::from(setup.length));
            } else {
                answer = out_data;
            }
            self.complete_td(slot_id, 1, &td, &answer, setup.is_in());
        }
    }

    fn interrupt_in(&mut self, slot_id: u8, dci: u8) {
        loop {
            let endpoint = self.slots[usize::from(slot_id)].endpoints[usize::from(dci)];
            if endpoint.state != EP_RUNNING {
                return;
            }
            let mut ring = endpoint.ring;
            let Some(td) = self.take_td(&mut ring, false) else {
                return;
            };
            let Some(report) = self
                .slot_device(slot_id)
                .and_then(|device| device.interrupt_in(dci / 2))
            else {
                return;
            };
            self.slots[usize::from(slot_id)].endpoints[usize::from(dci)].ring = ring;
            self.complete_td(slot_id, dci, &td, &report, true);
        }
    }

    /// Completes OUT transfers on endpoints the devices don't use.
    fn discard_transfers(&mut self, slot_id: u8, dci: u8) {
        loop {
            let mut ring = self.slots[usize::from(slot_id)].endpoints[usize::from(dci)].ring;
            let Some(td) = self.take_td(&mut ring, false) else {
                return;
            };
            self.slots[usize::from(slot_id)].endpoints[usize::from(dci)].ring = ring;
            let moved = td
                .iter()
                .map(|(_, trb)| trb.length() as usize)
                .sum::<usize>();
            self.complete_td(slot_id, dci, &td, &vec![0; moved], false);
        }
    }

    fn read_buffer(&self, trb: &Trb, out: &mut [u8]) -> bool {
        if trb.control & TRB_IDT != 0 {
            let bytes = trb.parameter.to_le_bytes();
            let len = out.len().min(8);
            out[..len].copy_from_slice(&bytes[..len]);
            return true;
        }
        self.memory
            .read_slice(out, GuestAddress(trb.parameter))
            .is_ok()
    }

    /// Moves `data` through a TD's data TRBs (into guest memory for IN transfers) and
    /// posts the events its TRBs ask for: short packets, Event Data, and IOC.
    fn complete_td(&mut self, slot_id: u8, dci: u8, td: &[(u64, Trb)], data: &[u8], to_host: bool) {
        let mut offset = 0usize;
        let mut since_event_data = 0u32;
        let mut short = false;
        for &(address, trb) in td {
            match trb.kind() {
                TRB_DATA | TRB_NORMAL => {
                    let length = trb.length() as usize;
                    let moved = if short {
                        0
                    } else {
                        length.min(data.len().saturating_sub(offset))
                    };
                    if to_host && moved > 0 {
                        let _ = self.memory.write_slice(
                            &data[offset..offset + moved],
                            GuestAddress(trb.parameter),
                        );
                    }
                    offset += moved;
                    since_event_data += moved as u32;
                    let residual = (length - moved) as u32;
                    if residual > 0 && !short {
                        short = true;
                        if trb.control & (TRB_ISP | TRB_IOC) != 0 {
                            self.transfer_event(
                                address,
                                residual,
                                SHORT_PACKET,
                                slot_id,
                                dci,
                                false,
                            );
                        }
                    } else if trb.control & TRB_IOC != 0 {
                        self.transfer_event(address, residual, SUCCESS, slot_id, dci, false);
                    }
                }
                TRB_EVENT_DATA => {
                    if trb.control & TRB_IOC != 0 {
                        let code = if short { SHORT_PACKET } else { SUCCESS };
                        self.transfer_event(
                            trb.parameter,
                            since_event_data,
                            code,
                            slot_id,
                            dci,
                            true,
                        );
                    }
                    since_event_data = 0;
                }
                _ => {
                    if trb.control & TRB_IOC != 0 {
                        self.transfer_event(address, 0, SUCCESS, slot_id, dci, false);
                    }
                }
            }
        }
    }

    // ---- Ports ----------------------------------------------------------------------

    fn write_portsc(&mut self, index: usize, value: u32) {
        let Some(port) = self.ports.get_mut(index) else {
            return;
        };
        let mut portsc = port.portsc & !(value & PORT_CHANGES);
        portsc = (portsc & !PORT_WAKE) | (value & PORT_WAKE);
        if value & PORT_PED != 0 {
            portsc &= !PORT_PED;
        }
        if value & PORT_LWS != 0 && portsc & PORT_PED != 0 {
            let state = (value & PORT_PLS) >> PORT_PLS_SHIFT;
            let state = if state == PLS_RESUME { PLS_U0 } else { state };
            if matches!(state, PLS_U0 | PLS_U3) {
                portsc = (portsc & !PORT_PLS) | (state << PORT_PLS_SHIFT);
            }
        }
        let mut changed = false;
        if value & (PORT_PR | PORT_WPR) != 0 && portsc & PORT_CCS != 0 {
            if let Some(device) = &mut port.device {
                device.reset();
            }
            portsc = (portsc & !PORT_PLS) | (PLS_U0 << PORT_PLS_SHIFT) | PORT_PED | PORT_PRC;
            changed = true;
        }
        if !port.usb3 && portsc & PORT_PED == 0 && portsc & PORT_CCS != 0 {
            portsc = (portsc & !PORT_PLS) | (PLS_POLLING << PORT_PLS_SHIFT);
        }
        tracing::trace!(
            port = index + 1,
            value = format_args!("{value:#x}"),
            before = format_args!("{:#x}", port.portsc),
            after = format_args!("{portsc:#x}"),
            "xHCI: PORTSC write"
        );
        port.portsc = portsc;
        if changed && self.running() {
            self.port_status_change(index as u8 + 1);
            self.interrupt();
        }
    }

    // ---- Registers ------------------------------------------------------------------

    fn read_register(&self, offset: u64) -> u64 {
        match offset {
            0x00 => CAP_LENGTH | (u64::from(HCIVERSION) << 16),
            0x04 => HCSPARAMS1.into(),
            0x08 => HCSPARAMS2.into(),
            0x0c => 0,
            0x10 => HCCPARAMS1.into(),
            0x14 => DOORBELLS,
            0x18 => RUNTIME,
            0x1c => 0,
            o if (OPERATIONAL..PORTS).contains(&o) => match o - OPERATIONAL {
                USBCMD => self.usbcmd.into(),
                USBSTS => self.usbsts.into(),
                PAGESIZE => 1,
                DNCTRL => self.dnctrl.into(),
                // The command ring pointer reads as zero; only CRR is visible.
                CRCR => {
                    if self.command_ring_running {
                        CRCR_CRR
                    } else {
                        0
                    }
                }
                DCBAAP => self.dcbaap,
                CONFIG => self.config_register.into(),
                _ => 0,
            },
            o if (PORTS..PORTS + 0x10 * u64::from(MAX_PORTS)).contains(&o) => {
                let port = &self.ports[((o - PORTS) / 0x10) as usize];
                match (o - PORTS) % 0x10 {
                    0 => port.portsc.into(),
                    _ => 0,
                }
            }
            RUNTIME => {
                // MFINDEX: 125 µs microframes.
                ((self.started.elapsed().as_micros() / 125) as u64) & 0x3fff
            }
            o if (INTERRUPTER_0..INTERRUPTER_0 + 0x20).contains(&o) => match o - INTERRUPTER_0 {
                IMAN => self.interrupter.iman.into(),
                IMOD => self.interrupter.imod.into(),
                ERSTSZ => self.interrupter.erstsz.into(),
                ERSTBA => self.interrupter.erstba,
                ERDP => self.interrupter.erdp,
                _ => 0,
            },
            o if (EXTENDED_CAPS..EXTENDED_CAPS + 0x20).contains(&o) => {
                // Supported Protocol capabilities: USB 2.0 on ports 1-2, USB 3.0 on 3-4.
                let words: [u32; 8] = [
                    0x0200_0402,
                    u32::from_le_bytes(*b"USB "),
                    1 | (u32::from(USB2_PORTS) << 8),
                    0,
                    0x0300_0002,
                    u32::from_le_bytes(*b"USB "),
                    u32::from(USB2_PORTS + 1) | (u32::from(MAX_PORTS - USB2_PORTS) << 8),
                    0,
                ];
                words[((o - EXTENDED_CAPS) / 4) as usize].into()
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u64) {
        if !(PORTS..PORTS + 0x10 * u64::from(MAX_PORTS)).contains(&offset) && offset < DOORBELLS {
            tracing::trace!(
                offset = format_args!("{offset:#x}"),
                value = format_args!("{value:#x}"),
                "xHCI: register write"
            );
        }
        match offset {
            o if (OPERATIONAL..PORTS).contains(&o) => match o - OPERATIONAL {
                USBCMD => self.write_usbcmd(value as u32),
                USBSTS => self.usbsts &= !(value as u32 & STS_RW1C),
                DNCTRL => self.dnctrl = value as u32,
                CRCR => self.write_crcr(value),
                // A 32-bit driver writes the halves separately.
                o if o == CRCR + 4 => {
                    if !self.command_ring_running {
                        self.command_ring.dequeue =
                            (self.command_ring.dequeue & 0xffff_ffff) | (value << 32);
                    }
                }
                DCBAAP => self.dcbaap = (self.dcbaap & !0xffff_ffff) | (value & 0xffff_ffc0),
                o if o == DCBAAP + 4 => {
                    self.dcbaap = (self.dcbaap & 0xffff_ffff) | (value << 32);
                }
                CONFIG => self.config_register = value as u32 & 0x3ff,
                _ => {}
            },
            o if (PORTS..PORTS + 0x10 * u64::from(MAX_PORTS)).contains(&o) => {
                if (o - PORTS).is_multiple_of(0x10) {
                    self.write_portsc(((o - PORTS) / 0x10) as usize, value as u32);
                }
            }
            o if (INTERRUPTER_0..INTERRUPTER_0 + 0x20).contains(&o) => match o - INTERRUPTER_0 {
                IMAN => {
                    let mut iman = self.interrupter.iman & !(value as u32 & IMAN_IP);
                    iman = (iman & !IMAN_IE) | (value as u32 & IMAN_IE);
                    self.interrupter.iman = iman;
                    self.update_intx();
                }
                IMOD => self.interrupter.imod = value as u32,
                ERSTSZ => self.interrupter.erstsz = (value as u32 & 0xffff).min(1 << ERST_MAX_LOG2),
                ERSTBA => {
                    self.interrupter.erstba =
                        (self.interrupter.erstba & !0xffff_ffff) | (value & 0xffff_ffc0);
                    self.start_event_ring();
                }
                o if o == ERSTBA + 4 => {
                    self.interrupter.erstba =
                        (self.interrupter.erstba & 0xffff_ffff) | (value << 32);
                    self.start_event_ring();
                }
                ERDP => {
                    let busy = self.interrupter.erdp & ERDP_EHB & !(value & ERDP_EHB);
                    self.interrupter.erdp =
                        (self.interrupter.erdp & !0xffff_ffff) | (value & 0xffff_fff7) | busy;
                }
                o if o == ERDP + 4 => {
                    self.interrupter.erdp = (self.interrupter.erdp & 0xffff_ffff) | (value << 32);
                }
                _ => {}
            },
            o if (DOORBELLS..DOORBELLS + 4 * (u64::from(MAX_SLOTS) + 1)).contains(&o) => {
                let slot = ((o - DOORBELLS) / 4) as u8;
                let target = value as u8;
                if slot == 0 {
                    if target == 0 {
                        self.run_commands();
                    }
                } else {
                    self.ring_endpoint(slot, target);
                }
            }
            _ => {}
        }
    }

    fn start_event_ring(&mut self) {
        self.interrupter.cycle = true;
        self.interrupter.enqueue = 0;
        if self.interrupter.erstsz > 0 && !self.load_segment(0) {
            self.interrupter.enqueue = 0;
        }
    }

    fn write_crcr(&mut self, value: u64) {
        if self.command_ring_running {
            if value & (CRCR_CS | CRCR_CA) != 0 {
                self.command_ring_running = false;
                let dequeue = self.command_ring.dequeue;
                self.command_completion(dequeue, COMMAND_RING_STOPPED, 0);
                self.interrupt();
            }
            return;
        }
        self.command_ring = Ring {
            dequeue: (self.command_ring.dequeue & !0xffff_ffff) | (value & 0xffff_ffc0),
            cycle: value & CRCR_RCS != 0,
        };
    }

    fn write_usbcmd(&mut self, value: u32) {
        if value & CMD_RESET != 0 {
            self.reset();
            return;
        }
        let was_running = self.running();
        self.usbcmd = value & !CMD_RESET;
        if self.running() && !was_running {
            self.usbsts &= !STS_HALTED;
            // Tell software about the devices already plugged in.
            for index in 0..self.ports.len() {
                if self.ports[index].portsc & PORT_CHANGES != 0 {
                    self.port_status_change(index as u8 + 1);
                }
            }
            self.interrupt();
        } else if !self.running() {
            self.usbsts |= STS_HALTED;
            self.command_ring_running = false;
        }
        self.update_intx();
    }

    fn read_bytes(&self, offset: u64, data: &mut [u8]) {
        let base = offset & !3;
        let mut bytes = [0u8; 16];
        let words = (offset + data.len() as u64 - base).div_ceil(4);
        for i in 0..words.min(4) {
            let register = base + 4 * i;
            // 64-bit registers read as two halves.
            let value = if register % 8 == 4 && self.is_wide(register - 4) {
                (self.read_register(register - 4) >> 32) as u32
            } else {
                self.read_register(register) as u32
            };
            bytes[4 * i as usize..4 * i as usize + 4].copy_from_slice(&value.to_le_bytes());
        }
        let start = (offset - base) as usize;
        let end = start + data.len();
        data.copy_from_slice(&bytes[start..end.min(16)][..data.len()]);
    }

    /// Whether the register at `offset` is 64 bits wide.
    fn is_wide(&self, offset: u64) -> bool {
        matches!(offset, o if o == OPERATIONAL + CRCR || o == OPERATIONAL + DCBAAP)
            || matches!(offset, o if o == INTERRUPTER_0 + ERSTBA || o == INTERRUPTER_0 + ERDP)
    }
}

impl PciDevice for Xhci {
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
            tracing::trace!(enabled, masked, "xHCI: MSI-X control");
            self.msix.set_control(enabled, masked);
            self.update_intx();
        }
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar != XHCI_BAR {
            data.fill(0xff);
            return;
        }
        match offset {
            o if (u64::from(MSIX_PBA)..EXTENDED_CAPS).contains(&o) => {
                self.msix.read_pba(o - u64::from(MSIX_PBA), data);
            }
            o if (u64::from(MSIX_TABLE)..u64::from(MSIX_PBA)).contains(&o) => {
                self.msix.read_table(o - u64::from(MSIX_TABLE), data);
            }
            _ => self.read_bytes(offset, data),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar != XHCI_BAR {
            return;
        }
        match offset {
            o if (u64::from(MSIX_PBA)..EXTENDED_CAPS).contains(&o) => {}
            o if (u64::from(MSIX_TABLE)..u64::from(MSIX_PBA)).contains(&o) => {
                self.msix.write_table(o - u64::from(MSIX_TABLE), data);
            }
            _ => {
                let mut bytes = [0u8; 8];
                let len = data.len().min(8);
                bytes[..len].copy_from_slice(&data[..len]);
                let value = u64::from_le_bytes(bytes);
                if len == 8 {
                    // Low half first, as a 32-bit driver would write it.
                    self.write_register(offset, value & 0xffff_ffff);
                    self.write_register(offset + 4, value >> 32);
                } else {
                    self.write_register(offset, value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
