//! 8042 keyboard controller, enough of it for guests to find one and to reset through
//! it. There is no keyboard behind it yet: keyboard commands are acknowledged, and no
//! keys ever arrive.

use std::collections::VecDeque;

use vm_core::BusDevice;

use crate::irq::IrqLine;
use crate::{PowerEvent, PowerHandler};

/// Data port; the status/command port is at `I8042_DATA + 4`.
pub const I8042_DATA: u16 = 0x60;
pub const I8042_COMMAND: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_SYSTEM: u8 = 1 << 2;
const STATUS_COMMAND: u8 = 1 << 3;
/// Keyboard not inhibited.
const STATUS_UNLOCKED: u8 = 1 << 4;

const CONFIG_KBD_INTERRUPT: u8 = 1 << 0;
const CONFIG_SYSTEM: u8 = 1 << 2;
const CONFIG_KBD_DISABLED: u8 = 1 << 4;
const CONFIG_AUX_DISABLED: u8 = 1 << 5;
const CONFIG_TRANSLATE: u8 = 1 << 6;

const ACK: u8 = 0xfa;

/// What the next byte written to the data port means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    KeyboardCommand,
    Config,
    OutputPort,
    /// A byte for the (absent) mouse.
    Aux,
    /// The argument of a keyboard command (LEDs, typematic rate, scan code set).
    KeyboardArgument,
}

pub struct I8042 {
    config: u8,
    output: VecDeque<u8>,
    pending: Pending,
    last_was_command: bool,
    irq1: IrqLine,
    power: PowerHandler,
}

impl I8042 {
    pub fn new(irq1: IrqLine, power: PowerHandler) -> Self {
        Self {
            config: CONFIG_KBD_INTERRUPT | CONFIG_SYSTEM | CONFIG_TRANSLATE | CONFIG_AUX_DISABLED,
            output: VecDeque::new(),
            pending: Pending::KeyboardCommand,
            last_was_command: false,
            irq1,
            power,
        }
    }

    fn status(&self) -> u8 {
        let mut status = STATUS_SYSTEM | STATUS_UNLOCKED;
        if !self.output.is_empty() {
            status |= STATUS_OUTPUT_FULL;
        }
        if self.last_was_command {
            status |= STATUS_COMMAND;
        }
        status
    }

    /// Queues bytes for the guest, interrupting if the keyboard interrupt is on.
    fn respond(&mut self, bytes: &[u8]) {
        self.output.extend(bytes);
        if self.config & CONFIG_KBD_INTERRUPT != 0 {
            self.irq1.pulse();
        }
    }

    fn controller_command(&mut self, command: u8) {
        match command {
            0x20 => {
                // Controller responses don't interrupt.
                self.output.push_back(self.config);
            }
            0x60 => self.pending = Pending::Config,
            0xa7 => self.config |= CONFIG_AUX_DISABLED,
            0xa8 => self.config &= !CONFIG_AUX_DISABLED,
            // Mouse port test: report a clock line stuck low, meaning no mouse.
            0xa9 => self.output.push_back(0x01),
            // Controller self-test passes.
            0xaa => self.output.push_back(0x55),
            // Keyboard interface test passes.
            0xab => self.output.push_back(0x00),
            0xad => self.config |= CONFIG_KBD_DISABLED,
            0xae => self.config &= !CONFIG_KBD_DISABLED,
            0xd1 => self.pending = Pending::OutputPort,
            0xd4 => self.pending = Pending::Aux,
            // Pulse output lines; bit 0 low pulses the CPU reset line.
            0xf0..=0xff => {
                if command & 1 == 0 {
                    (self.power)(PowerEvent::Reset);
                }
            }
            _ => tracing::debug!(command, "unhandled i8042 command"),
        }
    }

    fn data(&mut self, value: u8) {
        match std::mem::replace(&mut self.pending, Pending::KeyboardCommand) {
            Pending::Config => self.config = value,
            Pending::OutputPort => {
                if value & 1 == 0 {
                    (self.power)(PowerEvent::Reset);
                }
            }
            Pending::Aux => {}
            Pending::KeyboardArgument => self.respond(&[ACK]),
            Pending::KeyboardCommand => match value {
                // Reset: ACK, then self-test passed.
                0xff => self.respond(&[ACK, 0xaa]),
                // Identify: an MF2 keyboard.
                0xf2 => self.respond(&[ACK, 0xab, 0x83]),
                // Commands with an argument byte.
                0xed | 0xf0 | 0xf3 => {
                    self.pending = Pending::KeyboardArgument;
                    self.respond(&[ACK]);
                }
                _ => self.respond(&[ACK]),
            },
        }
    }
}

impl BusDevice for I8042 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = if offset == 0 {
            self.output.pop_front().unwrap_or(0)
        } else {
            self.status()
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.last_was_command = offset != 0;
        if offset == 0 {
            self.data(data[0]);
        } else {
            self.controller_command(data[0]);
        }
    }
}

/// The 8042's command port, as a bus device sharing the controller.
pub struct I8042CommandPort(pub std::sync::Arc<std::sync::Mutex<I8042>>);

impl BusDevice for I8042CommandPort {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        self.0.lock().unwrap().read(4, data);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        self.0.lock().unwrap().write(4, data);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::irq::testing::RecordingSink;
    use crate::legacy::ioapic::IoApic;

    fn i8042() -> (I8042, Arc<Mutex<Vec<PowerEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let ioapic = Arc::new(Mutex::new(IoApic::new(
            0,
            Arc::new(RecordingSink::default()),
        )));
        let power: PowerHandler = Arc::new({
            let events = events.clone();
            move |event| events.lock().unwrap().push(event)
        });
        (I8042::new(IrqLine::new(ioapic, 1), power), events)
    }

    fn command(dev: &mut I8042, value: u8) {
        dev.write(4, &[value]);
    }

    fn read_data(dev: &mut I8042) -> u8 {
        let mut data = [0];
        dev.read(0, &mut data);
        data[0]
    }

    fn status(dev: &mut I8042) -> u8 {
        let mut data = [0];
        dev.read(4, &mut data);
        data[0]
    }

    #[test]
    fn self_test_and_config_byte_round_trip() {
        let (mut dev, _) = i8042();
        assert_ne!(status(&mut dev), 0xff, "present");
        command(&mut dev, 0xaa);
        assert_ne!(status(&mut dev) & STATUS_OUTPUT_FULL, 0);
        assert_eq!(read_data(&mut dev), 0x55);
        assert_eq!(status(&mut dev) & STATUS_OUTPUT_FULL, 0);

        command(&mut dev, 0x60);
        dev.write(0, &[0x47]);
        command(&mut dev, 0x20);
        assert_eq!(read_data(&mut dev), 0x47);
    }

    #[test]
    fn keyboard_reset_and_identify() {
        let (mut dev, _) = i8042();
        dev.write(0, &[0xff]);
        assert_eq!((read_data(&mut dev), read_data(&mut dev)), (ACK, 0xaa));
        dev.write(0, &[0xf2]);
        assert_eq!(
            [
                read_data(&mut dev),
                read_data(&mut dev),
                read_data(&mut dev)
            ],
            [ACK, 0xab, 0x83]
        );
        // Set LEDs takes an argument; both bytes are acknowledged.
        dev.write(0, &[0xed]);
        dev.write(0, &[0x02]);
        assert_eq!((read_data(&mut dev), read_data(&mut dev)), (ACK, ACK));
    }

    #[test]
    fn pulse_reset_line_resets() {
        let (mut dev, events) = i8042();
        command(&mut dev, 0xfe);
        assert_eq!(*events.lock().unwrap(), [PowerEvent::Reset]);
    }
}
