//! Clipboard sharing with vmmc's agent in the guest, over the guest's COM2 (see
//! `ipc::clipboard`): what the guest copies is kept for the app to ask for, and what
//! the app sends goes to the guest's clipboard. A guest reset rebuilds the machine; the
//! clipboard stays.

use std::io::{self, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use ipc::clipboard::{Parser, frame};
use machine::SerialInput;

/// How long text for the guest waits for its agent to read it.
const FEED_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Clipboard {
    /// What the guest copied last, and how many times it has.
    guest: Mutex<(u64, String)>,
    /// Frames for the guest.
    outbox: Mutex<Sender<Vec<u8>>>,
    /// Say on the console when the guest copies something.
    announce: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Clipboard {
    /// A clipboard whose text for the guest goes to `agent`'s port: the running
    /// machine's COM2, when there is one.
    pub fn start(
        agent: impl Fn() -> Option<SerialInput> + Send + 'static,
        announce: bool,
    ) -> io::Result<Arc<Self>> {
        let (outbox, frames) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                for frame in frames {
                    feed(&agent, frame);
                }
            })?;
        Ok(Arc::new(Self {
            guest: Mutex::default(),
            outbox: Mutex::new(outbox),
            announce,
        }))
    }

    /// Puts `text` on the guest's clipboard.
    pub fn put_in_guest(&self, text: &str) {
        let _ = lock(&self.outbox).send(frame(text));
    }

    /// How many times the guest has copied something, and what it copied last if
    /// that's newer than `since`.
    pub fn guest_copied(&self, since: u64) -> (u64, Option<String>) {
        let guest = lock(&self.guest);
        (guest.0, (guest.0 > since).then(|| guest.1.clone()))
    }

    /// Where a machine's COM2 output goes.
    pub fn sink(self: &Arc<Self>) -> Box<dyn Write + Send> {
        Box::new(Sink {
            parser: Parser::default(),
            clipboard: self.clone(),
        })
    }
}

/// Feeds a frame to the agent's port as fast as the guest reads it. With no machine,
/// or no agent reading, it's given up after a while.
fn feed(agent: &impl Fn() -> Option<SerialInput>, mut pending: Vec<u8>) {
    let mut waited = Duration::ZERO;
    while !pending.is_empty() {
        let Some(port) = agent() else { return };
        let sent = port.send(&pending);
        pending.drain(..sent);
        if sent > 0 {
            waited = Duration::ZERO;
            continue;
        }
        if waited > FEED_TIMEOUT {
            tracing::debug!("the guest's agent doesn't read its port");
            return;
        }
        thread::sleep(Duration::from_millis(1));
        waited += Duration::from_millis(1);
    }
}

/// The guest's COM2 output: frames from its agent.
struct Sink {
    parser: Parser,
    clipboard: Arc<Clipboard>,
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for text in self.parser.feed(bytes) {
            let characters = text.chars().count();
            {
                let mut guest = lock(&self.clipboard.guest);
                guest.0 += 1;
                guest.1 = text;
            }
            if self.clipboard.announce {
                // On the console's output, with the guest's: the clipboard's length,
                // never its text.
                print!("\r\nvmmc-vmx: the guest copied {characters} characters\r\n");
                let _ = io::stdout().flush();
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
