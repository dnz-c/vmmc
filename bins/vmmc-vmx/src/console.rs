//! The host terminal as the guest's serial console.
//!
//! Input goes to the guest byte for byte. Ctrl-A starts an escape: Ctrl-A x quits,
//! Ctrl-A p presses the ACPI power button, Ctrl-A s saves a screenshot (with
//! `--screenshot`), Ctrl-A r prints each vCPU's registers, Ctrl-A m saves guest RAM
//! and vCPU state to a temp directory, Ctrl-A Ctrl-A sends a literal Ctrl-A.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use machine::{Display, PowerButton, SerialInput};

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
    PowerButton,
    Screenshot,
    DumpState,
    SaveMemory,
    Quit,
}

/// Splits terminal input into bytes for the guest and escape commands, in order.
#[derive(Debug, Default)]
pub struct EscapeFilter {
    after_ctrl_a: bool,
}

impl EscapeFilter {
    pub fn filter(&mut self, input: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        let mut bytes = Vec::new();
        let flush = |bytes: &mut Vec<u8>, out: &mut Vec<Input>| {
            if !bytes.is_empty() {
                out.push(Input::Guest(std::mem::take(bytes)));
            }
        };
        for &byte in input {
            if std::mem::take(&mut self.after_ctrl_a) {
                match byte {
                    b'x' | b'X' => {
                        flush(&mut bytes, &mut out);
                        out.push(Input::Quit);
                        return out;
                    }
                    b'p' | b'P' => {
                        flush(&mut bytes, &mut out);
                        out.push(Input::PowerButton);
                    }
                    b's' | b'S' => {
                        flush(&mut bytes, &mut out);
                        out.push(Input::Screenshot);
                    }
                    b'r' | b'R' => {
                        flush(&mut bytes, &mut out);
                        out.push(Input::DumpState);
                    }
                    b'm' | b'M' => {
                        flush(&mut bytes, &mut out);
                        out.push(Input::SaveMemory);
                    }
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
        flush(&mut bytes, &mut out);
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

    /// Whether the user asked to quit, so the VM must not be restarted.
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }
}

/// Forwards stdin to the current machine on a background thread until the user quits
/// or stdin closes. The thread is never joined: a blocked console read can't be
/// interrupted.
pub fn spawn_input(targets: Targets) -> io::Result<()> {
    thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut filter = EscapeFilter::default();
            let mut buffer = [0u8; 256];
            let mut stdin = io::stdin().lock();
            loop {
                let read = match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                for input in filter.filter(&buffer[..read]) {
                    match input {
                        Input::Guest(pending) => send(&targets, pending),
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
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

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
    fn ctrl_a_ctrl_a_sends_one_ctrl_a_and_unknown_escapes_vanish() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"\x01\x01z\x01qy"), [guest(b"\x01zy")]);
    }
}
