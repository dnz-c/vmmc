//! The desktop, through X11: XWayland under a Wayland session (GNOME's included), which
//! bridges the clipboard and drags to Wayland programs, or an X server.
//!
//! - **The clipboard:** XFixes says when the CLIPBOARD selection changes hands; its text
//!   goes to the host. The host's text makes the agent the owner, serving it.
//! - **Drags from the host:** an invisible window over the screen takes the worker's
//!   button press; the agent then is an XDND source of the staged files' `text/uri-list`
//!   (to Wayland programs through mutter's windows for that: see [`Desktop::stand_in`]).
//! - **Drags out to the host:** another invisible window over the screen, which says it
//!   takes drops (XdndAware), catches the guest's drag when the worker nudges the
//!   pointer; the agent reads its `text/uri-list` and refuses the drop.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_proto::{Item, Message, path};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ButtonPressEvent, ClientMessageEvent, ClipOrdering, ColormapAlloc,
    ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode,
    MotionNotifyEvent, PropMode, Property, PropertyNotifyEvent, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, SelectionRequestEvent, StackMode, Timestamp, VisualClass, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use crate::files::{self, Config};
use crate::serial::Line;
use crate::{Input, Waker, log};

/// The XDND version the agent speaks.
const XDND_VERSION: u32 = 5;
/// How long the catcher waits for a drag to come onto it.
const PROBE_TIMEOUT: Duration = Duration::from_millis(700);
/// How long the catcher stays up after it found a drag, for the drop.
const CATCH_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a host drag's window waits for the worker's button press.
const ARM_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after a host drag's window shows the worker hears it's ready.
const ARM_SETTLE: Duration = Duration::from_millis(150);
/// How long the agent waits for that window to say it showed before going on anyway.
const EXPOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a drop target gets to say it's done.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);
/// Data longer than this is served in pieces (INCR).
const INCR_CHUNK: usize = 256 << 10;
/// When the agent claims the clipboard a second time (see where it does).
const RECLAIM_DELAY: Duration = Duration::from_millis(300);

macro_rules! atoms {
    ($($field:ident = $name:expr),* $(,)?) => {
        struct Atoms { $($field: u32),* }
        impl Atoms {
            fn new(conn: &RustConnection) -> Result<Self, String> {
                $(let $field = conn.intern_atom(false, $name).map_err(|e| e.to_string())?;)*
                Ok(Self { $($field: $field.reply().map_err(|e| e.to_string())?.atom),* })
            }
        }
    };
}

atoms! {
    clipboard = b"CLIPBOARD",
    utf8 = b"UTF8_STRING",
    targets = b"TARGETS",
    incr = b"INCR",
    text = b"TEXT",
    string = b"STRING",
    text_plain_utf8 = b"text/plain;charset=utf-8",
    text_plain = b"text/plain",
    uri_list = b"text/uri-list",
    xdnd_aware = b"XdndAware",
    xdnd_proxy = b"XdndProxy",
    xdnd_selection = b"XdndSelection",
    xdnd_enter = b"XdndEnter",
    xdnd_position = b"XdndPosition",
    xdnd_status = b"XdndStatus",
    xdnd_leave = b"XdndLeave",
    xdnd_drop = b"XdndDrop",
    xdnd_finished = b"XdndFinished",
    xdnd_type_list = b"XdndTypeList",
    xdnd_action_copy = b"XdndActionCopy",
    wm_check = b"_NET_SUPPORTING_WM_CHECK",
    clip_property = b"VMMC_CLIPBOARD",
    drag_property = b"VMMC_DRAG",
}

/// Where a drag from the host is: a window that takes drops.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Target {
    window: Window,
    /// Where messages for it go (its proxy), and the XDND version it speaks.
    to: Window,
    version: u32,
    /// Not under the pointer: mutter's window, standing in for a Wayland program's.
    stand_in: bool,
}

/// A drag from the host.
struct HostDrag {
    drag: u32,
    staging: PathBuf,
    items: Vec<PathBuf>,
    /// The window the worker's button press lands on.
    window: Window,
    /// When the window went up, when it said it showed, and whether the worker heard
    /// it's ready.
    armed_at: Instant,
    exposed_at: Option<Instant>,
    armed: bool,
    started: bool,
    /// The XDND target under the pointer, whether it answered since it heard of the drag,
    /// and whether it takes the drop.
    target: Option<Target>,
    answered: bool,
    accepted: bool,
    /// A position sent and not yet answered, and the latest one since.
    awaiting_status: bool,
    next_position: Option<(i16, i16, Timestamp)>,
    /// Dropped, and waiting for the target to finish, since then.
    dropped_at: Option<Instant>,
    /// Called off, and waiting for the button to go up, since then.
    cancelled_at: Option<Instant>,
    /// The data connection, shut if the drag is called off.
    connection: Option<std::net::TcpStream>,
}

/// A look for a drag in the guest.
#[derive(Default)]
struct Probe {
    probe: u32,
    /// The drag's source, once it came onto the catcher.
    source: Option<Window>,
    has_uris: bool,
    asked: bool,
    answered: bool,
    paths: Vec<PathBuf>,
    hide_at: Option<Instant>,
}

/// Text being served to another client in pieces.
struct Outgoing {
    data: Arc<Vec<u8>>,
    sent: usize,
    kind: u32,
}

pub struct Desktop {
    conn: RustConnection,
    root: Window,
    size: (u16, u16),
    atoms: Atoms,
    /// The agent's hidden window, which owns selections and receives conversions.
    window: Window,
    /// A visual and colormap for invisible windows (32-bit, with alpha), if there is one.
    argb: Option<(u32, u32)>,
    line: Arc<Line>,
    config: Arc<Mutex<Option<Config>>>,
    /// The text the agent owns the clipboard with (from the host).
    ours: Option<Arc<Vec<u8>>>,
    /// The guest's clipboard, being read in pieces.
    reading: Option<Vec<u8>>,
    /// What the agent last sent the host, so it isn't sent again.
    sent: Option<Vec<u8>>,
    serving: HashMap<(Window, u32), Outgoing>,
    host_drag: Option<HostDrag>,
    probe: Probe,
    catcher: Option<Window>,
    /// Mutter's hidden XDND window, once looked for (see `stand_in`).
    bridge: Option<Option<Window>>,
    /// The last time the server reported.
    time: Timestamp,
    /// When to claim the clipboard again.
    reclaim_at: Option<Instant>,
}

impl Desktop {
    /// Connects to the session's display.
    pub fn connect(line: Arc<Line>, config: Arc<Mutex<Option<Config>>>) -> Result<Self, String> {
        find_display();
        let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let screen = conn.setup().roots[screen].clone();
        let atoms = Atoms::new(&conn)?;
        let window = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            0,
            window,
            screen.root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("no XFixes: {e}"))?;
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        )
        .map_err(|e| e.to_string())?;
        let argb = screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.class == VisualClass::TRUE_COLOR)
            .map(|visual| visual.visual_id);
        let argb = match argb {
            Some(visual) => {
                let colormap = conn.generate_id().map_err(|e| e.to_string())?;
                conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)
                    .map_err(|e| e.to_string())?;
                Some((visual, colormap))
            }
            None => None,
        };
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Self {
            root: screen.root,
            size: (screen.width_in_pixels, screen.height_in_pixels),
            conn,
            atoms,
            window,
            argb,
            line,
            config,
            ours: None,
            reading: None,
            sent: None,
            serving: HashMap::new(),
            host_drag: None,
            probe: Probe::default(),
            catcher: None,
            bridge: None,
            time: CURRENT_TIME,
            reclaim_at: None,
        })
    }

    /// Runs until the display goes away.
    pub fn run(&mut self, inputs: &Receiver<Input>, waker: &Waker) -> Result<(), String> {
        let x_fd = self.conn.stream().as_raw_fd();
        loop {
            while let Some(event) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
                if let Err(e) = self.event(event) {
                    log(&self.line, &format!("X11: {e}"));
                }
            }
            while let Ok(input) = inputs.try_recv() {
                if let Err(e) = self.input(input) {
                    log(&self.line, &format!("X11: {e}"));
                }
            }
            self.timers();
            self.conn.flush().map_err(|e| e.to_string())?;
            let mut fds = [
                libc::pollfd {
                    fd: x_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: waker.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: two valid descriptors, polled for at most 100 ms.
            unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Err("the display closed".into());
            }
            waker.drain();
        }
    }

    fn input(&mut self, input: Input) -> Result<(), String> {
        match input {
            Input::HostClipboard(text) => {
                let data = text.into_bytes();
                self.sent = Some(data.clone());
                self.ours = Some(Arc::new(data));
                // The server's time now, not that of the last event the agent saw: an
                // owner set at an older time is easily taken for a stale one.
                self.conn
                    .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)
                    .map_err(|e| e.to_string())?;
                // And again in a moment. A Wayland program that owned the clipboard can
                // hear of its new owner (mutter's offer of the agent's text) before it
                // hears it no longer owns it, and GTK ignores an offer then: so the
                // program would see an empty clipboard. The second claim brings another
                // offer, which it takes.
                self.reclaim_at = Some(Instant::now() + RECLAIM_DELAY);
            }
            Input::Message(message) => match message {
                Message::HostDrag { drag, items, .. } => self.start_host_drag(drag, &items)?,
                Message::CancelDrag { drag } => {
                    if self.host_drag.as_ref().is_some_and(|d| d.drag == drag) {
                        self.cancel_host_drag()?;
                    }
                }
                Message::Probe { probe } => self.start_probe(probe)?,
                Message::SendFiles { probe, transfer } => {
                    if probe != self.probe.probe || self.probe.paths.is_empty() {
                        log(
                            &self.line,
                            "asked for the files of a drag the agent doesn't know",
                        );
                    } else {
                        let (paths, config, line) = (
                            self.probe.paths.clone(),
                            *self.config.lock().unwrap(),
                            self.line.clone(),
                        );
                        std::thread::spawn(move || {
                            let Some(config) = config else {
                                log(&line, "no data address from the worker yet");
                                return;
                            };
                            let sent = files::connect(&config, transfer)
                                .map_err(|e| format!("can't connect to the worker for files: {e}"))
                                .and_then(|stream| files::send(stream, transfer, &paths));
                            match sent {
                                Ok((files, bytes)) => log(
                                    &line,
                                    &format!("sent {files} files ({bytes} bytes) to the host"),
                                ),
                                Err(e) => {
                                    log(&line, &format!("sending files to the host failed: {e}"))
                                }
                            }
                        });
                    }
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::XfixesSelectionNotify(e) => {
                self.time = e.timestamp;
                if e.selection == self.atoms.clipboard && e.owner != self.window && e.owner != NONE
                {
                    self.conn
                        .convert_selection(
                            self.window,
                            self.atoms.clipboard,
                            self.atoms.utf8,
                            self.atoms.clip_property,
                            e.timestamp,
                        )
                        .map_err(|e| e.to_string())?;
                }
            }
            Event::SelectionNotify(e) => {
                if e.time != CURRENT_TIME {
                    self.time = e.time;
                }
                if e.requestor == self.window && e.selection == self.atoms.clipboard {
                    self.clipboard_converted(e.property)?;
                } else if Some(e.requestor) == self.catcher
                    && e.selection == self.atoms.xdnd_selection
                {
                    self.uris_converted(e.property)?;
                }
            }
            Event::PropertyNotify(e) => {
                self.time = e.time;
                self.property_changed(&e)?;
            }
            Event::SelectionRequest(e) => self.serve(&e)?,
            Event::SelectionClear(e) => {
                if e.selection == self.atoms.clipboard {
                    self.ours = None;
                }
            }
            Event::ButtonPress(e) => self.pressed(&e)?,
            Event::Expose(e) => {
                if let Some(drag) = self.host_drag.as_mut()
                    && drag.window == e.window
                    && drag.exposed_at.is_none()
                {
                    drag.exposed_at = Some(Instant::now());
                }
            }
            Event::MotionNotify(e) => {
                self.time = e.time;
                self.moved(&e)?;
            }
            Event::ButtonRelease(e) => {
                self.time = e.time;
                if e.detail == 1 {
                    self.released(e.time)?;
                }
            }
            Event::ClientMessage(e) => self.client_message(&e)?,
            _ => {}
        }
        Ok(())
    }

    /// Reads (and deletes) a property of the agent's window: its type and contents.
    fn take_property(&self, window: Window, property: u32) -> Result<(u32, Vec<u8>), String> {
        let reply = self
            .conn
            .get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX / 4)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok((reply.type_, reply.value))
    }

    fn clipboard_converted(&mut self, property: u32) -> Result<(), String> {
        if property == NONE {
            return Ok(());
        }
        let (kind, value) = self.take_property(self.window, self.atoms.clip_property)?;
        if kind == self.atoms.incr {
            // In pieces: each comes as a new value of the property.
            self.reading = Some(Vec::new());
            return Ok(());
        }
        self.copied(value);
        Ok(())
    }

    /// The guest copied `text`: it goes to the host, unless it came from there.
    fn copied(&mut self, text: Vec<u8>) {
        if self.sent.as_ref() == Some(&text) {
            return;
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        self.line.send(&agent_proto::clipboard::frame(&text));
        self.sent = Some(text.into_bytes());
    }

    fn property_changed(&mut self, e: &PropertyNotifyEvent) -> Result<(), String> {
        if e.window == self.window
            && e.atom == self.atoms.clip_property
            && e.state == Property::NEW_VALUE
            && self.reading.is_some()
        {
            let (_, value) = self.take_property(self.window, self.atoms.clip_property)?;
            if value.is_empty() {
                let text = self.reading.take().unwrap_or_default();
                self.copied(text);
            } else if let Some(reading) = &mut self.reading {
                reading.extend_from_slice(&value);
            }
            return Ok(());
        }
        // A client took a piece of what the agent serves: the next.
        if e.state == Property::DELETE
            && let Some(outgoing) = self.serving.get_mut(&(e.window, e.atom))
        {
            let end = (outgoing.sent + INCR_CHUNK).min(outgoing.data.len());
            let piece = outgoing.data[outgoing.sent..end].to_vec();
            outgoing.sent = end;
            let kind = outgoing.kind;
            if piece.is_empty() {
                self.serving.remove(&(e.window, e.atom));
            }
            self.conn
                .change_property8(PropMode::REPLACE, e.window, e.atom, kind, &piece)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Another client asks for a selection the agent owns.
    fn serve(&mut self, e: &SelectionRequestEvent) -> Result<(), String> {
        let a = &self.atoms;
        let property = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let (data, formats): (Option<Arc<Vec<u8>>>, Vec<u32>) = if e.selection == a.clipboard {
            (
                self.ours.clone(),
                vec![a.utf8, a.text_plain_utf8, a.string, a.text, a.text_plain],
            )
        } else if e.selection == a.xdnd_selection
            && let Some(drag) = &self.host_drag
        {
            let data = if e.target == a.uri_list {
                files::uri_list(&drag.items)
            } else {
                drag.items
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (
                Some(Arc::new(data.into_bytes())),
                vec![a.uri_list, a.utf8, a.text_plain_utf8, a.text_plain],
            )
        } else {
            (None, Vec::new())
        };
        let answered = match data {
            Some(_) if e.target == a.targets => {
                let mut list = vec![a.targets];
                list.extend(&formats);
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .map_err(|e| e.to_string())?;
                property
            }
            Some(data) if formats.contains(&e.target) => {
                if data.len() > INCR_CHUNK {
                    self.conn
                        .change_window_attributes(
                            e.requestor,
                            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )
                        .map_err(|e| e.to_string())?;
                    self.conn
                        .change_property32(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            a.incr,
                            &[data.len() as u32],
                        )
                        .map_err(|e| e.to_string())?;
                    self.serving.insert(
                        (e.requestor, property),
                        Outgoing {
                            data,
                            sent: 0,
                            kind: e.target,
                        },
                    );
                } else {
                    self.conn
                        .change_property8(PropMode::REPLACE, e.requestor, property, e.target, &data)
                        .map_err(|e| e.to_string())?;
                }
                property
            }
            _ => NONE,
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: answered,
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// An invisible, override-redirect window over the whole screen.
    fn invisible_window(&self, events: EventMask) -> Result<Window, String> {
        let window = self.conn.generate_id().map_err(|e| e.to_string())?;
        let (width, height) = self.size;
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(events);
        match self.argb {
            Some((visual, colormap)) => self.conn.create_window(
                32,
                window,
                self.root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &aux.background_pixel(0).border_pixel(0).colormap(colormap),
            ),
            None => self.conn.create_window(
                0,
                window,
                self.root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &aux,
            ),
        }
        .map_err(|e| e.to_string())?;
        Ok(window)
    }

    fn show(&self, window: Window) -> Result<(), String> {
        self.conn.map_window(window).map_err(|e| e.to_string())?;
        self.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn start_host_drag(&mut self, drag: u32, items: &[Item]) -> Result<(), String> {
        if self.host_drag.is_some() {
            self.end_host_drag(false)?;
        }
        files::clean_staging(false);
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let staging = files::staging_root().join(format!("drop-{drag}-{stamp}"));
        let finished = |line: &Line, why: &str| {
            log(line, why);
            line.send(
                &Message::DragFinished {
                    drag,
                    dropped: false,
                }
                .frame(),
            );
        };
        if let Err(e) = fs::create_dir_all(&staging) {
            finished(
                &self.line,
                &format!("can't make {}: {e}", staging.display()),
            );
            return Ok(());
        }
        let mut paths = Vec::new();
        for item in items {
            if path::check_name(&item.name).is_err() {
                let _ = fs::remove_dir_all(&staging);
                finished(
                    &self.line,
                    &format!("a bad name from the host: {:?}", item.name),
                );
                return Ok(());
            }
            let path = staging.join(&item.name);
            let _ = if item.dir {
                fs::create_dir_all(&path)
            } else {
                fs::File::create(&path).map(|_| ())
            };
            paths.push(path);
        }
        let config = *self.config.lock().unwrap();
        let connection = match config.map(|config| files::connect(&config, drag)) {
            Some(Ok(stream)) => stream,
            Some(Err(e)) => {
                let _ = fs::remove_dir_all(&staging);
                finished(
                    &self.line,
                    &format!(
                        "can't connect to the worker for files: {e} (is the VM's network adapter on?)"
                    ),
                );
                return Ok(());
            }
            None => {
                let _ = fs::remove_dir_all(&staging);
                finished(&self.line, "no data address from the worker yet");
                return Ok(());
            }
        };
        let keep = connection.try_clone().ok();
        {
            let (staging, line) = (staging.clone(), self.line.clone());
            std::thread::spawn(move || match files::receive(connection, drag, &staging) {
                Ok((files, bytes)) => log(
                    &line,
                    &format!("received {files} files ({bytes} bytes) from the host"),
                ),
                Err(e) => log(&line, &format!("receiving files from the host failed: {e}")),
            });
        }
        let window = self.invisible_window(
            EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION
                | EventMask::EXPOSURE,
        )?;
        self.show(window)?;
        // Armed once the window shows (its first Expose) and the compositor has had a
        // moment to put it on the screen: a press before then lands on what's under it.
        self.host_drag = Some(HostDrag {
            drag,
            staging,
            items: paths,
            window,
            armed_at: Instant::now(),
            exposed_at: None,
            armed: false,
            started: false,
            target: None,
            answered: false,
            accepted: false,
            awaiting_status: false,
            next_position: None,
            dropped_at: None,
            cancelled_at: None,
            connection: keep,
        });
        self.conn.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Where the pointer is and what's under it, for the log.
    fn pointer_report(&self) -> String {
        match self
            .conn
            .query_pointer(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        {
            Some(reply) => format!(
                "pointer {},{}, over {:#x}, buttons {:#x}",
                reply.root_x,
                reply.root_y,
                reply.child,
                u16::from(reply.mask)
            ),
            None => "pointer unknown".into(),
        }
    }

    /// The worker's button press: the drag starts.
    fn pressed(&mut self, e: &ButtonPressEvent) -> Result<(), String> {
        self.time = e.time;
        let Some(drag) = self.host_drag.as_mut() else {
            return Ok(());
        };
        if e.event != drag.window || e.detail != 1 || drag.started {
            return Ok(());
        }
        drag.started = true;
        let window = drag.window;
        // At the server's time, not the press's: XWayland stamps input with the
        // compositor's clock, which can be behind the server's, and a claim older than
        // the selection's last change (the previous drag letting it go) is ignored.
        self.conn
            .set_selection_owner(window, self.atoms.xdnd_selection, CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.xdnd_selection)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .owner;
        if owner != window {
            log(
                &self.line,
                &format!("the drag's selection went to {owner:#x}, not the agent"),
            );
        }
        // Out of the way of the targets under it, but still holding the pointer: no input
        // shape (so a compositor's picking looks through it at once, whatever it makes of
        // its size meanwhile), and small.
        self.conn
            .shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(i32::from(e.root_x))
                    .y(i32::from(e.root_y))
                    .width(1)
                    .height(1),
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .grab_pointer(
                false,
                window,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                e.time,
            )
            .map_err(|e| e.to_string())?;
        log(&self.line, "dragging the host's files in the guest");
        self.drag_to(e.root_x, e.root_y, e.time)
    }

    fn moved(&mut self, e: &MotionNotifyEvent) -> Result<(), String> {
        if self
            .host_drag
            .as_ref()
            .is_some_and(|d| d.started && d.dropped_at.is_none() && d.cancelled_at.is_none())
        {
            self.drag_to(e.root_x, e.root_y, e.time)?;
        }
        Ok(())
    }

    /// The XDND target at `(x, y)`: the deepest window there that says it takes drops.
    fn target_at(&self, x: i16, y: i16, skip: Window) -> Result<Option<Target>, String> {
        let mut window = self.root;
        let mut found = None;
        for _ in 0..32 {
            let reply = self
                .conn
                .translate_coordinates(self.root, window, x, y)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            if reply.child == NONE || reply.child == skip {
                break;
            }
            window = reply.child;
            let aware = self
                .conn
                .get_property(false, window, self.atoms.xdnd_aware, AtomEnum::ATOM, 0, 1)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            if let Some(version) = aware.value32().and_then(|mut v| v.next()) {
                let proxy = self
                    .conn
                    .get_property(false, window, self.atoms.xdnd_proxy, AtomEnum::WINDOW, 0, 1)
                    .map_err(|e| e.to_string())?
                    .reply()
                    .map_err(|e| e.to_string())?
                    .value32()
                    .and_then(|mut v| v.next())
                    .filter(|&proxy| proxy != NONE)
                    .unwrap_or(window);
                found = Some(Target {
                    window,
                    to: proxy,
                    version: version.min(XDND_VERSION),
                    stand_in: false,
                });
            }
        }
        Ok(found)
    }

    /// Mutter's window standing in for the Wayland program under the pointer, when the
    /// agent sees nothing there that takes drops.
    ///
    /// X11 programs see nothing of Wayland ones. In a drag from an X11 program, mutter puts
    /// one of two XdndAware windows of its own over the Wayland window under the pointer
    /// and passes on what's sent there. But it shows it only when that's another window
    /// than it last picked, which it remembers from the last drag; and the agent's window,
    /// where the drag starts, it doesn't pick (it's override-redirect). So from the second
    /// drag on, over the same window as before, mutter shows nothing. It takes messages
    /// for the window it picked at either of its windows, shown or not, though.
    fn stand_in(&mut self) -> Result<Option<Target>, String> {
        if self.bridge.is_none() {
            self.bridge = Some(self.find_bridge()?);
        }
        Ok(self.bridge.flatten().map(|window| Target {
            window,
            to: window,
            version: XDND_VERSION,
            stand_in: true,
        }))
    }

    /// Mutter's hidden XDND windows: the window manager's own, XdndAware, and put away
    /// (1x1 at -1,-1).
    fn find_bridge(&self) -> Result<Option<Window>, String> {
        let wm = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.wm_check,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .value32()
            .and_then(|mut v| v.next());
        let Some(wm) = wm else {
            return Ok(None);
        };
        // A client's windows share the bits outside the mask.
        let client = wm & !self.conn.setup().resource_id_mask;
        let children = self
            .conn
            .query_tree(self.root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .children;
        for window in children {
            if window & !self.conn.setup().resource_id_mask != client || window == wm {
                continue;
            }
            let Ok(Ok(geometry)) = self.conn.get_geometry(window).map(|c| c.reply()) else {
                continue;
            };
            if (geometry.x, geometry.y, geometry.width, geometry.height) != (-1, -1, 1, 1) {
                continue;
            }
            let aware = self
                .conn
                .get_property(false, window, self.atoms.xdnd_aware, AtomEnum::ATOM, 0, 1)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            if aware.value32().and_then(|mut v| v.next()).is_some() {
                return Ok(Some(window));
            }
        }
        Ok(None)
    }

    fn xdnd(&self, to: Window, window: Window, kind: u32, data: [u32; 5]) -> Result<(), String> {
        let message = ClientMessageEvent::new(32, window, kind, data);
        self.conn
            .send_event(false, to, EventMask::NO_EVENT, message)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn enter(&self, target: Target, source: Window) -> Result<(), String> {
        let a = &self.atoms;
        self.xdnd(
            target.to,
            target.window,
            a.xdnd_enter,
            [
                source,
                target.version << 24,
                a.uri_list,
                a.text_plain_utf8,
                a.utf8,
            ],
        )
    }

    /// The host's drag is at `(x, y)` in the guest: XDND messages to what's there.
    fn drag_to(&mut self, x: i16, y: i16, time: Timestamp) -> Result<(), String> {
        let Some(drag) = self.host_drag.as_ref() else {
            return Ok(());
        };
        let source = drag.window;
        let previous = drag.target;
        let target = match self.target_at(x, y, source)? {
            Some(target) => Some(target),
            None => self.stand_in()?,
        };
        if previous != target {
            log(
                &self.line,
                &match target {
                    Some(Target {
                        window,
                        stand_in: true,
                        ..
                    }) => format!(
                        "the drag is over a Wayland window (through {window:#x}) at {x},{y}"
                    ),
                    Some(Target {
                        window,
                        to,
                        version,
                        ..
                    }) => format!(
                        "the drag is over {window:#x} (messages to {to:#x}, XDND {version}) at {x},{y}"
                    ),
                    None => format!("the drag is over nothing that takes drops at {x},{y}"),
                },
            );
            if let Some(previous) = previous {
                self.xdnd(
                    previous.to,
                    previous.window,
                    self.atoms.xdnd_leave,
                    [source, 0, 0, 0, 0],
                )?;
            }
            if let Some(target) = target {
                self.enter(target, source)?;
            }
            let drag = self.host_drag.as_mut().expect("checked above");
            drag.target = target;
            drag.answered = false;
            drag.accepted = false;
            drag.awaiting_status = false;
        }
        let drag = self.host_drag.as_mut().expect("checked above");
        let Some(target) = drag.target else {
            return Ok(());
        };
        if drag.awaiting_status {
            if !target.stand_in || drag.answered {
                drag.next_position = Some((x, y, time));
                return Ok(());
            }
            // Mutter doesn't answer when it hadn't picked a window when it heard of the
            // drag (or hadn't heard of the drag yet): it hears of it again.
            self.enter(target, source)?;
        }
        let drag = self.host_drag.as_mut().expect("checked above");
        drag.awaiting_status = true;
        let position = (u32::from(x as u16) << 16) | u32::from(y as u16);
        self.xdnd(
            target.to,
            target.window,
            self.atoms.xdnd_position,
            [source, 0, position, time, self.atoms.xdnd_action_copy],
        )
    }

    /// The worker let the button go: the drop, or the end of a drag called off.
    fn released(&mut self, time: Timestamp) -> Result<(), String> {
        let Some(drag) = self.host_drag.as_mut() else {
            return Ok(());
        };
        if !drag.started || drag.dropped_at.is_some() {
            return Ok(());
        }
        self.conn.ungrab_pointer(time).map_err(|e| e.to_string())?;
        if drag.cancelled_at.is_some() {
            let drag = self.host_drag.take().expect("checked above");
            self.tidy(&drag, false);
            return Ok(());
        }
        match drag.target {
            Some(target) if drag.accepted => {
                drag.dropped_at = Some(Instant::now());
                let source = drag.window;
                self.xdnd(
                    target.to,
                    target.window,
                    self.atoms.xdnd_drop,
                    [source, 0, time, 0, 0],
                )
            }
            _ => {
                log(&self.line, "the drag ended where nothing takes files");
                self.end_host_drag(false)
            }
        }
    }

    /// Calls the host's drag off. One under way ends as a drag let go over nothing does:
    /// the target hears it's left at once, and so does the worker, which then lets the
    /// button go; the agent tidies up on that release (XWayland's bridge ends the Wayland
    /// drag it runs for the agent's then, and might not otherwise).
    fn cancel_host_drag(&mut self) -> Result<(), String> {
        let Some(drag) = self.host_drag.as_mut() else {
            return Ok(());
        };
        if !drag.started || drag.dropped_at.is_some() {
            return self.end_host_drag(false);
        }
        if drag.cancelled_at.is_some() {
            return Ok(());
        }
        drag.cancelled_at = Some(Instant::now());
        let (id, source, target) = (drag.drag, drag.window, drag.target.take());
        if let Some(target) = target {
            self.xdnd(
                target.to,
                target.window,
                self.atoms.xdnd_leave,
                [source, 0, 0, 0, 0],
            )?;
        }
        log(&self.line, "the drag ended: called off");
        self.line.send(
            &Message::DragFinished {
                drag: id,
                dropped: false,
            }
            .frame(),
        );
        Ok(())
    }

    /// What's left of a drag that's over: its window, its selection, and (unless it
    /// dropped) its staging folder and connection.
    fn tidy(&mut self, drag: &HostDrag, dropped: bool) {
        if drag.started {
            // The drag is over for XWayland's bridge too: without its selection's owner,
            // mutter ends the Wayland drag it runs for the agent's.
            let _ = self
                .conn
                .set_selection_owner(NONE, self.atoms.xdnd_selection, CURRENT_TIME);
        }
        let _ = self.conn.destroy_window(drag.window);
        if !dropped {
            if let Some(connection) = &drag.connection {
                let _ = connection.shutdown(std::net::Shutdown::Both);
            }
            let _ = fs::remove_dir_all(&drag.staging);
        }
    }

    fn end_host_drag(&mut self, dropped: bool) -> Result<(), String> {
        let Some(drag) = self.host_drag.take() else {
            return Ok(());
        };
        if drag.started && drag.dropped_at.is_none() {
            if let Some(target) = drag.target {
                self.xdnd(
                    target.to,
                    target.window,
                    self.atoms.xdnd_leave,
                    [drag.window, 0, 0, 0, 0],
                )?;
            }
            let _ = self.conn.ungrab_pointer(self.time);
        }
        self.tidy(&drag, dropped);
        if drag.cancelled_at.is_some() {
            // The worker heard already.
            return Ok(());
        }
        log(
            &self.line,
            &format!(
                "the drag ended: {}",
                if dropped { "dropped" } else { "called off" }
            ),
        );
        self.line.send(
            &Message::DragFinished {
                drag: drag.drag,
                dropped,
            }
            .frame(),
        );
        Ok(())
    }

    fn start_probe(&mut self, probe: u32) -> Result<(), String> {
        let catcher = match self.catcher {
            Some(catcher) => catcher,
            None => {
                let catcher = self.invisible_window(EventMask::BUTTON_PRESS)?;
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        catcher,
                        self.atoms.xdnd_aware,
                        AtomEnum::ATOM,
                        &[XDND_VERSION],
                    )
                    .map_err(|e| e.to_string())?;
                self.catcher = Some(catcher);
                catcher
            }
        };
        self.probe = Probe {
            probe,
            hide_at: Some(Instant::now() + PROBE_TIMEOUT),
            ..Probe::default()
        };
        self.show(catcher)
    }

    fn hide_catcher(&mut self) {
        if let Some(catcher) = self.catcher {
            let _ = self.conn.unmap_window(catcher);
        }
        self.probe.hide_at = None;
    }

    fn no_guest_drag(&mut self) {
        if !self.probe.answered {
            self.probe.answered = true;
            self.line.send(
                &Message::NoGuestDrag {
                    probe: self.probe.probe,
                }
                .frame(),
            );
        }
    }

    fn uris_converted(&mut self, property: u32) -> Result<(), String> {
        let catcher = self.catcher.unwrap_or(NONE);
        let paths = if property == NONE {
            Vec::new()
        } else {
            let (_, value) = self.take_property(catcher, self.atoms.drag_property)?;
            files::uri_paths(&String::from_utf8_lossy(&value))
        };
        if self.probe.answered {
            return Ok(());
        }
        if paths.is_empty() {
            log(&self.line, "the guest's drag carries no files");
            self.no_guest_drag();
            self.hide_catcher();
            return Ok(());
        }
        let items: Vec<Item> = paths
            .iter()
            .map(|path| Item {
                name: path
                    .file_name()
                    .map(|name| path::fix_name(&name.to_string_lossy()))
                    .unwrap_or_default(),
                dir: path.is_dir(),
            })
            .collect();
        log(
            &self.line,
            &format!("the guest's drag carries {} items", items.len()),
        );
        self.probe.answered = true;
        self.probe.paths = paths;
        self.probe.hide_at = Some(Instant::now() + CATCH_TIMEOUT);
        self.line.send(
            &Message::GuestDrag {
                probe: self.probe.probe,
                items,
            }
            .frame(),
        );
        Ok(())
    }

    fn client_message(&mut self, e: &ClientMessageEvent) -> Result<(), String> {
        let data = e.data.as_data32();
        let a = &self.atoms;
        if Some(e.window) == self.catcher {
            let catcher = e.window;
            if e.type_ == a.xdnd_enter {
                self.probe.source = Some(data[0]);
                let mut types: Vec<u32> = data[2..].to_vec();
                if data[1] & 1 != 0 {
                    let list = self
                        .conn
                        .get_property(false, data[0], a.xdnd_type_list, AtomEnum::ATOM, 0, 64)
                        .map_err(|e| e.to_string())?
                        .reply()
                        .map_err(|e| e.to_string())?;
                    types = list.value32().map(Iterator::collect).unwrap_or_default();
                }
                self.probe.has_uris = types.contains(&a.uri_list);
                if !self.probe.has_uris {
                    log(&self.line, "the guest's drag carries no files");
                    self.no_guest_drag();
                }
            } else if e.type_ == a.xdnd_position {
                let accept = self.probe.has_uris;
                let status = [
                    catcher,
                    u32::from(accept) | 2,
                    0,
                    0,
                    if accept { a.xdnd_action_copy } else { NONE },
                ];
                let (status_kind, uri_list, drag_property, selection) =
                    (a.xdnd_status, a.uri_list, a.drag_property, a.xdnd_selection);
                self.xdnd(data[0], data[0], status_kind, status)?;
                if accept && !self.probe.asked {
                    self.probe.asked = true;
                    self.conn
                        .convert_selection(catcher, selection, uri_list, drag_property, data[3])
                        .map_err(|e| e.to_string())?;
                }
            } else if e.type_ == a.xdnd_drop {
                // Refused: the files go to the host, and stay where they are here.
                let finished = a.xdnd_finished;
                self.xdnd(data[0], data[0], finished, [catcher, 0, NONE, 0, 0])?;
                self.hide_catcher();
            } else if e.type_ == a.xdnd_leave {
                self.probe.source = None;
            }
            return Ok(());
        }
        let Some(drag) = self.host_drag.as_mut() else {
            return Ok(());
        };
        if e.window != drag.window {
            return Ok(());
        }
        if e.type_ == a.xdnd_status {
            let accepted = data[1] & 1 != 0;
            if accepted != drag.accepted {
                log(
                    &self.line,
                    &format!(
                        "the target {} the drop",
                        if accepted { "takes" } else { "refuses" }
                    ),
                );
            }
            let drag = self.host_drag.as_mut().expect("checked above");
            drag.answered = true;
            drag.accepted = accepted;
            drag.awaiting_status = false;
            if let Some((x, y, time)) = drag.next_position.take() {
                self.drag_to(x, y, time)?;
            }
        } else if e.type_ == a.xdnd_finished {
            let version = drag.target.map_or(XDND_VERSION, |t| t.version);
            let dropped = version < 5 || data[1] & 1 != 0;
            self.end_host_drag(dropped)?;
        }
        Ok(())
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if self.reclaim_at.is_some_and(|at| now >= at) {
            self.reclaim_at = None;
            if self.ours.is_some() {
                let _ =
                    self.conn
                        .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME);
            }
        }
        if let Some(at) = self.probe.hide_at
            && now >= at
        {
            self.no_guest_drag();
            self.hide_catcher();
        }
        // A host drag's window is up: armed (or anyway, if it never says it showed).
        if let Some(drag) = self.host_drag.as_mut()
            && !drag.armed
            && (drag.exposed_at.is_some_and(|at| now - at > ARM_SETTLE)
                || now - drag.armed_at > EXPOSE_TIMEOUT)
        {
            drag.armed = true;
            let id = drag.drag;
            self.line.send(&Message::DragArmed { drag: id }.frame());
        }
        if self
            .host_drag
            .as_ref()
            .is_some_and(|drag| !drag.started && now - drag.armed_at > ARM_TIMEOUT)
        {
            log(
                &self.line,
                &format!("no press came: {}", self.pointer_report()),
            );
        }
        let expired = self.host_drag.as_ref().is_some_and(|drag| {
            (!drag.started && now - drag.armed_at > ARM_TIMEOUT)
                || drag.dropped_at.is_some_and(|at| now - at > FINISH_TIMEOUT)
                || drag
                    .cancelled_at
                    .is_some_and(|at| now - at > FINISH_TIMEOUT)
        });
        if expired {
            let (dropped, started) = self
                .host_drag
                .as_ref()
                .map_or((false, false), |d| (d.dropped_at.is_some(), d.started));
            if !started {
                log(&self.line, "the worker never pressed the button");
            }
            let _ = self.end_host_drag(dropped);
        }
    }
}

/// Without `DISPLAY` (started from outside the session), the session's XWayland: the
/// lowest X socket, and GNOME's authority file for it.
fn find_display() {
    if std::env::var_os("DISPLAY").is_none() {
        let mut displays: Vec<u32> = fs::read_dir("/tmp/.X11-unix")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.strip_prefix('X')?.parse().ok())
            .collect();
        displays.sort();
        if let Some(display) = displays.first() {
            // SAFETY: before any other thread reads the environment.
            unsafe { std::env::set_var("DISPLAY", format!(":{display}")) };
        }
    }
    if std::env::var_os("XAUTHORITY").is_none() {
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        let run = PathBuf::from(format!("/run/user/{uid}"));
        let newest = fs::read_dir(&run)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mutter-Xwaylandauth.")
            })
            .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
        if let Some(file) = newest {
            // SAFETY: as above.
            unsafe { std::env::set_var("XAUTHORITY", file.path()) };
        }
    }
}
