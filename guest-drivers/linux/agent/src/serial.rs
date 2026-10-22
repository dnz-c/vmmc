//! The control line: the guest's second serial port, raw.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

/// The port the worker listens on: COM2.
pub const PORT: &str = "/dev/ttyS1";

pub struct Line {
    reader: File,
    writer: Mutex<File>,
}

impl Line {
    /// Opens the port, raw at 115200 baud, and keeps others out of it.
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(PORT)?;
        let fd = file.as_raw_fd();
        // SAFETY: termios calls on an open descriptor, with a zeroed struct they fill.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut termios);
            libc::cfsetspeed(&mut termios, libc::B115200);
            termios.c_cflag |= libc::CLOCAL | libc::CREAD;
            // Reads wait for at least a byte.
            termios.c_cc[libc::VMIN] = 1;
            termios.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::ioctl(fd, libc::TIOCEXCL);
            // Stale input from before the agent ran.
            libc::tcflush(fd, libc::TCIFLUSH);
        }
        Ok(Self {
            writer: Mutex::new(file.try_clone()?),
            reader: file,
        })
    }

    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        (&self.reader).read(buffer)
    }

    /// Sends a frame whole, between other threads' frames.
    pub fn send(&self, frame: &[u8]) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(frame).and_then(|()| writer.flush());
    }
}
