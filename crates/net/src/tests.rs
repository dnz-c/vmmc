//! NAT tests with a second smoltcp stack as the guest: it gets its address over DHCP,
//! then talks TCP and UDP through the NAT to servers on the host's loopback.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use super::*;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The guest's NIC: frames it sends go to the NAT; the NAT's go into `received`.
struct GuestNic {
    nat: Arc<Nat>,
    received: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a Nat);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.send(&frame);
        result
    }
}

impl Device for GuestNic {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let frame = self.received.lock().unwrap().pop_front()?;
        Some((Rx(frame), Tx(&self.nat)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(&self.nat))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = 1514;
        capabilities
    }
}

struct Guest {
    nic: GuestNic,
    iface: Interface,
    sockets: SocketSet<'static>,
    epoch: StdInstant,
}

impl Guest {
    /// A guest that has done DHCP.
    fn new() -> Self {
        let received = Arc::new(Mutex::new(VecDeque::new()));
        let sink_queue = received.clone();
        let nat = Nat::start(
            NatConfig::default(),
            Arc::new(move |frame: &[u8]| sink_queue.lock().unwrap().push_back(frame.to_vec())),
        )
        .unwrap();
        let mut nic = GuestNic {
            nat: Arc::new(nat),
            received,
        };
        let mac = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(mac)),
            &mut nic,
            Instant::from_micros(0),
        );
        let mut guest = Self {
            nic,
            iface,
            sockets: SocketSet::new(Vec::new()),
            epoch: StdInstant::now(),
        };
        guest.dhcp();
        guest
    }

    fn poll(&mut self) {
        let now = Instant::from_micros(self.epoch.elapsed().as_micros() as i64);
        self.iface.poll(now, &mut self.nic, &mut self.sockets);
        std::thread::sleep(Duration::from_millis(1));
    }

    /// Polls until `done` says so, or panics after the timeout.
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = StdInstant::now() + TIMEOUT;
        while !done(self) {
            assert!(StdInstant::now() < deadline, "timed out waiting for {what}");
            self.poll();
        }
    }

    fn dhcp(&mut self) {
        let handle = self.sockets.add(dhcpv4::Socket::new());
        let mut lease = None;
        self.until("a DHCP lease", |guest| {
            if let Some(dhcpv4::Event::Configured(config)) =
                guest.sockets.get_mut::<dhcpv4::Socket>(handle).poll()
            {
                lease = Some((config.address, config.router, config.dns_servers.to_vec()));
            }
            lease.is_some()
        });
        let (address, router, dns) = lease.unwrap();
        let config = NatConfig::default();
        assert_eq!(
            (address.address(), address.prefix_len()),
            (config.guest, 24)
        );
        assert_eq!(router, Some(config.gateway));
        assert_eq!(dns, [config.dns]);
        self.iface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::Ipv4(address));
        });
        let _ = self
            .iface
            .routes_mut()
            .add_default_ipv4_route(config.gateway);
    }

    fn tcp_socket(&mut self) -> SocketHandle {
        self.sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 64 * 1024]),
            tcp::SocketBuffer::new(vec![0; 64 * 1024]),
        ))
    }

    fn udp_socket(&mut self, port: u16) -> SocketHandle {
        let mut socket = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
        );
        socket.bind(port).unwrap();
        self.sockets.add(socket)
    }
}

fn gateway(port: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(NatConfig::default().gateway), port)
}

#[test]
fn tcp_through_the_gateway_reaches_host_loopback() {
    // An echo server on the host.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => stream.write_all(&buffer[..n]).unwrap(),
            }
        }
    });

    let mut guest = Guest::new();
    let handle = guest.tcp_socket();
    let context = guest.iface.context();
    guest
        .sockets
        .get_mut::<tcp::Socket>(handle)
        .connect(context, gateway(port), 49_152)
        .unwrap();
    guest.until("the connection", |guest| {
        guest.sockets.get::<tcp::Socket>(handle).may_send()
    });

    // More than the buffers hold, so flow control has to work.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut sent = 0;
    let mut echoed = Vec::new();
    guest.until("the echo", |guest| {
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        if sent < data.len() && socket.can_send() {
            sent += socket.send_slice(&data[sent..]).unwrap();
        }
        while socket.can_recv() {
            socket
                .recv(|chunk| {
                    echoed.extend_from_slice(chunk);
                    (chunk.len(), ())
                })
                .unwrap();
        }
        echoed.len() == data.len()
    });
    assert_eq!(echoed, data);

    guest.sockets.get_mut::<tcp::Socket>(handle).close();
    guest.until("the close", |guest| {
        !guest.sockets.get::<tcp::Socket>(handle).is_open()
            || guest.sockets.get::<tcp::Socket>(handle).state() == tcp::State::TimeWait
    });
    server.join().unwrap();
}

#[test]
fn refused_connections_are_reset() {
    // A port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut guest = Guest::new();
    let handle = guest.tcp_socket();
    let context = guest.iface.context();
    guest
        .sockets
        .get_mut::<tcp::Socket>(handle)
        .connect(context, gateway(port), 49_153)
        .unwrap();
    guest.until("the reset", |guest| {
        guest.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
    });
}

#[test]
fn dns_answers_from_the_host_resolver() {
    let mut guest = Guest::new();
    let handle = guest.udp_socket(50_000);
    let config = NatConfig::default();
    // A query for "localhost", type A.
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 9];
    query.extend_from_slice(b"localhost");
    query.extend_from_slice(&[0, 0, 1, 0, 1]);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(&query, IpEndpoint::new(IpAddress::Ipv4(config.dns), 53))
        .unwrap();
    let mut answer = Vec::new();
    guest.until("the DNS answer", |guest| {
        if let Ok((data, _)) = guest.sockets.get_mut::<udp::Socket>(handle).recv() {
            answer = data.to_vec();
        }
        !answer.is_empty()
    });
    assert_eq!(&answer[..2], &[0x12, 0x34]);
    assert!(answer.windows(4).any(|w| w == [127, 0, 0, 1]), "{answer:?}");
}

#[test]
fn udp_flows_reach_the_host_and_back() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let mut buffer = [0u8; 1500];
        let (n, from) = server.recv_from(&mut buffer).unwrap();
        server.send_to(&buffer[..n], from).unwrap();
    });

    let mut guest = Guest::new();
    let handle = guest.udp_socket(50_001);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"vmmc udp", gateway(port))
        .unwrap();
    let mut reply = None;
    guest.until("the UDP echo", |guest| {
        if let Ok((data, meta)) = guest.sockets.get_mut::<udp::Socket>(handle).recv() {
            reply = Some((data.to_vec(), meta.endpoint));
        }
        reply.is_some()
    });
    let (data, from) = reply.unwrap();
    assert_eq!(data, b"vmmc udp");
    assert_eq!(
        from,
        gateway(port),
        "the reply comes from where the guest sent it"
    );
    echo.join().unwrap();
}
