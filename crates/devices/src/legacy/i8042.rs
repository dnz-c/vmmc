//! 8042 keyboard controller, with a PS/2 keyboard and a PS/2 (IntelliMouse) mouse.
//!
//! - **Controller:** one output buffer shared by the keyboard and the mouse (the AUX
//!   port). A byte moving into it raises IRQ 1 or IRQ 12 if the configuration byte
//!   enables that, and status bit 5 says it came from the mouse. A disabled port's
//!   bytes wait until it's enabled.
//! - **Keyboard:** the host sends keys as scan code set 1 (`0xE0nn` for extended keys).
//!   With the controller translating, as Windows and Linux both have it, that's what
//!   the guest reads. Without translation the keyboard sends set 2.
//! - **Mouse:** relative motion and buttons in 3-byte packets, or 4 with a wheel once
//!   the guest switches it to IntelliMouse mode (ID 3).
//! - Pulsing the reset line resets the machine.

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
/// The byte in the output buffer is from the mouse.
const STATUS_AUX: u8 = 1 << 5;

const CONFIG_KBD_INTERRUPT: u8 = 1 << 0;
const CONFIG_AUX_INTERRUPT: u8 = 1 << 1;
const CONFIG_SYSTEM: u8 = 1 << 2;
const CONFIG_KBD_DISABLED: u8 = 1 << 4;
const CONFIG_AUX_DISABLED: u8 = 1 << 5;
const CONFIG_TRANSLATE: u8 = 1 << 6;

const ACK: u8 = 0xfa;
const RESEND: u8 = 0xfe;
const SELF_TEST_PASSED: u8 = 0xaa;

/// Where a byte in the output buffer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Controller,
    Keyboard,
    Mouse,
}

/// What the next byte written to the data port means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    /// A byte for the keyboard.
    Keyboard,
    Config,
    OutputPort,
    /// Bytes to place in the output buffer as if the keyboard or mouse sent them.
    FakeKeyboard,
    FakeMouse,
    /// A byte for the mouse.
    Mouse,
}

pub struct I8042 {
    config: u8,
    pending: Pending,
    last_was_command: bool,
    /// The byte the guest reads next, and where it came from.
    buffer: Option<(u8, Source)>,
    controller: VecDeque<u8>,
    keyboard: Keyboard,
    mouse: Mouse,
    irq1: IrqLine,
    irq12: IrqLine,
    power: PowerHandler,
}

impl I8042 {
    pub fn new(irq1: IrqLine, irq12: IrqLine, power: PowerHandler) -> Self {
        Self {
            config: CONFIG_KBD_INTERRUPT | CONFIG_SYSTEM | CONFIG_TRANSLATE | CONFIG_AUX_DISABLED,
            pending: Pending::Keyboard,
            last_was_command: false,
            buffer: None,
            controller: VecDeque::new(),
            keyboard: Keyboard::new(),
            mouse: Mouse::new(),
            irq1,
            irq12,
            power,
        }
    }

    /// A key, as scan code set 1 (`0xE0nn` for an extended key, [`PAUSE`] for Pause),
    /// went down or up on the host.
    pub fn key(&mut self, scancode: u16, pressed: bool) {
        let translate = self.config & CONFIG_TRANSLATE != 0;
        self.keyboard.key(scancode, pressed, translate);
        self.refill();
    }

    /// The host pointer moved by `(dx, dy)` (right and down are positive) and the
    /// wheel by `dz` (towards the user is positive), with these buttons held (bit 0
    /// left, 1 right, 2 middle).
    pub fn mouse_motion(&mut self, dx: i32, dy: i32, dz: i32, buttons: u8) {
        self.mouse.motion(dx, dy, dz, buttons);
        self.refill();
    }

    fn status(&self) -> u8 {
        let mut status = STATUS_SYSTEM | STATUS_UNLOCKED;
        if let Some((_, source)) = self.buffer {
            status |= STATUS_OUTPUT_FULL;
            if source == Source::Mouse {
                status |= STATUS_AUX;
            }
        }
        if self.last_was_command {
            status |= STATUS_COMMAND;
        }
        status
    }

    /// Moves the next waiting byte into the empty output buffer and interrupts for it.
    fn refill(&mut self) {
        if self.buffer.is_some() {
            return;
        }
        let next = if let Some(byte) = self.controller.pop_front() {
            Some((byte, Source::Controller))
        } else if self.config & CONFIG_KBD_DISABLED == 0
            && let Some(byte) = self.keyboard.output.pop_front()
        {
            Some((byte, Source::Keyboard))
        } else if self.config & CONFIG_AUX_DISABLED == 0
            && let Some(byte) = self.mouse.output.pop_front()
        {
            Some((byte, Source::Mouse))
        } else {
            None
        };
        self.buffer = next;
        match next {
            Some((_, Source::Keyboard)) if self.config & CONFIG_KBD_INTERRUPT != 0 => {
                self.irq1.pulse();
            }
            Some((_, Source::Mouse)) if self.config & CONFIG_AUX_INTERRUPT != 0 => {
                self.irq12.pulse();
            }
            // Controller responses don't interrupt; drivers poll for them.
            _ => {}
        }
    }

    fn controller_command(&mut self, command: u8) {
        match command {
            0x20 => self.controller.push_back(self.config),
            0x60 => self.pending = Pending::Config,
            0xa7 => self.config |= CONFIG_AUX_DISABLED,
            0xa8 => self.config &= !CONFIG_AUX_DISABLED,
            // Mouse port test: passes, there's a mouse.
            0xa9 => self.controller.push_back(0x00),
            // Controller self-test passes.
            0xaa => self.controller.push_back(0x55),
            // Keyboard interface test passes.
            0xab => self.controller.push_back(0x00),
            0xad => self.config |= CONFIG_KBD_DISABLED,
            0xae => self.config &= !CONFIG_KBD_DISABLED,
            // Input port: keyboard not inhibited, colour display.
            0xc0 => self.controller.push_back(0xa0),
            // Output port: A20 on, reset line high.
            0xd0 => self.controller.push_back(0x03),
            0xd1 => self.pending = Pending::OutputPort,
            0xd2 => self.pending = Pending::FakeKeyboard,
            0xd3 => self.pending = Pending::FakeMouse,
            0xd4 => self.pending = Pending::Mouse,
            // Pulse output lines; bit 0 low pulses the CPU reset line.
            0xf0..=0xff => {
                if command & 1 == 0 {
                    (self.power)(PowerEvent::Reset);
                }
            }
            _ => tracing::debug!(command, "unhandled i8042 command"),
        }
        self.refill();
    }

    fn data(&mut self, value: u8) {
        match std::mem::replace(&mut self.pending, Pending::Keyboard) {
            Pending::Config => self.config = value,
            Pending::OutputPort => {
                if value & 1 == 0 {
                    (self.power)(PowerEvent::Reset);
                }
            }
            Pending::FakeKeyboard => self.keyboard.output.push_back(value),
            Pending::FakeMouse => self.mouse.output.push_back(value),
            Pending::Mouse => self.mouse.command(value),
            Pending::Keyboard => {
                // Writing the data port addresses the keyboard, enabling its port.
                self.config &= !CONFIG_KBD_DISABLED;
                self.keyboard.command(value);
            }
        }
        self.refill();
    }
}

impl BusDevice for I8042 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = if offset == 0 {
            let value = self.buffer.take().map_or(0, |(byte, _)| byte);
            self.refill();
            value
        } else {
            self.status()
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        tracing::trace!(offset, value = data[0], "i8042 write");
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

/// The Pause key's pseudo scan code: it has no break code and an odd make sequence.
pub const PAUSE: u16 = 0xe11d;

/// What the next byte written to the keyboard means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardArgument {
    None,
    Leds,
    ScanCodeSet,
    Typematic,
}

/// A PS/2 MF2 keyboard.
struct Keyboard {
    output: VecDeque<u8>,
    argument: KeyboardArgument,
    scanning: bool,
    /// The scan code set the keyboard sends: 1 or 2 (set 3 is treated as 2).
    set: u8,
    leds: u8,
    /// The last byte sent, for Resend.
    last: u8,
}

impl Keyboard {
    fn new() -> Self {
        Self {
            output: VecDeque::new(),
            argument: KeyboardArgument::None,
            scanning: true,
            set: 2,
            leds: 0,
            last: 0,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Some(&last) = bytes.last() {
            self.last = last;
        }
        self.output.extend(bytes);
    }

    fn command(&mut self, value: u8) {
        match std::mem::replace(&mut self.argument, KeyboardArgument::None) {
            KeyboardArgument::Leds => {
                self.leds = value & 0x07;
                return self.send(&[ACK]);
            }
            KeyboardArgument::Typematic => return self.send(&[ACK]),
            KeyboardArgument::ScanCodeSet => {
                match value {
                    0 => self.send(&[ACK, self.set]),
                    1 | 2 => {
                        self.set = value;
                        self.send(&[ACK]);
                    }
                    3 => {
                        self.set = 2;
                        self.send(&[ACK]);
                    }
                    _ => self.send(&[RESEND]),
                }
                return;
            }
            KeyboardArgument::None => {}
        }
        match value {
            0xed => {
                self.argument = KeyboardArgument::Leds;
                self.send(&[ACK]);
            }
            0xee => self.send(&[0xee]),
            0xf0 => {
                self.argument = KeyboardArgument::ScanCodeSet;
                self.send(&[ACK]);
            }
            // Identify: an MF2 keyboard.
            0xf2 => self.send(&[ACK, 0xab, 0x83]),
            0xf3 => {
                self.argument = KeyboardArgument::Typematic;
                self.send(&[ACK]);
            }
            0xf4 => {
                self.scanning = true;
                self.send(&[ACK]);
            }
            // Disable (reverting to defaults), and set defaults.
            0xf5 | 0xf6 => {
                self.scanning = value == 0xf6;
                self.set = 2;
                self.output.clear();
                self.send(&[ACK]);
            }
            0xfe => {
                let last = self.last;
                self.output.push_back(last);
            }
            0xff => {
                *self = Self::new();
                self.send(&[ACK, SELF_TEST_PASSED]);
            }
            // The set 3 per-key commands and anything else: acknowledged, ignored.
            _ => self.send(&[ACK]),
        }
    }

    fn key(&mut self, scancode: u16, pressed: bool, translate: bool) {
        if !self.scanning {
            return;
        }
        // The controller turns set 2 back into set 1 when translating, so either way
        // the guest reads set 1 then.
        let set1 = translate || self.set == 1;
        if scancode == PAUSE {
            if pressed {
                let bytes: &[u8] = if set1 {
                    &[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]
                } else {
                    &[0xe1, 0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77]
                };
                self.send(bytes);
            }
            return;
        }
        let extended = scancode & 0xff00 == 0xe000;
        let code = scancode as u8 & 0x7f;
        let mut bytes = Vec::with_capacity(3);
        if extended {
            bytes.push(0xe0);
        }
        if set1 {
            bytes.push(if pressed { code } else { code | 0x80 });
        } else {
            let Some(set2) = set1_to_set2(code) else {
                tracing::debug!(scancode, "no set 2 code for this key");
                return;
            };
            if !pressed {
                bytes.push(0xf0);
            }
            bytes.push(set2);
        }
        self.send(&bytes);
    }
}

/// Scan code set 2 for a set 1 make code (extended keys use the same table after
/// their 0xE0 prefix).
fn set1_to_set2(code: u8) -> Option<u8> {
    const TABLE: [u8; 0x80] = [
        0x00, 0x76, 0x16, 0x1e, 0x26, 0x25, 0x2e, 0x36, // 00-07
        0x3d, 0x3e, 0x46, 0x45, 0x4e, 0x55, 0x66, 0x0d, // 08-0f
        0x15, 0x1d, 0x24, 0x2d, 0x2c, 0x35, 0x3c, 0x43, // 10-17
        0x44, 0x4d, 0x54, 0x5b, 0x5a, 0x14, 0x1c, 0x1b, // 18-1f
        0x23, 0x2b, 0x34, 0x33, 0x3b, 0x42, 0x4b, 0x4c, // 20-27
        0x52, 0x0e, 0x12, 0x5d, 0x1a, 0x22, 0x21, 0x2a, // 28-2f
        0x32, 0x31, 0x3a, 0x41, 0x49, 0x4a, 0x59, 0x7c, // 30-37
        0x11, 0x29, 0x58, 0x05, 0x06, 0x04, 0x0c, 0x03, // 38-3f
        0x0b, 0x83, 0x0a, 0x01, 0x09, 0x77, 0x7e, 0x6c, // 40-47
        0x75, 0x7d, 0x7b, 0x6b, 0x73, 0x74, 0x79, 0x69, // 48-4f
        0x72, 0x7a, 0x70, 0x71, 0x84, 0x00, 0x61, 0x78, // 50-57
        0x07, 0x00, 0x00, 0x1f, 0x27, 0x2f, 0x37, 0x3f, // 58-5f
        0x00, 0x00, 0x00, 0x5e, 0x08, 0x10, 0x18, 0x20, // 60-67
        0x28, 0x30, 0x38, 0x40, 0x48, 0x50, 0x57, 0x6f, // 68-6f
        0x13, 0x00, 0x00, 0x51, 0x00, 0x00, 0x00, 0x00, // 70-77
        0x00, 0x64, 0x00, 0x67, 0x00, 0x6a, 0x00, 0x00, // 78-7f
    ];
    let set2 = TABLE[usize::from(code & 0x7f)];
    (set2 != 0).then_some(set2)
}

/// What the next byte written to the mouse means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseArgument {
    None,
    Resolution,
    SampleRate,
}

/// A PS/2 mouse that becomes an IntelliMouse (with a wheel) when the guest knocks.
struct Mouse {
    output: VecDeque<u8>,
    argument: MouseArgument,
    reporting: bool,
    remote: bool,
    wrap: bool,
    sample_rate: u8,
    resolution: u8,
    scaling_2_to_1: bool,
    /// 0 for a plain PS/2 mouse, 3 for an IntelliMouse.
    id: u8,
    /// The last three sample rates set, for the IntelliMouse knock (200, 100, 80).
    rates: [u8; 3],
    buttons: u8,
    /// Motion not yet reported, for remote mode's Read Data.
    unreported: (i32, i32, i32),
}

impl Mouse {
    fn new() -> Self {
        Self {
            output: VecDeque::new(),
            argument: MouseArgument::None,
            reporting: false,
            remote: false,
            wrap: false,
            sample_rate: 100,
            resolution: 2,
            scaling_2_to_1: false,
            id: 0,
            rates: [0; 3],
            buttons: 0,
            unreported: (0, 0, 0),
        }
    }

    fn command(&mut self, value: u8) {
        match std::mem::replace(&mut self.argument, MouseArgument::None) {
            MouseArgument::Resolution => {
                self.resolution = value & 3;
                return self.output.push_back(ACK);
            }
            MouseArgument::SampleRate => {
                self.sample_rate = value;
                self.rates = [self.rates[1], self.rates[2], value];
                if self.rates == [200, 100, 80] {
                    self.id = 3;
                }
                return self.output.push_back(ACK);
            }
            MouseArgument::None => {}
        }
        // Wrap mode echoes everything except leaving it and reset.
        if self.wrap && !matches!(value, 0xec | 0xff) {
            return self.output.push_back(value);
        }
        match value {
            0xe6 | 0xe7 => {
                self.scaling_2_to_1 = value == 0xe7;
                self.output.push_back(ACK);
            }
            0xe8 => {
                self.argument = MouseArgument::Resolution;
                self.output.push_back(ACK);
            }
            0xe9 => {
                let status = (u8::from(self.remote) << 6)
                    | (u8::from(self.reporting) << 5)
                    | (u8::from(self.scaling_2_to_1) << 4)
                    | (self.buttons & 0x07);
                self.output
                    .extend([ACK, status, self.resolution, self.sample_rate]);
            }
            0xea => {
                self.remote = false;
                self.output.push_back(ACK);
            }
            0xeb => {
                self.output.push_back(ACK);
                let (dx, dy, dz) = std::mem::take(&mut self.unreported);
                self.packets(dx, dy, dz);
            }
            0xec => {
                self.wrap = false;
                self.output.push_back(ACK);
            }
            0xee => {
                self.wrap = true;
                self.output.push_back(ACK);
            }
            0xf0 => {
                self.remote = true;
                self.output.push_back(ACK);
            }
            0xf2 => self.output.extend([ACK, self.id]),
            0xf3 => {
                self.argument = MouseArgument::SampleRate;
                self.output.push_back(ACK);
            }
            0xf4 | 0xf5 => {
                self.reporting = value == 0xf4;
                self.output.push_back(ACK);
            }
            0xf6 => {
                let id = self.id;
                *self = Self::new();
                self.id = id;
                self.output.push_back(ACK);
            }
            0xff => {
                *self = Self::new();
                self.output.extend([ACK, SELF_TEST_PASSED, 0x00]);
            }
            _ => self.output.push_back(RESEND),
        }
    }

    fn motion(&mut self, dx: i32, dy: i32, dz: i32, buttons: u8) {
        let buttons_changed = buttons & 0x07 != self.buttons;
        self.buttons = buttons & 0x07;
        if self.remote || !self.reporting {
            self.unreported.0 += dx;
            self.unreported.1 += dy;
            self.unreported.2 += dz;
            return;
        }
        if dx != 0 || dy != 0 || dz != 0 || buttons_changed {
            self.packets(dx, dy, dz);
        }
    }

    /// Sends motion as packets of at most ±255 each way (PS/2 counts up as positive).
    fn packets(&mut self, mut dx: i32, mut dy: i32, mut dz: i32) {
        loop {
            let x = dx.clamp(-255, 255);
            let y = dy.clamp(-255, 255);
            let z = dz.clamp(-8, 7);
            let up = -y;
            let mut first = 0x08 | self.buttons;
            if x < 0 {
                first |= 0x10;
            }
            if up < 0 {
                first |= 0x20;
            }
            self.output.extend([first, x as u8, up as u8]);
            if self.id == 3 {
                self.output.push_back(z as u8);
            }
            dx -= x;
            dy -= y;
            dz -= z;
            if dx == 0 && dy == 0 && dz == 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::irq::testing::RecordingSink;
    use crate::legacy::ioapic::IoApic;

    struct Fixture {
        dev: I8042,
        events: Arc<Mutex<Vec<PowerEvent>>>,
        sink: Arc<RecordingSink>,
    }

    fn i8042() -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink::default());
        let ioapic = Arc::new(Mutex::new(IoApic::new(0, sink.clone())));
        {
            // Pins 1 and 12: edge-triggered, unmasked, vectors 0x31 and 0x3c.
            let mut ioapic = ioapic.lock().unwrap();
            for (pin, vector) in [(1u32, 0x31u32), (12, 0x3c)] {
                ioapic.write(0, &(0x10 + 2 * pin).to_le_bytes());
                ioapic.write(0x10, &vector.to_le_bytes());
            }
        }
        let power: PowerHandler = Arc::new({
            let events = events.clone();
            move |event| events.lock().unwrap().push(event)
        });
        let dev = I8042::new(
            IrqLine::new(ioapic.clone(), 1),
            IrqLine::new(ioapic, 12),
            power,
        );
        Fixture { dev, events, sink }
    }

    fn command(dev: &mut I8042, value: u8) {
        dev.write(4, &[value]);
    }

    fn read_data(dev: &mut I8042) -> u8 {
        let mut data = [0];
        dev.read(0, &mut data);
        data[0]
    }

    fn read_all(dev: &mut I8042) -> Vec<u8> {
        let mut bytes = Vec::new();
        while status(dev) & STATUS_OUTPUT_FULL != 0 {
            bytes.push(read_data(dev));
        }
        bytes
    }

    fn status(dev: &mut I8042) -> u8 {
        let mut data = [0];
        dev.read(4, &mut data);
        data[0]
    }

    fn to_mouse(dev: &mut I8042, value: u8) {
        command(dev, 0xd4);
        dev.write(0, &[value]);
    }

    #[test]
    fn self_test_and_config_byte_round_trip() {
        let mut f = i8042();
        assert_ne!(status(&mut f.dev), 0xff, "present");
        command(&mut f.dev, 0xaa);
        assert_ne!(status(&mut f.dev) & STATUS_OUTPUT_FULL, 0);
        assert_eq!(read_data(&mut f.dev), 0x55);
        assert_eq!(status(&mut f.dev) & STATUS_OUTPUT_FULL, 0);

        command(&mut f.dev, 0x60);
        f.dev.write(0, &[0x47]);
        command(&mut f.dev, 0x20);
        assert_eq!(read_data(&mut f.dev), 0x47);
        command(&mut f.dev, 0xa9);
        assert_eq!(read_data(&mut f.dev), 0x00, "a mouse is there");
    }

    #[test]
    fn keyboard_reset_identify_and_leds() {
        let mut f = i8042();
        f.dev.write(0, &[0xff]);
        assert_eq!(read_all(&mut f.dev), [ACK, SELF_TEST_PASSED]);
        f.dev.write(0, &[0xf2]);
        assert_eq!(read_all(&mut f.dev), [ACK, 0xab, 0x83]);
        f.dev.write(0, &[0xed]);
        f.dev.write(0, &[0x02]);
        assert_eq!(read_all(&mut f.dev), [ACK, ACK]);
        assert_eq!(f.dev.keyboard.leds, 0x02);
        f.dev.write(0, &[0xf0]);
        f.dev.write(0, &[0x00]);
        assert_eq!(read_all(&mut f.dev), [ACK, ACK, 2], "scan code set 2");
    }

    #[test]
    fn keys_arrive_as_set_1_when_translating_and_interrupt() {
        let mut f = i8042();
        f.sink.take();
        // A, then right Ctrl (extended), pressed and released.
        f.dev.key(0x1e, true);
        f.dev.key(0x1e, false);
        f.dev.key(0xe01d, true);
        f.dev.key(0xe01d, false);
        assert_eq!(
            f.sink.take().len(),
            1,
            "one interrupt per byte in the buffer"
        );
        assert_eq!(read_all(&mut f.dev), [0x1e, 0x9e, 0xe0, 0x1d, 0xe0, 0x9d]);
        f.dev.key(PAUSE, true);
        f.dev.key(PAUSE, false);
        assert_eq!(read_all(&mut f.dev), [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]);
    }

    #[test]
    fn keys_arrive_as_set_2_without_translation() {
        let mut f = i8042();
        command(&mut f.dev, 0x60);
        f.dev.write(0, &[CONFIG_KBD_INTERRUPT | CONFIG_SYSTEM]);
        f.dev.key(0x1e, true);
        f.dev.key(0x1e, false);
        f.dev.key(0xe048, true); // Up arrow
        f.dev.key(0xe048, false);
        assert_eq!(
            read_all(&mut f.dev),
            [0x1c, 0xf0, 0x1c, 0xe0, 0x75, 0xe0, 0xf0, 0x75]
        );
    }

    #[test]
    fn a_disabled_keyboard_holds_its_bytes() {
        let mut f = i8042();
        command(&mut f.dev, 0xad);
        f.dev.key(0x1e, true);
        assert_eq!(status(&mut f.dev) & STATUS_OUTPUT_FULL, 0);
        command(&mut f.dev, 0xae);
        assert_eq!(read_all(&mut f.dev), [0x1e]);
    }

    #[test]
    fn mouse_reports_motion_on_irq12_once_enabled() {
        let mut f = i8042();
        command(&mut f.dev, 0xa8);
        command(&mut f.dev, 0x60);
        f.dev.write(
            0,
            &[CONFIG_KBD_INTERRUPT | CONFIG_AUX_INTERRUPT | CONFIG_SYSTEM | CONFIG_TRANSLATE],
        );
        to_mouse(&mut f.dev, 0xff);
        assert_eq!(read_all(&mut f.dev), [ACK, SELF_TEST_PASSED, 0x00]);
        f.dev.mouse_motion(5, 5, 0, 0);
        assert!(read_all(&mut f.dev).is_empty(), "reporting is off");
        to_mouse(&mut f.dev, 0xf4);
        assert_eq!(read_all(&mut f.dev), [ACK]);
        f.sink.take();
        // Right 3, down 2 (PS/2 counts up), left button.
        f.dev.mouse_motion(3, 2, 0, 1);
        let sent = f.sink.take();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].vector(), 0x3c);
        assert_ne!(status(&mut f.dev) & STATUS_AUX, 0);
        assert_eq!(read_all(&mut f.dev), [0x08 | 0x20 | 1, 3, 0xfe]);
        // A big move splits into packets of at most 255.
        f.dev.mouse_motion(-300, 0, 0, 1);
        assert_eq!(
            read_all(&mut f.dev),
            [0x08 | 0x10 | 1, 0x01, 0, 0x08 | 0x10 | 1, 0xd3, 0]
        );
    }

    #[test]
    fn intellimouse_knock_adds_the_wheel() {
        let mut f = i8042();
        command(&mut f.dev, 0xa8);
        for rate in [200, 100, 80] {
            to_mouse(&mut f.dev, 0xf3);
            to_mouse(&mut f.dev, rate);
        }
        to_mouse(&mut f.dev, 0xf2);
        to_mouse(&mut f.dev, 0xf4);
        assert_eq!(
            read_all(&mut f.dev),
            [ACK, ACK, ACK, ACK, ACK, ACK, ACK, 3, ACK]
        );
        f.dev.mouse_motion(0, 0, -1, 0);
        assert_eq!(read_all(&mut f.dev), [0x08, 0, 0, 0xff]);
    }

    #[test]
    fn pulse_reset_line_resets() {
        let mut f = i8042();
        command(&mut f.dev, 0xfe);
        assert_eq!(*f.events.lock().unwrap(), [PowerEvent::Reset]);
    }

    #[test]
    fn set_2_table_matches_known_keys() {
        assert_eq!(set1_to_set2(0x01), Some(0x76), "Esc");
        assert_eq!(set1_to_set2(0x1c), Some(0x5a), "Enter");
        assert_eq!(set1_to_set2(0x39), Some(0x29), "Space");
        assert_eq!(set1_to_set2(0x5b), Some(0x1f), "left Windows (after E0)");
        assert_eq!(set1_to_set2(0x58), Some(0x07), "F12");
        assert_eq!(set1_to_set2(0x55), None);
    }
}
