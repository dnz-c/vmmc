//! The NAT engine: one thread that owns a smoltcp interface acting as the guest's
//! gateway, plus helper threads for host sockets.
//!
//! - **TCP:** when the guest sends a SYN to any address, a smoltcp socket starts
//!   listening on exactly that address and port (smoltcp's AnyIP mode), and a host
//!   connection opens to the real destination. Data flows between the two; closes and
//!   resets follow. Connections to the gateway address go to the host's loopback, except
//!   on the port of the config's [`LocalService`], which the service takes in-process.
//! - **UDP:** handled without smoltcp. DHCP (port 67) and DNS (to the DNS address) are
//!   answered here; every other datagram goes out through a host UDP socket per flow.
//! - **ARP and ping to the gateway:** smoltcp. ARP is only answered for the NAT's
//!   own addresses (see [`Engine::guest_frame`]).
//! - **Flow control:** while the guest's NIC is full, smoltcp sends nothing more, so
//!   TCP data waits in its buffers instead of being dropped by the NIC; the NIC wakes
//!   the engine when the guest makes room (see [`FrameSink::has_room`]).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress,
    IpAddress, IpCidr, IpListenEndpoint, IpProtocol, Ipv4Packet, TcpPacket, UdpPacket,
};

use crate::frames::{self, UdpDatagram};
use crate::local::LocalStream;
use crate::{FrameSink, NatConfig, NetBackend, Waker, dhcp, dns};

const TCP_BUFFER: usize = 256 * 1024;
/// How much host data may wait for the guest per connection before reading pauses.
const HOST_BACKLOG: usize = 512 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const UDP_IDLE: Duration = Duration::from_secs(60);
/// A listening socket nobody connected to is dropped after this.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_POLL: Duration = Duration::from_millis(250);
const MTU: usize = 1500;

/// A connection's host side: a socket, or a local service's stream.
trait HostStream: Send + Sync {
    fn read_some(&self, buffer: &mut [u8]) -> std::io::Result<usize>;
    fn write_all_of(&self, data: &[u8]) -> std::io::Result<()>;
    fn shut(&self, how: Shutdown) -> std::io::Result<()>;
}

impl HostStream for TcpStream {
    fn read_some(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&*self).read(buffer)
    }

    fn write_all_of(&self, data: &[u8]) -> std::io::Result<()> {
        (&*self).write_all(data)
    }

    fn shut(&self, how: Shutdown) -> std::io::Result<()> {
        self.shutdown(how)
    }
}

impl HostStream for LocalStream {
    fn read_some(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&*self).read(buffer)
    }

    fn write_all_of(&self, data: &[u8]) -> std::io::Result<()> {
        (&*self).write_all(data)
    }

    fn shut(&self, how: Shutdown) -> std::io::Result<()> {
        self.shutdown(how);
        Ok(())
    }
}

enum Event {
    Guest(Vec<u8>),
    Connected {
        id: u64,
        stream: Arc<dyn HostStream>,
    },
    ConnectFailed {
        id: u64,
    },
    HostData {
        id: u64,
        data: Vec<u8>,
    },
    HostClosed {
        id: u64,
    },
    /// A datagram for the guest from `from` (as the guest knows it) to `to_port`.
    Udp {
        from: SocketAddrV4,
        to_port: u16,
        data: Vec<u8>,
    },
    UdpExpired {
        key: (u16, SocketAddrV4),
    },
    /// The guest's NIC has room again.
    Room,
    Stop,
}

/// The NAT, as a guest NIC's backend. Dropping it stops the engine.
pub struct Nat {
    events: Sender<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Nat {
    /// Starts the engine. Frames for the guest go to `sink`.
    pub fn start(config: NatConfig, sink: Arc<dyn FrameSink>) -> std::io::Result<Self> {
        let (events, receiver) = mpsc::channel();
        let engine_events = events.clone();
        let waker_events = events.clone();
        let waker: Waker = Arc::new(move || {
            let _ = waker_events.send(Event::Room);
        });
        sink.set_waker(waker);
        let thread = thread::Builder::new()
            .name("net-nat".into())
            .spawn(move || Engine::new(config, sink, engine_events).run(receiver))?;
        Ok(Self {
            events,
            thread: Some(thread),
        })
    }
}

impl NetBackend for Nat {
    fn send(&self, frame: &[u8]) {
        let _ = self.events.send(Event::Guest(frame.to_vec()));
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// smoltcp's device: frames from the guest wait in a queue, frames for the guest go
/// straight to its NIC.
struct Queues {
    from_guest: VecDeque<Vec<u8>>,
    sink: Arc<dyn FrameSink>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a dyn FrameSink);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.deliver(&frame);
        result
    }
}

impl Device for Queues {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let frame = self.from_guest.pop_front()?;
        Some((Rx(frame), Tx(&*self.sink)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        self.sink.has_room().then(|| Tx(&*self.sink))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = MTU + frames::ETHERNET_HEADER;
        capabilities
    }
}

/// Host bytes waiting for the guest; the reader thread pauses while there are too many.
#[derive(Default)]
struct Backlog {
    bytes: Mutex<(usize, bool)>,
    drained: Condvar,
}

impl Backlog {
    /// Waits until there's room, then counts `len` more bytes. False once closed.
    fn add(&self, len: usize) -> bool {
        let mut state = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        while state.0 > HOST_BACKLOG && !state.1 {
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.0 += len;
        !state.1
    }

    fn remove(&self, len: usize) {
        let mut state = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = state.0.saturating_sub(len);
        self.drained.notify_all();
    }

    fn close(&self) {
        self.bytes.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.drained.notify_all();
    }
}

enum Host {
    Connecting,
    Connected {
        stream: Arc<dyn HostStream>,
        writer: Sender<Option<Vec<u8>>>,
        backlog: Arc<Backlog>,
    },
    Failed,
}

struct Connection {
    handle: SocketHandle,
    key: (u16, Ipv4Addr, u16),
    host: Host,
    /// Host data not yet in the smoltcp socket.
    to_guest: VecDeque<u8>,
    host_closed: bool,
    sent_fin: bool,
    forwarded_guest_fin: bool,
    created: std::time::Instant,
}

struct UdpFlow {
    socket: UdpSocket,
}

struct Engine {
    config: NatConfig,
    sink: Arc<dyn FrameSink>,
    events: Sender<Event>,
    iface: Interface,
    queues: Queues,
    sockets: SocketSet<'static>,
    gateway_mac: EthernetAddress,
    guest_mac: Option<EthernetAddress>,
    connections: HashMap<u64, Connection>,
    by_key: HashMap<(u16, Ipv4Addr, u16), u64>,
    next_id: u64,
    udp: HashMap<(u16, SocketAddrV4), UdpFlow>,
    epoch: std::time::Instant,
}

impl Engine {
    fn new(config: NatConfig, sink: Arc<dyn FrameSink>, events: Sender<Event>) -> Self {
        let epoch = std::time::Instant::now();
        let gateway_mac = EthernetAddress(config.gateway_mac);
        let mut queues = Queues {
            from_guest: VecDeque::new(),
            sink: sink.clone(),
        };
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(gateway_mac)),
            &mut queues,
            Instant::from_micros(0),
        );
        iface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(config.gateway), config.prefix));
            let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(config.dns), config.prefix));
        });
        // Everything off the guest's subnet is "behind" the gateway, which AnyIP then
        // accepts as local.
        let _ = iface.routes_mut().add_default_ipv4_route(config.gateway);
        iface.set_any_ip(true);
        Self {
            config,
            sink,
            events,
            iface,
            queues,
            sockets: SocketSet::new(Vec::new()),
            gateway_mac,
            guest_mac: None,
            connections: HashMap::new(),
            by_key: HashMap::new(),
            next_id: 1,
            udp: HashMap::new(),
            epoch,
        }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.epoch.elapsed().as_micros() as i64)
    }

    fn run(mut self, events: Receiver<Event>) {
        loop {
            // While the guest's NIC is full, smoltcp's timers can't send anything:
            // wait for the NIC's wake-up instead.
            let delay = if self.sink.has_room() {
                self.iface
                    .poll_delay(self.now(), &self.sockets)
                    .map_or(IDLE_POLL, |delay| {
                        Duration::from_micros(delay.total_micros())
                    })
                    .min(IDLE_POLL)
            } else {
                IDLE_POLL
            };
            let first = match events.recv_timeout(delay) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            for event in first.into_iter().chain(events.try_iter()) {
                if !self.handle(event) {
                    self.shut_down();
                    return;
                }
            }
            self.step();
        }
    }

    /// One round: smoltcp, then moving data, then smoltcp again for what that queued.
    fn step(&mut self) {
        let now = self.now();
        self.iface.poll(now, &mut self.queues, &mut self.sockets);
        self.shuttle();
        let now = self.now();
        self.iface.poll(now, &mut self.queues, &mut self.sockets);
    }

    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Guest(frame) => self.guest_frame(frame),
            Event::Connected { id, stream } => self.connected(id, stream),
            Event::ConnectFailed { id } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.host = Host::Failed;
                    self.sockets
                        .get_mut::<tcp::Socket>(connection.handle)
                        .abort();
                }
            }
            Event::HostData { id, data } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.to_guest.extend(data);
                }
            }
            Event::HostClosed { id } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.host_closed = true;
                }
            }
            Event::Udp {
                from,
                to_port,
                data,
            } => self.udp_to_guest(from, to_port, &data),
            Event::UdpExpired { key } => {
                self.udp.remove(&key);
            }
            // The step that follows sends what waited.
            Event::Room => {}
            Event::Stop => return false,
        }
        true
    }

    fn guest_frame(&mut self, frame: Vec<u8>) {
        let Ok(ethernet) = EthernetFrame::new_checked(&frame[..]) else {
            return;
        };
        let source = ethernet.src_addr();
        if source.is_unicast() {
            self.guest_mac = Some(source);
        }
        // AnyIP makes smoltcp answer ARP for every address, the guest's own included.
        // A guest checking its new address (RFC 5227, as NetworkManager does) takes
        // that for a conflict and leaves the interface without routes. So ARP only
        // reaches smoltcp for the NAT's own addresses.
        if ethernet.ethertype() == EthernetProtocol::Arp
            && let Ok(packet) = ArpPacket::new_checked(ethernet.payload())
            && let Ok(ArpRepr::EthernetIpv4 {
                target_protocol_addr,
                ..
            }) = ArpRepr::parse(&packet)
            && target_protocol_addr != self.config.gateway
            && target_protocol_addr != self.config.dns
        {
            return;
        }
        if ethernet.ethertype() == EthernetProtocol::Ipv4
            && let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload())
        {
            match ip.next_header() {
                IpProtocol::Udp => {
                    match frames::parse_udp(&frame) {
                        Some(datagram) => self.guest_udp(datagram),
                        None => tracing::debug!(
                            src = %ip.src_addr(),
                            dst = %ip.dst_addr(),
                            ip_checksum_ok = ip.verify_checksum(),
                            udp_checksum_ok = UdpPacket::new_checked(ip.payload()).is_ok_and(
                                |udp| udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into())
                            ),
                            "NAT: dropped a UDP datagram that failed its checks"
                        ),
                    }
                    return;
                }
                IpProtocol::Tcp => {
                    if let Ok(tcp) = TcpPacket::new_checked(ip.payload())
                        && tcp.syn()
                        && !tcp.ack()
                    {
                        // smoltcp drops frames whose checksums are wrong, silently.
                        tracing::debug!(
                            from_port = tcp.src_port(),
                            to = %ip.dst_addr(),
                            port = tcp.dst_port(),
                            ip_checksum_ok = ip.verify_checksum(),
                            tcp_checksum_ok =
                                tcp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()),
                            "NAT: guest opens a TCP connection"
                        );
                        self.listen_for(tcp.src_port(), ip.dst_addr(), tcp.dst_port());
                    }
                }
                _ => {}
            }
        }
        self.queues.from_guest.push_back(frame);
    }

    /// Where a connection to `address` really goes: the gateway means this host.
    fn host_address(&self, address: Ipv4Addr) -> Ipv4Addr {
        if address == self.config.gateway {
            Ipv4Addr::LOCALHOST
        } else {
            address
        }
    }

    /// Readies a smoltcp socket for the guest's SYN, and opens the host side.
    fn listen_for(&mut self, guest_port: u16, address: Ipv4Addr, port: u16) {
        let key = (guest_port, address, port);
        // The DNS address only exists for UDP. With no socket, smoltcp resets the
        // connection at once and the guest stays with UDP (Windows tries TCP too).
        if self.by_key.contains_key(&key)
            || address.is_broadcast()
            || address.is_multicast()
            || address == self.config.dns
        {
            return;
        }
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
        );
        let endpoint = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(address)),
            port,
        };
        if socket.listen(endpoint).is_err() {
            return;
        }
        socket.set_nagle_enabled(false);
        let handle = self.sockets.add(socket);
        let id = self.next_id;
        self.next_id += 1;
        self.by_key.insert(key, id);
        self.connections.insert(
            id,
            Connection {
                handle,
                key,
                host: Host::Connecting,
                to_guest: VecDeque::new(),
                host_closed: false,
                sent_fin: false,
                forwarded_guest_fin: false,
                created: std::time::Instant::now(),
            },
        );
        if address == self.config.gateway
            && let Some(service) = self
                .config
                .service
                .as_ref()
                .filter(|service| service.port == port)
        {
            let (near, far) = LocalStream::pair();
            let accept = service.accept.clone();
            let spawned = thread::Builder::new()
                .name("net-local".into())
                .spawn(move || accept(far));
            if spawned.is_ok() {
                self.connected(id, Arc::new(near));
            } else {
                let _ = self.events.send(Event::ConnectFailed { id });
            }
            return;
        }
        let destination = SocketAddr::V4(SocketAddrV4::new(self.host_address(address), port));
        let events = self.events.clone();
        let spawned = thread::Builder::new()
            .name("net-connect".into())
            .spawn(move || {
                let event = match TcpStream::connect_timeout(&destination, CONNECT_TIMEOUT) {
                    Ok(stream) => {
                        let _ = stream.set_nodelay(true);
                        tracing::debug!(peer = ?stream.peer_addr().ok(), "NAT: host connection open");
                        Event::Connected {
                            id,
                            stream: Arc::new(stream),
                        }
                    }
                    Err(e) => {
                        tracing::debug!(%destination, %e, "NAT: host connection failed");
                        Event::ConnectFailed { id }
                    }
                };
                let _ = events.send(event);
            });
        if spawned.is_err() {
            let _ = self.events.send(Event::ConnectFailed { id });
        }
    }

    fn connected(&mut self, id: u64, stream: Arc<dyn HostStream>) {
        let Some(connection) = self.connections.get_mut(&id) else {
            let _ = stream.shut(Shutdown::Both);
            return;
        };
        let backlog = Arc::new(Backlog::default());
        let (writer, writes) = mpsc::channel::<Option<Vec<u8>>>();
        // One socket, read and written from two threads: no `try_clone`, whose
        // duplicated socket can't always be shut down on Windows (WSAENOTCONN), which
        // left the host's side of a closed connection open.
        let reader = stream.clone();
        let write_stream = stream.clone();
        let events = self.events.clone();
        let reader_backlog = backlog.clone();
        let read = thread::Builder::new()
            .name("net-read".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match reader.read_some(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if !reader_backlog.add(n)
                                || events
                                    .send(Event::HostData {
                                        id,
                                        data: buffer[..n].to_vec(),
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                let _ = events.send(Event::HostClosed { id });
            });
        let write = thread::Builder::new()
            .name("net-write".into())
            .spawn(move || {
                while let Ok(Some(data)) = writes.recv() {
                    if write_stream.write_all_of(&data).is_err() {
                        break;
                    }
                }
                if let Err(e) = write_stream.shut(Shutdown::Write) {
                    tracing::debug!(%e, "NAT: can't pass the guest's FIN on");
                }
            });
        if read.is_err() || write.is_err() {
            connection.host = Host::Failed;
            self.sockets
                .get_mut::<tcp::Socket>(connection.handle)
                .abort();
            return;
        }
        connection.host = Host::Connected {
            stream,
            writer,
            backlog,
        };
    }

    /// Moves data between each smoltcp socket and its host connection, and retires
    /// connections that are done.
    fn shuttle(&mut self) {
        let mut finished = Vec::new();
        for (&id, connection) in &mut self.connections {
            let socket = self.sockets.get_mut::<tcp::Socket>(connection.handle);
            if let Host::Connected {
                writer, backlog, ..
            } = &connection.host
            {
                // Guest to host.
                while socket.can_recv() {
                    let sent = socket.recv(|data| {
                        let ok = writer.send(Some(data.to_vec())).is_ok();
                        (data.len(), ok)
                    });
                    if !matches!(sent, Ok(true)) {
                        break;
                    }
                }
                // The guest sent FIN (all its data is read by now): pass it on.
                let guest_finished = matches!(
                    socket.state(),
                    tcp::State::CloseWait
                        | tcp::State::LastAck
                        | tcp::State::Closing
                        | tcp::State::TimeWait
                        | tcp::State::Closed
                );
                if guest_finished && !socket.can_recv() && !connection.forwarded_guest_fin {
                    let _ = writer.send(None);
                    connection.forwarded_guest_fin = true;
                }
                // Host to guest.
                while !connection.to_guest.is_empty() && socket.can_send() {
                    let (front, _) = connection.to_guest.as_slices();
                    match socket.send_slice(front) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            connection.to_guest.drain(..n);
                            backlog.remove(n);
                        }
                    }
                }
                if connection.host_closed && connection.to_guest.is_empty() && !connection.sent_fin
                {
                    socket.close();
                    connection.sent_fin = true;
                }
            }
            // Done once closed (or lingering in TIME-WAIT), or if the guest never followed
            // up its SYN.
            let done = match socket.state() {
                tcp::State::Listen => connection.created.elapsed() > LISTEN_TIMEOUT,
                tcp::State::Closed | tcp::State::TimeWait => true,
                _ => false,
            };
            if done {
                finished.push(id);
            }
        }
        for id in finished {
            self.retire(id);
        }
    }

    fn retire(&mut self, id: u64) {
        let Some(connection) = self.connections.remove(&id) else {
            return;
        };
        self.by_key.remove(&connection.key);
        if let Host::Connected {
            stream, backlog, ..
        } = &connection.host
        {
            backlog.close();
            let _ = stream.shut(Shutdown::Both);
        }
        self.sockets.remove(connection.handle);
    }

    fn guest_udp(&mut self, datagram: UdpDatagram) {
        let config = self.config.clone();
        if datagram.dst.port() == 67 {
            if let Some(reply) = dhcp::reply(&config, &datagram.payload) {
                let frame = frames::udp_frame(
                    self.gateway_mac,
                    EthernetAddress::BROADCAST,
                    SocketAddrV4::new(config.gateway, 67),
                    SocketAddrV4::new(Ipv4Addr::BROADCAST, 68),
                    &reply,
                );
                self.sink.deliver(&frame);
            }
            return;
        }
        if *datagram.dst.ip() == config.dns && datagram.dst.port() == 53 {
            // Lookups can take a while; answer from a worker.
            let events = self.events.clone();
            let (from, to_port) = (datagram.dst, datagram.src.port());
            let _ = thread::Builder::new()
                .name("net-dns".into())
                .spawn(move || {
                    if let Some(data) = dns::respond(&datagram.payload, dns::host_lookup) {
                        let _ = events.send(Event::Udp {
                            from,
                            to_port,
                            data,
                        });
                    }
                });
            return;
        }
        let destination = *datagram.dst.ip();
        if destination.is_broadcast()
            || destination.is_multicast()
            || destination == config.broadcast()
            || destination.is_unspecified()
        {
            return;
        }
        let key = (datagram.src.port(), datagram.dst);
        if !self.udp.contains_key(&key) {
            match self.open_udp_flow(key) {
                Some(flow) => {
                    self.udp.insert(key, flow);
                }
                None => return,
            }
        }
        if let Some(flow) = self.udp.get(&key)
            && let Err(e) = flow.socket.send(&datagram.payload)
        {
            tracing::debug!(%e, "NAT: UDP send failed");
        }
    }

    fn open_udp_flow(&self, key: (u16, SocketAddrV4)) -> Option<UdpFlow> {
        let (guest_port, destination) = key;
        let real = SocketAddrV4::new(self.host_address(*destination.ip()), destination.port());
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect(real).ok()?;
        socket.set_read_timeout(Some(UDP_IDLE)).ok()?;
        let reader = socket.try_clone().ok()?;
        let events = self.events.clone();
        thread::Builder::new()
            .name("net-udp".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 65_536];
                while let Ok(n) = reader.recv(&mut buffer) {
                    let event = Event::Udp {
                        from: destination,
                        to_port: guest_port,
                        data: buffer[..n].to_vec(),
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                let _ = events.send(Event::UdpExpired { key });
            })
            .ok()?;
        Some(UdpFlow { socket })
    }

    fn udp_to_guest(&mut self, from: SocketAddrV4, to_port: u16, data: &[u8]) {
        let Some(guest_mac) = self.guest_mac else {
            return;
        };
        // Larger datagrams would need IP fragmentation; drop them like a small MTU would.
        if data.len() > MTU - 28 {
            return;
        }
        let frame = frames::udp_frame(
            self.gateway_mac,
            guest_mac,
            from,
            SocketAddrV4::new(self.config.guest, to_port),
            data,
        );
        self.sink.deliver(&frame);
    }

    fn shut_down(&mut self) {
        let ids: Vec<u64> = self.connections.keys().copied().collect();
        for id in ids {
            self.retire(id);
        }
        self.udp.clear();
    }
}
