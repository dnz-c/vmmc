//! How vmmc's app and command line talk to a running VM's worker (`vmmc-vmx serve`).
//!
//! - **Control:** a named pipe per VM, `\\.\pipe\vmmc-<uuid>`, carrying [`Request`]s
//!   and their [`Response`]s, each a little-endian `u32` length and a postcard body.
//!   A connection starts with [`Request::Hello`], whose versions must match.
//! - **Screen:** the worker shares a memory section holding the guest's picture and
//!   cursor ([`screen`]); a client gets it with [`Request::Screen`].

#[cfg(windows)]
pub mod pipe;
pub mod screen;

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The protocol version; both ends must speak the same one.
pub const VERSION: u32 = 2;
/// No message is larger.
pub const MAX_MESSAGE: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The first request on a connection.
    Hello {
        version: u32,
    },
    Status,
    /// Presses the ACPI power button: the guest shuts down.
    PowerButton,
    /// Stops the VM at once, as pulling the plug; the worker exits.
    Stop,
    Reset,
    /// A key went down or up: a PS/2 set 1 scan code, `0xE0nn` for extended keys.
    Key {
        code: u16,
        pressed: bool,
    },
    /// The pointer (the USB tablet) is at `x, y`, each 0 to `TABLET_MAX`, with these
    /// buttons held (bit 0 left, 1 right, 2 middle) and the wheel turned.
    Pointer {
        x: u32,
        y: u32,
        buttons: u8,
        wheel: i32,
    },
    /// Asks the guest for a screen this size (as resizing its window does).
    RequestSize {
        width: u32,
        height: u32,
    },
    /// The shared screen: answered with [`Response::Screen`].
    Screen,
    /// Puts a disc (an ISO the worker can read) into DVD drive `drive` (0 for the
    /// first), or takes it out (`None`), while the guest runs. Lasts until the VM
    /// stops; the VM's file isn't changed.
    InsertDisc {
        drive: u32,
        path: Option<String>,
    },
    /// Takes a snapshot of the running VM, with its memory: answered with
    /// [`Response::Snapshot`].
    Snapshot {
        name: String,
        description: String,
    },
    /// Reverts the VM to a snapshot (by ID or name): what it did since is gone. A
    /// snapshot of a running VM carries on from its state; one taken while the VM was
    /// off leaves it off, and the worker exits.
    Revert {
        snapshot: String,
    },
    /// Saves the VM's state and stops it; starting it again resumes it. The worker exits.
    Suspend,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u32,
        /// The VM's name and UUID, and the worker's process ID.
        name: String,
        uuid: String,
        pid: u32,
    },
    Status(Status),
    Done,
    /// The screen's section, as a handle in the worker's process (duplicate it with
    /// the `pid` from [`Response::Hello`]), and its size.
    Screen {
        handle: u64,
        size: u64,
    },
    /// A snapshot was taken: its ID, and how long the guest was paused for it.
    Snapshot {
        id: String,
        paused_ms: u64,
    },
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    /// How the worker is confined, as it sees its own process.
    pub sandbox: String,
    /// The guest's picture, if it shows one.
    pub screen: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Between machines: starting, or restarting after a guest reset.
    Starting,
    Running,
}

/// Writes one message.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let body =
        postcard::to_stdvec(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Reads one message; an end of stream before it starts is `UnexpectedEof`.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Sends a request and reads its response.
pub fn call(stream: &mut (impl Read + Write), request: &Request) -> io::Result<Response> {
    write_message(stream, request)?;
    read_message(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_in_frames() {
        let requests = [
            Request::Hello { version: VERSION },
            Request::Pointer {
                x: 1,
                y: 32767,
                buttons: 5,
                wheel: -2,
            },
            Request::Key {
                code: 0xe053,
                pressed: true,
            },
        ];
        let mut wire = Vec::new();
        for request in &requests {
            write_message(&mut wire, request).unwrap();
        }
        let mut reader = &wire[..];
        for request in &requests {
            assert_eq!(&read_message::<Request>(&mut reader).unwrap(), request);
        }
        assert_eq!(
            read_message::<Request>(&mut reader).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_oversized_and_garbled_frames() {
        let mut wire = ((MAX_MESSAGE + 1) as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&[0; 16]);
        assert!(read_message::<Request>(&mut &wire[..]).is_err());
        let garbled = [3u8, 0, 0, 0, 0xff, 0xff, 0xff];
        assert!(read_message::<Request>(&mut &garbled[..]).is_err());
        // A cheap fuzz: no input panics.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 24) as usize;
            let mut bytes = vec![(len as u8).saturating_sub(4), 0, 0, 0];
            bytes.extend((0..len).map(|i| (seed >> (i % 8 * 8)) as u8));
            let _ = read_message::<Request>(&mut &bytes[..]);
            let _ = read_message::<Response>(&mut &bytes[..]);
        }
    }
}
