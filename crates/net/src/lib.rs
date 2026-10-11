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

/// Gives a frame to the guest NIC, which may drop it if its receive ring is full.
pub type FrameSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

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
