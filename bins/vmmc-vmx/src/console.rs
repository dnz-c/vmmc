//! The host terminal as the guest's serial console.
//!
//! Input goes to the guest byte for byte. Ctrl-A starts an escape: Ctrl-A x quits,
//! Ctrl-A p presses the ACPI power button, Ctrl-A s saves a screenshot (with
//! `--screenshot`), Ctrl-A r prints each vCPU's registers, Ctrl-A m saves guest RAM
//! and vCPU state to a temp directory, Ctrl-A k switches between typing on the serial
//! port and typing on the PS/2 keyboard (US layout), Ctrl-A z WIDTHxHEIGHT Enter asks
//! the guest for that screen size (as resizing the window does), Ctrl-A Ctrl-A sends a
//! literal Ctrl-A.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use machine::{Display, PowerButton, Ps2Input, SerialInput, UsbInput};

use crate::screenshot::Screenshots;
use vm_core::{GuestMemoryMmap, StopHandle, StopReason};

const CTRL_A: u8 = 0x01;

/// The stop code used when the user quits with Ctrl-A x.
pub const QUIT_CODE: u32 = 0;

/// What a piece of terminal input means.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Bytes for the guest's serial port.
    Guest(Vec<u8>),
    /// Characters to type on the guest's keyboard.
    Keys(Vec<u8>),
    PowerButton,
    Screenshot,
    DumpState,
    SaveMemory,
    /// Ask the guest for a screen this size.
    RequestSize(u32, u32),
    Quit,
}

/// `WIDTHxHEIGHT`, as Ctrl-A z takes it.
fn parse_size(text: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(text).ok()?;
    let (width, height) = text.trim().split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// Splits terminal input into bytes for the guest and escape commands, in order.
#[derive(Debug, Default)]
pub struct EscapeFilter {
    after_ctrl_a: bool,
    /// Input goes to the keyboard rather than the serial port (Ctrl-A k).
    typing: bool,
    /// The size typed after Ctrl-A z so far, until Enter.
    size: Option<Vec<u8>>,
}

impl EscapeFilter {
    pub fn filter(&mut self, input: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        let mut bytes = Vec::new();
        let flush = |bytes: &mut Vec<u8>, out: &mut Vec<Input>, typing: bool| {
            if !bytes.is_empty() {
                let bytes = std::mem::take(bytes);
                out.push(if typing {
                    Input::Keys(bytes)
                } else {
                    Input::Guest(bytes)
                });
            }
        };
        for &byte in input {
            if let Some(size) = &mut self.size {
                if byte == b'\r' || byte == b'\n' {
                    if let Some((width, height)) = parse_size(size) {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::RequestSize(width, height));
                    }
                    self.size = None;
                } else if size.len() < 16 {
                    size.push(byte);
                }
                continue;
            }
            if std::mem::take(&mut self.after_ctrl_a) {
                match byte {
                    b'x' | b'X' => {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::Quit);
                        return out;
                    }
                    b'p' | b'P' => {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::PowerButton);
                    }
                    b's' | b'S' => {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::Screenshot);
                    }
                    b'r' | b'R' => {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::DumpState);
                    }
                    b'm' | b'M' => {
                        flush(&mut bytes, &mut out, self.typing);
                        out.push(Input::SaveMemory);
                    }
                    b'k' | b'K' => {
                        flush(&mut bytes, &mut out, self.typing);
                        self.typing = !self.typing;
                    }
                    b'z' | b'Z' => self.size = Some(Vec::new()),
                    CTRL_A => bytes.push(CTRL_A),
                    // Unknown escapes are swallowed, like QEMU's.
                    _ => {}
                }
            } else if byte == CTRL_A {
                self.after_ctrl_a = true;
            } else {
                bytes.push(byte);
            }
        }
        flush(&mut bytes, &mut out, self.typing);
        out
    }
}

/// The input handles of the machine that's running now.
#[derive(Clone)]
pub struct Target {
    pub serial: SerialInput,
    pub power: PowerButton,
    pub stop: StopHandle,
    pub display: Display,
    pub memory: GuestMemoryMmap,
    pub ps2: Ps2Input,
    pub usb: UsbInput,
    pub discs: Vec<machine::DiscDrive>,
}

/// Where console input goes. A guest reset rebuilds the machine, so the target changes
/// over the life of the process.
#[derive(Clone, Default)]
pub struct Targets {
    current: Arc<Mutex<Option<Target>>>,
    quit: Arc<AtomicBool>,
    screenshots: Option<Arc<Screenshots>>,
}

impl Targets {
    /// Targets that save Ctrl-A s screenshots with `screenshots`, if given.
    pub fn new(screenshots: Option<Arc<Screenshots>>) -> Self {
        Self {
            screenshots,
            ..Self::default()
        }
    }

    /// The display of the machine running now.
    pub fn display(&self) -> Option<Display> {
        self.get().map(|target| target.display)
    }

    pub fn set(&self, target: Option<Target>) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = target;
    }

    pub fn get(&self) -> Option<Target> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops the VM for good: the user closed its window.
    pub fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(target) = self.get() {
            target.stop.request_stop(StopReason::Requested(QUIT_CODE));
        }
    }

    /// Whether the user asked to quit, so the VM must not be restarted.
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }
}

/// The next bytes typed, or `None` once input has ended.
type InputSource = Box<dyn FnMut() -> Option<Vec<u8>> + Send>;

/// Where terminal input comes from: a Windows console is read as input events (see
/// `windows_console::reader`), anything else (a pipe, say) as a stream.
fn input_source() -> InputSource {
    #[cfg(windows)]
    if let Some(reader) = windows_console::reader() {
        return Box::new(reader);
    }
    let mut stdin = io::stdin();
    let mut buffer = [0u8; 256];
    Box::new(move || match stdin.read(&mut buffer) {
        Ok(0) | Err(_) => None,
        Ok(read) => Some(buffer[..read].to_vec()),
    })
}

/// Forwards stdin to the current machine on a background thread until the user quits
/// or stdin closes. The thread is never joined: a blocked read can't be interrupted.
pub fn spawn_input(targets: Targets) -> io::Result<()> {
    thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut filter = EscapeFilter::default();
            let mut next = input_source();
            while let Some(bytes) = next() {
                for input in filter.filter(&bytes) {
                    match input {
                        Input::Guest(pending) => send(&targets, pending),
                        Input::Keys(text) => type_keys(&targets, &text),
                        Input::Screenshot => {
                            if let (Some(screenshots), Some(display)) =
                                (&targets.screenshots, targets.display())
                            {
                                match screenshots.save(&display, "screenshot", true) {
                                    Ok(Some(path)) => {
                                        eprint!("\r\nvmmc-vmx: saved {}\r\n", path.display());
                                    }
                                    Ok(None) => eprint!("\r\nvmmc-vmx: no picture to save\r\n"),
                                    Err(e) => eprint!("\r\nvmmc-vmx: screenshot failed: {e}\r\n"),
                                }
                            }
                        }
                        Input::DumpState => {
                            if let Some(target) = targets.get() {
                                // The console is in raw mode, so lines need a CR.
                                target.stop.request_state_dump(|state| {
                                    eprint!("\r\n{}\r\n", state.replace('\n', "\r\n"));
                                });
                            }
                        }
                        Input::SaveMemory => {
                            if let Some(target) = targets.get() {
                                match crate::memdump::save(&target.memory, &target.stop) {
                                    Ok(dir) => eprint!(
                                        "\r\nvmmc-vmx: saved guest RAM to {}\r\n",
                                        dir.display()
                                    ),
                                    Err(e) => eprint!("\r\nvmmc-vmx: saving RAM failed: {e}\r\n"),
                                }
                            }
                        }
                        Input::PowerButton => {
                            if let Some(target) = targets.get() {
                                target.power.press();
                            }
                        }
                        Input::RequestSize(width, height) => {
                            if let Some(display) = targets.display() {
                                let (width, height) = display.request_size(width, height);
                                eprint!("\r\nvmmc-vmx: asked the guest for {width}x{height}\r\n");
                            }
                        }
                        Input::Quit => {
                            targets.quit.store(true, Ordering::Relaxed);
                            if let Some(target) = targets.get() {
                                target.stop.request_stop(StopReason::Requested(QUIT_CODE));
                            }
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(())
}

/// Feeds `pending` to the guest's UART as fast as it drains. Input typed while the
/// machine is stopping or restarting is dropped.
fn send(targets: &Targets, mut pending: Vec<u8>) {
    let Some(target) = targets.get() else { return };
    while !pending.is_empty() {
        let sent = target.serial.send(&pending);
        pending.drain(..sent);
        if sent == 0 {
            // The guest hasn't drained the UART's FIFO yet.
            thread::sleep(Duration::from_millis(1));
        }
        if target.stop.reason().is_some() {
            return;
        }
    }
}

/// Types `text` on the guest's PS/2 keyboard, a key at a time.
fn type_keys(targets: &Targets, text: &[u8]) {
    const LEFT_SHIFT: u16 = 0x2a;
    let Some(target) = targets.get() else { return };
    for &byte in text {
        let Some((scancode, shift)) = key_for(byte) else {
            continue;
        };
        if shift {
            target.ps2.key(LEFT_SHIFT, true);
        }
        target.ps2.key(scancode, true);
        target.ps2.key(scancode, false);
        if shift {
            target.ps2.key(LEFT_SHIFT, false);
        }
        // About as fast as a person types quickly.
        thread::sleep(Duration::from_millis(20));
    }
}

/// The set 1 scan code for a character on a US keyboard, and whether it needs Shift.
fn key_for(byte: u8) -> Option<(u16, bool)> {
    const ROWS: [(&[u8], &[u8], u16); 4] = [
        (b"1234567890-=", b"!@#$%^&*()_+", 0x02),
        (b"qwertyuiop[]", b"QWERTYUIOP{}", 0x10),
        (b"asdfghjkl;'`", b"ASDFGHJKL:\"~", 0x1e),
        (b"zxcvbnm,./", b"ZXCVBNM<>?", 0x2c),
    ];
    for (plain, shifted, first) in ROWS {
        if let Some(at) = plain.iter().position(|&c| c == byte) {
            return Some((first + at as u16, false));
        }
        if let Some(at) = shifted.iter().position(|&c| c == byte) {
            return Some((first + at as u16, true));
        }
    }
    match byte {
        b'\\' => Some((0x2b, false)),
        b'|' => Some((0x2b, true)),
        b' ' => Some((0x39, false)),
        b'\r' | b'\n' => Some((0x1c, false)),
        b'\t' => Some((0x0f, false)),
        0x08 | 0x7f => Some((0x0e, false)),
        0x1b => Some((0x01, false)),
        _ => None,
    }
}

/// Guest console output, written straight through to stdout.
pub struct ConsoleOutput;

impl Write for ConsoleOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(buf)?;
        // Interactive output (prompts, echoed keys) must show up without a newline.
        stdout.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Puts the Windows console into raw VT mode for as long as it lives, so keys reach
/// the guest unprocessed and the guest's escape sequences render. Does nothing when
/// stdin or stdout isn't a console.
pub struct RawConsole {
    #[cfg(windows)]
    saved: Option<windows_console::Saved>,
}

impl RawConsole {
    pub fn enable() -> Self {
        Self {
            #[cfg(windows)]
            saved: windows_console::enable_raw(),
        }
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(saved) = self.saved.take() {
            windows_console::restore(saved);
        }
    }
}

#[cfg(windows)]
mod windows_console {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_DELETE, VK_DOWN, VK_END, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
    };

    /// What keys without a character send, as a VT terminal does.
    const SEQUENCES: [(u16, &[u8]); 10] = [
        (VK_UP.0, b"\x1b[A"),
        (VK_DOWN.0, b"\x1b[B"),
        (VK_RIGHT.0, b"\x1b[C"),
        (VK_LEFT.0, b"\x1b[D"),
        (VK_HOME.0, b"\x1b[H"),
        (VK_END.0, b"\x1b[F"),
        (VK_INSERT.0, b"\x1b[2~"),
        (VK_DELETE.0, b"\x1b[3~"),
        (VK_PRIOR.0, b"\x1b[5~"),
        (VK_NEXT.0, b"\x1b[6~"),
    ];

    /// Reads what's typed in the console as input events, waiting on the console
    /// between them. A read left pending on a console in raw mode stops Windows from
    /// calling the window's low-level keyboard hook, so the guest got no keys from
    /// the window; waiting on the handle doesn't. `None` if stdin isn't a console.
    pub fn reader() -> Option<impl FnMut() -> Option<Vec<u8>> + Send> {
        // SAFETY: a plain call that returns the process's standard handle.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
        mode(handle)?;
        // Console handles are process-wide; the reader just keeps the value.
        let handle = handle.0 as usize;
        Some(move || {
            let handle = HANDLE(handle as *mut _);
            let mut records = [INPUT_RECORD::default(); 64];
            loop {
                let mut count = 0u32;
                // SAFETY: a console input handle; `records` and `count` are valid
                // out-buffers, and only key events' union members are read.
                unsafe {
                    if WaitForSingleObject(handle, INFINITE) != WAIT_OBJECT_0 {
                        return None;
                    }
                    ReadConsoleInputW(handle, &mut records, &mut count).ok()?;
                }
                let mut units = Vec::new();
                let mut bytes = Vec::new();
                for record in &records[..count as usize] {
                    if u32::from(record.EventType) != KEY_EVENT {
                        continue;
                    }
                    // SAFETY: a key event's record holds a KEY_EVENT_RECORD.
                    let key = unsafe { record.Event.KeyEvent };
                    if !key.bKeyDown.as_bool() {
                        continue;
                    }
                    let repeat = usize::from(key.wRepeatCount.max(1));
                    // SAFETY: the Unicode member, as ReadConsoleInputW fills it.
                    let unit = unsafe { key.uChar.UnicodeChar };
                    if unit != 0 {
                        units.extend(std::iter::repeat_n(unit, repeat));
                    } else if let Some((_, sequence)) = SEQUENCES
                        .iter()
                        .find(|(key_code, _)| *key_code == key.wVirtualKeyCode)
                    {
                        // Characters before it stay in order.
                        bytes.extend(String::from_utf16_lossy(&units).bytes());
                        units.clear();
                        for _ in 0..repeat {
                            bytes.extend_from_slice(sequence);
                        }
                    }
                }
                bytes.extend(String::from_utf16_lossy(&units).bytes());
                if !bytes.is_empty() {
                    return Some(bytes);
                }
            }
        })
    }

    pub struct Saved {
        input: (HANDLE, CONSOLE_MODE),
        output: (HANDLE, CONSOLE_MODE),
    }

    fn mode(handle: HANDLE) -> Option<CONSOLE_MODE> {
        let mut mode = CONSOLE_MODE::default();
        // SAFETY: `mode` is a valid out-pointer; a non-console handle just fails.
        unsafe { GetConsoleMode(handle, &mut mode) }.ok()?;
        Some(mode)
    }

    pub fn enable_raw() -> Option<Saved> {
        // SAFETY: plain calls that return the process's standard handles.
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
        // SAFETY: as above.
        let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.ok()?;
        let saved = Saved {
            input: (input, mode(input)?),
            output: (output, mode(output)?),
        };
        let raw_input = (saved.input.1
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let vt_output = saved.output.1 | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        // SAFETY: both handles are consoles (GetConsoleMode succeeded on them).
        unsafe {
            SetConsoleMode(input, raw_input).ok()?;
            SetConsoleMode(output, vt_output).ok()?;
        }
        Some(saved)
    }

    pub fn restore(saved: Saved) {
        // SAFETY: restoring modes read from these console handles earlier.
        unsafe {
            let _ = SetConsoleMode(saved.input.0, saved.input.1);
            let _ = SetConsoleMode(saved.output.0, saved.output.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_a_z_asks_for_a_screen_size() {
        let mut filter = EscapeFilter::default();
        // The size can come in pieces; input around it still reaches the guest.
        assert_eq!(filter.filter(b"a\x01z1500x8"), [guest(b"a")]);
        assert_eq!(
            filter.filter(b"44\rb"),
            [Input::RequestSize(1500, 844), guest(b"b")]
        );
        assert!(filter.filter(b"\x01zbig\r").is_empty(), "not a size");
    }

    fn guest(bytes: &[u8]) -> Input {
        Input::Guest(bytes.to_vec())
    }

    #[test]
    fn plain_input_passes_through() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"ls -l\r"), [guest(b"ls -l\r")]);
    }

    #[test]
    fn ctrl_a_x_quits_even_across_reads() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"ab\x01"), [guest(b"ab")]);
        assert_eq!(filter.filter(b"x"), [Input::Quit]);
    }

    #[test]
    fn ctrl_a_p_presses_the_power_button_in_order() {
        let mut filter = EscapeFilter::default();
        assert_eq!(
            filter.filter(b"a\x01pb"),
            [guest(b"a"), Input::PowerButton, guest(b"b")]
        );
    }

    #[test]
    fn ctrl_a_r_dumps_state() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"\x01R"), [Input::DumpState]);
    }

    #[test]
    fn ctrl_a_k_switches_input_to_the_keyboard_and_back() {
        let mut filter = EscapeFilter::default();
        assert_eq!(
            filter.filter(b"ls\x01kvmmc\r\x01kpwd"),
            [guest(b"ls"), Input::Keys(b"vmmc\r".to_vec()), guest(b"pwd")]
        );
    }

    #[test]
    fn characters_map_to_us_keyboard_scan_codes() {
        assert_eq!(key_for(b'a'), Some((0x1e, false)));
        assert_eq!(key_for(b'A'), Some((0x1e, true)));
        assert_eq!(key_for(b'1'), Some((0x02, false)));
        assert_eq!(key_for(b'0'), Some((0x0b, false)));
        assert_eq!(key_for(b')'), Some((0x0b, true)));
        assert_eq!(key_for(b'm'), Some((0x32, false)));
        assert_eq!(key_for(b'/'), Some((0x35, false)));
        assert_eq!(key_for(b'"'), Some((0x28, true)));
        assert_eq!(key_for(b'~'), Some((0x29, true)));
        assert_eq!(key_for(b' '), Some((0x39, false)));
        assert_eq!(key_for(b'\r'), Some((0x1c, false)));
        assert_eq!(key_for(0), None);
    }

    #[test]
    fn ctrl_a_ctrl_a_sends_one_ctrl_a_and_unknown_escapes_vanish() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"\x01\x01z\x01qy"), [guest(b"\x01zy")]);
    }
}
