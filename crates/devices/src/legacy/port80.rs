//! Port 0x80, the POST diagnostic port. Guests also write it just to pause briefly
//! between slow I/O accesses. Claiming it keeps those writes out of the unclaimed-access
//! logs.

use vm_core::BusDevice;

use crate::snapshot::{self, Snapshot, StateError};

pub const POST_PORT: u16 = 0x80;

#[derive(Debug, Default)]
pub struct PostPort {
    last: u8,
}

impl Snapshot for PostPort {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&self.last)
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        self.last = snapshot::decode(state)?;
        Ok(())
    }
}

impl BusDevice for PostPort {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(self.last);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        self.last = data[0];
    }
}
