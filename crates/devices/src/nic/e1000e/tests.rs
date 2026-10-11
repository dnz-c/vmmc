//! e1000e tests, driven the way a driver does: registers, descriptor rings in guest
//! memory, and the frames that come out of the backend.

use super::*;
use crate::irq::testing::RecordingSink;

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef];
const TX_RING: u64 = 0x1_0000;
const RX_RING: u64 = 0x2_0000;
const TX_DATA: u64 = 0x10_0000;
const RX_BUFFERS: u64 = 0x20_0000;

#[derive(Default)]
struct Recorder(Mutex<Vec<Vec<u8>>>);

impl NetBackend for Recorder {
    fn send(&self, frame: &[u8]) {
        self.0.lock().unwrap().push(frame.to_vec());
    }
}

struct Nic {
    device: E1000e,
    memory: GuestMemoryMmap,
    sent: Arc<Recorder>,
    sink: Arc<RecordingSink>,
}

impl Nic {
    fn new() -> Self {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap();
        let sink = Arc::new(RecordingSink::default());
        let mut device = E1000e::new(MAC, memory.clone(), sink.clone(), None);
        let sent = Arc::new(Recorder::default());
        device.set_backend(sent.clone());
        Self {
            device,
            memory,
            sent,
            sink,
        }
    }

    fn read(&mut self, offset: usize) -> u32 {
        let mut data = [0u8; 4];
        self.device.bar_read(REGISTER_BAR, offset as u64, &mut data);
        u32::from_le_bytes(data)
    }

    fn write(&mut self, offset: usize, value: u32) {
        self.device
            .bar_write(REGISTER_BAR, offset as u64, &value.to_le_bytes());
    }

    fn eeprom(&mut self, word: u32) -> u16 {
        self.write(EERD, (word << 2) | EERD_START);
        let value = self.read(EERD);
        assert_ne!(value & EERD_DONE, 0);
        (value >> 16) as u16
    }

    fn phy(&mut self, register: u32) -> u16 {
        self.write(MDIC, (register << 16) | (1 << 21) | (0b10 << 26));
        let value = self.read(MDIC);
        assert_ne!(value & MDIC_READY, 0);
        assert_eq!(value & MDIC_ERROR, 0);
        value as u16
    }

    fn setup_tx(&mut self) {
        self.write(TDBAL, TX_RING as u32);
        self.write(TDBAH, 0);
        self.write(TDLEN, 16 * DESCRIPTOR as u32);
        self.write(TDH, 0);
        self.write(TDT, 0);
        self.write(TCTL, TCTL_EN);
    }

    fn setup_rx(&mut self, extended: bool, buffer_size_code: u32) {
        for i in 0..16u64 {
            let mut descriptor = [0u8; 16];
            descriptor[..8].copy_from_slice(&(RX_BUFFERS + i * 0x1000).to_le_bytes());
            self.memory
                .write_slice(&descriptor, GuestAddress(RX_RING + i * DESCRIPTOR))
                .unwrap();
        }
        self.write(RDBAL, RX_RING as u32);
        self.write(RDLEN, 16 * DESCRIPTOR as u32);
        self.write(RDH, 0);
        self.write(RDT, 15);
        if extended {
            self.write(RFCTL, RFCTL_EXSTEN);
        }
        self.write(RCTL, RCTL_EN | RCTL_BAM | (buffer_size_code << 16));
    }

    fn descriptor(&self, ring: u64, index: u64) -> [u8; 16] {
        let mut descriptor = [0u8; 16];
        self.memory
            .read_slice(&mut descriptor, GuestAddress(ring + index * DESCRIPTOR))
            .unwrap();
        descriptor
    }

    fn put_descriptor(&self, index: u64, descriptor: [u8; 16]) {
        self.memory
            .write_slice(&descriptor, GuestAddress(TX_RING + index * DESCRIPTOR))
            .unwrap();
    }
}

fn legacy_tx(address: u64, len: u16, cmd: u8) -> [u8; 16] {
    let mut descriptor = [0u8; 16];
    descriptor[..8].copy_from_slice(&address.to_le_bytes());
    descriptor[8..10].copy_from_slice(&len.to_le_bytes());
    descriptor[11] = cmd;
    descriptor
}

#[test]
fn eeprom_and_phy_identify_the_part() {
    let mut nic = Nic::new();
    let mac: Vec<u8> = (0..3).flat_map(|w| nic.eeprom(w).to_le_bytes()).collect();
    assert_eq!(mac, MAC);
    let sum = (0..64).fold(0u16, |sum, word| sum.wrapping_add(nic.eeprom(word)));
    assert_eq!(sum, 0xbaba, "EEPROM checksum");
    assert_eq!((nic.phy(2), nic.phy(3)), (PHY_ID1, PHY_ID2));
    assert_ne!(nic.phy(1) & (1 << 2), 0, "link up");
    assert_eq!(nic.read(STATUS) & 0b11, 0b11, "full duplex, link up");
    assert_eq!(nic.read(RAH0) & RAH_AV, RAH_AV);

    // The software semaphore: the first read takes it, the next sees it taken.
    assert_eq!(nic.read(SWSM) & SWSM_SMBI, 0);
    assert_eq!(nic.read(SWSM) & SWSM_SMBI, SWSM_SMBI);
    nic.write(SWSM, 0);
    assert_eq!(nic.read(SWSM) & SWSM_SMBI, 0);
}

#[test]
fn transmits_legacy_descriptors_and_reports_done() {
    let mut nic = Nic::new();
    nic.setup_tx();
    nic.write(IMS, ICR_TXDW);
    let frame: Vec<u8> = (0..100).map(|i| i as u8).collect();
    nic.memory
        .write_slice(&frame, GuestAddress(TX_DATA))
        .unwrap();
    // Split across two descriptors.
    nic.put_descriptor(0, legacy_tx(TX_DATA, 40, 0));
    nic.put_descriptor(1, legacy_tx(TX_DATA + 40, 60, TX_CMD_EOP | (1 << 3)));
    nic.write(TDT, 2);
    assert_eq!(*nic.sent.0.lock().unwrap(), [frame]);
    assert_eq!(nic.read(TDH), 2);
    assert_ne!(nic.descriptor(TX_RING, 1)[12] & TX_STA_DD, 0);
    assert_eq!(nic.read(GPTC), 1);
    let icr = nic.read(ICR);
    assert_ne!(icr & ICR_TXDW, 0);
    assert_eq!(nic.read(ICR), 0, "read clears");
}

/// An IPv4/TCP packet: 14 + 20 + 20 bytes of headers and `payload` bytes.
fn tcp_packet(payload: usize) -> Vec<u8> {
    let mut packet = vec![0u8; 54 + payload];
    packet[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    packet[6..12].copy_from_slice(&MAC);
    packet[12..14].copy_from_slice(&[0x08, 0x00]);
    packet[14] = 0x45;
    packet[16..18].copy_from_slice(&((40 + payload) as u16).to_be_bytes());
    packet[18..20].copy_from_slice(&0x1234u16.to_be_bytes());
    packet[22] = 64;
    packet[23] = 6;
    packet[26..30].copy_from_slice(&[10, 0, 2, 15]);
    packet[30..34].copy_from_slice(&[93, 184, 216, 34]);
    packet[34..36].copy_from_slice(&40000u16.to_be_bytes());
    packet[36..38].copy_from_slice(&443u16.to_be_bytes());
    packet[38..42].copy_from_slice(&1000u32.to_be_bytes());
    packet[46] = 0x50;
    packet[47] = 0x18; // PSH, ACK
    for (i, byte) in packet[54..].iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    // What the driver leaves in the TCP checksum field for TSO: the pseudo-header sum
    // without the length.
    let pseudo = sum16(&packet[26..34], 6);
    packet[50..52].copy_from_slice(&fold(pseudo).to_be_bytes());
    packet
}

fn tcp_checksum_ok(segment: &[u8]) -> bool {
    let tcp_len = segment.len() - 34;
    let mut sum = sum16(&segment[26..34], 6 + tcp_len as u32);
    sum = sum16(&segment[34..], sum);
    fold(sum) == 0xffff
}

#[test]
fn tso_splits_into_mss_segments_with_valid_checksums() {
    let mut nic = Nic::new();
    nic.setup_tx();
    let packet = tcp_packet(2500);
    nic.memory
        .write_slice(&packet, GuestAddress(TX_DATA))
        .unwrap();
    // Context: IP at 14 (checksum at 24), TCP at 34 (checksum at 50), TCP+IPv4+TSE,
    // 54 header bytes, MSS 1000.
    let mut context = [0u8; 16];
    context[0] = 14;
    context[1] = 24;
    context[4] = 34;
    context[5] = 50;
    let payload_len = (packet.len() - 54) as u32;
    context[8..12].copy_from_slice(&(payload_len | (0b0111 << 24) | (1 << 29)).to_le_bytes());
    context[13] = 54;
    context[14..16].copy_from_slice(&1000u16.to_le_bytes());
    nic.put_descriptor(0, context);
    // Data descriptor: DEXT, DTYP 1, EOP|TSE|RS, IXSM|TXSM.
    let mut data = [0u8; 16];
    data[..8].copy_from_slice(&TX_DATA.to_le_bytes());
    let len = packet.len() as u32;
    data[8..12].copy_from_slice(&(len | (1 << 20)).to_le_bytes());
    data[11] = TX_CMD_DEXT | TX_CMD_EOP | TX_CMD_TSE | (1 << 3);
    data[13] = TX_POPTS_IXSM | TX_POPTS_TXSM;
    nic.put_descriptor(1, data);
    nic.write(TDT, 2);

    let sent = nic.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 3);
    let mut payload = Vec::new();
    for (index, segment) in sent.iter().enumerate() {
        assert_eq!(
            u16::from_be_bytes([segment[16], segment[17]]) as usize,
            segment.len() - 14,
            "IP total length"
        );
        assert_eq!(fold(sum16(&segment[14..34], 0)), 0xffff, "IP checksum");
        assert_eq!(
            u16::from_be_bytes([segment[18], segment[19]]),
            0x1234 + index as u16
        );
        let seq = u32::from_be_bytes(segment[38..42].try_into().unwrap());
        assert_eq!(seq, 1000 + 1000 * index as u32);
        assert!(tcp_checksum_ok(segment), "TCP checksum of segment {index}");
        let psh = segment[47] & 0x08 != 0;
        assert_eq!(psh, index == 2, "PSH only on the last");
        payload.extend_from_slice(&segment[54..]);
    }
    assert_eq!(payload, packet[54..]);
}

#[test]
fn receives_into_legacy_and_extended_descriptors() {
    for extended in [false, true] {
        let mut nic = Nic::new();
        nic.setup_rx(extended, 0);
        nic.write(IMS, ICR_RXT0);
        let receiver = nic.device.receiver();
        let mut frame = MAC.to_vec();
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0x08, 0x00]);
        frame.extend((0..100).map(|i| i as u8));
        receiver.deliver(&frame);
        // Not for us: dropped.
        let mut other = frame.clone();
        other[5] ^= 1;
        receiver.deliver(&other);

        let mut buffer = vec![0u8; frame.len()];
        nic.memory
            .read_slice(&mut buffer, GuestAddress(RX_BUFFERS))
            .unwrap();
        assert_eq!(buffer, frame);
        let descriptor = nic.descriptor(RX_RING, 0);
        let (len, status) = if extended {
            (
                u16::from_le_bytes([descriptor[12], descriptor[13]]),
                descriptor[8],
            )
        } else {
            (
                u16::from_le_bytes([descriptor[8], descriptor[9]]),
                descriptor[12],
            )
        };
        assert_eq!(usize::from(len), frame.len());
        assert_eq!(status & (RX_STA_DD | RX_STA_EOP), RX_STA_DD | RX_STA_EOP);
        assert_eq!(nic.read(RDH), 1, "one frame taken");
        assert_ne!(nic.read(ICR) & ICR_RXT0, 0);
        assert!(nic.sink.take().is_empty(), "no MSI configured");
    }
}

#[test]
fn large_frames_span_several_small_buffers() {
    let mut nic = Nic::new();
    // BSIZE 01: 1024-byte buffers.
    nic.setup_rx(false, 1);
    let receiver = nic.device.receiver();
    let mut frame = vec![0xff; 6];
    frame.extend((0..2500).map(|i| i as u8));
    receiver.deliver(&frame);
    assert_eq!(nic.read(RDH), 3);
    let last = nic.descriptor(RX_RING, 2);
    assert_ne!(last[12] & RX_STA_EOP, 0);
    assert_eq!(nic.descriptor(RX_RING, 0)[12] & RX_STA_EOP, 0);
}

#[test]
fn interrupts_go_out_as_msi_when_enabled() {
    let mut nic = Nic::new();
    let msi = nic.device.msi_capability;
    nic.device
        .config_write(msi + 4, &0xfee0_0000u32.to_le_bytes());
    nic.device.config_write(msi + 12, &0x0051u16.to_le_bytes());
    nic.device.config_write(msi + 2, &1u16.to_le_bytes());
    nic.sink.take();
    nic.write(ICS, ICR_LSC);
    assert!(nic.sink.take().is_empty(), "masked");
    nic.write(IMS, ICR_LSC);
    assert!(nic.sink.take().iter().any(|m| m.vector() == 0x51));
}

#[test]
fn link_change_is_latched_at_reset_and_auto_mask_follows_icr_reads() {
    // Linux's MSI self-test: clear ICR, unmask, fire RXSEQ through ICS, and expect an
    // interrupt with RXSEQ set. A stale link change must not get in the way.
    let mut nic = Nic::new();
    let msi = nic.device.msi_capability;
    nic.device
        .config_write(msi + 4, &0xfee0_0000u32.to_le_bytes());
    nic.device.config_write(msi + 12, &0x0052u16.to_le_bytes());
    nic.device.config_write(msi + 2, &1u16.to_le_bytes());
    assert_ne!(nic.read(ICR) & ICR_LSC, 0, "link up after reset");
    nic.write(CTRL_EXT, CTRL_EXT_IAME);
    nic.write(IAM, u32::MAX);
    nic.sink.take();
    nic.write(IMS, ICR_LSC | (1 << 3));
    assert!(
        nic.sink.take().is_empty(),
        "nothing pending after the clearing read"
    );
    nic.write(ICS, 1 << 3);
    assert_eq!(nic.sink.take().len(), 1);
    assert_ne!(nic.read(ICR) & (1 << 3), 0);
    assert_eq!(nic.read(IMS), 0, "the ICR read auto-masked everything");
}
