//! A running VM's console in the app: its shared screen, and a thread that sends input
//! and commands down its pipe, so a slow worker never stalls the window. The thread
//! also asks now and then what the guest copied to its clipboard, and answers the calls
//! drag and drop waits on.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ipc::pipe::Section;
use ipc::screen::{Cursor, Picture};
use ipc::{Request, Response};
use vmconfig::Vm;

/// How often the guest's clipboard is asked for.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// How long [`Session::call`] waits for its answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// A request for the console's thread, and where its answer goes if someone waits.
type Queued = (Request, Option<Sender<Result<Response, String>>>);

pub struct Session {
    /// The VM's file.
    pub path: PathBuf,
    /// The VM, for connections of its own (a drag's files).
    pub vm: Vm,
    requests: Sender<Queued>,
    /// Where the keyboard hook's keys go, into `requests`.
    keys: Sender<Request>,
    /// What the guest copies, from the request thread.
    copied: Receiver<String>,
    section: Section,
    frame_seq: u32,
    cursor_seq: u32,
    /// The size of the last picture.
    pub frame: (u32, u32),
    /// The size last asked of the guest.
    pub asked: Option<(u32, u32)>,
}

impl Session {
    /// Connects to the running VM's worker and maps its screen.
    pub fn open(vm: &Vm) -> Result<Self, String> {
        let mut connection = vmmc_client::connect(vm, Duration::from_millis(500))?;
        let section = connection.screen()?;
        let (requests, queued) = mpsc::channel::<Queued>();
        let (copies, copied) = mpsc::channel::<String>();
        // Keys, as plain requests, for the hook's thread.
        let (keys, typed) = mpsc::channel::<Request>();
        {
            let requests = requests.clone();
            thread::Builder::new()
                .name("console-keys".into())
                .spawn(move || {
                    for key in typed {
                        if requests.send((key, None)).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        thread::Builder::new()
            .name("console-requests".into())
            .spawn(move || {
                // What the guest had copied before the console opened stays there.
                let mut since = u64::MAX;
                let mut polled = Instant::now() - CLIPBOARD_POLL;
                loop {
                    match queued.recv_timeout(CLIPBOARD_POLL) {
                        Ok((request, reply)) => {
                            let answer = connection.call(&request);
                            let gone = answer
                                .as_ref()
                                .is_err_and(|e| e.starts_with("the VM's worker stopped"));
                            if let Some(reply) = reply {
                                let _ = reply.send(answer);
                            }
                            if gone {
                                tracing::debug!("the VM's worker went away");
                                return;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if polled.elapsed() < CLIPBOARD_POLL {
                        continue;
                    }
                    polled = Instant::now();
                    match connection.call(&Request::GuestClipboard { since }) {
                        Ok(Response::GuestClipboard { seq, text }) => {
                            if let Some(text) = text
                                && since != u64::MAX
                                && copies.send(text).is_err()
                            {
                                return;
                            }
                            since = seq;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(%e, "the VM's worker went away");
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: vm.path.clone(),
            vm: vm.clone(),
            requests,
            keys,
            copied,
            section,
            frame_seq: 0,
            cursor_seq: 0,
            frame: (1, 1),
            asked: None,
        })
    }

    /// Sends a request; false once the worker is gone.
    pub fn send(&self, request: Request) -> bool {
        self.requests.send((request, None)).is_ok()
    }

    /// Sends a request and waits (a little) for its answer.
    pub fn call(&self, request: Request) -> Result<Response, String> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .send((request, Some(reply)))
            .map_err(|_| "the VM's worker went away".to_owned())?;
        answer
            .recv_timeout(CALL_TIMEOUT)
            .map_err(|_| "the VM's worker didn't answer".to_owned())?
    }

    /// The text the guest copied last, if it copied something since this was asked.
    pub fn guest_copied(&self) -> Option<String> {
        self.copied.try_iter().last()
    }

    /// Where keys go, for the keyboard hook.
    pub fn sender(&self) -> Sender<Request> {
        self.keys.clone()
    }

    /// A new picture, if the guest drew one.
    pub fn picture(&mut self) -> Option<Picture> {
        let picture = self.section.view().read_picture(&mut self.frame_seq)?;
        self.frame = (picture.width, picture.height);
        Some(picture)
    }

    /// The guest's cursor, if it changed.
    pub fn cursor(&mut self) -> Option<Cursor> {
        self.section.view().read_cursor(&mut self.cursor_seq)
    }
}

/// Where `(x, y)` in an `area`-sized view lands on the tablet, when a `frame`-sized
/// picture is scaled to fit the view and centred (Slint's `contain`).
pub fn tablet_position(x: f32, y: f32, area: (f32, f32), frame: (u32, u32)) -> (u32, u32) {
    const TABLET_MAX: f32 = 32767.0;
    let (frame_w, frame_h) = (frame.0.max(1) as f32, frame.1.max(1) as f32);
    let scale = (area.0 / frame_w)
        .min(area.1 / frame_h)
        .max(f32::MIN_POSITIVE);
    let (shown_w, shown_h) = (frame_w * scale, frame_h * scale);
    let (left, top) = ((area.0 - shown_w) / 2.0, (area.1 - shown_h) / 2.0);
    let fraction = |value: f32, start: f32, size: f32| ((value - start) / size).clamp(0.0, 1.0);
    (
        (fraction(x, left, shown_w) * TABLET_MAX).round() as u32,
        (fraction(y, top, shown_h) * TABLET_MAX).round() as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_maps_onto_the_letterboxed_picture() {
        // A 4:3 picture in a wide view: bars left and right.
        let area = (2000.0, 750.0);
        let frame = (1024, 768);
        assert_eq!(tablet_position(500.0, 0.0, area, frame), (0, 0));
        assert_eq!(tablet_position(1000.0, 375.0, area, frame), (16384, 16384));
        assert_eq!(tablet_position(1999.0, 749.0, area, frame), (32767, 32723));
        assert_eq!(tablet_position(10.0, 10.0, area, frame).0, 0, "on the bar");
    }
}
