//! The Bochs/QEMU debug console port. OVMF's debug builds log here.

use std::io::Write;

use vm_core::BusDevice;

/// OVMF's debug output port.
pub const OVMF_DEBUG_PORT: u16 = 0x402;

/// What reading the port returns: firmware checks for it before logging.
const PRESENT: u8 = 0xe9;

pub struct DebugConsole {
    out: Box<dyn Write + Send>,
}

impl DebugConsole {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out }
    }
}

impl BusDevice for DebugConsole {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(PRESENT);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        let _ = self.out.write_all(data);
        if data.contains(&b'\n') {
            let _ = self.out.flush();
        }
    }
}
