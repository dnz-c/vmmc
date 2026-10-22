//! Drag and drop between the host and a guest, through vmmc's agent, driven as the app
//! drives it: the worker's pipe (`vmmc-vmx run --pipe-id`) and `vmmc_client::transfer`.
//! What the app adds, Windows' own drag on the host, is the one part left out.
//!
//! `cargo xtask win-dnd-test`:
//!
//! - **The image:** Windows installed once (as win11-test does, Secure Boot on) with vmmc
//!   Tools' agent, set to log on by itself and run the script on any test disc.
//!   `VMMC_REINSTALL=1` installs it again.
//! - **Each run:** boots it with a test disc holding the agent just built and
//!   `dnd-test.ps1`, which runs that agent and shows a window over the screen: its left
//!   half takes drops, its right half starts a drag of test files. Then a drag into the
//!   guest that leaves again, one of a file and a folder that drops, and one out of the
//!   guest whose files the test fetches. The guest reports files' SHA-256 hashes on COM1
//!   (`VMMC-DND:` lines), which must match the host's.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use ipc::{DragState, GuestDrag, Item, Request, Response};
use sha2::{Digest, Sha256};
use vmmc_client::{Connection, transfer};

use crate::drivers::{self, Drivers};
use crate::firmware::Session;
use crate::windows::{self, Install};
use crate::workspace;

const NAME: &str = "win-dnd-test";
const INSTALL: &str = include_str!("../guest/dnd-install.ps1");
const TEST: &str = include_str!("../guest/dnd-test.ps1");
/// What the guest script says once its window shows, with where things are.
const READY: &str = "VMMC-DND: ready";
const READY_TIMEOUT: Duration = Duration::from_secs(300);

pub fn win_test() -> Result<(), String> {
    let drivers = drivers::build(false)?;
    let image = crate::image_dir()?.join(format!("{NAME}-disk.vmdk"));
    if !image.is_file() || env::var_os("VMMC_REINSTALL").is_some() {
        install(&drivers)?;
    }
    let dir = workspace().join("target").join(NAME);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let disc = dir.join("test.iso");
    let script = windows::crlf(TEST);
    let agent = fs::read(&drivers.agent)
        .map_err(|e| format!("can't read {}: {e}", drivers.agent.display()))?;
    let files: [(&str, &[u8]); 2] = [
        ("vmmc-test.ps1", script.as_bytes()),
        ("vmmcagent.exe", &agent),
    ];
    fs::write(&disc, media::iso::build("VMMC_TEST", &files, None))
        .map_err(|e| format!("can't write {}: {e}", disc.display()))?;
    let id = format!("{NAME}-{}", std::process::id());
    let mut vm = windows::boot_installed_with(
        NAME,
        &dir.join("boot.log"),
        &[
            "--cdrom".into(),
            disc.into(),
            "--pipe-id".into(),
            id.clone().into(),
            "--screenshot-every".into(),
            "20".into(),
        ],
    )?;
    let result = drive(&mut vm, &id, &dir);
    for line in vm
        .text(0, vm.len())
        .lines()
        .filter(|line| line.contains("VMMC-DND:") || line.contains("vmmc-vmx: agent"))
    {
        eprintln!("{}", line.trim());
    }
    if result.is_err() {
        vm.dump_state();
    }
    // Shut down cleanly even after a failure: a Windows that was cut off may not log on
    // by itself next time.
    let _ = vm.send("\x01p");
    if vm.wait(Duration::from_secs(180)).is_none() {
        eprintln!("xtask: Windows didn't shut down; stopped it");
    }
    result?;
    eprintln!("{NAME} passed");
    Ok(())
}

const UBUNTU: &str = "ubuntu-dnd-test";
const UBUNTU_USER_DATA: &str = include_str!("../guest/ubuntu-dnd-user-data.yaml");
const UBUNTU_TEST: &str = include_str!("../guest/dnd-test.sh");
const UBUNTU_PEER: &str = include_str!("../guest/dnd-peer.js");

/// `cargo xtask ubuntu-dnd-test`: as `win-dnd-test`, with Ubuntu Desktop (GNOME on
/// Wayland), its agent working through XWayland. The image is installed once with
/// autoinstall: it logs on by itself, installs vmmc Tools from their disc at its first
/// boot, and runs the test disc's script at each logon. The script's peer is a native
/// Wayland program (GTK 4, through gjs), so what's checked is XWayland's bridges too; it
/// also checks the clipboard both ways first.
pub fn ubuntu_test() -> Result<(), String> {
    // The Tools disc, with the Linux agent.
    let drivers = drivers::build(false)?;
    let image = crate::image_dir()?.join(format!("{UBUNTU}-disk.vmdk"));
    if !image.is_file() || env::var_os("VMMC_REINSTALL").is_some() {
        crate::ubuntu::install(
            UBUNTU,
            &UBUNTU_USER_DATA.replace(char::from(13), ""),
            "VMMC-DND: autoinstall",
            "VMMC-DND: installed",
        )?;
    }
    let dir = workspace().join("target").join(UBUNTU);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let disc = dir.join("test.iso");
    let script = UBUNTU_TEST.replace(char::from(13), "");
    let peer = UBUNTU_PEER.replace(char::from(13), "");
    let agent = fs::read(&drivers.linux_agent)
        .map_err(|e| format!("can't read {}: {e}", drivers.linux_agent.display()))?;
    let files: [(&str, &[u8]); 3] = [
        ("vmmc-test.sh", script.as_bytes()),
        ("dnd-peer.js", peer.as_bytes()),
        ("vmmc-agent", &agent),
    ];
    fs::write(&disc, media::iso::build("VMMC_TEST", &files, None))
        .map_err(|e| format!("can't write {}: {e}", disc.display()))?;
    let tools = workspace()
        .join("target")
        .join("guest-drivers")
        .join(drivers::TOOLS_ISO);
    let id = format!("{UBUNTU}-{}", std::process::id());
    let mut command = crate::ubuntu::machine(UBUNTU, "screens-test")?;
    command
        .args(["--nic", "e1000e", "--pipe-id", &id])
        .arg("--cdrom")
        .arg(&tools)
        .arg("--cdrom")
        .arg(&disc);
    eprintln!("$ {command:?}");
    let mut vm = Session::spawn(command)?;
    // The drags too when the clipboard fails, to hear about both.
    let copied = clipboard(&mut vm, &id);
    let dragged = drive(&mut vm, &id, &dir);
    let result = match (copied, dragged) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(format!("the clipboard: {e}")),
        (Ok(()), Err(e)) => Err(e),
        (Err(clip), Err(drag)) => Err(format!("the clipboard: {clip}; and {drag}")),
    };
    for line in vm
        .text(0, vm.len())
        .lines()
        .filter(|line| line.contains("VMMC-DND:") || line.contains("vmmc-vmx: agent"))
    {
        eprintln!("{}", line.trim());
    }
    if result.is_err() {
        vm.dump_state();
    }
    // The power button: logind powers off at once (the image says so).
    let _ = vm.send("\x01p");
    if vm.wait(Duration::from_secs(180)).is_none() {
        eprintln!("xtask: Ubuntu didn't shut down; stopped it");
    }
    result?;
    eprintln!("{UBUNTU} passed");
    Ok(())
}

/// The clipboard both ways, with the guest's peer: it copies, and waits for the host's.
fn clipboard(vm: &mut Session, id: &str) -> Result<(), String> {
    // A click gives the peer's window the focus, which GNOME didn't.
    vm.expect("VMMC-DND: peer-shown", 0, READY_TIMEOUT)?;
    let mut pipe = vmmc_client::connect_id(id, Duration::from_secs(30))?;
    let middle = (16_384, 16_384);
    pointer(&mut pipe, middle, false)?;
    thread::sleep(Duration::from_millis(200));
    pointer(&mut pipe, middle, true)?;
    thread::sleep(Duration::from_millis(100));
    pointer(&mut pipe, middle, false)?;
    // (It may have had the focus already.)
    vm.expect("VMMC-DND: peer-focused", 0, Duration::from_secs(10))?;
    let at = vm.expect("VMMC-DND: clipboard-copied", 0, Duration::from_secs(10))?;
    vm.expect(
        "vmmc-vmx: the guest copied 29 characters",
        at,
        Duration::from_secs(30),
    )?;
    pipe.command(&Request::SetGuestClipboard {
        text: "vmmc clipboard from the host".into(),
    })?;
    vm.expect(
        "VMMC-DND: clipboard=vmmc clipboard from the host",
        at,
        Duration::from_secs(40),
    )?;
    eprintln!("xtask: the clipboard went both ways");
    Ok(())
}

/// Installs the test's Windows: its first logon installs vmmc Tools and sets up later
/// logons.
fn install(drivers: &Drivers) -> Result<(), String> {
    let setup = drivers::tools_file("setup.ps1")?;
    let agent = fs::read(&drivers.agent)
        .map_err(|e| format!("can't read {}: {e}", drivers.agent.display()))?;
    windows::install(
        &Install {
            name: NAME,
            vars: "OVMF_VARS.secboot.fd",
            first_logon: windows::crlf(INSTALL),
            files: vec![("setup.ps1".into(), setup), ("vmmcagent.exe".into(), agent)],
            specialize: &[],
            markers: &["VMMC-DND: booted", "VMMC-DND: done"],
        },
        |_, _, _| Ok(()),
    )
}

/// Where things are on the guest's screen, from its ready line, in pixels.
struct Layout {
    screen: (u32, u32),
    /// Where drops land.
    target: (u32, u32),
    /// Where a press starts a drag out.
    source: (u32, u32),
}

impl Layout {
    fn parse(line: &str) -> Result<Self, String> {
        let field = |key: &str| -> Result<(u32, u32), String> {
            let value = line
                .split_whitespace()
                .find_map(|word| word.strip_prefix(key))
                .ok_or_else(|| format!("no {key} in {line:?}"))?;
            let (a, b) = value
                .split_once(['x', ','])
                .ok_or_else(|| format!("bad {key} in {line:?}"))?;
            Ok((
                a.parse().map_err(|_| format!("bad {key} in {line:?}"))?,
                b.parse().map_err(|_| format!("bad {key} in {line:?}"))?,
            ))
        };
        Ok(Self {
            screen: field("screen=")?,
            target: field("target=")?,
            source: field("source=")?,
        })
    }

    /// A point on the screen in tablet units, as the app sends the pointer.
    fn tablet(&self, (x, y): (u32, u32)) -> (u32, u32) {
        let scale = |v: u32, size: u32| (u64::from(v) * 32767 / u64::from(size.max(2) - 1)) as u32;
        (scale(x, self.screen.0), scale(y, self.screen.1))
    }
}

/// `kind`'s lines after offset `from`: `VMMC-DND: kind HASH PATH`, as path → hash.
fn reported(vm: &Session, from: usize, kind: &str) -> BTreeMap<String, String> {
    let prefix = format!("VMMC-DND: {kind} ");
    vm.text(from, vm.len())
        .lines()
        .filter_map(|line| line.split_once(&prefix))
        .filter_map(|(_, rest)| {
            let (hash, path) = rest.trim_end().split_once(' ')?;
            Some((path.to_owned(), hash.to_owned()))
        })
        .collect()
}

fn sha256(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Every file under `dir`, as path (from `dir`, `/` between names) → SHA-256, and every
/// folder.
fn hashed(dir: &Path) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    fn walk(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<String, String>,
        dirs: &mut Vec<String>,
    ) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| format!("can't list {}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if path.is_dir() {
                dirs.push(relative);
                walk(root, &path, files, dirs)?;
            } else {
                files.insert(relative, sha256(&path)?);
            }
        }
        Ok(())
    }
    let (mut files, mut dirs) = (BTreeMap::new(), Vec::new());
    walk(dir, dir, &mut files, &mut dirs)?;
    dirs.sort();
    Ok((files, dirs))
}

/// The host's files for the drag in: a file, and a folder with a file in a folder and
/// an empty folder in it.
fn host_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let _ = fs::remove_dir_all(dir);
    let deep = dir.join("Mappe").join("sub");
    let empty = dir.join("Mappe").join("leer");
    for folder in [&deep, &empty] {
        fs::create_dir_all(folder)
            .map_err(|e| format!("can't create {}: {e}", folder.display()))?;
    }
    // More than a pipe message and a few queue batches.
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let data: Vec<u8> = (0..(5 << 20) + 5)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect();
    let file = dir.join("host file.bin");
    fs::write(&file, data).map_err(|e| format!("can't write {}: {e}", file.display()))?;
    let text = deep.join("\u{f6}l.txt");
    fs::write(&text, "vom Host").map_err(|e| format!("can't write {}: {e}", text.display()))?;
    Ok(vec![file, dir.join("Mappe")])
}

fn call(pipe: &mut Connection, request: &Request) -> Result<Response, String> {
    pipe.call(request)
}

fn pointer(pipe: &mut Connection, (x, y): (u32, u32), pressed: bool) -> Result<(), String> {
    pipe.command(&Request::Pointer {
        x,
        y,
        buttons: u8::from(pressed),
        wheel: 0,
    })
}

/// Waits until drag `drag` is in a state `done` accepts.
fn wait_drag(
    pipe: &mut Connection,
    drag: u32,
    timeout: Duration,
    done: impl Fn(&DragState) -> bool,
) -> Result<DragState, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = match call(pipe, &Request::DragState { drag })? {
            Response::DragState(state) => state,
            other => return Err(format!("unexpected answer {other:?}")),
        };
        if done(&state) {
            return Ok(state);
        }
        if let DragState::Failed(e) = &state {
            return Err(format!("the drag failed: {e}"));
        }
        if Instant::now() > deadline {
            return Err(format!("the drag stayed {state:?}"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn drag_enter(pipe: &mut Connection, items: Vec<Item>, at: (u32, u32)) -> Result<u32, String> {
    match call(
        pipe,
        &Request::DragEnter {
            items,
            x: at.0,
            y: at.1,
        },
    )? {
        Response::Drag { drag } => Ok(drag),
        other => Err(format!("unexpected answer {other:?}")),
    }
}

/// The test proper, once the guest is booting: see the module's documentation.
pub(crate) fn drive(vm: &mut Session, id: &str, dir: &Path) -> Result<(), String> {
    if vm.expect(READY, 0, READY_TIMEOUT / 2).is_err() {
        // Asleep at a lock screen, say: the pointer wakes the display, and a screenshot
        // shows what's there.
        if let Ok(mut pipe) = vmmc_client::connect_id(id, Duration::from_secs(5)) {
            for step in 0..10 {
                let _ = pointer(&mut pipe, (10_000 + step * 500, 10_000), false);
                thread::sleep(Duration::from_millis(100));
            }
        }
        thread::sleep(Duration::from_secs(3));
        let _ = vm.send("\x01s");
        eprintln!("xtask: no ready line yet; woke the display and took a screenshot");
        vm.expect(READY, 0, READY_TIMEOUT / 2)?;
    }
    thread::sleep(Duration::from_millis(300));
    let line = vm
        .text(0, vm.len())
        .lines()
        .find(|line| line.contains(READY))
        .unwrap_or_default()
        .to_owned();
    let layout = Layout::parse(&line)?;
    let guest_files = reported(vm, 0, "guest-file");
    let mut pipe = vmmc_client::connect_id(id, Duration::from_secs(30))?;
    // The agent says hello once it opens its port.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = pipe.status()?;
        if let Some(agent) = status.agent {
            eprintln!("xtask: the guest's agent: {agent:?}");
            break;
        }
        if Instant::now() > deadline {
            return Err("the guest's agent never said hello".into());
        }
        thread::sleep(Duration::from_millis(250));
    }

    let host_dir = dir.join("host-files");
    let paths = host_files(&host_dir)?;
    let target = layout.tablet(layout.target);
    // Each part runs whatever came of the ones before, to hear about all of them. The
    // drag out first: on a guest no drag has been through yet.
    let mut failures = Vec::new();
    if let Err(e) = drag_out(vm, &mut pipe, dir, &layout, &guest_files) {
        failures.push(format!("a drag out: {e}"));
    }
    if let Err(e) = drag_that_drops(vm, &mut pipe, id, &host_dir, &paths, target) {
        failures.push(format!("a drag that drops: {e}"));
        // Whatever's left of it in the guest gives up meanwhile.
        thread::sleep(Duration::from_secs(6));
    }
    if let Err(e) = drag_that_leaves(vm, &mut pipe, &paths, target) {
        failures.push(format!("a drag that leaves: {e}"));
    }
    // And one that drops after it.
    if let Err(e) = drag_that_drops(vm, &mut pipe, id, &host_dir, &paths, target) {
        failures.push(format!("a drag that drops after one that left: {e}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// A drag into the guest that leaves again: it's called off there.
fn drag_that_leaves(
    vm: &mut Session,
    pipe: &mut Connection,
    paths: &[PathBuf],
    target: (u32, u32),
) -> Result<(), String> {
    let pipe = &mut *pipe;
    let mark = vm.len();
    let drag = drag_enter(pipe, transfer::items(paths), target)?;
    wait_drag(pipe, drag, Duration::from_secs(5), |s| *s == DragState::InGuest)?;
    for step in 1..=5 {
        pipe.command(&Request::DragMove {
            drag,
            x: target.0 + step * 300,
            y: target.1,
        })?;
        thread::sleep(Duration::from_millis(50));
    }
    pipe.command(&Request::DragLeave { drag })?;
    vm.expect(
        "agent: the drag ended: called off",
        mark,
        Duration::from_secs(10),
    )?;
    if vm.text(mark, vm.len()).contains("VMMC-DND: dropped") {
        return Err("the drag that left was dropped in the guest".into());
    }
    eprintln!("xtask: a drag that left the guest was called off there");
    Ok(())
}

/// A drag into the guest that drops, its files sent on a connection of their own: the
/// guest gets them as they are on the host.
fn drag_that_drops(
    vm: &mut Session,
    pipe: &mut Connection,
    id: &str,
    host_dir: &Path,
    paths: &[PathBuf],
    target: (u32, u32),
) -> Result<(), String> {
    let mark = vm.len();
    let started = Instant::now();
    let drag = drag_enter(pipe, transfer::items(paths), target)?;
    let result = drop_files(vm, pipe, id, host_dir, paths, target, drag, mark, started);
    if result.is_err() {
        let _ = pipe.command(&Request::DragLeave { drag });
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn drop_files(
    vm: &mut Session,
    pipe: &mut Connection,
    id: &str,
    host_dir: &Path,
    paths: &[PathBuf],
    target: (u32, u32),
    drag: u32,
    mark: usize,
    started: Instant,
) -> Result<(), String> {
    let sender = {
        let (id, paths) = (id.to_owned(), paths.to_vec());
        thread::spawn(move || {
            let mut connection = vmmc_client::connect_id(&id, Duration::from_secs(5))?;
            transfer::send(
                &mut connection,
                drag,
                &paths,
                &AtomicBool::new(false),
                |_| {},
            )
        })
    };
    wait_drag(pipe, drag, Duration::from_secs(5), |s| *s == DragState::InGuest)?;
    for step in (0..=4).rev() {
        pipe.command(&Request::DragMove {
            drag,
            x: target.0 + step * 200,
            y: target.1 + step * 100,
        })?;
        thread::sleep(Duration::from_millis(50));
    }
    pipe.command(&Request::DragDrop {
        drag,
        x: target.0,
        y: target.1,
    })?;
    vm.expect("VMMC-DND: dropped", mark, Duration::from_secs(120))?;
    sender
        .join()
        .map_err(|_| "the sending thread panicked".to_owned())??;
    let state = wait_drag(pipe, drag, Duration::from_secs(10), |s| {
        matches!(s, DragState::Finished { .. })
    })?;
    if state != (DragState::Finished { dropped: true }) {
        return Err(format!("the drag ended {state:?}"));
    }
    let (host_hashes, host_dirs) = hashed(host_dir)?;
    let dropped = reported(vm, mark, "dropped-file");
    if dropped != host_hashes {
        return Err(format!(
            "the guest got {dropped:?}, not the host's {host_hashes:?}"
        ));
    }
    let dropped_dirs: Vec<String> = vm
        .text(mark, vm.len())
        .lines()
        .filter_map(|line| line.split_once("VMMC-DND: dropped-dir "))
        .map(|(_, dir)| dir.trim().to_owned())
        .collect();
    for folder in &host_dirs {
        if !dropped_dirs.contains(folder)
            && !host_hashes
                .keys()
                .any(|f| f.starts_with(&format!("{folder}/")))
        {
            return Err(format!(
                "the guest didn't get the folder {folder}: {dropped_dirs:?}"
            ));
        }
    }
    eprintln!(
        "xtask: dropped {} files into the guest in {} ms",
        host_hashes.len(),
        started.elapsed().as_millis()
    );
    Ok(())
}

/// A drag out of the guest: pressed on the source, out past the screen's left edge, and
/// the host fetches the files it carries.
fn drag_out(
    vm: &mut Session,
    pipe: &mut Connection,
    dir: &Path,
    layout: &Layout,
    guest_files: &BTreeMap<String, String>,
) -> Result<(), String> {
    let source = layout.tablet(layout.source);
    let mark = vm.len();
    let result = drag_out_files(vm, pipe, dir, source, guest_files, mark);
    // Whatever happened, the button goes up.
    let _ = pointer(pipe, source, false);
    result
}

fn drag_out_files(
    vm: &mut Session,
    pipe: &mut Connection,
    dir: &Path,
    source: (u32, u32),
    guest_files: &BTreeMap<String, String>,
    mark: usize,
) -> Result<(), String> {
    pointer(pipe, source, false)?;
    thread::sleep(Duration::from_millis(300));
    pointer(pipe, source, true)?;
    // Some programs start their drag on the press (WinForms), others once the pointer
    // has moved a little (GTK).
    let steps = 8;
    for step in 1..=steps {
        let x = source.0 - source.0 * step / steps;
        pointer(pipe, (x, source.1), true)?;
        thread::sleep(Duration::from_millis(60));
    }
    vm.expect("VMMC-DND: drag-started", mark, Duration::from_secs(10))?;
    let edge = (0, source.1);
    let probe = match call(
        pipe,
        &Request::ProbeGuestDrag {
            x: edge.0,
            y: edge.1,
        },
    )? {
        Response::Probe { probe } => probe,
        other => return Err(format!("unexpected answer {other:?}")),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let items = loop {
        match call(pipe, &Request::GuestDrag { probe })? {
            Response::GuestDrag(GuestDrag::Files(items)) => break items,
            Response::GuestDrag(GuestDrag::Looking) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            other => return Err(format!("the probe found {other:?}")),
        }
    };
    let expected = vec![
        Item {
            name: "guest file.bin".into(),
            dir: false,
        },
        Item {
            name: "Ordner".into(),
            dir: true,
        },
    ];
    if items != expected {
        return Err(format!("the guest's drag carries {items:?}"));
    }
    let started = Instant::now();
    let transfer = match call(pipe, &Request::ReceiveFiles { probe })? {
        Response::Transfer { transfer } => transfer,
        other => return Err(format!("unexpected answer {other:?}")),
    };
    let into = dir.join("from-guest");
    let _ = fs::remove_dir_all(&into);
    fs::create_dir_all(&into).map_err(|e| format!("can't create {}: {e}", into.display()))?;
    transfer::receive(pipe, transfer, &into, &AtomicBool::new(false), |_| {})?;
    let (got, got_dirs) = hashed(&into)?;
    if got != *guest_files {
        return Err(format!(
            "the host got {got:?}, not the guest's {guest_files:?}"
        ));
    }
    if !got_dirs.contains(&"Ordner/leer".to_owned()) {
        return Err(format!("the empty folder didn't come: {got_dirs:?}"));
    }
    eprintln!(
        "xtask: fetched {} files from the guest's drag in {} ms",
        got.len(),
        started.elapsed().as_millis()
    );
    vm.expect(
        "VMMC-DND: drag-done effect=None",
        mark,
        Duration::from_secs(10),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ready_line_parses() {
        let layout =
            Layout::parse("VMMC-DND: ready screen=1920x1080 target=480,540 source=1440,540")
                .unwrap();
        assert_eq!(layout.screen, (1920, 1080));
        assert_eq!(layout.target, (480, 540));
        assert_eq!(layout.tablet((1919, 1079)), (32767, 32767));
        assert_eq!(layout.tablet((0, 0)), (0, 0));
    }
}
