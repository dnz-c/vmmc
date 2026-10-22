//! `vmmc-vmx serve`: the VM runs headless, controlled through its pipe (see the `ipc`
//! crate) by vmmc's app and command line, which may come and go while it runs.
//!
//! - **The pipe:** each client gets a thread, which answers its requests by acting on
//!   the machine running at the time (a guest reset replaces it).
//! - **The screen:** a thread copies the guest's picture and cursor into a shared
//!   section whenever they change, for clients to show.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use ipc::pipe::{PipeServer, Section, pipe_path};
use ipc::screen::CursorImage;
use ipc::{Request, Response, State, Status, VERSION, read_message, write_message};
use machine::{Capture, TABLET_MAX};
use vm_core::StopReason;

use crate::console::Targets;

/// How often the screen thread looks for a new picture.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct Shared {
    targets: Targets,
    agent: Arc<crate::agent::Agent>,
    section: Section,
    name: String,
    uuid: String,
    /// The picture's size, width << 32 | height; 0 before the first.
    picture: AtomicU64,
    sandbox: String,
}

/// Starts serving the VM `name` (`uuid` names its pipe). Fails if the pipe exists:
/// another worker has the VM.
pub fn start(
    targets: Targets,
    agent: Arc<crate::agent::Agent>,
    name: &str,
    uuid: &str,
) -> io::Result<()> {
    let section = Section::create()?;
    section.view().init();
    let shared = Arc::new(Shared {
        targets,
        agent,
        section,
        name: name.to_owned(),
        uuid: uuid.to_owned(),
        picture: AtomicU64::new(0),
        sandbox: crate::sandbox::describe(),
    });
    let path = pipe_path(uuid);
    let mut server = PipeServer::new(&path);
    // The first instance now, so a second worker for this VM fails here.
    let first = server.instance()?;
    eprintln!("vmmc-vmx: serving {name} on {path} ({})", shared.sandbox);
    let screen = shared.clone();
    thread::Builder::new()
        .name("screen".into())
        .spawn(move || publish_screen(&screen))?;
    let clients = shared.clone();
    thread::Builder::new().name("pipe".into()).spawn(move || {
        let mut pending = Some(first);
        loop {
            let pipe = match pending.take() {
                Some(instance) => instance.connect(),
                None => server.accept(),
            };
            match pipe {
                Ok(pipe) => {
                    let shared = clients.clone();
                    let _ = thread::Builder::new()
                        .name("client".into())
                        .spawn(move || serve_client(&shared, pipe));
                }
                Err(e) => {
                    tracing::warn!(%e, "the control pipe failed");
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    })?;
    Ok(())
}

fn publish_screen(shared: &Shared) {
    let view = shared.section.view();
    let mut last = Capture::default();
    let mut cursor_count = 0;
    let mut cursor_shown = None;
    loop {
        thread::sleep(FRAME_INTERVAL);
        let Some(display) = shared.targets.display() else {
            continue;
        };
        if let Some(frame) = display.capture(&mut last) {
            shared.picture.store(
                u64::from(frame.width) << 32 | u64::from(frame.height),
                Ordering::Relaxed,
            );
            view.publish_picture(frame.width, frame.height, &frame.rgb);
        }
        let (count, shown, image) = display.cursor(cursor_count);
        if image.is_some() || count != cursor_count || cursor_shown != Some(shown) {
            let image = image.as_ref().map(|cursor| CursorImage {
                width: cursor.width,
                height: cursor.height,
                hotspot: cursor.hotspot,
                rgba: &cursor.rgba,
            });
            view.publish_cursor(count, shown, image);
            cursor_count = count;
            cursor_shown = Some(shown);
        }
    }
}

fn serve_client(shared: &Shared, mut pipe: std::fs::File) {
    let mut greeted = false;
    loop {
        let request: Request = match read_message(&mut pipe) {
            Ok(request) => request,
            // The client went away.
            Err(_) => return,
        };
        let response = if greeted {
            handle(shared, request)
        } else {
            match request {
                Request::Hello { version } if version == VERSION => {
                    greeted = true;
                    Response::Hello {
                        version: VERSION,
                        name: shared.name.clone(),
                        uuid: shared.uuid.clone(),
                        pid: std::process::id(),
                    }
                }
                Request::Hello { version } => Response::Error(format!(
                    "this worker speaks version {VERSION}, not {version}"
                )),
                _ => Response::Error("say hello first".into()),
            }
        };
        if write_message(&mut pipe, &response).is_err() {
            return;
        }
    }
}

fn handle(shared: &Shared, request: Request) -> Response {
    let targets = &shared.targets;
    match request {
        Request::Hello { .. } => Response::Error("already said hello".into()),
        Request::SetGuestClipboard { text } => {
            shared.agent.put_in_guest(&text);
            Response::Done
        }
        Request::GuestClipboard { since } => {
            let (seq, text) = shared.agent.guest_copied(since);
            Response::GuestClipboard { seq, text }
        }
        // Drag and drop, through the agent.
        Request::SendChunks { transfer, chunks } => shared.agent.send_chunks(transfer, chunks),
        Request::ReceiveChunks { transfer } => shared.agent.receive_chunks(transfer),
        Request::CancelTransfer { transfer } => shared.agent.cancel_transfer(transfer),
        request @ (Request::DragEnter { .. }
        | Request::DragMove { .. }
        | Request::DragDrop { .. }
        | Request::DragLeave { .. }
        | Request::DragState { .. }
        | Request::ProbeGuestDrag { .. }
        | Request::GuestDrag { .. }
        | Request::ReceiveFiles { .. }) => shared.agent.call(request),
        // Before taking the machine's handles: a revert waits for the machine to stop,
        // and its disks must close then.
        Request::Snapshot { name, description } => {
            match targets.take_snapshot(&name, &description) {
                Ok((id, paused)) => {
                    eprintln!(
                        "vmmc-vmx: took snapshot {name:?} (paused for {} ms)",
                        paused.as_millis()
                    );
                    Response::Snapshot {
                        id,
                        paused_ms: paused.as_millis() as u64,
                    }
                }
                Err(e) => Response::Error(format!("can't take the snapshot: {e}")),
            }
        }
        Request::Revert { snapshot } => match targets.revert(&snapshot) {
            Ok(done) => {
                eprintln!("vmmc-vmx: {done}");
                Response::Done
            }
            Err(e) => Response::Error(format!("can't revert: {e}")),
        },
        Request::Suspend => match targets.suspend() {
            Ok(paused) => {
                eprintln!(
                    "vmmc-vmx: suspended (paused for {} ms to save)",
                    paused.as_millis()
                );
                Response::Done
            }
            Err(e) => Response::Error(format!("can't suspend: {e}")),
        },
        Request::Status => {
            let picture = shared.picture.load(Ordering::Relaxed);
            Response::Status(Status {
                state: if targets.get().is_some() {
                    State::Running
                } else {
                    State::Starting
                },
                sandbox: shared.sandbox.clone(),
                screen: (picture != 0).then_some(((picture >> 32) as u32, picture as u32)),
                agent: shared.agent.info(),
            })
        }
        Request::Stop => {
            targets.quit();
            Response::Done
        }
        Request::Screen => Response::Screen {
            handle: shared.section.raw_handle(),
            size: ipc::screen::SECTION_LEN as u64,
        },
        request => {
            let Some(target) = targets.get() else {
                return Response::Error("the VM is starting".into());
            };
            match request {
                Request::PowerButton => target.power.press(),
                Request::Reset => target.stop.request_stop(StopReason::Reset),
                Request::Key { code, pressed } => target.ps2.key(code, pressed),
                Request::Pointer {
                    x,
                    y,
                    buttons,
                    wheel,
                } => {
                    let clamp = |v: u32| v.min(u32::from(TABLET_MAX)) as u16;
                    target.usb.pointer(clamp(x), clamp(y), buttons, wheel);
                }
                Request::RequestSize { width, height } => {
                    let (width, height) = target.display.request_size(width, height);
                    tracing::debug!(width, height, "a client asked the guest for a size");
                }
                Request::InsertDisc { drive, path } => {
                    let Some(slot) = target.discs.get(drive as usize) else {
                        return Response::Error(format!("the VM has no DVD drive {drive}"));
                    };
                    let medium = match path {
                        Some(path) => match disk::open(std::path::Path::new(&path), true) {
                            Ok(medium) => Some(medium),
                            Err(e) => return Response::Error(format!("{path}: {e}")),
                        },
                        None => None,
                    };
                    slot.change(medium);
                    eprintln!("vmmc-vmx: a client changed the disc in DVD drive {drive}");
                }
                Request::Hello { .. }
                | Request::Status
                | Request::Stop
                | Request::Screen
                | Request::Snapshot { .. }
                | Request::Revert { .. }
                | Request::Suspend
                | Request::SetGuestClipboard { .. }
                | Request::GuestClipboard { .. }
                | Request::SendChunks { .. }
                | Request::ReceiveChunks { .. }
                | Request::CancelTransfer { .. }
                | Request::DragEnter { .. }
                | Request::DragMove { .. }
                | Request::DragDrop { .. }
                | Request::DragLeave { .. }
                | Request::DragState { .. }
                | Request::ProbeGuestDrag { .. }
                | Request::GuestDrag { .. }
                | Request::ReceiveFiles { .. } => {
                    unreachable!("handled above")
                }
            }
            Response::Done
        }
    }
}
