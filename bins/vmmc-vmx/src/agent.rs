//! vmmc's agent in the guest, as the worker sees it: the clipboard, and drag and drop.
//! The `agent_proto` crate has the protocol, and how a drag goes step by step.
//!
//! - **The control line** is the guest's COM2. What the agent sends arrives on a vCPU's
//!   thread, which only parses it and hands it on; frames for the agent go out from a
//!   thread of their own, as fast as the guest reads them.
//! - **Drags** are run by one thread that owns their state: the app's requests and the
//!   agent's messages reach it as events, and it drives the guest's pointer.
//! - **Transfers** (a drag's files) go over data connections, which the guest's NAT hands
//!   to [`Agent::service`]. Their chunks wait in bounded queues between the connection
//!   and the app, which pushes and pulls them through its pipe.
//!
//! A guest reset rebuilds the machine; the clipboard and the agent's state stay (the
//! agent says hello again once the guest is back).

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::net::Shutdown;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use agent_proto::{Chunk, Frame, Message, Parser, features};
use machine::{LocalService, LocalStream, SerialInput, TABLET_MAX};

use crate::console::Targets;

/// The gateway port data connections go to (the NAT's local service).
const DATA_PORT: u16 = 5640;
/// The gateway's address, as the NAT has it.
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// How long a frame for the guest waits for the agent to read it.
const FEED_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the worker asks for a hello while the agent hasn't said one.
const PING_INTERVAL: Duration = Duration::from_secs(3);
/// How long the agent gets to ready a drag from the host.
const ARM_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a probe looks for a drag in the guest.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the guest's pointer is nudged, while a probe looks or a drag is called off.
const NUDGE_INTERVAL: Duration = Duration::from_millis(40);
/// How far a nudge moves the pointer, in tablet units: a pixel or more on screens at
/// least 512 pixels wide.
const NUDGE_STEP: u32 = 64;
/// How long a drag that's being called off is nudged before the button goes up anyway.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the worker waits to hear how a dropped drag ended.
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);
/// How much a transfer's queue holds before whoever fills it waits.
const QUEUE_LIMIT: usize = 4 << 20;
/// The most a [`Agent::receive_chunks`] answer carries: it must fit a pipe message.
const BATCH_LIMIT: usize = 768 << 10;
/// How long a pipe request waits for chunks or for room.
const PIPE_WAIT: Duration = Duration::from_secs(1);
/// How long a data connection gets to say which transfer it's for.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the drag thread acts on.
enum Event {
    /// From the agent.
    Message(Message),
    /// A request from the app, and where its answer goes.
    Request(ipc::Request, Sender<ipc::Response>),
}

pub struct Agent {
    /// What the guest copied last, and how many times it has.
    clipboard: Mutex<(u64, String)>,
    /// Say on the console when the guest copies something.
    announce: bool,
    /// Frames for the agent.
    outbox: Mutex<Sender<Vec<u8>>>,
    events: Mutex<Sender<Event>>,
    /// The agent's hello, once it has said one.
    info: Mutex<Option<ipc::Agent>>,
    /// What a data connection must show.
    token: [u8; 16],
    transfers: Mutex<HashMap<u32, Arc<Transfer>>>,
}

impl Agent {
    /// The agent's side of things, for a VM whose machines `targets` holds.
    pub fn start(targets: Targets, announce: bool) -> io::Result<Arc<Self>> {
        let (outbox, frames) = mpsc::channel::<Vec<u8>>();
        let feed_targets = targets.clone();
        thread::Builder::new()
            .name("agent-feed".into())
            .spawn(move || {
                for frame in frames {
                    feed(|| feed_targets.get().map(|target| target.agent), frame);
                }
            })?;
        let (events, receiver) = mpsc::channel();
        let agent = Arc::new(Self {
            clipboard: Mutex::default(),
            announce,
            outbox: Mutex::new(outbox),
            events: Mutex::new(events),
            info: Mutex::new(None),
            token: random_token(),
            transfers: Mutex::default(),
        });
        let drags = Drags {
            agent: Arc::downgrade(&agent),
            targets,
            next_id: 1,
            host: None,
            probe: None,
            pinged: None,
            generation: 0,
        };
        thread::Builder::new()
            .name("agent-drags".into())
            .spawn(move || drags.run(receiver))?;
        Ok(agent)
    }

    fn send(&self, message: &Message) {
        let _ = lock(&self.outbox).send(message.frame());
    }

    /// A line for the log: on the console's output, with the guest's, if there's a
    /// console (where tests look for it).
    fn say(&self, text: &str) {
        let line = format!("\r\nvmmc-vmx: {}\r\n", text.replace('\n', "\r\n"));
        if self.announce {
            print!("{line}");
            let _ = io::stdout().flush();
        } else {
            eprint!("{line}");
        }
    }

    fn event(&self, event: Event) {
        let _ = lock(&self.events).send(event);
    }

    /// Puts `text` on the guest's clipboard.
    pub fn put_in_guest(&self, text: &str) {
        let _ = lock(&self.outbox).send(agent_proto::clipboard::frame(text));
    }

    /// How many times the guest has copied something, and what it copied last if
    /// that's newer than `since`.
    pub fn guest_copied(&self, since: u64) -> (u64, Option<String>) {
        let guest = lock(&self.clipboard);
        (guest.0, (guest.0 > since).then(|| guest.1.clone()))
    }

    /// The agent, once it has said hello.
    pub fn info(&self) -> Option<ipc::Agent> {
        lock(&self.info).clone()
    }

    /// Where a machine's COM2 output goes.
    pub fn sink(self: &Arc<Self>) -> Box<dyn Write + Send> {
        Box::new(Sink {
            parser: Parser::default(),
            agent: self.clone(),
        })
    }

    /// The NAT service data connections reach.
    pub fn service(self: &Arc<Self>) -> LocalService {
        let agent = Arc::downgrade(self);
        LocalService {
            port: DATA_PORT,
            accept: Arc::new(move |stream| {
                if let Some(agent) = agent.upgrade() {
                    serve_connection(&agent, stream);
                }
            }),
        }
    }

    /// Answers one of the app's drag requests (the ones about transfers' chunks have
    /// their own methods, since they wait).
    pub fn call(&self, request: ipc::Request) -> ipc::Response {
        let (reply, answer) = mpsc::channel();
        self.event(Event::Request(request, reply));
        answer
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| ipc::Response::Error("the worker's drag thread is stuck".into()))
    }

    fn transfer(&self, id: u32) -> Option<Arc<Transfer>> {
        lock(&self.transfers).get(&id).cloned()
    }

    /// [`ipc::Request::SendChunks`].
    pub fn send_chunks(&self, transfer: u32, chunks: Vec<Chunk>) -> ipc::Response {
        let Some(found) = self.transfer(transfer).filter(|t| t.to_guest) else {
            return ipc::Response::Error(format!("no transfer {transfer} into the guest"));
        };
        match found.push(chunks) {
            Ok(()) => ipc::Response::Done,
            Err(e) => ipc::Response::Error(e),
        }
    }

    /// [`ipc::Request::ReceiveChunks`].
    pub fn receive_chunks(&self, transfer: u32) -> ipc::Response {
        let Some(found) = self.transfer(transfer).filter(|t| !t.to_guest) else {
            return ipc::Response::Error(format!("no transfer {transfer} from the guest"));
        };
        match found.pop(PIPE_WAIT) {
            Ok((chunks, finished)) => {
                if finished {
                    lock(&self.transfers).remove(&transfer);
                }
                ipc::Response::Chunks(chunks)
            }
            Err(e) => {
                lock(&self.transfers).remove(&transfer);
                ipc::Response::Error(e)
            }
        }
    }

    /// [`ipc::Request::CancelTransfer`].
    pub fn cancel_transfer(&self, transfer: u32) -> ipc::Response {
        if let Some(found) = lock(&self.transfers).remove(&transfer) {
            found.end("called off");
            self.send(&Message::CancelTransfer { transfer });
        }
        ipc::Response::Done
    }

    fn add_transfer(&self, id: u32, to_guest: bool) -> Arc<Transfer> {
        let transfer = Arc::new(Transfer {
            id,
            to_guest,
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        let mut transfers = lock(&self.transfers);
        // Ones that ended and were never collected go now.
        transfers.retain(|_, t| lock(&t.state).over.is_none());
        transfers.insert(id, transfer.clone());
        transfer
    }

    fn remove_transfer(&self, id: u32, reason: &str) {
        if let Some(transfer) = lock(&self.transfers).remove(&id) {
            transfer.end(reason);
        }
    }
}

/// A token nobody in the guest can guess, for data connections to show.
fn random_token() -> [u8; 16] {
    let mut token = [0u8; 16];
    for (index, half) in token.chunks_mut(8).enumerate() {
        // Each RandomState is keyed afresh from the system's random source.
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(index);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        half.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    token
}

/// Feeds a frame to the agent's port as fast as the guest reads it. With no machine,
/// or no agent reading, it's given up after a while.
fn feed(agent: impl Fn() -> Option<SerialInput>, mut pending: Vec<u8>) {
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

/// The guest's COM2 output: frames from its agent. It runs on a vCPU's thread, so it
/// only hands messages on.
struct Sink {
    parser: Parser,
    agent: Arc<Agent>,
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for frame in self.parser.feed(bytes) {
            match frame {
                Frame::Clipboard(text) => {
                    let characters = text.chars().count();
                    {
                        let mut guest = lock(&self.agent.clipboard);
                        guest.0 += 1;
                        guest.1 = text;
                    }
                    if self.agent.announce {
                        // On the console's output, with the guest's: the clipboard's
                        // length, never its text.
                        print!("\r\nvmmc-vmx: the guest copied {characters} characters\r\n");
                        let _ = io::stdout().flush();
                    }
                }
                Frame::Message(message) => self.agent.event(Event::Message(message)),
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A drag's files, on their way between the app and the guest: a bounded queue of
/// chunks.
struct Transfer {
    id: u32,
    to_guest: bool,
    state: Mutex<TransferState>,
    changed: Condvar,
}

#[derive(Default)]
struct TransferState {
    queue: VecDeque<Chunk>,
    /// What the queue holds, by [`Chunk::cost`].
    cost: usize,
    /// Why it's over before its end, if it is.
    over: Option<String>,
    /// The data connection, shut when the transfer ends early.
    stream: Option<Arc<LocalStream>>,
}

impl Transfer {
    /// Queues `chunks`, waiting a while for room.
    fn push(&self, chunks: Vec<Chunk>) -> Result<(), String> {
        let mut state = lock(&self.state);
        let deadline = Instant::now() + Duration::from_secs(30);
        while state.cost > QUEUE_LIMIT && state.over.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the other side stopped taking the files".into());
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if let Some(reason) = &state.over {
            return Err(reason.clone());
        }
        for chunk in chunks {
            state.cost += chunk.cost();
            state.queue.push_back(chunk);
        }
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    /// Takes queued chunks (up to [`BATCH_LIMIT`]), waiting up to `wait` for some.
    /// Says whether the last of them was the transfer's end. Fails once it's over
    /// early and nothing is left.
    fn pop(&self, wait: Duration) -> Result<(Vec<Chunk>, bool), String> {
        let mut state = lock(&self.state);
        let deadline = Instant::now() + wait;
        while state.queue.is_empty() && state.over.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok((Vec::new(), false));
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.queue.is_empty() {
            return Err(state.over.clone().unwrap_or_default());
        }
        let mut chunks = Vec::new();
        let mut taken = 0;
        let mut finished = false;
        while let Some(chunk) = state.queue.front() {
            if !chunks.is_empty() && taken + chunk.cost() > BATCH_LIMIT {
                break;
            }
            let chunk = state.queue.pop_front().expect("there's a front");
            taken += chunk.cost();
            state.cost -= chunk.cost();
            finished = matches!(chunk, Chunk::Finish { .. });
            chunks.push(chunk);
            if finished {
                break;
            }
        }
        drop(state);
        self.changed.notify_all();
        Ok((chunks, finished))
    }

    /// Ends the transfer early: its queue empties, its connection shuts, and both
    /// sides' next call fails with `reason`.
    fn end(&self, reason: &str) {
        let mut state = lock(&self.state);
        if state.over.is_none() {
            state.over = Some(reason.to_owned());
        }
        state.queue.clear();
        state.cost = 0;
        if let Some(stream) = state.stream.take() {
            stream.shutdown(Shutdown::Both);
        }
        drop(state);
        self.changed.notify_all();
    }
}

/// Reads the next message from a data connection.
fn next_message(
    stream: &LocalStream,
    parser: &mut Parser,
    pending: &mut VecDeque<Message>,
) -> Option<Message> {
    use std::io::Read;
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        if let Some(message) = pending.pop_front() {
            return Some(message);
        }
        let read = (&*stream).read(&mut buffer).ok().filter(|&n| n > 0)?;
        for frame in parser.feed(&buffer[..read]) {
            if let Frame::Message(message) = frame {
                pending.push_back(message);
            }
        }
    }
}

/// One data connection: the agent says which transfer it's for, then the files go one
/// way and (for a drag into the guest) the agent's receipt comes back.
fn serve_connection(agent: &Arc<Agent>, stream: LocalStream) {
    let stream = Arc::new(stream);
    stream.set_read_timeout(Some(ATTACH_TIMEOUT));
    let mut parser = Parser::default();
    let mut pending = VecDeque::new();
    let Some(Message::Attach { token, transfer }) =
        next_message(&stream, &mut parser, &mut pending)
    else {
        return;
    };
    if token != agent.token {
        tracing::warn!("a data connection from the guest had the wrong token");
        return;
    }
    let Some(found) = agent.transfer(transfer) else {
        return;
    };
    stream.set_read_timeout(None);
    {
        let mut state = lock(&found.state);
        if state.over.is_some() {
            return;
        }
        state.stream = Some(stream.clone());
    }
    if found.to_guest {
        let writer = {
            let (found, stream) = (found.clone(), stream.clone());
            thread::Builder::new()
                .name("agent-send".into())
                .spawn(move || {
                    loop {
                        let Ok((chunks, finished)) = found.pop(Duration::from_secs(60)) else {
                            return;
                        };
                        for chunk in chunks {
                            let frame = Message::Chunk {
                                transfer: found.id,
                                chunk,
                            }
                            .frame();
                            if (&*stream).write_all(&frame).is_err() {
                                found.end("the guest closed the connection");
                                return;
                            }
                        }
                        if finished {
                            return;
                        }
                    }
                })
        };
        if writer.is_err() {
            found.end("can't start a thread");
            return;
        }
        // The agent's receipt.
        while let Some(message) = next_message(&stream, &mut parser, &mut pending) {
            match message {
                Message::Received { transfer: id, .. } if id == found.id => {
                    agent.event(Event::Message(message));
                    return;
                }
                Message::Log { .. } => agent.event(Event::Message(message)),
                _ => {}
            }
        }
        found.end("the guest closed the connection");
        agent.event(Event::Message(Message::Received {
            transfer: found.id,
            error: Some("the guest closed the connection".into()),
        }));
    } else {
        while let Some(message) = next_message(&stream, &mut parser, &mut pending) {
            match message {
                Message::Chunk {
                    transfer: id,
                    chunk,
                } if id == found.id => {
                    let last = matches!(chunk, Chunk::Finish { .. });
                    if found.push(vec![chunk]).is_err() {
                        return;
                    }
                    if last {
                        return;
                    }
                }
                Message::Log { .. } => agent.event(Event::Message(message)),
                _ => {}
            }
        }
        // Whatever's queued still reaches the app; then it hears why there's no more.
        let mut state = lock(&found.state);
        if state.over.is_none() {
            state.over = Some("the guest stopped sending".into());
        }
        drop(state);
        found.changed.notify_all();
    }
}

/// A drag from the host into the guest.
struct HostDrag {
    drag: u32,
    phase: Phase,
    /// Where the host's pointer is, in tablet units.
    at: (u32, u32),
    /// When the phase began.
    since: Instant,
    /// The host dropped before the agent was ready.
    dropped_early: bool,
    /// The agent's receipt for the files: `Some(None)` once it has them all.
    received: Option<Option<String>>,
    /// The next nudge, while calling the drag off.
    nudge: Instant,
    nudged: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Arming,
    InGuest,
    /// The host dropped; the button stays down until the files are there.
    Dropping,
    /// The button is up: the guest dropped, and the agent will say how it went.
    Released,
    /// Being called off: the pointer is nudged until the agent says it's over.
    Cancelling(String),
    Over(Result<bool, String>),
}

/// A look for a drag in the guest.
struct Probe {
    probe: u32,
    at: (u32, u32),
    since: Instant,
    found: ipc::GuestDrag,
    nudge: Instant,
    nudged: u32,
}

/// The drag thread's state.
struct Drags {
    agent: Weak<Agent>,
    targets: Targets,
    next_id: u32,
    host: Option<HostDrag>,
    probe: Option<Probe>,
    /// When the worker last asked for a hello.
    pinged: Option<Instant>,
    /// The machine the agent said hello in.
    generation: u64,
}

impl Drags {
    fn run(mut self, events: Receiver<Event>) {
        loop {
            let busy = self
                .probe
                .as_ref()
                .is_some_and(|p| p.found == ipc::GuestDrag::Looking)
                || self
                    .host
                    .as_ref()
                    .is_some_and(|h| !matches!(h.phase, Phase::InGuest | Phase::Over(_)));
            let wait = if busy {
                NUDGE_INTERVAL / 2
            } else {
                Duration::from_millis(500)
            };
            match events.recv_timeout(wait) {
                Ok(Event::Message(message)) => self.message(message),
                Ok(Event::Request(request, reply)) => {
                    let response = self.request(request);
                    let _ = reply.send(response);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.agent.strong_count() == 0 {
                return;
            }
            self.tick();
        }
    }

    fn with_agent(&self, act: impl FnOnce(&Agent)) {
        if let Some(agent) = self.agent.upgrade() {
            act(&agent);
        }
    }

    fn send(&self, message: Message) {
        self.with_agent(|agent| agent.send(&message));
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Puts the guest's pointer at `at` with the left button up or down.
    fn pointer(&self, at: (u32, u32), pressed: bool) {
        if let Some(target) = self.targets.get() {
            let clamp = |v: u32| v.min(u32::from(TABLET_MAX)) as u16;
            target
                .usb
                .pointer(clamp(at.0), clamp(at.1), u8::from(pressed), 0);
        }
    }

    /// `at`, moved `NUDGE_STEP` towards the middle of the screen.
    fn inward(at: (u32, u32)) -> (u32, u32) {
        let middle = u32::from(TABLET_MAX) / 2;
        let step = |v: u32| {
            if v > middle {
                v.saturating_sub(NUDGE_STEP)
            } else {
                v + NUDGE_STEP
            }
        };
        (step(at.0), step(at.1))
    }

    fn has_feature(&self, feature: u32) -> Result<(), String> {
        let info = self.agent.upgrade().and_then(|agent| agent.info());
        match info {
            Some(info) if info.features & feature != 0 => Ok(()),
            Some(_) => Err(
                "vmmc's agent in the guest is too old for drag and drop: install vmmc Tools again"
                    .into(),
            ),
            None => Err("vmmc's agent isn't running in the guest: install vmmc Tools".into()),
        }
    }

    fn message(&mut self, message: Message) {
        match message {
            Message::Hello {
                version,
                os,
                features,
            } => {
                self.with_agent(|agent| {
                    let info = Some(ipc::Agent { os, features });
                    let mut known = lock(&agent.info);
                    if *known != info {
                        agent.say(&format!(
                            "vmmc's agent in the guest said hello ({}, protocol {version})",
                            info.as_ref().map_or("", |i| i.os.as_str())
                        ));
                    }
                    *known = info;
                    drop(known);
                    agent.send(&Message::Config {
                        address: GATEWAY,
                        port: DATA_PORT,
                        token: agent.token,
                    });
                });
                self.generation = self.targets.generation();
            }
            Message::Log { text } => self.with_agent(|agent| agent.say(&format!("agent: {text}"))),
            Message::DragArmed { drag } => {
                if let Some(host) = self.host.as_mut().filter(|h| h.drag == drag)
                    && host.phase == Phase::Arming
                {
                    host.phase = if host.dropped_early {
                        Phase::Dropping
                    } else {
                        Phase::InGuest
                    };
                    host.since = Instant::now();
                    let at = host.at;
                    self.pointer(at, true);
                    self.release_if_ready();
                }
            }
            Message::DragFinished { drag, dropped } => {
                if let Some(host) = self.host.as_mut().filter(|h| h.drag == drag) {
                    let held = matches!(
                        host.phase,
                        Phase::InGuest | Phase::Dropping | Phase::Cancelling(_)
                    );
                    host.phase = match &host.phase {
                        Phase::Cancelling(reason) => Phase::Over(Err(reason.clone())),
                        _ => Phase::Over(Ok(dropped)),
                    };
                    let at = host.at;
                    if held {
                        self.pointer(at, false);
                    }
                    if !dropped {
                        self.with_agent(|agent| agent.remove_transfer(drag, "the drag ended"));
                    }
                }
            }
            Message::Received { transfer, error } => {
                // (A drag being called off closes its connection: that's no news.)
                if let Some(host) = self.host.as_mut().filter(|h| {
                    h.drag == transfer && !matches!(h.phase, Phase::Cancelling(_) | Phase::Over(_))
                }) {
                    host.received = Some(error.clone());
                    if let Some(error) = error {
                        self.with_agent(|agent| {
                            agent.say(&format!("the guest couldn't take the files: {error}"));
                        });
                        self.cancel_host_drag(&format!(
                            "the guest couldn't take the files: {error}"
                        ));
                    } else {
                        self.release_if_ready();
                    }
                }
                self.with_agent(|agent| {
                    lock(&agent.transfers).remove(&transfer);
                });
            }
            Message::GuestDrag { probe, items } => {
                if let Some(found) = self.probe.as_mut().filter(|p| p.probe == probe)
                    && found.found == ipc::GuestDrag::Looking
                {
                    found.found = ipc::GuestDrag::Files(items);
                    // The guest's drag ends on the agent's window, which refuses it.
                    let at = found.at;
                    self.pointer(at, false);
                }
            }
            Message::NoGuestDrag { probe } => {
                if let Some(found) = self.probe.as_mut().filter(|p| p.probe == probe)
                    && found.found == ipc::GuestDrag::Looking
                {
                    found.found = ipc::GuestDrag::Nothing;
                    let at = found.at;
                    self.pointer(at, true);
                }
            }
            // Not the agent's to send.
            Message::Ping
            | Message::Config { .. }
            | Message::HostDrag { .. }
            | Message::CancelDrag { .. }
            | Message::Probe { .. }
            | Message::SendFiles { .. }
            | Message::CancelTransfer { .. }
            | Message::Attach { .. }
            | Message::Chunk { .. } => {}
        }
    }

    /// Lets the button go once the host has dropped and the guest has the files.
    fn release_if_ready(&mut self) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        if host.phase == Phase::Dropping && host.received == Some(None) {
            host.phase = Phase::Released;
            host.since = Instant::now();
            let at = host.at;
            self.pointer(at, false);
        }
    }

    /// Calls the host's drag off: the agent stops its drag in the guest at its next
    /// pointer event, which nudges give it.
    fn cancel_host_drag(&mut self, reason: &str) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        let drag = host.drag;
        let held = matches!(host.phase, Phase::InGuest | Phase::Dropping);
        match host.phase {
            Phase::Over(_) | Phase::Cancelling(_) => return,
            _ if held => {
                host.phase = Phase::Cancelling(reason.to_owned());
                host.since = Instant::now();
                host.nudge = Instant::now() + NUDGE_INTERVAL;
                host.nudged = 0;
            }
            _ => host.phase = Phase::Over(Err(reason.to_owned())),
        }
        self.send(Message::CancelDrag { drag });
        self.with_agent(|agent| agent.remove_transfer(drag, reason));
    }

    fn request(&mut self, request: ipc::Request) -> ipc::Response {
        use ipc::{Request, Response};
        match request {
            Request::DragEnter { items, x, y } => {
                if let Err(e) = self.has_feature(features::DRAG_TO_GUEST) {
                    return Response::Error(e);
                }
                self.cancel_host_drag("another drag began");
                let drag = self.id();
                self.with_agent(|agent| {
                    agent.add_transfer(drag, true);
                });
                self.pointer((x, y), false);
                self.send(Message::HostDrag {
                    drag,
                    items,
                    files: 0,
                    bytes: 0,
                });
                self.host = Some(HostDrag {
                    drag,
                    phase: Phase::Arming,
                    at: (x, y),
                    since: Instant::now(),
                    dropped_early: false,
                    received: None,
                    nudge: Instant::now(),
                    nudged: 0,
                });
                Response::Drag { drag }
            }
            Request::DragMove { drag, x, y } => {
                if let Some(host) = self.host.as_mut().filter(|h| h.drag == drag) {
                    host.at = (x, y);
                    let pressed = matches!(host.phase, Phase::InGuest | Phase::Dropping);
                    if matches!(host.phase, Phase::Arming | Phase::InGuest | Phase::Dropping) {
                        self.pointer((x, y), pressed);
                    }
                }
                Response::Done
            }
            Request::DragDrop { drag, x, y } => {
                if let Some(host) = self.host.as_mut().filter(|h| h.drag == drag) {
                    host.at = (x, y);
                    match host.phase {
                        Phase::Arming => host.dropped_early = true,
                        Phase::InGuest => {
                            host.phase = Phase::Dropping;
                            host.since = Instant::now();
                            self.pointer((x, y), true);
                            self.release_if_ready();
                        }
                        _ => {}
                    }
                }
                Response::Done
            }
            Request::DragLeave { drag } => {
                if self.host.as_ref().is_some_and(|h| h.drag == drag) {
                    self.cancel_host_drag("the drag left the guest");
                }
                Response::Done
            }
            Request::DragState { drag } => match self.host.as_ref().filter(|h| h.drag == drag) {
                Some(host) => Response::DragState(match &host.phase {
                    Phase::Arming => ipc::DragState::Arming,
                    Phase::InGuest => ipc::DragState::InGuest,
                    Phase::Dropping | Phase::Released => ipc::DragState::Dropping,
                    Phase::Cancelling(reason) | Phase::Over(Err(reason)) => {
                        ipc::DragState::Failed(reason.clone())
                    }
                    Phase::Over(Ok(dropped)) => ipc::DragState::Finished { dropped: *dropped },
                }),
                None => Response::Error(format!("no drag {drag}")),
            },
            Request::ProbeGuestDrag { x, y } => {
                if let Err(e) = self.has_feature(features::DRAG_FROM_GUEST) {
                    return Response::Error(e);
                }
                let probe = self.id();
                self.send(Message::Probe { probe });
                self.probe = Some(Probe {
                    probe,
                    at: (x, y),
                    since: Instant::now(),
                    found: ipc::GuestDrag::Looking,
                    nudge: Instant::now() + NUDGE_INTERVAL,
                    nudged: 0,
                });
                Response::Probe { probe }
            }
            Request::GuestDrag { probe } => {
                match self.probe.as_ref().filter(|p| p.probe == probe) {
                    Some(found) => Response::GuestDrag(found.found.clone()),
                    None => Response::Error(format!("no probe {probe}")),
                }
            }
            Request::ReceiveFiles { probe } => {
                let found = self.probe.as_ref().filter(|p| p.probe == probe);
                if !matches!(found.map(|p| &p.found), Some(ipc::GuestDrag::Files(_))) {
                    return Response::Error(format!("probe {probe} found no files"));
                }
                let transfer = self.id();
                self.with_agent(|agent| {
                    agent.add_transfer(transfer, false);
                });
                self.send(Message::SendFiles { probe, transfer });
                Response::Transfer { transfer }
            }
            other => Response::Error(format!("not a drag request: {other:?}")),
        }
    }

    /// Timers: hellos, the agent's time to arm, nudges, and probes that found nothing.
    fn tick(&mut self) {
        let now = Instant::now();
        // A new machine (a guest reset) has an agent that hasn't said hello yet.
        let generation = self.targets.generation();
        if generation != self.generation {
            self.with_agent(|agent| *lock(&agent.info) = None);
            self.generation = generation;
        }
        let has_agent = self.agent.upgrade().is_some_and(|a| a.info().is_some());
        if !has_agent
            && self.targets.get().is_some()
            && self.pinged.is_none_or(|at| now - at > PING_INTERVAL)
        {
            self.pinged = Some(now);
            self.send(Message::Ping);
        }

        if let Some(host) = self.host.as_mut() {
            match host.phase.clone() {
                Phase::Arming if now - host.since > ARM_TIMEOUT => {
                    self.cancel_host_drag("vmmc's agent in the guest didn't take the drag");
                }
                Phase::Cancelling(reason) => {
                    if now - host.since > CANCEL_TIMEOUT {
                        host.phase = Phase::Over(Err(reason));
                        let at = host.at;
                        self.pointer(at, false);
                    } else if now >= host.nudge {
                        host.nudge = now + NUDGE_INTERVAL;
                        host.nudged += 1;
                        let at = if host.nudged % 2 == 1 {
                            Self::inward(host.at)
                        } else {
                            host.at
                        };
                        self.pointer(at, true);
                    }
                }
                Phase::Released if now - host.since > FINISH_TIMEOUT => {
                    host.phase = Phase::Over(Ok(true));
                }
                _ => {}
            }
        }

        if let Some(probe) = self.probe.as_mut()
            && probe.found == ipc::GuestDrag::Looking
        {
            if now - probe.since > PROBE_TIMEOUT {
                probe.found = ipc::GuestDrag::Nothing;
                let at = probe.at;
                self.pointer(at, true);
            } else if now >= probe.nudge {
                probe.nudge = now + NUDGE_INTERVAL;
                probe.nudged += 1;
                let at = if probe.nudged % 2 == 1 {
                    Self::inward(probe.at)
                } else {
                    probe.at
                };
                self.pointer(at, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_hands_chunks_over_in_bounded_batches() {
        let transfer = Arc::new(Transfer {
            id: 1,
            to_guest: false,
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        let filler = {
            let transfer = transfer.clone();
            thread::spawn(move || {
                for _ in 0..64 {
                    transfer
                        .push(vec![Chunk::Data(vec![7; agent_proto::MAX_DATA])])
                        .unwrap();
                }
                transfer.push(vec![Chunk::Finish { error: None }]).unwrap();
            })
        };
        let mut bytes = 0;
        loop {
            let (chunks, finished) = transfer.pop(Duration::from_secs(5)).unwrap();
            assert!(chunks.iter().map(Chunk::cost).sum::<usize>() <= BATCH_LIMIT);
            bytes += chunks
                .iter()
                .map(|c| match c {
                    Chunk::Data(data) => data.len(),
                    _ => 0,
                })
                .sum::<usize>();
            if finished {
                break;
            }
        }
        filler.join().unwrap();
        assert_eq!(bytes, 64 * agent_proto::MAX_DATA);
    }

    #[test]
    fn an_ended_transfer_fails_both_sides() {
        let transfer = Transfer {
            id: 1,
            to_guest: true,
            state: Mutex::default(),
            changed: Condvar::new(),
        };
        transfer.push(vec![Chunk::End]).unwrap();
        transfer.end("called off");
        assert_eq!(transfer.push(vec![Chunk::End]), Err("called off".into()));
        assert_eq!(transfer.pop(Duration::ZERO), Err("called off".into()));
    }

    #[test]
    fn nudges_move_towards_the_middle() {
        assert_eq!(Drags::inward((0, 32767)), (64, 32767 - 64));
        assert_eq!(Drags::inward((20000, 100)), (20000 - 64, 164));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(random_token(), random_token());
    }
}
