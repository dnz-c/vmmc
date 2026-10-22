//! Local services: guest connections the NAT answers inside this process, through an
//! in-memory stream, rather than with a host socket. vmmc's agent in the guest sends a
//! drag's files this way, at the network's speed and without a listening socket on the
//! host.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How much one direction of a [`LocalStream`] holds before writes wait.
const CAPACITY: usize = 1 << 20;

/// A service the NAT answers itself: the guest's connections to the gateway on `port`
/// reach `accept`, as a [`LocalStream`], instead of the host's loopback.
#[derive(Clone)]
pub struct LocalService {
    pub port: u16,
    /// Called on its own thread for each connection.
    pub accept: Arc<dyn Fn(LocalStream) + Send + Sync>,
}

impl fmt::Debug for LocalService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalService")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// One direction: bytes written at one end, read at the other.
#[derive(Default)]
struct Pipe {
    state: Mutex<PipeState>,
    changed: Condvar,
}

#[derive(Default)]
struct PipeState {
    bytes: VecDeque<u8>,
    /// No more will be written: reads end once the bytes are gone.
    writer_closed: bool,
    /// Nothing more will be read: writes fail.
    reader_closed: bool,
}

impl Pipe {
    fn lock(&self) -> MutexGuard<'_, PipeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close_writer(&self) {
        self.lock().writer_closed = true;
        self.changed.notify_all();
    }

    fn close_reader(&self) {
        let mut state = self.lock();
        state.reader_closed = true;
        state.bytes.clear();
        drop(state);
        self.changed.notify_all();
    }
}

/// One end of an in-memory, two-way byte stream, like a connected socket: what one end
/// writes, the other reads. Shared between threads by reference (both `&LocalStream`
/// and `LocalStream` read and write). Dropping it shuts both directions.
pub struct LocalStream {
    incoming: Arc<Pipe>,
    outgoing: Arc<Pipe>,
    read_timeout: Mutex<Option<Duration>>,
}

impl LocalStream {
    /// A connected pair.
    pub fn pair() -> (Self, Self) {
        let (a, b) = (Arc::new(Pipe::default()), Arc::new(Pipe::default()));
        let end = |incoming: &Arc<Pipe>, outgoing: &Arc<Pipe>| Self {
            incoming: incoming.clone(),
            outgoing: outgoing.clone(),
            read_timeout: Mutex::new(None),
        };
        (end(&a, &b), end(&b, &a))
    }

    /// Reads wait at most this long, then fail with `TimedOut`; `None` waits for ever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) {
        *self
            .read_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = timeout;
    }

    pub fn shutdown(&self, how: Shutdown) {
        if matches!(how, Shutdown::Write | Shutdown::Both) {
            self.outgoing.close_writer();
        }
        if matches!(how, Shutdown::Read | Shutdown::Both) {
            self.incoming.close_reader();
        }
    }

    fn read_some(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let timeout = *self
            .read_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let pipe = &self.incoming;
        let mut state = pipe.lock();
        loop {
            if !state.bytes.is_empty() {
                let n = buffer.len().min(state.bytes.len());
                for (slot, byte) in buffer.iter_mut().zip(state.bytes.drain(..n)) {
                    *slot = byte;
                }
                drop(state);
                pipe.changed.notify_all();
                return Ok(n);
            }
            if state.writer_closed || state.reader_closed {
                return Ok(0);
            }
            state = match deadline {
                None => pipe
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    pipe.changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    fn write_some(&self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let pipe = &self.outgoing;
        let mut state = pipe.lock();
        loop {
            if state.reader_closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if state.writer_closed {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the stream is shut for writing",
                ));
            }
            let room = CAPACITY.saturating_sub(state.bytes.len());
            if room > 0 {
                let n = room.min(data.len());
                state.bytes.extend(&data[..n]);
                drop(state);
                pipe.changed.notify_all();
                return Ok(n);
            }
            state = pipe
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        self.shutdown(Shutdown::Both);
    }
}

impl Read for &LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_some(buffer)
    }
}

impl Write for &LocalStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_some(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_some(buffer)
    }
}

impl Write for LocalStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_some(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_cross_and_ends_are_seen() {
        let (a, b) = LocalStream::pair();
        let data: Vec<u8> = (0..3 * CAPACITY as u32).map(|i| i as u8).collect();
        let sent = data.clone();
        let writer = std::thread::spawn(move || {
            (&a).write_all(&sent).unwrap();
            a.shutdown(Shutdown::Write);
            // The other way still works.
            let mut back = Vec::new();
            (&a).read_to_end(&mut back).unwrap();
            back
        });
        let mut got = Vec::new();
        (&b).read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
        (&b).write_all(b"done").unwrap();
        drop(b);
        assert_eq!(writer.join().unwrap(), b"done");
    }

    #[test]
    fn writes_fail_once_the_reader_is_gone_and_reads_time_out() {
        let (a, b) = LocalStream::pair();
        b.set_read_timeout(Some(Duration::from_millis(20)));
        let mut buffer = [0u8; 4];
        assert_eq!(
            (&b).read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(b);
        assert_eq!(
            (&a).write_all(&[1; 10]).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!((&a).read(&mut buffer).unwrap(), 0);
    }
}
