//! Drag and drop between the host and the console's guest, through vmmc's agent in the
//! guest (see `ipc`'s drag requests, and the `agent_proto` crate for how a drag goes).
//!
//! - **Into the guest:** the window takes drops with a drop target of its own (in place
//!   of winit's). A drag of files over the console starts a drag in the guest, which
//!   follows the host's pointer, and the files go to the guest at once, on a thread of
//!   their own; the guest drops where the host does. Leaving the console calls the
//!   guest's drag off.
//! - **Out of the guest:** while the guest's left button is held, the host's pointer
//!   leaving the console asks the worker whether a drag is going on in the guest. If one
//!   is, the host starts a drag of its own of the guest's items, staged in
//!   `%TEMP%\vmmc\drag-N`, and fetches them meanwhile: a drop waits for them.

use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use ipc::{GuestDrag, Item, Request, Response};
use slint::ComponentHandle;
use vmmc_client::transfer::{self, Progress};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, POINT, POINTL,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, HDROP, ILCreateFromPathW, ILFindLastID, ILFree, SHCreateDataObject,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
use windows::core::{BOOL, HRESULT, HSTRING, Ref, implement};

use crate::{AppWindow, Shared, console};

/// How long a finished transfer's line stays in the status bar.
const SHOW_DONE: Duration = Duration::from_secs(4);

/// A transfer's progress, and how it ended once it has, shared with its thread.
type Tracked = Arc<Mutex<(Progress, Option<Result<(), String>>)>>;

/// A transfer the status bar reports on.
pub struct Shown {
    /// "to the guest" or "from the guest".
    way: &'static str,
    progress: Tracked,
    finished: Option<Instant>,
}

/// A drag from the host, over the window.
pub struct Incoming {
    paths: Vec<PathBuf>,
    /// The worker's drag, while over the console.
    drag: Option<u32>,
    /// Its files' thread stops when this is set.
    cancelled: Arc<AtomicBool>,
    /// Why the guest won't take it, if it won't.
    refused: bool,
    at: (u32, u32),
}

/// Where a drag out of the guest is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Outgoing {
    /// The button is up, or no drag has been looked for.
    #[default]
    Idle,
    /// The pointer left the console with the button held: the worker is looking.
    Probing(u32),
    /// The host's drag runs (or is about to).
    Dragging,
    /// No drag there, or the guest can't say: nothing more until the button goes up.
    Declined,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The window's handle.
fn hwnd(window: &AppWindow) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window().window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as _)),
        _ => None,
    }
}

/// Puts the window's drop target in place of winit's, once the window exists.
pub fn register(app: &Shared, window: &AppWindow) -> bool {
    let Some(hwnd) = hwnd(window) else {
        return false;
    };
    // SAFETY: on the window's thread, which winit has initialized OLE on already (that
    // makes this a no-op), or not (then it's this).
    let _ = unsafe { OleInitialize(None) };
    let target: IDropTarget = Target {
        app: Rc::downgrade(app),
        window: window.as_weak(),
    }
    .into();
    // SAFETY: the window is this thread's; the target lives as long as OLE holds it.
    unsafe {
        let _ = RevokeDragDrop(hwnd);
        if let Err(e) = RegisterDragDrop(hwnd, &target) {
            tracing::warn!(%e, "the window can't take drops");
        }
    }
    true
}

/// The files a data object carries.
fn dropped_files(object: &IDataObject) -> Vec<PathBuf> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: a standard query; the medium is released below.
    let Ok(mut medium) = (unsafe { object.GetData(&format) }) else {
        return Vec::new();
    };
    // SAFETY: CF_HDROP data is an HDROP in an HGLOBAL.
    let drop = HDROP(unsafe { medium.u.hGlobal.0 });
    let mut paths = Vec::new();
    // SAFETY: the HDROP is valid until the medium is released.
    unsafe {
        let count = DragQueryFileW(drop, u32::MAX, None);
        for index in 0..count {
            let len = DragQueryFileW(drop, index, None) as usize;
            let mut buffer = vec![0u16; len + 1];
            DragQueryFileW(drop, index, Some(&mut buffer));
            paths.push(PathBuf::from(String::from_utf16_lossy(&buffer[..len])));
        }
        ReleaseStgMedium(&mut medium);
    }
    paths
}

#[implement(IDropTarget)]
struct Target {
    app: Weak<std::cell::RefCell<crate::App>>,
    window: slint::Weak<AppWindow>,
}

impl Target {
    /// The drag moved to `pt` (screen pixels): it's over the console or it isn't.
    fn moved(&self, pt: &POINTL, effect: *mut DROPEFFECT) {
        let result = self.update(pt);
        // SAFETY: OLE passes a valid pointer.
        unsafe { *effect = result };
    }

    fn update(&self, pt: &POINTL) -> DROPEFFECT {
        let (Some(app), Some(window)) = (self.app.upgrade(), self.window.upgrade()) else {
            return DROPEFFECT_NONE;
        };
        let mut state = app.borrow_mut();
        // The app's own drag out of the guest, passing over its window.
        if state.outgoing == Outgoing::Dragging {
            return DROPEFFECT_NONE;
        }
        let at = console_point(&state, &window, pt);
        let state = &mut *state;
        let (Some(incoming), Some(session)) = (state.incoming.as_mut(), state.console.as_ref())
        else {
            return DROPEFFECT_NONE;
        };
        match (at, incoming.drag) {
            (Some(at), Some(drag)) => {
                if at != incoming.at {
                    incoming.at = at;
                    session.send(Request::DragMove {
                        drag,
                        x: at.0,
                        y: at.1,
                    });
                }
                DROPEFFECT_COPY
            }
            (Some(at), None) if !incoming.refused => {
                let items = transfer::items(&incoming.paths);
                match session.call(Request::DragEnter {
                    items,
                    x: at.0,
                    y: at.1,
                }) {
                    Ok(Response::Drag { drag }) => {
                        incoming.drag = Some(drag);
                        incoming.at = at;
                        incoming.cancelled = Arc::new(AtomicBool::new(false));
                        let shown =
                            send_files(&session.vm, drag, &incoming.paths, &incoming.cancelled);
                        state.transfers.push(shown);
                        DROPEFFECT_COPY
                    }
                    Ok(_) => DROPEFFECT_NONE,
                    Err(e) => {
                        incoming.refused = true;
                        window.set_status_text(format!("Can't drop into the guest: {e}").into());
                        DROPEFFECT_NONE
                    }
                }
            }
            (None, Some(drag)) => {
                // Out of the console: the guest's drag is called off, and a return
                // starts another.
                session.send(Request::DragLeave { drag });
                incoming.cancelled.store(true, Ordering::Relaxed);
                incoming.drag = None;
                DROPEFFECT_NONE
            }
            _ => DROPEFFECT_NONE,
        }
    }
}

/// Where the screen point `pt` is on the console's guest, in tablet units, if it's on
/// the console.
fn console_point(state: &crate::App, window: &AppWindow, pt: &POINTL) -> Option<(u32, u32)> {
    let session = state.console.as_ref()?;
    let (origin, size) = state.console_area?;
    let hwnd = hwnd(window)?;
    let mut point = POINT { x: pt.x, y: pt.y };
    // SAFETY: plain conversion for this thread's window.
    if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() {
        return None;
    }
    let scale = window.window().scale_factor();
    let (x, y) = (
        point.x as f32 / scale - origin.0,
        point.y as f32 / scale - origin.1,
    );
    if x < 0.0 || y < 0.0 || x >= size.0 || y >= size.1 {
        return None;
    }
    Some(console::tablet_position(x, y, size, session.frame))
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        object: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = object.as_ref().map(dropped_files).unwrap_or_default();
        if let Some(app) = self.app.upgrade() {
            let mut state = app.borrow_mut();
            state.incoming = (!paths.is_empty()).then(|| Incoming {
                paths,
                drag: None,
                cancelled: Arc::new(AtomicBool::new(false)),
                refused: false,
                at: (u32::MAX, u32::MAX),
            });
        }
        self.moved(pt, effect);
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.moved(pt, effect);
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Some(app) = self.app.upgrade() {
            let mut state = app.borrow_mut();
            if let Some(incoming) = state.incoming.take()
                && let Some(drag) = incoming.drag
            {
                incoming.cancelled.store(true, Ordering::Relaxed);
                if let Some(session) = state.console.as_ref() {
                    session.send(Request::DragLeave { drag });
                }
            }
        }
        Ok(())
    }

    fn Drop(
        &self,
        _object: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let result = self.update(pt);
        if let Some(app) = self.app.upgrade() {
            let mut state = app.borrow_mut();
            if let Some(incoming) = state.incoming.take()
                && let Some(drag) = incoming.drag
                && let Some(session) = state.console.as_ref()
            {
                // The files keep going; the guest drops once it has them.
                session.send(Request::DragDrop {
                    drag,
                    x: incoming.at.0,
                    y: incoming.at.1,
                });
            }
        }
        // SAFETY: OLE passes a valid pointer.
        unsafe { *effect = result };
        Ok(())
    }
}

/// Sends a drag's files on a connection and thread of their own.
fn send_files(
    vm: &vmconfig::Vm,
    drag: u32,
    paths: &[PathBuf],
    cancelled: &Arc<AtomicBool>,
) -> Shown {
    let progress: Tracked = Arc::default();
    let (vm, paths, cancelled, shared) = (
        vm.clone(),
        paths.to_vec(),
        cancelled.clone(),
        progress.clone(),
    );
    std::thread::spawn(move || {
        let result =
            vmmc_client::connect(&vm, Duration::from_secs(2)).and_then(|mut connection| {
                transfer::send(&mut connection, drag, &paths, &cancelled, |done| {
                    lock(&shared).0 = done.clone();
                })
            });
        lock(&shared).1 = Some(result);
    });
    Shown {
        way: "to the guest",
        progress,
        finished: None,
    }
}

/// The pointer moved on the console with the left button held: past its edge, that
/// may be a drag out of the guest. Returns whether the pointer stays with the host (not
/// the guest) for now.
pub fn pointer_left(
    app: &Shared,
    window: &AppWindow,
    outside: bool,
    left: bool,
    at: (u32, u32),
) -> bool {
    let mut state = app.borrow_mut();
    if !left {
        if state.outgoing != Outgoing::Dragging {
            state.outgoing = Outgoing::Idle;
        }
        return false;
    }
    match state.outgoing.clone() {
        Outgoing::Idle if outside => {
            let Some(session) = state.console.as_ref() else {
                return false;
            };
            match session.call(Request::ProbeGuestDrag { x: at.0, y: at.1 }) {
                Ok(Response::Probe { probe }) => {
                    state.outgoing = Outgoing::Probing(probe);
                    true
                }
                _ => {
                    state.outgoing = Outgoing::Declined;
                    false
                }
            }
        }
        Outgoing::Probing(_) => {
            drop(state);
            poll_probe(app, window);
            !matches!(app.borrow().outgoing, Outgoing::Declined)
        }
        Outgoing::Dragging => true,
        Outgoing::Idle | Outgoing::Declined => false,
    }
}

/// While a probe looks: what it found. Files start the host's drag.
pub fn poll_probe(app: &Shared, window: &AppWindow) {
    let mut state = app.borrow_mut();
    let Outgoing::Probing(probe) = state.outgoing else {
        return;
    };
    let Some(session) = state.console.as_ref() else {
        state.outgoing = Outgoing::Declined;
        return;
    };
    match session.call(Request::GuestDrag { probe }) {
        Ok(Response::GuestDrag(GuestDrag::Looking)) => {}
        Ok(Response::GuestDrag(GuestDrag::Files(items))) => {
            let vm = session.vm.clone();
            state.outgoing = Outgoing::Dragging;
            drop(state);
            // Not from inside a pointer event: the drag runs a loop of its own.
            let (app, weak) = (app.clone(), window.as_weak());
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(window) = weak.upgrade() {
                    drag_out(&app, &window, &vm, probe, &items);
                }
            });
        }
        _ => state.outgoing = Outgoing::Declined,
    }
}

/// Where drags out of the guest are staged.
fn staging_root() -> PathBuf {
    std::env::temp_dir().join("vmmc")
}

/// Removes staged drags older than an hour: a program may still be copying from a
/// recent one.
pub fn clean_staging() {
    let Ok(entries) = std::fs::read_dir(staging_root()) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old && entry.file_name().to_string_lossy().starts_with("drag-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The host's drag of a guest drag's items: they're staged (empty at first) and fetched
/// while the drag runs, and a drop waits until they're all there.
fn drag_out(app: &Shared, window: &AppWindow, vm: &vmconfig::Vm, probe: u32, items: &[Item]) {
    clean_staging();
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let staging = staging_root().join(format!("drag-{probe}-{stamp}"));
    let finish = |app: &Shared| {
        let mut state = app.borrow_mut();
        state.outgoing = Outgoing::Idle;
        // The host's drag swallowed the button's release.
        drop(state);
        window.set_left(false);
    };
    if std::fs::create_dir_all(&staging).is_err() {
        finish(app);
        return;
    }
    let mut paths = Vec::new();
    for item in items {
        // The names come from the guest.
        if agent_proto::path::check_name(&item.name).is_err() {
            continue;
        }
        let path = staging.join(&item.name);
        let made = if item.dir {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::File::create(&path).map(|_| ())
        };
        if made.is_ok() {
            paths.push(path);
        }
    }
    let progress: Tracked = Arc::default();
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let (vm, staging, cancelled, shared) = (
            vm.clone(),
            staging.clone(),
            cancelled.clone(),
            progress.clone(),
        );
        std::thread::spawn(move || {
            let result =
                vmmc_client::connect(&vm, Duration::from_secs(2)).and_then(|mut connection| {
                    let transfer = match connection.call(&Request::ReceiveFiles { probe })? {
                        Response::Transfer { transfer } => transfer,
                        other => return Err(format!("unexpected answer {other:?}")),
                    };
                    transfer::receive(&mut connection, transfer, &staging, &cancelled, |done| {
                        lock(&shared).0 = done.clone();
                    })
                });
            lock(&shared).1 = Some(result);
        });
    }
    app.borrow_mut().transfers.push(Shown {
        way: "from the guest",
        progress: progress.clone(),
        finished: None,
    });
    let dropped = match data_object(&staging, &paths) {
        Some(object) => {
            let source: IDropSource = Source {
                progress: progress.clone(),
            }
            .into();
            let mut effect = DROPEFFECT_NONE;
            // A timer, so the drag's loop keeps asking the source after the button goes
            // up, while the files are still coming.
            // SAFETY: a thread timer, killed below.
            let timer = unsafe { SetTimer(None, 0, 50, None) };
            // SAFETY: OLE is initialized on this thread, and the button is down.
            let result = unsafe {
                DoDragDrop(
                    &object,
                    &source,
                    DROPEFFECT_COPY | DROPEFFECT_MOVE,
                    &mut effect,
                )
            };
            // SAFETY: the timer made above.
            let _ = unsafe { KillTimer(None, timer) };
            result == DRAGDROP_S_DROP
        }
        None => false,
    };
    if !dropped {
        cancelled.store(true, Ordering::Relaxed);
        let staging = staging.clone();
        // Once the thread has let go of the files.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let _ = std::fs::remove_dir_all(staging);
        });
    }
    finish(app);
}

/// A shell data object for staged items, as Explorer makes for its own drags.
fn data_object(folder: &Path, paths: &[PathBuf]) -> Option<IDataObject> {
    if paths.is_empty() {
        return None;
    }
    // SAFETY: the item ID lists are made, used and freed here.
    unsafe {
        let root = ILCreateFromPathW(&HSTRING::from(folder.as_os_str()));
        if root.is_null() {
            return None;
        }
        let full: Vec<*mut ITEMIDLIST> = paths
            .iter()
            .map(|path| ILCreateFromPathW(&HSTRING::from(path.as_os_str())))
            .filter(|pidl| !pidl.is_null())
            .collect();
        let children: Vec<*const ITEMIDLIST> = full
            .iter()
            .map(|&pidl| ILFindLastID(pidl).cast_const())
            .collect();
        let object = SHCreateDataObject::<_, IDataObject>(Some(root), Some(&children), None).ok();
        for pidl in full {
            ILFree(Some(pidl));
        }
        ILFree(Some(root));
        object
    }
}

#[implement(IDropSource)]
struct Source {
    progress: Tracked,
}

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.contains(MK_LBUTTON) {
            return HRESULT(0);
        }
        // Dropped: once the files are all there.
        match &lock(&self.progress).1 {
            Some(Ok(())) => DRAGDROP_S_DROP,
            Some(Err(_)) => DRAGDROP_S_CANCEL,
            None => HRESULT(0),
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// The status bar's line about transfers, if any are going or just finished.
pub fn status(transfers: &mut Vec<Shown>) -> Option<String> {
    transfers.retain(|shown| shown.finished.is_none_or(|at| at.elapsed() < SHOW_DONE));
    let mut lines = Vec::new();
    for shown in transfers.iter_mut() {
        let (progress, result) = lock(&shown.progress).clone();
        let mib = |bytes: u64| bytes as f64 / (1 << 20) as f64;
        let line = match result {
            None => match progress.total {
                Some((_, total)) if total > 0 => format!(
                    "Copying {}: {:.1} of {:.1} MB",
                    shown.way,
                    mib(progress.bytes),
                    mib(total)
                ),
                _ => format!("Copying {}…", shown.way),
            },
            Some(Ok(())) => {
                shown.finished.get_or_insert_with(Instant::now);
                format!(
                    "Copied {} file{} {}",
                    progress.files,
                    if progress.files == 1 { "" } else { "s" },
                    shown.way
                )
            }
            Some(Err(e)) => {
                shown.finished.get_or_insert_with(Instant::now);
                format!("Couldn't copy {}: {e}", shown.way)
            }
        };
        lines.push(line);
    }
    (!lines.is_empty()).then(|| lines.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_items_come_back_out_of_their_data_object() {
        // SAFETY: this test's thread, initialized once.
        let _ = unsafe { OleInitialize(None) };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file.txt");
        let folder = dir.path().join("Ordner");
        std::fs::write(&file, "x").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let object = data_object(dir.path(), &[file.clone(), folder.clone()]).unwrap();
        let mut got = dropped_files(&object);
        got.sort();
        let mut expected = vec![file, folder];
        expected.sort();
        assert_eq!(got, expected);
        assert!(data_object(dir.path(), &[]).is_none());
    }

    #[test]
    fn the_status_line_tells_how_transfers_go() {
        let going: Tracked = Arc::default();
        lock(&going).0 = Progress {
            files: 1,
            bytes: 1 << 20,
            total: Some((2, 4 << 20)),
        };
        let done: Tracked = Arc::default();
        lock(&done).0.files = 3;
        lock(&done).1 = Some(Ok(()));
        let mut transfers = vec![
            Shown {
                way: "to the guest",
                progress: going,
                finished: None,
            },
            Shown {
                way: "from the guest",
                progress: done,
                finished: None,
            },
        ];
        assert_eq!(
            status(&mut transfers).unwrap(),
            "Copying to the guest: 1.0 of 4.0 MB · Copied 3 files from the guest"
        );
    }
}
