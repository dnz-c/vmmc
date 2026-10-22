//! vmmc-agent: vmmc's helper in a Linux guest, from the vmmc Tools disc. It shares the
//! clipboard's text with the host and drags files between the host and the guest, as
//! `vmmcagent.exe` does in a Windows guest (the `agent_proto` crate has the protocol).
//!
//! - **The control line** is the guest's second serial port, `/dev/ttyS1`, which the
//!   install script lets the desktop's user open.
//! - **The desktop** is reached through X11 (see [`x11`]).
//! - **Files** go over TCP, to the address the worker names (see [`files`]).
//!
//! The install script puts it in `/usr/local/bin` and starts it with each desktop
//! session (`/etc/xdg/autostart`). One runs per user.

mod files;
mod serial;
mod x11;

use std::fs::File;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agent_proto::{Frame, Message, Parser, features};

use crate::files::Config;
use crate::serial::Line;

/// How often the agent says hello.
const HELLO_INTERVAL: Duration = Duration::from_secs(30);

/// What the desktop's thread acts on, besides the display's events.
pub enum Input {
    /// The host's clipboard.
    HostClipboard(String),
    Message(Message),
}

/// Wakes the desktop's thread from its wait on the display: a pipe it polls.
#[derive(Clone)]
pub struct Waker {
    read: Arc<OwnedFd>,
    write: Arc<OwnedFd>,
}

impl Waker {
    fn new() -> std::io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills both descriptors, which are then owned here.
        unsafe {
            if libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                read: Arc::new(OwnedFd::from_raw_fd(fds[0])),
                write: Arc::new(OwnedFd::from_raw_fd(fds[1])),
            })
        }
    }

    fn wake(&self) {
        // SAFETY: a one-byte write to an open, non-blocking pipe.
        unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    pub fn fd(&self) -> i32 {
        self.read.as_raw_fd()
    }

    pub fn drain(&self) {
        let mut buffer = [0u8; 64];
        // SAFETY: reads into a local buffer from an open, non-blocking pipe.
        while unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
    }
}

/// A line for the worker's log (and this program's).
pub fn log(line: &Line, text: &str) {
    eprintln!("vmmc-agent: {text}");
    line.send(&Message::Log { text: text.into() }.frame());
}

fn hello(line: &Line) {
    line.send(
        &Message::Hello {
            version: agent_proto::VERSION,
            os: "linux".into(),
            features: features::CLIPBOARD | features::DRAG_TO_GUEST | features::DRAG_FROM_GUEST,
        }
        .frame(),
    );
}

/// One agent per user: a lock on a file in the user's runtime directory.
fn single() -> Option<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let file = File::create(dir.join(format!("vmmc-agent-{uid}.lock"))).ok()?;
    // SAFETY: flock on an open file.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    locked.then_some(file)
}

/// Reads frames from the host: hellos and the data address are answered here, the
/// rest goes to the desktop's thread.
fn read_line(line: &Line, config: &Mutex<Option<Config>>, inputs: &Sender<Input>, waker: &Waker) {
    let mut parser = Parser::default();
    let mut buffer = [0u8; 4096];
    loop {
        let read = match line.read(&mut buffer) {
            Ok(0) | Err(_) => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Ok(read) => read,
        };
        for frame in parser.feed(&buffer[..read]) {
            let input = match frame {
                Frame::Clipboard(text) => Input::HostClipboard(text),
                Frame::Message(Message::Ping) => {
                    hello(line);
                    continue;
                }
                Frame::Message(Message::Config {
                    address,
                    port,
                    token,
                }) => {
                    *config.lock().unwrap() = Some(Config {
                        address: SocketAddrV4::new(Ipv4Addr::from(address), port),
                        token,
                    });
                    continue;
                }
                Frame::Message(Message::CancelTransfer { transfer }) => {
                    files::CANCELLED.store(transfer, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                Frame::Message(message) => Input::Message(message),
            };
            if inputs.send(input).is_err() {
                return;
            }
            waker.wake();
        }
    }
}

fn main() {
    let Some(_lock) = single() else {
        // Another one runs for this user.
        return;
    };
    let line = loop {
        match Line::open() {
            Ok(line) => break Arc::new(line),
            Err(e) => {
                eprintln!("vmmc-agent: can't open {}: {e}", serial::PORT);
                thread::sleep(Duration::from_secs(5));
            }
        }
    };
    files::clean_staging(true);
    let config = Arc::new(Mutex::new(None));
    let waker = Waker::new().expect("a pipe");
    let (inputs, received) = mpsc::channel();
    {
        let (line, config, waker) = (line.clone(), config.clone(), waker.clone());
        thread::spawn(move || read_line(&line, &config, &inputs, &waker));
    }
    // Hello now and then: the worker sends nothing until it has heard one (the firmware
    // reads the port as a keyboard), and a worker started anew, as a resume does, hasn't.
    {
        let line = line.clone();
        thread::spawn(move || {
            loop {
                hello(&line);
                thread::sleep(HELLO_INTERVAL);
            }
        });
    }
    let mut said = false;
    loop {
        match x11::Desktop::connect(line.clone(), config.clone()) {
            Ok(mut desktop) => {
                log(&line, "connected to the desktop");
                said = false;
                if let Err(e) = desktop.run(&received, &waker) {
                    log(&line, &format!("lost the desktop: {e}"));
                }
            }
            Err(e) => {
                if !said {
                    log(&line, &format!("no desktop yet: {e}"));
                    said = true;
                }
                // What the host sends meanwhile has nowhere to go.
                while received.try_recv().is_ok() {}
                thread::sleep(Duration::from_secs(5));
            }
        }
    }
}
