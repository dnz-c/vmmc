//! Networking for vmmc guests. Today that's user-mode NAT ([`Nat`]): the guest sits on a
//! private subnet behind a gateway that turns its connections into the host's own
//! sockets, so it needs no host drivers or administrator rights.
//!
//! A guest NIC hands transmitted frames to a [`NetBackend`] and gets received frames
//! through a [`FrameSink`].

mod dhcp;
mod dns;
mod frames;
mod nat;

use std::net::Ipv4Addr;
use std::sync::Arc;

pub use nat::Nat;

/// Where a guest NIC sends the frames the guest transmits.
pub trait NetBackend: Send + Sync {
    fn send(&self, frame: &[u8]);
}

/// A guest NIC's receive side, where the NAT hands frames for the guest.
pub trait FrameSink: Send + Sync {
    /// Gives a frame to the guest. A NIC with no room left drops it.
    fn deliver(&self, frame: &[u8]);

    /// Whether the NIC has room for more frames now. Once it has said no, it calls
    /// the waker given to [`FrameSink::set_waker`] when room frees up. The NAT holds
    /// TCP data back meanwhile: a frame the NIC drops costs a retransmission, and
    /// TCP stalls for a second or more before it retransmits.
    fn has_room(&self) -> bool {
        true
    }

    /// What to call when room frees up after [`FrameSink::has_room`] said no.
    fn set_waker(&self, _waker: Waker) {}
}

/// Wakes the NAT; see [`FrameSink::set_waker`].
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A function as a sink that always has room.
impl<F: Fn(&[u8]) + Send + Sync> FrameSink for F {
    fn deliver(&self, frame: &[u8]) {
        self(frame);
    }
}

/// The guest's network, as the NAT presents it.
#[derive(Clone, Debug)]
pub struct NatConfig {
    /// The address the DHCP server hands out.
    pub guest: Ipv4Addr,
    /// The gateway; connections to it reach the host's loopback.
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
    pub prefix: u8,
    pub gateway_mac: [u8; 6],
    /// The DNS domain DHCP offers.
    pub domain: String,
}

impl Default for NatConfig {
    /// 10.0.2.0/24, as QEMU's user networking has it: the guest is .15, the gateway .2,
    /// DNS .3.
    fn default() -> Self {
        Self {
            guest: Ipv4Addr::new(10, 0, 2, 15),
            gateway: Ipv4Addr::new(10, 0, 2, 2),
            dns: Ipv4Addr::new(10, 0, 2, 3),
            prefix: 24,
            gateway_mac: [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02],
            domain: "vmmc.internal".into(),
        }
    }
}

impl NatConfig {
    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - u32::from(self.prefix)))
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.guest) | !u32::from(self.netmask()))
    }
}

#[cfg(test)]
mod tests;
