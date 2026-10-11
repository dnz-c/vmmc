//! Building and taking apart the Ethernet/IPv4/UDP frames the NAT handles itself.

use std::net::SocketAddrV4;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Packet,
    Ipv4Repr, UdpPacket, UdpRepr,
};

pub const ETHERNET_HEADER: usize = 14;
const IPV4_HEADER: usize = 20;
const UDP_HEADER: usize = 8;

/// A UDP datagram the guest sent.
#[derive(Debug, PartialEq, Eq)]
pub struct UdpDatagram {
    pub src_mac: EthernetAddress,
    pub src: SocketAddrV4,
    pub dst: SocketAddrV4,
    pub payload: Vec<u8>,
}

/// The UDP datagram in `frame`, if it is one (and passes its checksums).
pub fn parse_udp(frame: &[u8]) -> Option<UdpDatagram> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp || !ip.verify_checksum() {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    let (src_ip, dst_ip) = (ip.src_addr(), ip.dst_addr());
    if !udp.verify_checksum(&src_ip.into(), &dst_ip.into()) {
        return None;
    }
    Some(UdpDatagram {
        src_mac: ethernet.src_addr(),
        src: SocketAddrV4::new(src_ip, udp.src_port()),
        dst: SocketAddrV4::new(dst_ip, udp.dst_port()),
        payload: udp.payload().to_vec(),
    })
}

/// A complete Ethernet frame carrying a UDP datagram.
pub fn udp_frame(
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src: SocketAddrV4,
    dst: SocketAddrV4,
    payload: &[u8],
) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();
    let udp = UdpRepr {
        src_port: src.port(),
        dst_port: dst.port(),
    };
    let ip = Ipv4Repr {
        src_addr: *src.ip(),
        dst_addr: *dst.ip(),
        next_header: IpProtocol::Udp,
        payload_len: UDP_HEADER + payload.len(),
        hop_limit: 64,
    };
    let ethernet = EthernetRepr {
        src_addr: src_mac,
        dst_addr: dst_mac,
        ethertype: EthernetProtocol::Ipv4,
    };
    let mut frame = vec![0u8; ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER + payload.len()];
    let mut ethernet_frame = EthernetFrame::new_unchecked(&mut frame[..]);
    ethernet.emit(&mut ethernet_frame);
    let mut ip_packet = Ipv4Packet::new_unchecked(ethernet_frame.payload_mut());
    ip.emit(&mut ip_packet, &checksums);
    let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
    udp.emit(
        &mut udp_packet,
        &(*src.ip()).into(),
        &(*dst.ip()).into(),
        payload.len(),
        |buffer| buffer.copy_from_slice(payload),
        &checksums,
    );
    frame
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn udp_frames_round_trip() {
        let src = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 5000);
        let dst = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 53);
        let mac = EthernetAddress([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let frame = udp_frame(mac, EthernetAddress::BROADCAST, src, dst, b"hello");
        let datagram = parse_udp(&frame).unwrap();
        assert_eq!(datagram.src, src);
        assert_eq!(datagram.dst, dst);
        assert_eq!(datagram.src_mac, mac);
        assert_eq!(datagram.payload, b"hello");

        let mut corrupt = frame.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(parse_udp(&corrupt), None, "bad UDP checksum");
    }
}
