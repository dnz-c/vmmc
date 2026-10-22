//! vmmc: the desktop app. The library of VMs, each opened in a tab that shows its
//! summary or, while it runs, its screen. VMs run in their own sandboxed workers
//! (`vmmc-vmx serve`), which keep running when the app closes; the app talks to them
//! through their pipes and shows their shared screens.

// No console window for the app.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod clipboard;
mod console;
mod dialogs;
#[cfg(windows)]
mod dnd;
#[cfg(test)]
mod snapshots;

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, folder: &str) -> VmRow {
        VmRow {
            name: name.into(),
            folder: folder.into(),
            ..VmRow::default()
        }
    }

    #[test]
    fn rows_group_into_folders_with_headings() {
        let mut rows = vec![
            named("zeta", ""),
            named("b", "Work"),
            named("alpha", ""),
            named("a", "Work"),
            named("t", "Test"),
        ];
        group_by_folder(&mut rows, &["Work".into(), "Test".into()]);
        let order: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["a", "b", "t", "alpha", "zeta"]);
        let headings: Vec<_> = rows.iter().map(|r| r.heading.as_str()).collect();
        assert_eq!(headings, ["WORK", "", "TEST", "OTHER VMS", ""]);
        // Without folders, no headings.
        let mut plain = vec![named("b", ""), named("a", "")];
        group_by_folder(&mut plain, &[]);
        assert!(plain.iter().all(|r| r.heading.is_empty()));
    }
}

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ipc::Request;
use slint::{
    ComponentHandle, Image, ModelRc, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use vmconfig::create::{EasyInstall, NewVm, create_vm};
use vmconfig::library::{self, Library};
use vmconfig::presets::{self, PRESETS};
use vmconfig::snapshots::Tree;
use vmconfig::{Bus, Vm};

slint::include_modules!();

/// How often the library's running states are checked.
const STATE_INTERVAL: Duration = Duration::from_secs(1);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// The console's size must hold this long before the guest is asked for it.
const FIT_SETTLE: Duration = Duration::from_millis(400);
/// Scan codes for Ctrl+Alt+Del: left Ctrl, left Alt, Delete.
const CTRL_ALT_DEL: [u16; 3] = [0x1d, 0x38, 0xe053];

struct App {
    window: slint::Weak<AppWindow>,
    library: Library,
    vms: Vec<Result<Vm, (PathBuf, String)>>,
    running: Vec<bool>,
    tabs: Vec<PathBuf>,
    console: Option<console::Session>,
    /// When opening the console last failed, so it isn't retried every frame.
    console_retry: Option<Instant>,
    /// The console's size in physical pixels, and since when.
    console_size: Option<((u32, u32), Instant)>,
    /// Where the keyboard hook sends keys: the current console's pipe.
    keys: Arc<Mutex<Option<Sender<Request>>>>,
    /// Whether the hook takes the keyboard now.
    capture: Arc<AtomicBool>,
    /// Set by the hook when Ctrl+Alt releases the keyboard.
    released: Arc<AtomicBool>,
    /// The host clipboard's sequence number the console's guest has (or set itself).
    clipboard_seen: Option<u32>,
    /// The console's place in the window and its size, in logical pixels.
    console_area: Option<((f32, f32), (f32, f32))>,
    /// Whether the window's drop target is in place.
    #[cfg(windows)]
    drop_target: bool,
    /// A drag from the host over the window.
    #[cfg(windows)]
    incoming: Option<dnd::Incoming>,
    #[cfg(windows)]
    outgoing: dnd::Outgoing,
    /// Drags' transfers, for the status bar.
    #[cfg(windows)]
    transfers: Vec<dnd::Shown>,
    /// The current VM's snapshots, as the window shows them.
    snapshot_rows: Vec<SnapshotRow>,
    /// What the question on screen does if the user says yes.
    confirm: Option<Confirm>,
    /// Set (from another thread) when the library's file changed: the next refresh
    /// loads it again.
    library_changed: Arc<AtomicBool>,
}

type Shared = Rc<RefCell<App>>;

/// A snapshot operation that waits for the user to say yes.
enum Confirm {
    Revert {
        vm: PathBuf,
        id: String,
        name: String,
    },
    Delete {
        vm: PathBuf,
        id: String,
        name: String,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMMC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    if let Err(e) = run() {
        eprintln!("vmmc: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let window = AppWindow::new().map_err(|e| format!("can't open the window: {e}"))?;
    let library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    let app = Rc::new(RefCell::new(App {
        window: window.as_weak(),
        library,
        vms: Vec::new(),
        running: Vec::new(),
        tabs: Vec::new(),
        console: None,
        console_retry: None,
        console_size: None,
        keys: Arc::new(Mutex::new(None)),
        capture: Arc::new(AtomicBool::new(false)),
        released: Arc::new(AtomicBool::new(false)),
        clipboard_seen: None,
        console_area: None,
        #[cfg(windows)]
        drop_target: false,
        #[cfg(windows)]
        incoming: None,
        #[cfg(windows)]
        outgoing: dnd::Outgoing::Idle,
        #[cfg(windows)]
        transfers: Vec::new(),
        snapshot_rows: Vec::new(),
        confirm: None,
        library_changed: Arc::new(AtomicBool::new(false)),
    }));
    #[cfg(windows)]
    dnd::clean_staging();
    window.set_os_labels(strings(PRESETS.iter().map(|p| p.label)));
    window.set_editions(strings(media_editions()));
    reload(&app);
    // Reopen the tabs of last time.
    {
        let mut state = app.borrow_mut();
        state.tabs = state
            .library
            .data
            .tabs
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .collect();
    }
    if !app.borrow().tabs.is_empty() {
        window.set_current_tab(0);
    }
    refresh(&app);
    wire(&window, &app);

    let state_timer = slint::Timer::default();
    let watched = app.clone();
    state_timer.start(slint::TimerMode::Repeated, STATE_INTERVAL, move || {
        let changed = watched
            .borrow()
            .library_changed
            .swap(false, Ordering::Relaxed);
        if changed {
            reload(&watched);
        }
        refresh(&watched);
    });
    let frame_timer = slint::Timer::default();
    let shown = app.clone();
    frame_timer.start(slint::TimerMode::Repeated, FRAME_INTERVAL, move || {
        frame(&shown);
    });

    #[cfg(windows)]
    let _hook = {
        keyhook::stop_raw_input();
        let state = app.borrow();
        let keys = state.keys.clone();
        let released = state.released.clone();
        let capture = state.capture.clone();
        let mut held = std::collections::BTreeSet::new();
        keyhook::install(
            keyhook::current_thread(),
            capture.clone(),
            move |code, pressed| {
                if pressed {
                    held.insert(code);
                } else {
                    held.remove(&code);
                }
                let send = |request| {
                    if let Some(keys) = keys.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
                    {
                        let _ = keys.send(request);
                    }
                };
                send(Request::Key { code, pressed });
                // Ctrl+Alt gives the keyboard back to the host: the guest lets go of both.
                let ctrl = held.contains(&0x1d) || held.contains(&0xe01d);
                let alt = held.contains(&0x38) || held.contains(&0xe038);
                if ctrl && alt {
                    for code in std::mem::take(&mut held) {
                        send(Request::Key {
                            code,
                            pressed: false,
                        });
                    }
                    capture.store(false, Ordering::Relaxed);
                    released.store(true, Ordering::Relaxed);
                }
            },
        )
    };

    window
        .run()
        .map_err(|e| format!("the window failed: {e}"))?;
    // Remember the open tabs.
    let mut state = app.borrow_mut();
    state.library.data.tabs = state.tabs.iter().map(|p| p.display().to_string()).collect();
    let _ = state.library.save();
    Ok(())
}

fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

fn media_editions() -> Vec<&'static str> {
    EDITIONS.iter().map(|(name, _)| *name).collect()
}

/// Windows editions Easy Install installs (by their generic keys).
const EDITIONS: [(&str, &str); 6] = [
    ("Windows 11 Pro", "windows11"),
    ("Windows 11 Home", "windows11"),
    ("Windows 11 Enterprise", "windows11"),
    ("Windows 10 Pro", "windows10"),
    ("Windows 10 Home", "windows10"),
    ("Windows 10 Enterprise", "windows10"),
];

/// Reads the library and its VMs' files again.
fn reload(app: &Shared) {
    let mut state = app.borrow_mut();
    if let Ok(library) = Library::load(&state.library.path) {
        state.library = library;
    }
    state.vms = state
        .library
        .entries()
        .iter()
        .map(|entry| {
            let path = PathBuf::from(&entry.path);
            Vm::load(&path).map_err(|e| (path, e.to_string()))
        })
        .collect();
    state.running = vec![false; state.vms.len()];
}

fn vm_at<'a>(state: &'a App, path: &Path) -> Option<(usize, &'a Vm)> {
    state
        .vms
        .iter()
        .enumerate()
        .find_map(|(index, vm)| match vm {
            Ok(vm) if vm.path == path => Some((index, vm)),
            _ => None,
        })
}

fn os_label(os: &str) -> String {
    presets::find(os).map_or_else(|| "Other".into(), |preset| preset.label.into())
}

fn row(vm: &Vm, folder: Option<&str>, running: bool) -> VmRow {
    let config = &vm.config;
    let gib = |mib: u64| format!("{:.1} GiB", mib as f64 / 1024.0);
    let mut details = vec![format!(
        "{} processor{}, {} memory",
        config.cpus,
        if config.cpus == 1 { "" } else { "s" },
        gib(config.memory_mib)
    )];
    for disk in &config.disks {
        let bus = match disk.bus {
            Bus::Nvme => "NVMe",
            Bus::Sata => "SATA",
            Bus::Virtio => "virtio",
        };
        details.push(format!(
            "{bus} disk: {}{}",
            disk.path,
            if disk.read_only { " (read-only)" } else { "" }
        ));
    }
    for cdrom in &config.cdroms {
        details.push(format!(
            "DVD drive: {}",
            cdrom.path.as_deref().unwrap_or("empty")
        ));
    }
    for nic in &config.nics {
        let model = match nic.model {
            vmconfig::NicModel::E1000e => "e1000e",
            vmconfig::NicModel::Virtio => "virtio-net",
        };
        details.push(format!("Network: {model} on NAT, {}", nic.mac));
    }
    details.push(format!(
        "UEFI firmware, Secure Boot {}, TPM 2.0 {}",
        if config.firmware.secure_boot {
            "on"
        } else {
            "off"
        },
        if config.firmware.tpm { "on" } else { "off" }
    ));
    if let Some(resolution) = &config.display.resolution {
        details.push(format!("Display: {resolution}"));
    }
    if config.install.is_some() {
        details.push("Easy Install: runs at the next start".into());
    }
    let summary = format!(
        "{} vCPU, {}{}",
        config.cpus,
        gib(config.memory_mib),
        config
            .disks
            .first()
            .map(|d| format!(", {:?}", d.bus))
            .unwrap_or_default()
    );
    VmRow {
        path: vm.path.display().to_string().into(),
        name: config.name.clone().into(),
        folder: folder.unwrap_or_default().into(),
        tools: vmmc_client::has_tools(&config.os),
        heading: "".into(),
        os: if config.vmware.is_some() {
            format!("{} (from VMware)", os_label(&config.os)).into()
        } else {
            os_label(&config.os).into()
        },
        running,
        suspended: !running && vmmc_client::is_suspended(vm),
        summary: summary.into(),
        details: details.join("\n").into(),
        notes: config.notes.clone().into(),
        created: config.created.clone().unwrap_or_default().into(),
        started: config.last_powered_on.clone().unwrap_or_default().into(),
        origin: config
            .vmware
            .as_ref()
            .map(|origin| {
                format!(
                    "Opened in place from VMware ({}): vmmc writes to its own delta disks, and VMware's files stay as they were.",
                    origin.vmx
                )
            })
            .unwrap_or_default()
            .into(),
    }
}

fn broken_row(path: &Path, error: &str) -> VmRow {
    VmRow {
        path: path.display().to_string().into(),
        name: path
            .file_stem()
            .map_or("?".into(), |s| s.to_string_lossy().into_owned())
            .into(),
        os: "can't be read".into(),
        details: error.into(),
        ..VmRow::default()
    }
}

/// Checks which VMs run, and shows the library and tabs.
fn refresh(app: &Shared) {
    let mut state = app.borrow_mut();
    let Some(window) = state.window.upgrade() else {
        return;
    };
    #[cfg(windows)]
    {
        let running: Vec<bool> = state
            .vms
            .iter()
            .map(|vm| vm.as_ref().is_ok_and(vmmc_client::is_running))
            .collect();
        // A VM that started or stopped may have a changed file: its worker writes it
        // (when it started, Easy Install's steps and discs, snapshots' disks).
        for (index, now) in running.iter().enumerate() {
            if state.running.get(index) != Some(now) {
                let path = match &state.vms[index] {
                    Ok(vm) => vm.path.clone(),
                    Err((path, _)) => path.clone(),
                };
                state.vms[index] = Vm::load(&path).map_err(|e| (path, e.to_string()));
            }
        }
        state.running = running;
    }
    let entries = state.library.entries().to_vec();
    let mut rows: Vec<VmRow> = state
        .vms
        .iter()
        .zip(&state.running)
        .zip(&entries)
        .map(|((vm, running), entry)| match vm {
            Ok(vm) => row(vm, entry.folder.as_deref(), *running),
            Err((path, error)) => broken_row(path, error),
        })
        .collect();
    group_by_folder(&mut rows, &state.library.data.folders);
    let tabs: Vec<TabRow> = state
        .tabs
        .iter()
        .map(|path| match vm_at(&state, path) {
            Some((index, vm)) => TabRow {
                path: path.display().to_string().into(),
                name: vm.config.name.clone().into(),
                running: state.running[index],
            },
            None => TabRow {
                path: path.display().to_string().into(),
                name: "?".into(),
                running: false,
            },
        })
        .collect();
    let current = window.get_current_tab();
    if current >= tabs.len() as i32 {
        window.set_current_tab(tabs.len() as i32 - 1);
    }
    let current = window.get_current_tab();
    if current >= 0 {
        let path = &state.tabs[current as usize];
        if let Some((index, vm)) = vm_at(&state, path) {
            let entry = entries.get(index).and_then(|e| e.folder.as_deref());
            window.set_current(row(vm, entry, state.running[index]));
        }
    }
    let snapshots = (current >= 0)
        .then(|| vm_at(&state, &state.tabs[current as usize]))
        .flatten()
        .map(|(_, vm)| snapshot_rows(vm))
        .unwrap_or_default();
    if snapshots != state.snapshot_rows {
        let selected = window.get_selected_snapshot();
        if !snapshots.iter().any(|row| row.id == selected) {
            window.set_selected_snapshot("".into());
        }
        window.set_snapshots(ModelRc::new(VecModel::from(snapshots.clone())));
        state.snapshot_rows = snapshots;
    }
    // Drop a console whose VM stopped, or that isn't the current tab's.
    let current_path = (current >= 0).then(|| state.tabs[current as usize].clone());
    let console_running = state
        .console
        .as_ref()
        .and_then(|console| vm_at(&state, &console.path))
        .is_some_and(|(index, _)| state.running[index]);
    if state.console.is_some()
        && (!console_running || state.console.as_ref().map(|c| &c.path) != current_path.as_ref())
    {
        close_console(&mut state, &window);
    }
    window.set_vms(ModelRc::new(VecModel::from(rows)));
    window.set_tabs(ModelRc::new(VecModel::from(tabs)));
}

/// `vm`'s snapshots as the window lists them: each under the one it was taken from.
fn snapshot_rows(vm: &Vm) -> Vec<SnapshotRow> {
    fn add(tree: &Tree, parent: Option<&str>, depth: i32, rows: &mut Vec<SnapshotRow>) {
        for snapshot in tree.children(parent) {
            // "2026-10-16T19:30:00Z" as "2026-10-16 19:30".
            let taken: String = snapshot
                .created
                .replacen('T', " ", 1)
                .chars()
                .take(16)
                .collect();
            let mut detail = format!("Taken {taken}");
            if snapshot.running {
                detail += ", with the VM's memory";
            }
            if !snapshot.description.is_empty() {
                detail += " · ";
                detail += &snapshot.description;
            }
            rows.push(SnapshotRow {
                id: snapshot.id.as_str().into(),
                name: snapshot.name.as_str().into(),
                detail: detail.into(),
                depth,
                current: tree.current.as_deref() == Some(snapshot.id.as_str()),
            });
            add(tree, Some(&snapshot.id), depth + 1, rows);
        }
    }
    let mut rows = Vec::new();
    if let Ok(tree) = Tree::load(vm) {
        add(&tree, None, 0, &mut rows);
    }
    rows
}

/// Runs `job` off the window's thread (a snapshot pauses the VM; a merge takes a while
/// for a big disk), saying `doing` meanwhile; then what it did, or its error under the
/// title `failed`.
fn in_background(
    window: &AppWindow,
    doing: String,
    failed: &'static str,
    job: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    window.set_status_text(doing.into());
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let result = job();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            match result {
                Ok(done) => window.set_status_text(capitalized(&done).into()),
                Err(e) => {
                    window.set_status_text("".into());
                    message(&window, failed, &e);
                }
            }
        });
    });
}

/// `text` with its first letter upper case, for the status line.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Sorts the library's rows into its folders (in the library's order, then the VMs
/// in none), by name within each, and puts each folder's heading on its first VM.
fn group_by_folder(rows: &mut [VmRow], folders: &[String]) {
    let rank = |folder: &str| {
        folders
            .iter()
            .position(|f| f == folder)
            .unwrap_or(if folder.is_empty() {
                usize::MAX
            } else {
                folders.len()
            })
    };
    rows.sort_by(|a, b| {
        rank(&a.folder)
            .cmp(&rank(&b.folder))
            .then_with(|| a.folder.cmp(&b.folder))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let any_folders = rows.iter().any(|row| !row.folder.is_empty());
    let mut previous: Option<SharedString> = None;
    for row in rows.iter_mut() {
        if any_folders && previous.as_ref() != Some(&row.folder) {
            row.heading = if row.folder.is_empty() {
                "OTHER VMS".into()
            } else {
                row.folder.to_uppercase().into()
            };
        }
        previous = Some(row.folder.clone());
    }
}

fn close_console(state: &mut App, window: &AppWindow) {
    state.console = None;
    state.clipboard_seen = None;
    state.console_size = None;
    *state.keys.lock().unwrap_or_else(PoisonError::into_inner) = None;
    state.capture.store(false, Ordering::Relaxed);
    window.set_console_live(false);
    window.set_keyboard_captured(false);
}

/// Shows the current console's newest picture and cursor, and fits the guest.
fn frame(app: &Shared) {
    #[cfg(windows)]
    drags(app);
    let mut state = app.borrow_mut();
    let Some(window) = state.window.upgrade() else {
        return;
    };
    let current = window.get_current_tab();
    if current < 0 || !window.get_current().running || !window.get_show_console() {
        state.capture.store(false, Ordering::Relaxed);
        return;
    }
    let path = state.tabs[current as usize].clone();
    if state.console.is_none() {
        if state
            .console_retry
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        let Some((_, vm)) = vm_at(&state, &path) else {
            return;
        };
        match console::Session::open(vm) {
            Ok(session) => {
                *state.keys.lock().unwrap_or_else(PoisonError::into_inner) = Some(session.sender());
                state.console = Some(session);
                state.console_retry = None;
                window.set_console_live(true);
            }
            Err(e) => {
                tracing::debug!(%e, "no console yet");
                state.console_retry = Some(Instant::now());
                return;
            }
        }
    }
    if state.released.swap(false, Ordering::Relaxed) {
        window.set_keyboard_captured(false);
    }
    state
        .capture
        .store(window.get_keyboard_captured(), Ordering::Relaxed);
    let fit = window.get_fit_guest();
    let scale = window.window().scale_factor();
    let console_size = state.console_size;
    // Its fields, borrowed apart.
    let state = &mut *state;
    let Some(session) = state.console.as_mut() else {
        return;
    };
    if let Some(picture) = session.picture() {
        let pixels = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(
            &picture.rgb,
            picture.width,
            picture.height,
        );
        window.set_frame_width(picture.width as i32);
        window.set_frame_height(picture.height as i32);
        window.set_screen(Image::from_rgb8(pixels));
    }
    #[cfg(windows)]
    share_clipboard(session, &mut state.clipboard_seen);
    if let Some(cursor) = session.cursor() {
        if cursor.width > 0 && cursor.height > 0 {
            let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                &cursor.rgba,
                cursor.width,
                cursor.height,
            );
            window.set_cursor_image(Image::from_rgba8(pixels));
        }
        window.set_cursor_hot_x(cursor.hotspot.0 as i32);
        window.set_cursor_hot_y(cursor.hotspot.1 as i32);
        window.set_cursor_hardware(cursor.count != 0);
        window.set_cursor_shown(cursor.shown);
    }
    // The guest's screen follows the console's size, once it holds still.
    if fit
        && let Some(((width, height), since)) = console_size
        && since.elapsed() >= FIT_SETTLE
    {
        let size = (
            (width as f32 * scale) as u32,
            (height as f32 * scale) as u32,
        );
        if size.0 >= 320 && size.1 >= 200 && session.asked != Some(size) && session.frame != size {
            session.asked = Some(size);
            session.send(Request::RequestSize {
                width: size.0,
                height: size.1,
            });
        }
    }
}

/// Drag and drop's part of each frame: the window's drop target once it exists, a drag
/// out of the guest that's being looked for, and the transfers in the status bar.
#[cfg(windows)]
fn drags(app: &Shared) {
    let Some(window) = app.borrow().window.upgrade() else {
        return;
    };
    if !app.borrow().drop_target && dnd::register(app, &window) {
        app.borrow_mut().drop_target = true;
    }
    if matches!(app.borrow().outgoing, dnd::Outgoing::Probing(_)) {
        dnd::poll_probe(app, &window);
    }
    let mut state = app.borrow_mut();
    if !state.transfers.is_empty() {
        let text = dnd::status(&mut state.transfers).unwrap_or_default();
        window.set_status_text(text.into());
    }
}

/// Shares the clipboard with the console's guest: what the host copies goes to the
/// guest (the clipboard as it is when the console opens, too), and what the guest
/// copies comes to the host.
#[cfg(windows)]
fn share_clipboard(session: &mut console::Session, seen: &mut Option<u32>) {
    if let Some(text) = session.guest_copied()
        && clipboard::set_text(&text)
    {
        *seen = Some(clipboard::sequence());
    }
    let sequence = clipboard::sequence();
    if *seen == Some(sequence) {
        return;
    }
    *seen = Some(sequence);
    if let Some(mut text) = clipboard::text() {
        // At a character's end, within what the guest's agent takes.
        let mut end = text.len().min(ipc::clipboard::MAX_TEXT);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        session.send(Request::SetGuestClipboard { text });
    }
}

fn message(window: &AppWindow, title: &str, text: &str) {
    window.set_message_title(title.into());
    window.set_message_text(text.into());
    window.set_message_open(true);
}

/// Asks the user a question; its answer comes as `confirmed`.
fn ask(window: &AppWindow, title: &str, text: &str, action: &str) {
    window.set_confirm_title(title.into());
    window.set_confirm_text(text.into());
    window.set_confirm_action(action.into());
    window.set_confirm_open(true);
}

/// The current tab's VM, loaded.
fn current_vm(app: &Shared, window: &AppWindow) -> Option<Vm> {
    let state = app.borrow();
    let current = window.get_current_tab();
    let path = state.tabs.get(usize::try_from(current).ok()?)?;
    vm_at(&state, path).map(|(_, vm)| vm.clone())
}

/// The current tab's VM as its file has it now, to change and save. The worker changes
/// the file (Easy Install's steps and discs, snapshots' disks), so what the library
/// loaded earlier may be out of date, and saving that would undo those changes.
fn current_vm_on_disk(app: &Shared, window: &AppWindow) -> Option<Result<Vm, String>> {
    let path = current_vm(app, window)?.path;
    Some(Vm::load(&path).map_err(|e| e.to_string()))
}

/// Sends `request` to the current VM, through its console or a fresh connection.
fn command(app: &Shared, window: &AppWindow, request: Request) {
    if let Some(console) = app.borrow().console.as_ref()
        && console.send(request.clone())
    {
        return;
    }
    let Some(vm) = current_vm(app, window) else {
        return;
    };
    #[cfg(windows)]
    match vmmc_client::connect(&vm, Duration::from_secs(1)).and_then(|mut c| c.command(&request)) {
        Ok(()) => {}
        Err(e) => message(window, "Couldn't do that", &e),
    }
}

fn open_tab(app: &Shared, window: &AppWindow, path: PathBuf) {
    let mut state = app.borrow_mut();
    let index = match state.tabs.iter().position(|tab| *tab == path) {
        Some(index) => index,
        None => {
            state.tabs.push(path);
            state.tabs.len() - 1
        }
    };
    drop(state);
    window.set_current_tab(index as i32);
    window.set_show_console(true);
    refresh(app);
}

/// Starts `vm`'s worker; the library shows it running on its next refresh.
fn start_vm(window: &AppWindow, vm: Vm) {
    window.set_status_text(format!("Starting {}…", vm.config.name).into());
    let weak = window.as_weak();
    // Starting waits for the worker's pipe: off the window's thread.
    std::thread::spawn(move || {
        #[cfg(windows)]
        let result = vmmc_client::start(&vm).map(|_| ());
        #[cfg(not(windows))]
        let result: Result<(), String> = Err("running VMs needs Windows".into());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.set_status_text("".into());
            if let Err(e) = result {
                message(&window, &format!("{} didn't start", vm.config.name), &e);
            }
        });
    });
}

fn wire(window: &AppWindow, app: &Shared) {
    let weak = || window.as_weak();
    macro_rules! with {
        ($app:ident, $window:ident, $body:expr) => {{
            let $app = app.clone();
            let w = weak();
            move || {
                let Some($window) = w.upgrade() else {
                    return;
                };
                $body
            }
        }};
    }

    {
        let app = app.clone();
        let w = weak();
        window.on_open_vm(move |path| {
            if let Some(window) = w.upgrade() {
                open_tab(&app, &window, PathBuf::from(path.as_str()));
            }
        });
    }
    {
        let app = app.clone();
        let w = weak();
        window.on_select_tab(move |index| {
            if let Some(window) = w.upgrade() {
                window.set_current_tab(index);
                window.set_show_console(true);
                refresh(&app);
            }
        });
    }
    {
        let app = app.clone();
        let w = weak();
        window.on_close_tab(move |index| {
            let Some(window) = w.upgrade() else {
                return;
            };
            let mut state = app.borrow_mut();
            if (index as usize) < state.tabs.len() {
                state.tabs.remove(index as usize);
            }
            let left = state.tabs.len() as i32;
            drop(state);
            let current = window.get_current_tab();
            if current >= left || current > index {
                window.set_current_tab((current - 1).max(if left > 0 { 0 } else { -1 }));
            }
            refresh(&app);
        });
    }
    window.on_start_vm(with!(app, window, {
        if let Some(vm) = current_vm(&app, &window) {
            start_vm(&window, vm);
        }
    }));
    window.on_power_button(with!(app, window, {
        command(&app, &window, Request::PowerButton);
    }));
    window.on_stop_vm(with!(app, window, {
        command(&app, &window, Request::Stop);
    }));
    window.on_reset_vm(with!(app, window, {
        command(&app, &window, Request::Reset);
    }));
    window.on_ctrl_alt_del(with!(app, window, {
        for code in CTRL_ALT_DEL {
            command(
                &app,
                &window,
                Request::Key {
                    code,
                    pressed: true,
                },
            );
        }
        for code in CTRL_ALT_DEL.iter().rev() {
            command(
                &app,
                &window,
                Request::Key {
                    code: *code,
                    pressed: false,
                },
            );
        }
    }));
    window.on_install_tools(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        #[cfg(windows)]
        match vmmc_client::insert_tools(&vm) {
            Ok(text) => message(&window, "vmmc Tools", &text),
            Err(e) => message(&window, "Couldn't insert vmmc Tools", &e),
        }
    }));
    window.on_suspend_vm(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        #[cfg(windows)]
        in_background(
            &window,
            format!("Suspending {}…", vm.config.name),
            "Couldn't suspend",
            move || {
                vmmc_client::suspend(&vm)
                    .map(|()| format!("suspended {}: starting it resumes it", vm.config.name))
            },
        );
    }));
    window.on_take_snapshot(with!(app, window, {
        let mut vm = match current_vm_on_disk(&app, &window) {
            Some(Ok(vm)) => vm,
            Some(Err(e)) => return message(&window, "Couldn't take the snapshot", &e),
            None => return,
        };
        let name = window.get_snapshot_name().trim().to_owned();
        let description = window.get_snapshot_description().trim().to_owned();
        window.set_snapshot_open(false);
        #[cfg(windows)]
        in_background(
            &window,
            format!("Taking snapshot {name:?} of {}…", vm.config.name),
            "Couldn't take the snapshot",
            move || vmmc_client::take_snapshot(&mut vm, &name, &description),
        );
    }));
    window.on_revert_snapshot(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        let id = window.get_selected_snapshot().to_string();
        let Some(snapshot) = Tree::load(&vm).ok().and_then(|tree| tree.get(&id).cloned()) else {
            return;
        };
        let then = if window.get_current().running {
            " The VM goes on from the snapshot at once."
        } else if snapshot.running {
            " It resumes from the snapshot when you start it."
        } else {
            ""
        };
        let text = format!(
            "What {} did since “{}” is lost, unless a snapshot taken since keeps it.{then}",
            vm.config.name, snapshot.name
        );
        app.borrow_mut().confirm = Some(Confirm::Revert {
            vm: vm.path.clone(),
            id,
            name: snapshot.name.clone(),
        });
        ask(
            &window,
            &format!("Revert to “{}”?", snapshot.name),
            &text,
            "Revert",
        );
    }));
    window.on_delete_snapshot(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        let id = window.get_selected_snapshot().to_string();
        let Some(snapshot) = Tree::load(&vm).ok().and_then(|tree| tree.get(&id).cloned()) else {
            return;
        };
        let when = if window.get_current().running {
            " once the VM stops"
        } else {
            ""
        };
        let text = format!(
            "{} stays as it is: only the way back to “{}” goes. Its disk changes merge into the disks beneath{when}, which can take a while for a big disk.",
            vm.config.name, snapshot.name
        );
        app.borrow_mut().confirm = Some(Confirm::Delete {
            vm: vm.path.clone(),
            id,
            name: snapshot.name.clone(),
        });
        ask(&window, &format!("Delete “{}”?", snapshot.name), &text, "Delete");
    }));
    window.on_open_clone(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        let id = window.get_selected_snapshot().to_string();
        let snapshot = Tree::load(&vm).ok().and_then(|tree| tree.get(&id).cloned());
        let running = window.get_current().running;
        let (source, allowed) = match &snapshot {
            Some(snapshot) => (
                format!(
                    "A new VM from the snapshot “{}” of {}: its disks and settings as they were then, but not its memory.",
                    snapshot.name, vm.config.name
                ),
                true,
            ),
            None if running => (
                format!(
                    "{} is running: shut it down to clone it as it is, or pick a snapshot of it in the list.",
                    vm.config.name
                ),
                false,
            ),
            None => (
                format!("A new VM from {} as it is: its disks and settings.", vm.config.name),
                true,
            ),
        };
        window.set_clone_source(source.into());
        window.set_clone_allowed(allowed);
        window.set_clone_of_snapshot(snapshot.is_some());
        window.set_clone_linked(false);
        window.set_clone_name(unique_name(&app, &format!("{} clone", vm.config.name)).into());
        window.set_clone_open(true);
    }));
    window.on_make_clone(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        let selected = window.get_selected_snapshot().to_string();
        let snapshot = (window.get_clone_of_snapshot() && !selected.is_empty()).then_some(selected);
        let linked = window.get_clone_linked() && snapshot.is_some();
        let name = window.get_clone_name().trim().to_owned();
        window.set_clone_open(false);
        let weak = window.as_weak();
        let library_changed = app.borrow().library_changed.clone();
        in_background(
            &window,
            format!("Cloning {}…", vm.config.name),
            "Couldn't clone",
            move || {
                let mut shown = 0;
                let clone = vmmc_client::clone_vm(
                    &vm,
                    snapshot.as_deref(),
                    &name,
                    linked,
                    |done, total| {
                        let percent = done * 100 / total.max(1);
                        if percent > shown {
                            shown = percent;
                            let (weak, text) = (
                                weak.clone(),
                                format!("Cloning {}: {percent}%", vm.config.name),
                            );
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(window) = weak.upgrade() {
                                    window.set_status_text(text.into());
                                }
                            });
                        }
                    },
                )?;
                library_changed.store(true, Ordering::Relaxed);
                Ok(format!(
                    "{} {} as {}",
                    if linked { "linked" } else { "cloned" },
                    vm.config.name,
                    clone.config.name
                ))
            },
        );
    }));
    window.on_confirmed(with!(app, window, {
        let Some(confirm) = app.borrow_mut().confirm.take() else {
            return;
        };
        let (path, id, name, failed) = match &confirm {
            Confirm::Revert { vm, id, name } => (vm, id.clone(), name.clone(), "Couldn't revert"),
            Confirm::Delete { vm, id, name } => {
                (vm, id.clone(), name.clone(), "Couldn't delete the snapshot")
            }
        };
        let mut vm = match Vm::load(path) {
            Ok(vm) => vm,
            Err(e) => return message(&window, failed, &e.to_string()),
        };
        match confirm {
            Confirm::Revert { .. } => {
                #[cfg(windows)]
                in_background(
                    &window,
                    format!("Reverting {} to {name:?}…", vm.config.name),
                    failed,
                    move || vmmc_client::revert(&mut vm, &id),
                );
            }
            Confirm::Delete { .. } => in_background(
                &window,
                format!("Deleting snapshot {name:?}…"),
                failed,
                move || vmmc_client::delete_snapshot(&mut vm, &id),
            ),
        }
    }));
    {
        let w = weak();
        window.on_toggle_fullscreen(move || {
            if let Some(window) = w.upgrade() {
                let full = !window.get_fullscreen();
                window.set_fullscreen(full);
                // The bar shows at first, so the way out is plain to see.
                window.set_fullscreen_bar(full);
                window.window().set_fullscreen(full);
            }
        });
    }
    {
        let app = app.clone();
        let w = weak();
        window.on_pointer(move |x, y, width, height, left, right, middle, wheel| {
            // In full screen, the top edge brings the bar, and leaving it hides it.
            if let Some(window) = w.upgrade()
                && window.get_fullscreen()
            {
                if y < 3.0 {
                    window.set_fullscreen_bar(true);
                } else if y > 60.0 {
                    window.set_fullscreen_bar(false);
                }
            }
            let frame = match app.borrow().console.as_ref() {
                Some(console) => console.frame,
                None => return,
            };
            let at = console::tablet_position(x, y, (width, height), frame);
            // Past the console's edge with the button held: maybe a drag out of the
            // guest, which takes the pointer from the guest while it's looked into.
            #[cfg(windows)]
            if let Some(window) = w.upgrade() {
                let outside = x < 0.0 || y < 0.0 || x >= width || y >= height;
                if dnd::pointer_left(&app, &window, outside, left, at) {
                    return;
                }
            }
            let state = app.borrow();
            let Some(console) = state.console.as_ref() else {
                return;
            };
            let (x, y) = at;
            let buttons = u8::from(left) | (u8::from(right) << 1) | (u8::from(middle) << 2);
            console.send(Request::Pointer {
                x,
                y,
                buttons,
                wheel,
            });
        });
    }
    {
        let app = app.clone();
        window.on_console_resized(move |x, y, width, height| {
            let size = (width.max(0.0) as u32, height.max(0.0) as u32);
            let mut state = app.borrow_mut();
            state.console_area = Some(((x, y), (width, height)));
            if state.console_size.map(|(s, _)| s) != Some(size) {
                state.console_size = Some((size, Instant::now()));
            }
        });
    }
    {
        let app = app.clone();
        let w = weak();
        window.on_save_notes(move |notes| {
            let Some(window) = w.upgrade() else {
                return;
            };
            if let Some(vm) = current_vm_on_disk(&app, &window) {
                let saved = vm.and_then(|mut vm| {
                    vm.config.notes = notes.to_string();
                    vm.save().map_err(|e| e.to_string())
                });
                if let Err(e) = saved {
                    message(&window, "Couldn't save the notes", &e);
                }
                reload(&app);
                refresh(&app);
            }
        });
    }
    window.on_remove_vm(with!(app, window, {
        let Some(vm) = current_vm(&app, &window) else {
            return;
        };
        let mut state = app.borrow_mut();
        state.library.remove(&vm.path);
        state.tabs.retain(|tab| *tab != vm.path);
        let saved = state.library.save();
        drop(state);
        if let Err(e) = saved {
            message(&window, "Couldn't update the library", &e.to_string());
        }
        reload(&app);
        refresh(&app);
    }));
    {
        let app = app.clone();
        let w = weak();
        window.on_move_to_folder(move |folder| {
            let Some(window) = w.upgrade() else {
                return;
            };
            let Some(vm) = current_vm(&app, &window) else {
                return;
            };
            let folder = folder.trim();
            let mut state = app.borrow_mut();
            state
                .library
                .add(&vm.path, (!folder.is_empty()).then_some(folder));
            // Folders nothing is in any more go.
            let used: Vec<String> = state
                .library
                .entries()
                .iter()
                .filter_map(|entry| entry.folder.clone())
                .collect();
            state.library.data.folders.retain(|f| used.contains(f));
            let saved = state.library.save();
            drop(state);
            if let Err(e) = saved {
                message(&window, "Couldn't update the library", &e.to_string());
            }
            reload(&app);
            refresh(&app);
        });
    }
    window.on_show_folder(with!(app, window, {
        if let Some(vm) = current_vm(&app, &window) {
            let _ = std::process::Command::new("explorer.exe")
                .arg(format!("/select,{}", vm.path.display()))
                .spawn();
        }
    }));
    window.on_import_vm(with!(app, window, {
        let Some(vmx) = dialogs::pick_file("Open a VMware VM", "VMware VM (*.vmx)", "*.vmx") else {
            return;
        };
        match vmconfig::import::import(&vmx, None) {
            Ok(imported) => {
                let mut state = app.borrow_mut();
                state.library.add(&imported.vm.path, None);
                let _ = state.library.save();
                drop(state);
                reload(&app);
                open_tab(&app, &window, imported.vm.path.clone());
                let mut text = format!(
                    "{} opens in place: vmmc keeps its own files in {}, and VMware's stay as they were.",
                    imported.vm.config.name,
                    imported.vm.dir().display()
                );
                if !imported.warnings.is_empty() {
                    text += "\n\nWorth knowing:\n";
                    for warning in &imported.warnings {
                        text += &format!("• {warning}\n");
                    }
                }
                message(&window, "Imported", &text);
            }
            Err(e) => message(&window, "Couldn't import that VM", &e),
        }
    }));
    window.on_new_vm(with!(app, window, {
        let _ = &app;
        window.set_wizard_name(unique_name(&app, "New VM").into());
        window.set_wizard_os(0);
        apply_preset(&window, 0);
        window.set_wizard_iso("".into());
        window.set_wizard_easy(true);
        window.set_wizard_user(vmmc_client::host::user_name().into());
        window.set_wizard_password("".into());
        window.set_wizard_folder(library::default_vm_root().display().to_string().into());
        window.set_wizard_error("".into());
        window.set_wizard_open(true);
    }));
    {
        let w = weak();
        window.on_wizard_os_changed(move |index| {
            if let Some(window) = w.upgrade() {
                apply_preset(&window, index);
            }
        });
    }
    {
        let w = weak();
        window.on_browse_iso(move || {
            if let Some(window) = w.upgrade()
                && let Some(iso) =
                    dialogs::pick_file("Install disc", "Disc images (*.iso)", "*.iso")
            {
                window.set_wizard_iso(iso.display().to_string().into());
            }
        });
    }
    {
        let w = weak();
        window.on_browse_folder(move || {
            if let Some(window) = w.upgrade()
                && let Some(folder) = dialogs::pick_folder("Where the VM's folder goes")
            {
                window.set_wizard_folder(folder.display().to_string().into());
            }
        });
    }
    window.on_create_vm(with!(app, window, {
        match create_from_wizard(&window) {
            Ok(vm) => {
                let mut state = app.borrow_mut();
                state.library.add(&vm.path, None);
                let saved = state.library.save();
                drop(state);
                if let Err(e) = saved {
                    window.set_wizard_error(e.to_string().into());
                    return;
                }
                window.set_wizard_open(false);
                reload(&app);
                open_tab(&app, &window, vm.path.clone());
                // With an ISO, the install begins at once.
                if !vm.config.cdroms.is_empty() {
                    start_vm(&window, vm);
                }
            }
            Err(e) => window.set_wizard_error(e.into()),
        }
    }));
    window.on_open_settings(with!(app, window, {
        let vm = match current_vm_on_disk(&app, &window) {
            Some(Ok(vm)) => vm,
            Some(Err(e)) => return message(&window, "Couldn't read the VM's file", &e),
            None => return,
        };
        let config = &vm.config;
        window.set_settings_cpus(i32::from(config.cpus));
        window.set_settings_memory_mib(config.memory_mib as i32);
        window.set_settings_cdrom(
            config
                .cdroms
                .first()
                .and_then(|cd| cd.path.clone())
                .unwrap_or_default()
                .into(),
        );
        window
            .set_settings_resolution(config.display.resolution.clone().unwrap_or_default().into());
        window.set_settings_secure_boot(config.firmware.secure_boot);
        window.set_settings_tpm(config.firmware.tpm);
        window.set_settings_enlightenments(config.advanced.enlightenments);
        window.set_settings_error("".into());
        window.set_settings_open(true);
    }));
    {
        let w = weak();
        window.on_browse_cdrom(move || {
            if let Some(window) = w.upgrade()
                && let Some(iso) =
                    dialogs::pick_file("Disc for the DVD drive", "Disc images (*.iso)", "*.iso")
            {
                window.set_settings_cdrom(iso.display().to_string().into());
            }
        });
    }
    window.on_save_settings(with!(app, window, {
        let mut vm = match current_vm_on_disk(&app, &window) {
            Some(Ok(vm)) => vm,
            Some(Err(e)) => return window.set_settings_error(e.into()),
            None => return,
        };
        let config = &mut vm.config;
        config.cpus = window
            .get_settings_cpus()
            .clamp(1, i32::from(vmconfig::MAX_CPUS)) as u8;
        config.memory_mib = window.get_settings_memory_mib().max(32) as u64;
        let cdrom = window.get_settings_cdrom().trim().to_owned();
        match (cdrom.is_empty(), config.cdroms.first_mut()) {
            (true, Some(first)) => first.path = None,
            (false, Some(first)) => first.path = Some(cdrom),
            (false, None) => config.cdroms.push(vmconfig::Cdrom {
                path: Some(cdrom),
                install_media: false,
            }),
            (true, None) => {}
        }
        let resolution = window.get_settings_resolution().trim().to_owned();
        config.display.resolution = (!resolution.is_empty()).then_some(resolution);
        let secure_boot_changed = config.firmware.secure_boot != window.get_settings_secure_boot();
        config.firmware.secure_boot = window.get_settings_secure_boot();
        config.firmware.tpm = window.get_settings_tpm();
        config.advanced.enlightenments = window.get_settings_enlightenments();
        if let Err(e) = config.validate() {
            window.set_settings_error(e.into());
            return;
        }
        if let Err(e) = vm.save() {
            window.set_settings_error(e.to_string().into());
            return;
        }
        // Secure Boot lives in the UEFI variables: the VM gets a fresh store, from the
        // template with Secure Boot on or off (the old one stays as nvram.fd.old). Boot
        // entries go with it; Windows and Linux boot again from their fallback loader.
        if secure_boot_changed && vm.nvram().is_file() {
            let old = vm.nvram().with_extension("fd.old");
            let _ = std::fs::remove_file(&old);
            if let Err(e) = std::fs::rename(vm.nvram(), &old) {
                window.set_settings_error(format!("can't reset the UEFI variables: {e}").into());
                return;
            }
        }
        window.set_settings_open(false);
        reload(&app);
        refresh(&app);
    }));
}

/// The wizard's hardware for preset `index`.
fn apply_preset(window: &AppWindow, index: i32) {
    let Some(preset) = PRESETS.get(index.max(0) as usize) else {
        return;
    };
    window.set_wizard_cpus(i32::from(preset.cpus));
    window.set_wizard_memory_gib((preset.memory_mib / 1024).max(1) as i32);
    window.set_wizard_disk_gib(preset.disk_gib as i32);
    window.set_wizard_easy(matches!(preset.id, "windows11" | "windows10" | "ubuntu"));
    window.set_wizard_edition(if preset.id == "windows10" { 3 } else { 0 });
}

fn unique_name(app: &Shared, base: &str) -> String {
    let state = app.borrow();
    let taken = |name: &str| {
        state.vms.iter().any(|vm| {
            vm.as_ref()
                .is_ok_and(|vm| vm.config.name.eq_ignore_ascii_case(name))
        })
    };
    let mut name = base.to_owned();
    let mut number = 2;
    while taken(&name) {
        name = format!("{base} {number}");
        number += 1;
    }
    name
}

fn create_from_wizard(window: &AppWindow) -> Result<Vm, String> {
    let preset = PRESETS
        .get(window.get_wizard_os().max(0) as usize)
        .ok_or("pick an operating system")?;
    let name = window.get_wizard_name().trim().to_owned();
    let iso = window.get_wizard_iso().trim().to_owned();
    let iso = (!iso.is_empty()).then(|| PathBuf::from(&iso));
    if let Some(iso) = &iso
        && !iso.is_file()
    {
        return Err(format!("{} isn't there", iso.display()));
    }
    let easy =
        window.get_wizard_easy() && matches!(preset.id, "windows11" | "windows10" | "ubuntu");
    let easy_install = if easy {
        if iso.is_none() {
            return Err("Easy Install needs the install disc (ISO)".into());
        }
        let user = window.get_wizard_user().trim().to_owned();
        let password = window.get_wizard_password().to_string();
        if user.is_empty() {
            return Err("Easy Install needs a user name".into());
        }
        if password.is_empty() {
            return Err("Easy Install needs a password for that user".into());
        }
        Some(match preset.id {
            "ubuntu" => EasyInstall::Ubuntu {
                real_name: user.clone(),
                user: user.to_ascii_lowercase(),
                password,
                keyboard: vmmc_client::host::keyboard_layout().into(),
                locale: vmmc_client::host::linux_locale(),
                time_zone: vmmc_client::host::linux_time_zone(),
            },
            _ => EasyInstall::Windows {
                edition: EDITIONS
                    .get(window.get_wizard_edition().max(0) as usize)
                    .map_or("Windows 11 Pro", |(name, _)| name)
                    .into(),
                user,
                password,
                language: None,
                time_zone: vmmc_client::host::windows_time_zone().unwrap_or_else(|| "UTC".into()),
                first_logon: Vec::new(),
            },
        })
    } else {
        None
    };
    let folder = PathBuf::from(window.get_wizard_folder().trim());
    create_vm(&NewVm {
        name: &name,
        parent_dir: &folder,
        preset,
        cpus: Some(window.get_wizard_cpus().clamp(1, 64) as u8),
        memory_mib: Some(window.get_wizard_memory_gib().max(1) as u64 * 1024),
        disk_bytes: Some((window.get_wizard_disk_gib().max(1) as u64) << 30),
        iso: iso.as_deref(),
        adopt: None,
        existing_disk: None,
        easy_install,
    })
}
