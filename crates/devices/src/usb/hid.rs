//! USB HID devices, from the USB 2.0 and HID 1.11 specifications:
//!
//! - **Tablet:** an absolute pointer, so the guest's pointer follows the host's with
//!   no capture. X and Y run 0 to [`TABLET_MAX`], with three buttons and a wheel. Guests
//!   see a mouse whose axes are absolute.
//! - **Keyboard:** a boot-protocol keyboard. The host sends keys as scan code set 1,
//!   which becomes HID usages.
//!
//! Both are full-speed devices with one interrupt IN endpoint (0x81). Host input goes
//! to a handle ([`TabletInput`], [`KeyboardInput`]); the controller collects the
//! resulting report when it next services the endpoint.

use std::sync::{Arc, Mutex, PoisonError};

use super::{Setup, UsbDevice};

/// pid.codes, the open-source vendor ID, with its test product IDs.
const VENDOR: u16 = 0x1209;
const TABLET_PRODUCT: u16 = 0x0001;
const KEYBOARD_PRODUCT: u16 = 0x0002;
const MAX_PACKET_0: u8 = 64;
/// Full-speed interrupt endpoints poll every bInterval ms.
const POLL_MS: u8 = 4;

/// The largest absolute coordinate the tablet reports.
pub const TABLET_MAX: u16 = 0x7fff;

const SPEED_FULL: u8 = 1;

const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const GET_DESCRIPTOR: u8 = 6;
const GET_CONFIGURATION: u8 = 8;
const SET_CONFIGURATION: u8 = 9;
const GET_INTERFACE: u8 = 10;
const SET_INTERFACE: u8 = 11;
const HID_GET_REPORT: u8 = 1;
const HID_GET_IDLE: u8 = 2;
const HID_GET_PROTOCOL: u8 = 3;
const HID_SET_REPORT: u8 = 9;
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;

const DESCRIPTOR_DEVICE: u8 = 1;
const DESCRIPTOR_CONFIGURATION: u8 = 2;
const DESCRIPTOR_STRING: u8 = 3;
const DESCRIPTOR_HID: u8 = 0x21;
const DESCRIPTOR_REPORT: u8 = 0x22;

const TABLET_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xa1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xa1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x03, //     Usage Maximum (3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x01, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xff, 0x7f, // Logical Maximum (32767)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0xff, 0x7f, // Physical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7f, //     Logical Maximum (127)
    0x35, 0x00, //     Physical Minimum (0)
    0x45, 0x00, //     Physical Maximum (0)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xc0, //       End Collection
    0xc0, //     End Collection
];

/// The boot keyboard's report descriptor (HID 1.11, appendix B.1).
const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

/// What a HID device describes itself as.
struct Identity {
    product_id: u16,
    product: &'static str,
    serial: &'static str,
    /// Interface subclass and protocol: (1, 1) for a boot keyboard, (0, 0) otherwise.
    subclass: u8,
    protocol: u8,
    report_descriptor: &'static [u8],
    max_packet: u8,
}

impl Identity {
    fn device_descriptor(&self) -> Vec<u8> {
        let mut d = vec![18, DESCRIPTOR_DEVICE, 0x00, 0x02, 0, 0, 0, MAX_PACKET_0];
        d.extend_from_slice(&VENDOR.to_le_bytes());
        d.extend_from_slice(&self.product_id.to_le_bytes());
        d.extend_from_slice(&0x0100u16.to_le_bytes()); // bcdDevice
        d.extend_from_slice(&[1, 2, 3, 1]); // strings, one configuration
        d
    }

    fn configuration_descriptor(&self) -> Vec<u8> {
        let report_len = self.report_descriptor.len() as u16;
        let total: u16 = 9 + 9 + 9 + 7;
        let mut d = vec![9, DESCRIPTOR_CONFIGURATION];
        d.extend_from_slice(&total.to_le_bytes());
        // One interface, configuration 1, bus powered with remote wakeup, 100 mA.
        d.extend_from_slice(&[1, 1, 0, 0xa0, 50]);
        // Interface 0: HID, one endpoint.
        d.extend_from_slice(&[9, 4, 0, 0, 1, 3, self.subclass, self.protocol, 0]);
        // HID 1.11, one report descriptor.
        d.extend_from_slice(&[9, DESCRIPTOR_HID, 0x11, 0x01, 0, 1, DESCRIPTOR_REPORT]);
        d.extend_from_slice(&report_len.to_le_bytes());
        // Endpoint 1 IN, interrupt.
        d.extend_from_slice(&[7, 5, 0x81, 3, self.max_packet, 0, POLL_MS]);
        d
    }

    fn string(&self, index: u8) -> Option<Vec<u8>> {
        let text = match index {
            // Language IDs: US English.
            0 => return Some(vec![4, DESCRIPTOR_STRING, 0x09, 0x04]),
            1 => "vmmc",
            2 => self.product,
            3 => self.serial,
            _ => return None,
        };
        let mut d = vec![0, DESCRIPTOR_STRING];
        for unit in text.encode_utf16() {
            d.extend_from_slice(&unit.to_le_bytes());
        }
        d[0] = d.len() as u8;
        Some(d)
    }
}

/// What a HID device saves: its control endpoint's state, and its input's.
#[derive(serde::Serialize, serde::Deserialize)]
struct HidState<T> {
    configuration: u8,
    idle: u8,
    protocol: u8,
    input: T,
}

/// The control-endpoint state every HID device keeps.
struct Function {
    identity: Identity,
    configuration: u8,
    idle: u8,
    /// 0 boot, 1 report.
    protocol: u8,
}

/// What a control request comes to.
enum Answer {
    Data(Vec<u8>),
    Stall,
    /// A HID SET_REPORT, with its data, for the device to apply.
    SetReport(Vec<u8>),
}

impl Function {
    fn new(identity: Identity) -> Self {
        Self {
            identity,
            configuration: 0,
            idle: 0,
            protocol: 1,
        }
    }

    fn control(&mut self, setup: &Setup, data: &[u8], report: impl FnOnce() -> Vec<u8>) -> Answer {
        // wValue: the descriptor's index in the low byte, its type in the high byte.
        let [descriptor_index, _] = setup.value.to_le_bytes();
        let descriptor_type = (setup.value >> 8) as u8;
        match (setup.request_type, setup.request) {
            (0x80, GET_DESCRIPTOR) => match descriptor_type {
                DESCRIPTOR_DEVICE => Answer::Data(self.identity.device_descriptor()),
                DESCRIPTOR_CONFIGURATION => Answer::Data(self.identity.configuration_descriptor()),
                DESCRIPTOR_STRING => self
                    .identity
                    .string(descriptor_index)
                    .map_or(Answer::Stall, Answer::Data),
                // A full-speed-only device has no device qualifier, no BOS.
                _ => Answer::Stall,
            },
            (0x81, GET_DESCRIPTOR) => match descriptor_type {
                DESCRIPTOR_REPORT => Answer::Data(self.identity.report_descriptor.to_vec()),
                DESCRIPTOR_HID => {
                    Answer::Data(self.identity.configuration_descriptor()[18..27].to_vec())
                }
                _ => Answer::Stall,
            },
            (0x80, GET_STATUS) => Answer::Data(vec![0, 0]),
            (0x81 | 0x82, GET_STATUS) => Answer::Data(vec![0, 0]),
            (0x00..=0x02, CLEAR_FEATURE | SET_FEATURE) => Answer::Data(Vec::new()),
            (0x80, GET_CONFIGURATION) => Answer::Data(vec![self.configuration]),
            (0x00, SET_CONFIGURATION) if setup.value <= 1 => {
                self.configuration = setup.value as u8;
                Answer::Data(Vec::new())
            }
            (0x81, GET_INTERFACE) => Answer::Data(vec![0]),
            (0x01, SET_INTERFACE) if setup.value == 0 => Answer::Data(Vec::new()),
            (0xa1, HID_GET_REPORT) => Answer::Data(report()),
            (0xa1, HID_GET_IDLE) => Answer::Data(vec![self.idle]),
            (0xa1, HID_GET_PROTOCOL) => Answer::Data(vec![self.protocol]),
            (0x21, HID_SET_IDLE) => {
                self.idle = (setup.value >> 8) as u8;
                Answer::Data(Vec::new())
            }
            (0x21, HID_SET_PROTOCOL) => {
                self.protocol = setup.value as u8;
                Answer::Data(Vec::new())
            }
            (0x21, HID_SET_REPORT) => Answer::SetReport(data.to_vec()),
            _ => {
                tracing::debug!(?setup, "USB HID: unsupported request");
                Answer::Stall
            }
        }
    }

    fn reset(&mut self) {
        self.configuration = 0;
        self.idle = 0;
        self.protocol = 1;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct TabletState {
    x: u16,
    y: u16,
    buttons: u8,
    wheel: i32,
    changed: bool,
}

impl TabletState {
    fn report(&mut self) -> Vec<u8> {
        let wheel = self.wheel.clamp(-127, 127);
        self.wheel -= wheel;
        self.changed = self.wheel != 0;
        let [x0, x1] = self.x.to_le_bytes();
        let [y0, y1] = self.y.to_le_bytes();
        vec![self.buttons, x0, x1, y0, y1, wheel as i8 as u8]
    }
}

/// Moves the tablet's pointer, from the host.
#[derive(Clone)]
pub struct TabletInput(Arc<Mutex<TabletState>>);

impl TabletInput {
    /// The pointer is at `(x, y)` (0 to [`TABLET_MAX`] each), with these buttons held
    /// (bit 0 left, 1 right, 2 middle), and the wheel turned by `wheel` notches (away
    /// from the user positive).
    pub fn update(&self, x: u16, y: u16, buttons: u8, wheel: i32) {
        let mut state = lock(&self.0);
        let (x, y, buttons) = (x.min(TABLET_MAX), y.min(TABLET_MAX), buttons & 7);
        if (x, y, buttons) != (state.x, state.y, state.buttons) || wheel != 0 {
            state.x = x;
            state.y = y;
            state.buttons = buttons;
            state.wheel += wheel;
            state.changed = true;
        }
    }
}

/// A USB tablet: an absolute pointer.
pub struct Tablet {
    function: Function,
    state: Arc<Mutex<TabletState>>,
}

impl Tablet {
    pub fn new() -> (Self, TabletInput) {
        let state = Arc::new(Mutex::new(TabletState::default()));
        let tablet = Self {
            function: Function::new(Identity {
                product_id: TABLET_PRODUCT,
                product: "vmmc USB Tablet",
                serial: "1",
                subclass: 0,
                protocol: 0,
                report_descriptor: TABLET_REPORT_DESCRIPTOR,
                max_packet: 8,
            }),
            state: state.clone(),
        };
        (tablet, TabletInput(state))
    }
}

impl UsbDevice for Tablet {
    fn save(&self) -> Vec<u8> {
        crate::snapshot::encode(&HidState {
            configuration: self.function.configuration,
            idle: self.function.idle,
            protocol: self.function.protocol,
            input: lock(&self.state).clone(),
        })
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), crate::snapshot::StateError> {
        let state: HidState<_> = crate::snapshot::decode(state)?;
        self.function.configuration = state.configuration;
        self.function.idle = state.idle;
        self.function.protocol = state.protocol;
        *lock(&self.state) = state.input;
        Ok(())
    }

    fn speed(&self) -> u8 {
        SPEED_FULL
    }

    fn control(&mut self, setup: &Setup, data: &[u8]) -> Option<Vec<u8>> {
        let state = &self.state;
        match self.function.control(setup, data, || lock(state).report()) {
            Answer::Data(data) => Some(data),
            Answer::SetReport(_) => Some(Vec::new()),
            Answer::Stall => None,
        }
    }

    fn interrupt_in(&mut self, endpoint: u8) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        (endpoint == 1 && self.function.configuration != 0 && state.changed).then(|| state.report())
    }

    fn reset(&mut self) {
        self.function.reset();
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct KeyboardState {
    /// Modifier keys held: bit 0 left Ctrl to bit 7 right GUI.
    modifiers: u8,
    /// Other keys held, as HID usages, in the order pressed.
    keys: Vec<u8>,
    leds: u8,
    changed: bool,
}

impl KeyboardState {
    fn report(&mut self) -> Vec<u8> {
        self.changed = false;
        let mut report = vec![self.modifiers, 0, 0, 0, 0, 0, 0, 0];
        if self.keys.len() > 6 {
            // Too many keys: report ErrorRollOver in every slot.
            report[2..].fill(0x01);
        } else {
            report[2..2 + self.keys.len()].copy_from_slice(&self.keys);
        }
        report
    }
}

/// Types on the USB keyboard, from the host.
#[derive(Clone)]
pub struct KeyboardInput(Arc<Mutex<KeyboardState>>);

impl KeyboardInput {
    /// A key, as scan code set 1 (`0xE0nn` for extended keys), went down or up.
    pub fn key(&self, scancode: u16, pressed: bool) {
        let Some(usage) = usage(scancode) else {
            tracing::debug!(scancode, "USB keyboard: no HID usage for this key");
            return;
        };
        let mut state = lock(&self.0);
        if (0xe0..=0xe7).contains(&usage) {
            let bit = 1 << (usage - 0xe0);
            let modifiers = if pressed {
                state.modifiers | bit
            } else {
                state.modifiers & !bit
            };
            state.changed |= modifiers != state.modifiers;
            state.modifiers = modifiers;
        } else if pressed {
            if !state.keys.contains(&usage) {
                state.keys.push(usage);
                state.changed = true;
            }
        } else if let Some(at) = state.keys.iter().position(|&key| key == usage) {
            state.keys.remove(at);
            state.changed = true;
        }
    }

    /// The LEDs the guest last set: bit 0 Num Lock, 1 Caps Lock, 2 Scroll Lock.
    pub fn leds(&self) -> u8 {
        lock(&self.0).leds
    }
}

/// A USB boot keyboard.
pub struct Keyboard {
    function: Function,
    state: Arc<Mutex<KeyboardState>>,
}

impl Keyboard {
    pub fn new() -> (Self, KeyboardInput) {
        let state = Arc::new(Mutex::new(KeyboardState::default()));
        let keyboard = Self {
            function: Function::new(Identity {
                product_id: KEYBOARD_PRODUCT,
                product: "vmmc USB Keyboard",
                serial: "2",
                subclass: 1,
                protocol: 1,
                report_descriptor: KEYBOARD_REPORT_DESCRIPTOR,
                max_packet: 8,
            }),
            state: state.clone(),
        };
        (keyboard, KeyboardInput(state))
    }
}

impl UsbDevice for Keyboard {
    fn save(&self) -> Vec<u8> {
        crate::snapshot::encode(&HidState {
            configuration: self.function.configuration,
            idle: self.function.idle,
            protocol: self.function.protocol,
            input: lock(&self.state).clone(),
        })
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), crate::snapshot::StateError> {
        let state: HidState<_> = crate::snapshot::decode(state)?;
        self.function.configuration = state.configuration;
        self.function.idle = state.idle;
        self.function.protocol = state.protocol;
        *lock(&self.state) = state.input;
        Ok(())
    }

    fn speed(&self) -> u8 {
        SPEED_FULL
    }

    fn control(&mut self, setup: &Setup, data: &[u8]) -> Option<Vec<u8>> {
        let state = &self.state;
        match self.function.control(setup, data, || lock(state).report()) {
            Answer::Data(data) => Some(data),
            Answer::SetReport(data) => {
                if let Some(&leds) = data.first() {
                    lock(&self.state).leds = leds & 0x07;
                }
                Some(Vec::new())
            }
            Answer::Stall => None,
        }
    }

    fn interrupt_in(&mut self, endpoint: u8) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        (endpoint == 1 && self.function.configuration != 0 && state.changed).then(|| state.report())
    }

    fn reset(&mut self) {
        self.function.reset();
    }
}

/// The HID usage (keyboard page) of a set 1 scan code.
fn usage(scancode: u16) -> Option<u8> {
    const PLAIN: [u8; 0x59] = [
        0x00, 0x29, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, // 00-07
        0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e, 0x2a, 0x2b, // 08-0f
        0x14, 0x1a, 0x08, 0x15, 0x17, 0x1c, 0x18, 0x0c, // 10-17
        0x12, 0x13, 0x2f, 0x30, 0x28, 0xe0, 0x04, 0x16, // 18-1f
        0x07, 0x09, 0x0a, 0x0b, 0x0d, 0x0e, 0x0f, 0x33, // 20-27
        0x34, 0x35, 0xe1, 0x31, 0x1d, 0x1b, 0x06, 0x19, // 28-2f
        0x05, 0x11, 0x10, 0x36, 0x37, 0x38, 0xe5, 0x55, // 30-37
        0xe2, 0x2c, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, // 38-3f
        0x3f, 0x40, 0x41, 0x42, 0x43, 0x53, 0x47, 0x5f, // 40-47
        0x60, 0x61, 0x56, 0x5c, 0x5d, 0x5e, 0x57, 0x59, // 48-4f
        0x5a, 0x5b, 0x62, 0x63, 0x00, 0x00, 0x64, 0x44, // 50-57
        0x45, // 58
    ];
    if scancode == crate::legacy::i8042::PAUSE {
        return Some(0x48);
    }
    let code = (scancode & 0x7f) as u8;
    let usage = if scancode & 0xff00 == 0xe000 {
        match code {
            0x1c => 0x58, // keypad Enter
            0x1d => 0xe4, // right Ctrl
            0x35 => 0x54, // keypad /
            0x37 => 0x46, // Print Screen
            0x38 => 0xe6, // right Alt
            0x47 => 0x4a, // Home
            0x48 => 0x52, // Up
            0x49 => 0x4b, // Page Up
            0x4b => 0x50, // Left
            0x4d => 0x4f, // Right
            0x4f => 0x4d, // End
            0x50 => 0x51, // Down
            0x51 => 0x4e, // Page Down
            0x52 => 0x49, // Insert
            0x53 => 0x4c, // Delete
            0x5b => 0xe3, // left GUI
            0x5c => 0xe7, // right GUI
            0x5d => 0x65, // Application
            _ => 0,
        }
    } else {
        match code {
            0x70 => 0x88, // Katakana/Hiragana
            0x73 => 0x87, // Ro
            0x79 => 0x8a, // Henkan
            0x7b => 0x8b, // Muhenkan
            0x7d => 0x89, // Yen
            _ => PLAIN.get(usize::from(code)).copied().unwrap_or(0),
        }
    };
    (usage != 0).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(request_type: u8, request: u8, value: u16, length: u16) -> Setup {
        Setup {
            request_type,
            request,
            value,
            index: 0,
            length,
        }
    }

    #[test]
    fn describes_itself() {
        let (mut tablet, _) = Tablet::new();
        let device = tablet
            .control(&setup(0x80, GET_DESCRIPTOR, 0x0100, 18), &[])
            .unwrap();
        assert_eq!(device.len(), 18);
        assert_eq!(&device[8..12], &[0x09, 0x12, 0x01, 0x00], "1209:0001");
        let config = tablet
            .control(&setup(0x80, GET_DESCRIPTOR, 0x0200, 255), &[])
            .unwrap();
        assert_eq!(
            u16::from_le_bytes([config[2], config[3]]) as usize,
            config.len()
        );
        let report_len = u16::from_le_bytes([config[25], config[26]]) as usize;
        let report = tablet
            .control(&setup(0x81, GET_DESCRIPTOR, 0x2200, 255), &[])
            .unwrap();
        assert_eq!(report.len(), report_len);
        let product = tablet
            .control(&setup(0x80, GET_DESCRIPTOR, 0x0302, 255), &[])
            .unwrap();
        assert_eq!(product[0] as usize, product.len());
        let text: Vec<u16> = product[2..]
            .chunks(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        assert_eq!(String::from_utf16(&text).unwrap(), "vmmc USB Tablet");
        assert!(
            tablet
                .control(&setup(0x80, GET_DESCRIPTOR, 0x0600, 10), &[])
                .is_none(),
            "no qualifier"
        );
    }

    #[test]
    fn tablet_reports_only_after_configuration_and_changes() {
        let (mut tablet, input) = Tablet::new();
        input.update(100, 200, 1, 0);
        assert_eq!(tablet.interrupt_in(1), None, "not configured");
        tablet
            .control(&setup(0x00, SET_CONFIGURATION, 1, 0), &[])
            .unwrap();
        assert_eq!(tablet.interrupt_in(1), Some(vec![1, 100, 0, 200, 0, 0]));
        assert_eq!(tablet.interrupt_in(1), None, "nothing new");
        input.update(TABLET_MAX + 5, 0, 0, -1);
        assert_eq!(
            tablet.interrupt_in(1),
            Some(vec![0, 0xff, 0x7f, 0, 0, 0xff])
        );
    }

    #[test]
    fn keyboard_reports_keys_and_modifiers() {
        let (mut keyboard, input) = Keyboard::new();
        keyboard
            .control(&setup(0x00, SET_CONFIGURATION, 1, 0), &[])
            .unwrap();
        input.key(0x2a, true); // left Shift
        input.key(0x1e, true); // A
        assert_eq!(
            keyboard.interrupt_in(1),
            Some(vec![0x02, 0, 0x04, 0, 0, 0, 0, 0])
        );
        input.key(0x1e, false);
        input.key(0xe048, true); // Up
        assert_eq!(
            keyboard.interrupt_in(1),
            Some(vec![0x02, 0, 0x52, 0, 0, 0, 0, 0])
        );
        keyboard
            .control(&setup(0x21, HID_SET_REPORT, 0x0200, 1), &[0x02])
            .unwrap();
        assert_eq!(input.leds(), 0x02, "Caps Lock LED");
    }

    #[test]
    fn usages_for_common_keys() {
        assert_eq!(usage(0x01), Some(0x29), "Esc");
        assert_eq!(usage(0x1c), Some(0x28), "Enter");
        assert_eq!(usage(0x39), Some(0x2c), "Space");
        assert_eq!(usage(0x58), Some(0x45), "F12");
        assert_eq!(usage(0xe05b), Some(0xe3), "left Windows");
        assert_eq!(usage(0x54), None);
    }
}
