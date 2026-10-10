//! The host terminal as the guest's serial console.
//!
//! Input goes to the guest byte for byte. Ctrl-A starts an escape: Ctrl-A x quits,
//! Ctrl-A Ctrl-A sends a literal Ctrl-A.

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use machine::SerialInput;
use vm_core::{StopHandle, StopReason};

const CTRL_A: u8 = 0x01;

/// The stop code used when the user quits with Ctrl-A x.
pub const QUIT_CODE: u32 = 0;

/// Splits terminal input into bytes for the guest and the quit command.
#[derive(Debug, Default)]
pub struct EscapeFilter {
    after_ctrl_a: bool,
}

impl EscapeFilter {
    /// Returns the bytes to forward, and whether the user asked to quit.
    pub fn filter(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut forward = Vec::with_capacity(input.len());
        for &byte in input {
            if std::mem::take(&mut self.after_ctrl_a) {
                match byte {
                    b'x' | b'X' => return (forward, true),
                    CTRL_A => forward.push(CTRL_A),
                    // Unknown escapes are swallowed, like QEMU's.
                    _ => {}
                }
            } else if byte == CTRL_A {
                self.after_ctrl_a = true;
            } else {
                forward.push(byte);
            }
        }
        (forward, false)
    }
}

/// Forwards stdin to the guest on a background thread until the user quits or stdin
/// closes. The thread is never joined: a blocked console read can't be interrupted.
pub fn spawn_input(serial: SerialInput, stop: StopHandle) -> io::Result<()> {
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
                let (mut pending, quit) = filter.filter(&buffer[..read]);
                while !pending.is_empty() {
                    let sent = serial.send(&pending);
                    pending.drain(..sent);
                    if sent == 0 {
                        // The guest hasn't drained the UART's FIFO yet.
                        thread::sleep(Duration::from_millis(1));
                    }
                    if stop.reason().is_some() {
                        return;
                    }
                }
                if quit {
                    stop.request_stop(StopReason::Requested(QUIT_CODE));
                    return;
                }
            }
        })?;
    Ok(())
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

    #[test]
    fn plain_input_passes_through() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"ls -l\r"), (b"ls -l\r".to_vec(), false));
    }

    #[test]
    fn ctrl_a_x_quits_even_across_reads() {
        let mut filter = EscapeFilter::default();
        assert_eq!(filter.filter(b"ab\x01"), (b"ab".to_vec(), false));
        assert_eq!(filter.filter(b"x"), (Vec::new(), true));
    }

    #[test]
    fn ctrl_a_ctrl_a_sends_one_ctrl_a_and_unknown_escapes_vanish() {
        let mut filter = EscapeFilter::default();
        assert_eq!(
            filter.filter(b"\x01\x01z\x01qy"),
            (b"\x01zy".to_vec(), false)
        );
    }
}
