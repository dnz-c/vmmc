//! USB: an xHCI host controller and the HID devices behind its root ports.

pub mod hid;
pub mod xhci;

/// A control transfer's setup packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    /// Parses the 8 bytes of a setup packet (little-endian fields).
    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        Self {
            request_type: bytes[0],
            request: bytes[1],
            value: u16::from_le_bytes([bytes[2], bytes[3]]),
            index: u16::from_le_bytes([bytes[4], bytes[5]]),
            length: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }

    /// Whether data flows to the host.
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// A device on a root port.
pub trait UsbDevice: Send {
    /// Its speed, as an xHCI protocol speed ID: 1 full, 2 low, 3 high.
    fn speed(&self) -> u8;
    /// A control transfer on endpoint 0, with the data stage's bytes for OUT
    /// transfers. Returns the data for IN transfers (the controller trims it to the
    /// requested length), or `None` to stall.
    fn control(&mut self, setup: &Setup, data: &[u8]) -> Option<Vec<u8>>;
    /// The next report for interrupt IN endpoint `endpoint` (its number, 1-15), if the
    /// device has one to send.
    fn interrupt_in(&mut self, endpoint: u8) -> Option<Vec<u8>>;
    /// A port or device reset: back to unconfigured, with nothing pending.
    fn reset(&mut self);
}
