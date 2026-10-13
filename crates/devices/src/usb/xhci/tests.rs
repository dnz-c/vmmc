//! The controller driven the way a driver does it: rings and contexts in guest memory,
//! registers through BAR 0.

use std::sync::Mutex;

use vm_core::BusDevice;

use super::*;
use crate::irq::IrqLine;
use crate::irq::testing::RecordingSink;
use crate::legacy::ioapic::IoApic;
use crate::usb::hid::{Tablet, TabletInput};

const DCBAA: u64 = 0x1000;
const DEVICE_CONTEXT: u64 = 0x2000;
const INPUT_CONTEXT: u64 = 0x3000;
const COMMAND_RING: u64 = 0x4000;
const EVENT_RING: u64 = 0x5000;
const EVENT_RING_TRBS: u32 = 64;
const ERST: u64 = 0x6000;
const EP0_RING: u64 = 0x7000;
const EP1_RING: u64 = 0x8000;
const DATA: u64 = 0x9000;
const REPORT: u64 = 0xa000;

struct Driver {
    xhci: Xhci,
    memory: GuestMemoryMmap,
    tablet: TabletInput,
    command: u64,
    ep0: u64,
    events_read: u64,
}

fn write32(xhci: &mut Xhci, offset: u64, value: u32) {
    xhci.bar_write(XHCI_BAR, offset, &value.to_le_bytes());
}

fn write64(xhci: &mut Xhci, offset: u64, value: u64) {
    xhci.bar_write(XHCI_BAR, offset, &value.to_le_bytes());
}

fn read32(xhci: &mut Xhci, offset: u64) -> u32 {
    let mut data = [0u8; 4];
    xhci.bar_read(XHCI_BAR, offset, &mut data);
    u32::from_le_bytes(data)
}

fn trb(parameter: u64, status: u32, kind: u8, flags: u32) -> Trb {
    Trb {
        parameter,
        status,
        control: (u32::from(kind) << 10) | flags | TRB_CYCLE,
    }
}

impl Driver {
    /// A running controller with a tablet on port 1 and the rings set up.
    fn new() -> Self {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let (tablet, input) = Tablet::new();
        let mut xhci = Xhci::new(
            memory.clone(),
            Arc::new(RecordingSink::default()),
            None,
            vec![Box::new(tablet)],
        );
        memory
            .write_obj(DEVICE_CONTEXT, GuestAddress(DCBAA + 8))
            .unwrap();
        memory.write_obj(EVENT_RING, GuestAddress(ERST)).unwrap();
        memory
            .write_obj(EVENT_RING_TRBS, GuestAddress(ERST + 8))
            .unwrap();
        write64(&mut xhci, OPERATIONAL + DCBAAP, DCBAA);
        write32(&mut xhci, OPERATIONAL + CONFIG, u32::from(MAX_SLOTS));
        write64(&mut xhci, OPERATIONAL + CRCR, COMMAND_RING | CRCR_RCS);
        write32(&mut xhci, INTERRUPTER_0 + ERSTSZ, 1);
        write64(&mut xhci, INTERRUPTER_0 + ERDP, EVENT_RING);
        write64(&mut xhci, INTERRUPTER_0 + ERSTBA, ERST);
        write32(&mut xhci, INTERRUPTER_0 + IMAN, IMAN_IE);
        write32(&mut xhci, OPERATIONAL + USBCMD, CMD_RUN | CMD_INTE);
        Self {
            xhci,
            memory,
            tablet: input,
            command: COMMAND_RING,
            ep0: EP0_RING,
            events_read: 0,
        }
    }

    fn put(&self, address: u64, trb: Trb) {
        self.memory
            .write_slice(&trb.to_bytes(), GuestAddress(address))
            .unwrap();
    }

    fn put_context(&self, address: u64, words: [u32; 8]) {
        for (i, word) in words.iter().enumerate() {
            self.memory
                .write_obj(*word, GuestAddress(address + 4 * i as u64))
                .unwrap();
        }
    }

    fn context(&self, address: u64) -> [u32; 8] {
        let mut words = [0u32; 8];
        for (i, word) in words.iter_mut().enumerate() {
            *word = self
                .memory
                .read_obj(GuestAddress(address + 4 * i as u64))
                .unwrap();
        }
        words
    }

    /// The events written since the last call.
    fn events(&mut self) -> Vec<Trb> {
        let mut events = Vec::new();
        loop {
            let address = EVENT_RING + 16 * self.events_read;
            let mut bytes = [0u8; 16];
            self.memory
                .read_slice(&mut bytes, GuestAddress(address))
                .unwrap();
            let event = Trb {
                parameter: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                status: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                control: u32::from_le_bytes(bytes[12..].try_into().unwrap()),
            };
            if !event.cycle() {
                break;
            }
            events.push(event);
            self.events_read += 1;
        }
        let dequeue = EVENT_RING + 16 * self.events_read;
        write64(&mut self.xhci, INTERRUPTER_0 + ERDP, dequeue | ERDP_EHB);
        events
    }

    /// Queues a command, rings the host doorbell, and returns the completion's code
    /// and slot.
    fn command(&mut self, command: Trb) -> (u8, u8) {
        self.put(self.command, command);
        let address = self.command;
        self.command += 16;
        write32(&mut self.xhci, DOORBELLS, 0);
        let events = self.events();
        let completion = events
            .iter()
            .find(|event| event.kind() == TRB_COMMAND_COMPLETION)
            .expect("a command completion");
        assert_eq!(completion.parameter, address);
        ((completion.status >> 24) as u8, completion.slot())
    }

    /// Runs a control transfer on endpoint 0; returns the events and the data.
    fn control(&mut self, setup: [u8; 8], length: u32) -> (Vec<Trb>, Vec<u8>) {
        let request_in = setup[0] & 0x80 != 0;
        let mut at = self.ep0;
        let transfer_type = if length == 0 {
            0
        } else if request_in {
            3
        } else {
            2
        };
        self.put(
            at,
            trb(
                u64::from_le_bytes(setup),
                8,
                TRB_SETUP,
                TRB_IDT | (transfer_type << 16),
            ),
        );
        at += 16;
        if length > 0 {
            let direction = if request_in { 1 << 16 } else { 0 };
            self.put(at, trb(DATA, length, TRB_DATA, direction | TRB_ISP));
            at += 16;
        }
        // The status stage goes the other way from the data.
        let status_in = if length == 0 || !request_in {
            1 << 16
        } else {
            0
        };
        self.put(at, trb(0, 0, TRB_STATUS, status_in | TRB_IOC));
        at += 16;
        self.ep0 = at;
        write32(&mut self.xhci, DOORBELLS + 4, 1);
        let mut data = vec![0u8; length as usize];
        self.memory
            .read_slice(&mut data, GuestAddress(DATA))
            .unwrap();
        (self.events(), data)
    }

    /// Resets port 1, enables a slot and addresses the tablet in it.
    fn address_tablet(&mut self) {
        let portsc = PORTS;
        assert_eq!(read32(&mut self.xhci, portsc) & PORT_CCS, PORT_CCS);
        write32(&mut self.xhci, portsc, PORT_PP | PORT_PR | PORT_CSC);
        let port = read32(&mut self.xhci, portsc);
        assert_eq!(port & (PORT_PED | PORT_PRC), PORT_PED | PORT_PRC);
        assert_eq!((port >> PORT_SPEED_SHIFT) & 0xf, 1, "full speed");
        let events = self.events();
        assert!(events.iter().any(|e| e.kind() == TRB_PORT_STATUS_CHANGE));

        let (code, slot) = self.command(trb(0, 0, TRB_ENABLE_SLOT, 0));
        assert_eq!((code, slot), (SUCCESS, 1));

        // Input context: add the slot and endpoint 0.
        self.put_context(INPUT_CONTEXT, [0, 0x3, 0, 0, 0, 0, 0, 0]);
        self.put_context(
            INPUT_CONTEXT + 32,
            [(1 << 27) | (1 << 20), 1 << 16, 0, 0, 0, 0, 0, 0],
        );
        self.put_context(
            INPUT_CONTEXT + 64,
            [
                0,
                (64 << 16) | (4 << 3) | (3 << 1),
                EP0_RING as u32 | 1,
                0,
                8,
                0,
                0,
                0,
            ],
        );
        let (code, _) = self.command(trb(INPUT_CONTEXT, 0, TRB_ADDRESS_DEVICE, 1 << 24));
        assert_eq!(code, SUCCESS);
        let slot_out = self.context(DEVICE_CONTEXT);
        assert_eq!(slot_out[3] >> 27, SLOT_STATE_ADDRESSED);
        assert_eq!(slot_out[3] & 0xff, 1, "USB address");
        assert_eq!(self.context(DEVICE_CONTEXT + 32)[0] & 7, EP_RUNNING);
    }
}

#[test]
fn reports_devices_already_plugged_in_when_started() {
    let mut driver = Driver::new();
    let events = driver.events();
    assert_eq!(events.len(), 1, "port 1 has the tablet");
    assert_eq!(events[0].kind(), TRB_PORT_STATUS_CHANGE);
    assert_eq!(events[0].parameter >> 24, 1);
    assert_eq!(
        read32(&mut driver.xhci, OPERATIONAL + USBSTS) & STS_HALTED,
        0
    );
    // Ports 3 and 4 are USB 3, by the supported protocol capabilities.
    assert_eq!(read32(&mut driver.xhci, EXTENDED_CAPS + 16) >> 24, 3);
    assert_eq!(read32(&mut driver.xhci, PORTS + 0x20) & PORT_CCS, 0);
}

#[test]
fn enumerates_the_tablet_and_delivers_reports() {
    let mut driver = Driver::new();
    driver.events();
    driver.address_tablet();

    // GET_DESCRIPTOR(device), 18 bytes.
    let (events, data) = driver.control([0x80, 6, 0, 1, 0, 0, 18, 0], 18);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status >> 24, u32::from(SUCCESS));
    assert_eq!(&data[..2], &[18, 1], "a device descriptor");
    // Asking for more than there is: a short packet, then the status stage.
    let (events, _) = driver.control([0x80, 6, 0, 1, 0, 0, 64, 0], 64);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].status >> 24, u32::from(SHORT_PACKET));
    assert_eq!(events[0].status & 0xff_ffff, 64 - 18, "residual");
    assert_eq!(events[1].status >> 24, u32::from(SUCCESS));

    // SET_CONFIGURATION(1), then add endpoint 1 IN (DCI 3).
    let (events, _) = driver.control([0x00, 9, 1, 0, 0, 0, 0, 0], 0);
    assert_eq!(events[0].status >> 24, u32::from(SUCCESS));
    driver.put_context(INPUT_CONTEXT, [0, 0x9, 0, 0, 0, 0, 0, 0]);
    driver.put_context(
        INPUT_CONTEXT + 32,
        [(3 << 27) | (1 << 20), 1 << 16, 0, 0, 0, 0, 0, 0],
    );
    driver.put_context(
        INPUT_CONTEXT + 4 * 32,
        [
            3 << 16,
            (8 << 16) | (7 << 3) | (3 << 1),
            EP1_RING as u32 | 1,
            0,
            8,
            0,
            0,
            0,
        ],
    );
    let (code, _) = driver.command(trb(INPUT_CONTEXT, 0, TRB_CONFIGURE_ENDPOINT, 1 << 24));
    assert_eq!(code, SUCCESS);
    assert_eq!(
        driver.context(DEVICE_CONTEXT)[3] >> 27,
        SLOT_STATE_CONFIGURED
    );

    // An 8-byte transfer waits for input.
    driver.put(EP1_RING, trb(REPORT, 8, TRB_NORMAL, TRB_ISP | TRB_IOC));
    write32(&mut driver.xhci, DOORBELLS + 4, 3);
    assert!(driver.events().is_empty(), "nothing to report yet");

    driver.tablet.update(100, 200, 1, 0);
    driver.xhci.service();
    let events = driver.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].parameter, EP1_RING);
    assert_eq!(events[0].endpoint(), 3);
    assert_eq!(events[0].status >> 24, u32::from(SHORT_PACKET));
    assert_eq!(
        events[0].status & 0xff_ffff,
        2,
        "a 6-byte report in 8 bytes"
    );
    let mut report = [0u8; 6];
    driver
        .memory
        .read_slice(&mut report, GuestAddress(REPORT))
        .unwrap();
    assert_eq!(report, [1, 100, 0, 200, 0, 0]);
}

#[test]
fn a_stalled_request_halts_endpoint_0_until_reset() {
    let mut driver = Driver::new();
    driver.events();
    driver.address_tablet();
    // GET_DESCRIPTOR(device qualifier): a full-speed device stalls it.
    let (events, _) = driver.control([0x80, 6, 0, 6, 0, 0, 10, 0], 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status >> 24, u32::from(STALL));
    assert_eq!(driver.context(DEVICE_CONTEXT + 32)[0] & 7, EP_HALTED);

    let (code, _) = driver.command(trb(0, 0, TRB_RESET_ENDPOINT, (1 << 24) | (1 << 16)));
    assert_eq!(code, SUCCESS);
    let (code, _) = driver.command(trb(
        driver.ep0 | 1,
        0,
        TRB_SET_TR_DEQUEUE,
        (1 << 24) | (1 << 16),
    ));
    assert_eq!(code, SUCCESS);
    let (events, data) = driver.control([0x80, 6, 0, 1, 0, 0, 18, 0], 18);
    assert_eq!(events[0].status >> 24, u32::from(SUCCESS), "working again");
    assert_eq!(data[1], 1);
}

#[test]
fn commands_on_slots_never_enabled_fail() {
    let mut driver = Driver::new();
    driver.events();
    let (code, _) = driver.command(trb(INPUT_CONTEXT, 0, TRB_ADDRESS_DEVICE, 5 << 24));
    assert_eq!(code, SLOT_NOT_ENABLED);
    let (code, slot) = driver.command(trb(0, 0, TRB_NO_OP_COMMAND, 0));
    assert_eq!((code, slot), (SUCCESS, 0));
}

#[test]
fn reset_halts_and_reconnects() {
    let mut driver = Driver::new();
    driver.events();
    write32(&mut driver.xhci, OPERATIONAL + USBCMD, CMD_RESET);
    assert_ne!(
        read32(&mut driver.xhci, OPERATIONAL + USBSTS) & STS_HALTED,
        0
    );
    assert_eq!(read32(&mut driver.xhci, PORTS) & PORT_CSC, PORT_CSC);
    assert_eq!(read32(&mut driver.xhci, 0) & 0xff, CAP_LENGTH as u32);
}

#[test]
fn without_msi_x_events_raise_inta_until_the_driver_clears_ip() {
    // INTA on IOAPIC pin 20: level-triggered, unmasked, vector 0x40.
    const PIN: u8 = 20;
    let sink = Arc::new(RecordingSink::default());
    let ioapic = Arc::new(Mutex::new(IoApic::new(0, sink.clone())));
    {
        let mut ioapic = ioapic.lock().unwrap();
        ioapic.write(0, &(0x10u32 + 2 * u32::from(PIN)).to_le_bytes());
        ioapic.write(0x10, &(0x40u32 | (1 << 15)).to_le_bytes());
    }
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
    let (tablet, _input) = Tablet::new();
    let mut xhci = Xhci::new(
        memory.clone(),
        Arc::new(RecordingSink::default()),
        Some(IrqLine::new(ioapic.clone(), PIN)),
        vec![Box::new(tablet)],
    );
    let mut pin = [0u8];
    xhci.config.read(config::INTERRUPT_PIN, &mut pin);
    assert_eq!(pin, [1], "INTA");
    memory.write_obj(EVENT_RING, GuestAddress(ERST)).unwrap();
    memory
        .write_obj(EVENT_RING_TRBS, GuestAddress(ERST + 8))
        .unwrap();
    write32(&mut xhci, INTERRUPTER_0 + ERSTSZ, 1);
    write64(&mut xhci, INTERRUPTER_0 + ERDP, EVENT_RING);
    write64(&mut xhci, INTERRUPTER_0 + ERSTBA, ERST);
    write32(&mut xhci, INTERRUPTER_0 + IMAN, IMAN_IE);
    // Starting with the tablet plugged in reports its port: an event, so INTA.
    write32(&mut xhci, OPERATIONAL + USBCMD, CMD_RUN | CMD_INTE);
    assert_ne!(read32(&mut xhci, INTERRUPTER_0 + IMAN) & IMAN_IP, 0);
    let fired = sink.take();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].vector(), 0x40);

    // The driver clears IP, then EOIs: the line is low, so nothing fires again.
    write32(&mut xhci, INTERRUPTER_0 + IMAN, IMAN_IP | IMAN_IE);
    assert_eq!(read32(&mut xhci, INTERRUPTER_0 + IMAN) & IMAN_IP, 0);
    ioapic.lock().unwrap().end_of_interrupt(0x40);
    assert!(sink.take().is_empty());
}

#[test]
fn msi_x_set_up_before_a_controller_reset_keeps_working() {
    // Windows' order: MSI-X on with vector 0 programmed, then HCRST, then the rings.
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
    let sink = Arc::new(RecordingSink::default());
    let (tablet, _input) = Tablet::new();
    let mut xhci = Xhci::new(memory.clone(), sink.clone(), None, vec![Box::new(tablet)]);
    let control = xhci.msix_capability + 2;
    xhci.config_write(control, &0x8000u16.to_le_bytes());
    let table = u64::from(MSIX_TABLE);
    write32(&mut xhci, table, 0xfee0_0000);
    write32(&mut xhci, table + 4, 0);
    write32(&mut xhci, table + 8, 0x51);
    write32(&mut xhci, table + 12, 0);
    write32(&mut xhci, OPERATIONAL + USBCMD, CMD_RESET);

    memory.write_obj(EVENT_RING, GuestAddress(ERST)).unwrap();
    memory
        .write_obj(EVENT_RING_TRBS, GuestAddress(ERST + 8))
        .unwrap();
    write32(&mut xhci, INTERRUPTER_0 + ERSTSZ, 1);
    write64(&mut xhci, INTERRUPTER_0 + ERDP, EVENT_RING);
    write64(&mut xhci, INTERRUPTER_0 + ERSTBA, ERST);
    write32(&mut xhci, INTERRUPTER_0 + IMAN, IMAN_IE);
    // Starting reports the plugged-in tablet: one event, one message.
    write32(&mut xhci, OPERATIONAL + USBCMD, CMD_RUN | CMD_INTE);
    let fired = sink.take();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].vector(), 0x51);
    assert_eq!(
        read32(&mut xhci, INTERRUPTER_0 + IMAN) & IMAN_IP,
        0,
        "IP clears once the message is sent"
    );
}
