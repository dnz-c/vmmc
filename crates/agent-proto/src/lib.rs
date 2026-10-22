//! The protocol between vmmc's agent in a guest (`vmmcagent.exe` on Windows,
//! `vmmc-agent` on Linux, both from the vmmc Tools disc) and the VM's worker.
//!
//! - **The control line:** the guest's second serial port (COM2, `/dev/ttyS1`). The
//!   clipboard's text goes in [`clipboard`] frames, as the first agents had it; everything
//!   else is a [`Message`] in a frame of its own ([`MAGIC`], a length, the message).
//! - **Data connections:** file contents would crawl down a UART, so a drag's files go
//!   over TCP instead: the agent connects to the address the worker names in
//!   [`Message::Config`], which the VM's NAT answers itself, inside the worker. Each
//!   connection starts with [`Message::Attach`] and carries one transfer's
//!   [`Message::Chunk`]s, in the same frames.
//!
//! A drag from the host into the guest:
//!
//! 1. The worker moves the guest's pointer to where the host's drag is, and sends
//!    [`Message::HostDrag`] with the dragged items' names.
//! 2. The agent makes the items in a staging folder, opens a data connection for the
//!    transfer, puts a small window under the pointer and answers [`Message::DragArmed`].
//! 3. The worker presses the guest's left button: on the agent's window, which starts a
//!    drag in the guest of the staged items. The host's drag moves the guest's pointer.
//! 4. The files arrive; the agent answers [`Message::Received`] on the data connection.
//! 5. Once the host's drag has dropped and the files are all there, the worker lets the
//!    button go: the guest drops wherever the pointer is. The agent says
//!    [`Message::DragFinished`]. [`Message::CancelDrag`] calls it off instead.
//!
//! A drag from the guest out to the host:
//!
//! 1. The guest's left button is held and the host's pointer leaves the console. The
//!    worker sends [`Message::Probe`] and nudges the guest's pointer.
//! 2. The agent shows a window over the whole screen that takes drops. If a drag of files
//!    is going on, the guest's pointer brings it onto that window, and the agent answers
//!    [`Message::GuestDrag`] with the items' names; otherwise [`Message::NoGuestDrag`].
//! 3. The worker lets the button go (the guest's drag ends on the agent's window, which
//!    refuses it), and the host starts a drag of its own.
//! 4. [`Message::SendFiles`] asks for the items: the agent opens a data connection and
//!    sends them.

pub mod clipboard;
mod codec;
pub mod path;

use codec::{Reader, Writer};

/// The protocol's version, which [`Message::Hello`] reports.
pub const VERSION: u32 = 1;
/// What starts a message frame: then the message's length (4 bytes, little-endian) and
/// the message.
pub const MAGIC: [u8; 6] = *b"\x1bVAGNT";
/// No message is longer.
pub const MAX_MESSAGE: usize = 1 << 20;
/// A [`Chunk::Data`] holds at most this much.
pub const MAX_DATA: usize = 256 << 10;

/// [`Message::Hello`]'s feature bits: what an agent does.
pub mod features {
    pub const CLIPBOARD: u32 = 1;
    /// Takes drags from the host.
    pub const DRAG_TO_GUEST: u32 = 2;
    /// Gives drags to the host.
    pub const DRAG_FROM_GUEST: u32 = 4;
}

/// One of a drag's items: a file or a folder (whose contents come with it).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Item {
    /// Its name, without a path.
    pub name: String,
    pub dir: bool,
}

/// A piece of a transfer, the files of a drag: [`Chunk::Begin`] for each file or folder
/// in turn (a folder before what's in it), a file's [`Chunk::Data`] and then its
/// [`Chunk::End`], and last a [`Chunk::Finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Chunk {
    /// How much is coming, if the sender knows: it may come first.
    Total { files: u64, bytes: u64 },
    /// The next file or folder, by its path from the drag's root, `/` between names
    /// (see [`path::check`]). A file's size is what it was when it was looked at.
    Begin { path: String, dir: bool, size: u64 },
    /// More of the current file.
    Data(Vec<u8>),
    /// The current file is complete.
    End,
    /// That's everything, or (with an error) all there will be.
    Finish { error: Option<String> },
}

impl Chunk {
    /// Roughly how many bytes it takes up, for keeping queues of chunks in bounds.
    pub fn cost(&self) -> usize {
        match self {
            Self::Data(data) => data.len() + 16,
            Self::Begin { path, .. } => path.len() + 32,
            Self::Finish { error } => error.as_ref().map_or(0, String::len) + 16,
            Self::Total { .. } | Self::End => 16,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    // From the agent.
    /// The agent started, or the worker asked ([`Message::Ping`]).
    Hello {
        version: u32,
        /// `windows` or `linux`.
        os: String,
        /// [`features`] bits.
        features: u32,
    },
    /// A line for the worker's log.
    Log { text: String },
    /// A host drag's window is under the guest's pointer: press the button.
    DragArmed { drag: u32 },
    /// A host drag ended in the guest: dropped somewhere, or not.
    DragFinished { drag: u32, dropped: bool },
    /// A drag of these items is going on in the guest.
    GuestDrag { probe: u32, items: Vec<Item> },
    /// No drag of files is going on in the guest.
    NoGuestDrag { probe: u32 },

    // From the worker.
    /// Asks for a [`Message::Hello`].
    Ping,
    /// Where data connections go, and the token they show.
    Config {
        address: [u8; 4],
        port: u16,
        token: [u8; 16],
    },
    /// A drag from the host is coming into the guest: these items, `files` files of
    /// `bytes` bytes in all (0 if not known yet), in transfer `drag`.
    HostDrag {
        drag: u32,
        items: Vec<Item>,
        files: u64,
        bytes: u64,
    },
    /// The host's drag left the guest, or was called off.
    CancelDrag { drag: u32 },
    /// Is a drag going on in the guest? See the crate's documentation.
    Probe { probe: u32 },
    /// Send the items of probe `probe`'s drag, as transfer `transfer`.
    SendFiles { probe: u32, transfer: u32 },
    /// Stop transfer `transfer`.
    CancelTransfer { transfer: u32 },

    // On a data connection.
    /// The first message on a data connection, from the agent.
    Attach { token: [u8; 16], transfer: u32 },
    /// Part of a transfer.
    Chunk { transfer: u32, chunk: Chunk },
    /// The agent has all of a transfer's files (or gave up, with an error).
    Received {
        transfer: u32,
        error: Option<String>,
    },
}

// Message kinds, as they're encoded.
const HELLO: u8 = 1;
const LOG: u8 = 2;
const DRAG_ARMED: u8 = 3;
const DRAG_FINISHED: u8 = 4;
const GUEST_DRAG: u8 = 5;
const NO_GUEST_DRAG: u8 = 6;
const PING: u8 = 32;
const CONFIG: u8 = 33;
const HOST_DRAG: u8 = 34;
const CANCEL_DRAG: u8 = 35;
const PROBE: u8 = 36;
const SEND_FILES: u8 = 37;
const CANCEL_TRANSFER: u8 = 38;
const ATTACH: u8 = 64;
const CHUNK: u8 = 65;
const RECEIVED: u8 = 66;
// Chunk kinds.
const TOTAL: u8 = 1;
const BEGIN: u8 = 2;
const DATA: u8 = 3;
const END: u8 = 4;
const FINISH: u8 = 5;

impl Message {
    /// The message's encoding: its kind (a byte), then its fields in order. Numbers are
    /// little-endian; strings and byte strings are a 4-byte length and the bytes
    /// (strings UTF-8); a list is a 4-byte count and the items; an optional string is a
    /// byte (0 or 1) and, with 1, the string.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Self::Hello {
                version,
                os,
                features,
            } => {
                w.u8(HELLO);
                w.u32(*version);
                w.str(os);
                w.u32(*features);
            }
            Self::Log { text } => {
                w.u8(LOG);
                w.str(text);
            }
            Self::DragArmed { drag } => {
                w.u8(DRAG_ARMED);
                w.u32(*drag);
            }
            Self::DragFinished { drag, dropped } => {
                w.u8(DRAG_FINISHED);
                w.u32(*drag);
                w.u8(u8::from(*dropped));
            }
            Self::GuestDrag { probe, items } => {
                w.u8(GUEST_DRAG);
                w.u32(*probe);
                w.items(items);
            }
            Self::NoGuestDrag { probe } => {
                w.u8(NO_GUEST_DRAG);
                w.u32(*probe);
            }
            Self::Ping => w.u8(PING),
            Self::Config {
                address,
                port,
                token,
            } => {
                w.u8(CONFIG);
                w.raw(address);
                w.u16(*port);
                w.raw(token);
            }
            Self::HostDrag {
                drag,
                items,
                files,
                bytes,
            } => {
                w.u8(HOST_DRAG);
                w.u32(*drag);
                w.items(items);
                w.u64(*files);
                w.u64(*bytes);
            }
            Self::CancelDrag { drag } => {
                w.u8(CANCEL_DRAG);
                w.u32(*drag);
            }
            Self::Probe { probe } => {
                w.u8(PROBE);
                w.u32(*probe);
            }
            Self::SendFiles { probe, transfer } => {
                w.u8(SEND_FILES);
                w.u32(*probe);
                w.u32(*transfer);
            }
            Self::CancelTransfer { transfer } => {
                w.u8(CANCEL_TRANSFER);
                w.u32(*transfer);
            }
            Self::Attach { token, transfer } => {
                w.u8(ATTACH);
                w.raw(token);
                w.u32(*transfer);
            }
            Self::Chunk { transfer, chunk } => {
                w.u8(CHUNK);
                w.u32(*transfer);
                match chunk {
                    Chunk::Total { files, bytes } => {
                        w.u8(TOTAL);
                        w.u64(*files);
                        w.u64(*bytes);
                    }
                    Chunk::Begin { path, dir, size } => {
                        w.u8(BEGIN);
                        w.str(path);
                        w.u8(u8::from(*dir));
                        w.u64(*size);
                    }
                    Chunk::Data(data) => {
                        w.u8(DATA);
                        w.bytes(data);
                    }
                    Chunk::End => w.u8(END),
                    Chunk::Finish { error } => {
                        w.u8(FINISH);
                        w.optional_str(error.as_deref());
                    }
                }
            }
            Self::Received { transfer, error } => {
                w.u8(RECEIVED);
                w.u32(*transfer);
                w.optional_str(error.as_deref());
            }
        }
        w.0
    }

    /// Decodes a message; `None` if it's malformed or of a kind this version doesn't
    /// know.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let message = match r.u8()? {
            HELLO => Self::Hello {
                version: r.u32()?,
                os: r.str()?,
                features: r.u32()?,
            },
            LOG => Self::Log { text: r.str()? },
            DRAG_ARMED => Self::DragArmed { drag: r.u32()? },
            DRAG_FINISHED => Self::DragFinished {
                drag: r.u32()?,
                dropped: r.bool()?,
            },
            GUEST_DRAG => Self::GuestDrag {
                probe: r.u32()?,
                items: r.items()?,
            },
            NO_GUEST_DRAG => Self::NoGuestDrag { probe: r.u32()? },
            PING => Self::Ping,
            CONFIG => Self::Config {
                address: r.array()?,
                port: r.u16()?,
                token: r.array()?,
            },
            HOST_DRAG => Self::HostDrag {
                drag: r.u32()?,
                items: r.items()?,
                files: r.u64()?,
                bytes: r.u64()?,
            },
            CANCEL_DRAG => Self::CancelDrag { drag: r.u32()? },
            PROBE => Self::Probe { probe: r.u32()? },
            SEND_FILES => Self::SendFiles {
                probe: r.u32()?,
                transfer: r.u32()?,
            },
            CANCEL_TRANSFER => Self::CancelTransfer { transfer: r.u32()? },
            ATTACH => Self::Attach {
                token: r.array()?,
                transfer: r.u32()?,
            },
            CHUNK => {
                let transfer = r.u32()?;
                let chunk = match r.u8()? {
                    TOTAL => Chunk::Total {
                        files: r.u64()?,
                        bytes: r.u64()?,
                    },
                    BEGIN => Chunk::Begin {
                        path: r.str()?,
                        dir: r.bool()?,
                        size: r.u64()?,
                    },
                    DATA => {
                        let data = r.bytes()?;
                        if data.len() > MAX_DATA {
                            return None;
                        }
                        Chunk::Data(data)
                    }
                    END => Chunk::End,
                    FINISH => Chunk::Finish {
                        error: r.optional_str()?,
                    },
                    _ => return None,
                };
                Self::Chunk { transfer, chunk }
            }
            RECEIVED => Self::Received {
                transfer: r.u32()?,
                error: r.optional_str()?,
            },
            _ => return None,
        };
        // Fields a later version adds go at the end, and are ignored.
        Some(message)
    }

    /// The message as a frame.
    pub fn frame(&self) -> Vec<u8> {
        let body = self.encode();
        let mut frame = Vec::with_capacity(MAGIC.len() + 4 + body.len());
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        frame
    }
}

/// What comes off the control line or a data connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// The clipboard's text.
    Clipboard(String),
    Message(Message),
}

/// Takes frames of both kinds out of a byte stream, skipping anything between them and
/// messages it can't decode.
#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// Where in `pending` the next frame is looked for: what's before has been used.
    start: usize,
}

impl Parser {
    /// The frames `bytes` completes.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let rest = &self.pending[self.start..];
            // The first magic of either kind.
            let found = (0..rest.len()).find(|&at| {
                let tail = &rest[at..];
                let could_be = |magic: &[u8]| {
                    let n = tail.len().min(magic.len());
                    tail[..n] == magic[..n]
                };
                could_be(&MAGIC) || could_be(&clipboard::MAGIC)
            });
            let Some(at) = found else {
                self.start = self.pending.len();
                break;
            };
            self.start += at;
            let rest = &self.pending[self.start..];
            // Both magics are 6 bytes, then a 4-byte length.
            if rest.len() < 10 {
                break;
            }
            let message = rest.starts_with(&MAGIC);
            if !message && !rest.starts_with(&clipboard::MAGIC) {
                self.start += 1;
                continue;
            }
            let len = u32::from_le_bytes(rest[6..10].try_into().unwrap()) as usize;
            let limit = if message {
                MAX_MESSAGE
            } else {
                clipboard::MAX_TEXT
            };
            if len > limit {
                // Not a frame after all.
                self.start += 1;
                continue;
            }
            if rest.len() < 10 + len {
                break;
            }
            let body = &rest[10..10 + len];
            if message {
                if let Some(message) = Message::decode(body) {
                    frames.push(Frame::Message(message));
                }
            } else {
                frames.push(Frame::Clipboard(String::from_utf8_lossy(body).into_owned()));
            }
            self.start += 10 + len;
        }
        // What's been used goes, now and then rather than on every frame.
        if self.start > 0 && (self.start == self.pending.len() || self.start >= 64 << 10) {
            self.pending.drain(..self.start);
            self.start = 0;
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_messages() -> Vec<Message> {
        let items = vec![
            Item {
                name: "report.pdf".into(),
                dir: false,
            },
            Item {
                name: "Fotos".into(),
                dir: true,
            },
        ];
        vec![
            Message::Hello {
                version: VERSION,
                os: "windows".into(),
                features: features::CLIPBOARD | features::DRAG_TO_GUEST,
            },
            Message::Log {
                text: "héllo".into(),
            },
            Message::DragArmed { drag: 7 },
            Message::DragFinished {
                drag: 7,
                dropped: true,
            },
            Message::GuestDrag {
                probe: 3,
                items: items.clone(),
            },
            Message::NoGuestDrag { probe: 3 },
            Message::Ping,
            Message::Config {
                address: [10, 0, 2, 2],
                port: 5640,
                token: [9; 16],
            },
            Message::HostDrag {
                drag: 1,
                items,
                files: 12,
                bytes: 1 << 33,
            },
            Message::CancelDrag { drag: 1 },
            Message::Probe { probe: 2 },
            Message::SendFiles {
                probe: 2,
                transfer: 5,
            },
            Message::CancelTransfer { transfer: 5 },
            Message::Attach {
                token: [1; 16],
                transfer: 5,
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::Total {
                    files: 2,
                    bytes: 10,
                },
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::Begin {
                    path: "Fotos/a.jpg".into(),
                    dir: false,
                    size: 10,
                },
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::Data(vec![0x1b, b'V', b'A', b'G', b'N', b'T', 0, 1]),
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::End,
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::Finish { error: None },
            },
            Message::Chunk {
                transfer: 5,
                chunk: Chunk::Finish {
                    error: Some("gone".into()),
                },
            },
            Message::Received {
                transfer: 5,
                error: None,
            },
        ]
    }

    #[test]
    fn messages_round_trip_and_come_out_of_any_split() {
        let messages = all_messages();
        let mut stream = b"boot noise \x1bVA".to_vec();
        for message in &messages {
            assert_eq!(Message::decode(&message.encode()).as_ref(), Some(message));
            stream.extend(message.frame());
            stream.extend(clipboard::frame("between"));
        }
        let expected: Vec<Frame> = messages
            .iter()
            .flat_map(|m| {
                [
                    Frame::Message(m.clone()),
                    Frame::Clipboard("between".into()),
                ]
            })
            .collect();
        assert_eq!(Parser::default().feed(&stream), expected);
        let mut parser = Parser::default();
        let mut frames = Vec::new();
        for byte in &stream {
            frames.extend(parser.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(frames, expected);
    }

    #[test]
    fn truncated_and_unknown_messages_are_skipped() {
        for message in all_messages() {
            let body = message.encode();
            for len in 0..body.len() {
                // Shorter is malformed, except where a message is a prefix of another
                // kind's (none are).
                assert_eq!(
                    Message::decode(&body[..len]),
                    None,
                    "{message:?} cut at {len}"
                );
            }
        }
        let mut stream = MAGIC.to_vec();
        stream.extend(1u32.to_le_bytes());
        stream.push(200);
        stream.extend(MAGIC);
        stream.extend(u32::MAX.to_le_bytes());
        stream.extend(Message::Ping.frame());
        assert_eq!(
            Parser::default().feed(&stream),
            [Frame::Message(Message::Ping)]
        );
    }

    #[test]
    fn oversized_data_is_refused() {
        let message = Message::Chunk {
            transfer: 1,
            chunk: Chunk::Data(vec![0; MAX_DATA + 1]),
        };
        assert_eq!(Message::decode(&message.encode()), None);
    }

    #[test]
    fn a_cheap_fuzz_never_panics() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut parser = Parser::default();
        for _ in 0..5000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 40) as usize;
            let mut bytes = if seed.is_multiple_of(3) {
                MAGIC.to_vec()
            } else {
                Vec::new()
            };
            bytes.extend((0..len).map(|i| (seed >> (i % 8 * 8)) as u8));
            let _ = Message::decode(&bytes);
            let _ = parser.feed(&bytes);
        }
    }
}
