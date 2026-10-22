//! `cargo xtask worker-test`: VMs run in sandboxed workers, as vmmc's app and command
//! line start them, and answer on their pipes.
//!
//! 1. The test Linux, from an EFI system partition, with a scratch VMDK in its folder:
//!    started with `vmmc_client::start`, it runs at low integrity with a restricted
//!    token, shares its screen, passes its own checks (disk, TPM, network) and powers
//!    off, ending its worker.
//! 2. A VM with nothing to boot waits in the firmware; a second start is refused, and
//!    a hard stop ends it.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ipc::Request;
use vmconfig::create::{NewVm, create_vm};
use vmconfig::{Bus, Disk, Vm, presets};

use crate::{build_vmmc_vmx, esp, firmware, native, workspace};

const TIMEOUT: Duration = Duration::from_secs(120);

/// PS/2 scan codes (set 1) the test types.
const ENTER: u16 = 0x1c;
const DOWN: u16 = 0xe050;

/// Presses and releases a key through the VM's pipe.
fn press(connection: &mut vmmc_client::Connection, code: u16) -> Result<(), String> {
    for pressed in [true, false] {
        connection.command(&Request::Key { code, pressed })?;
    }
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

/// The worker's log as plain text: the firmware draws its menus with escape sequences.
fn log_text(log: &Path) -> String {
    let bytes = fs::read(log).unwrap_or_default();
    String::from_utf8_lossy(&firmware::PlainText::default().filter(&bytes)).into_owned()
}

/// Opens the firmware's boot menu and starts the UEFI shell, which runs the ESP's
/// startup.nsh: all through the pipe's keyboard, as the UEFI tests do on the serial
/// console.
fn start_shell(connection: &mut vmmc_client::Connection, log: &Path) -> Result<(), String> {
    let at = expect_in_log(
        log,
        &["Press any key to enter the Boot Manager Menu"],
        TIMEOUT,
        0,
    )?;
    press(connection, ENTER)?;
    let menu = expect_in_log(log, &["Please select boot device:"], TIMEOUT, at)?;
    let end = expect_in_log(log, &["to move selection"], TIMEOUT, menu)?;
    let items = firmware::screen_texts(&log_text(log)[menu..end]);
    let index = items
        .iter()
        .position(|item| item == firmware::SHELL_OPTION)
        .ok_or_else(|| format!("no shell in the boot menu: {items:?}"))?;
    for _ in 0..index {
        press(connection, DOWN)?;
    }
    press(connection, ENTER)?;
    expect_in_log(log, &[firmware::SHELL_STARTED], TIMEOUT, end)?;
    press(connection, ENTER)
}

/// Waits for `markers`, in order, in the worker's log (as plain text), from byte
/// `from`. Returns the offset just past the last.
fn expect_in_log(
    log: &Path,
    markers: &[&str],
    timeout: Duration,
    from: usize,
) -> Result<usize, String> {
    let deadline = Instant::now() + timeout;
    let mut at = from;
    for marker in markers {
        loop {
            let text = log_text(log);
            if let Some(found) = text.get(at..).and_then(|rest| rest.find(marker)) {
                at += found + marker.len();
                eprintln!("xtask: {marker}");
                break;
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "no {marker:?} in {} after {}s",
                    log.display(),
                    timeout.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
    Ok(at)
}

fn wait_until_stopped(vm: &Vm) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while vmmc_client::is_running(vm) {
        if Instant::now() > deadline {
            return Err(format!("{}'s worker didn't end", vm.config.name));
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn new_vm(root: &Path, name: &str) -> Result<Vm, String> {
    create_vm(&NewVm {
        name,
        parent_dir: root,
        preset: presets::find("linux").unwrap(),
        cpus: Some(4),
        memory_mib: Some(1024),
        disk_bytes: Some(64 << 20),
        iso: None,
        adopt: None,
        existing_disk: None,
        easy_install: None,
    })
}

/// Snapshots of the running idle VM through its worker: one with its memory, a revert
/// to it, a suspend, and the start that resumes it.
fn snapshots(vm: &mut Vm) -> Result<(), String> {
    let log = vm.dir().join(vmmc_client::LOG_FILE);
    let restored = |log: &Path| log_text(log).matches("restored the VM's state").count();
    let image = vm.resolve(&vm.config.disks[0].path);
    eprintln!(
        "xtask: {}",
        vmmc_client::take_snapshot(vm, "In the firmware", "")?
    );
    *vm = Vm::load(&vm.path).map_err(|e| e.to_string())?;
    let tree = vmconfig::snapshots::Tree::load(vm).map_err(|e| e.to_string())?;
    let snapshot = tree
        .find("In the firmware")
        .ok_or("the snapshot isn't in the tree")?;
    if !snapshot.running || !vmconfig::snapshots::state_file(vm, &snapshot.id).is_file() {
        return Err("the snapshot has no memory state".into());
    }
    let delta = vm.resolve(&vm.config.disks[0].path);
    if delta == image || !delta.is_file() {
        return Err(format!(
            "the disk didn't move to a delta: {}",
            delta.display()
        ));
    }

    // Reverted while it runs: the worker carries on from the snapshot.
    eprintln!("xtask: {}", vmmc_client::revert(vm, "In the firmware")?);
    let deadline = Instant::now() + TIMEOUT;
    while restored(&log) < 1 {
        if Instant::now() > deadline {
            return Err("the reverted VM didn't start from the snapshot".into());
        }
        thread::sleep(Duration::from_millis(250));
    }
    *vm = Vm::load(&vm.path).map_err(|e| e.to_string())?;
    if vmmc_client::connect(vm, Duration::from_secs(5))?
        .status()?
        .state
        != ipc::State::Running
    {
        return Err("the reverted VM isn't running".into());
    }
    // The delta that held what came after the snapshot is gone.
    if delta.exists() {
        return Err(format!("the revert left {}", delta.display()));
    }

    // Suspended, then resumed by its next start.
    vmmc_client::suspend(vm)?;
    wait_until_stopped(vm)?;
    if !vmmc_client::is_suspended(vm) {
        return Err("the suspended VM has no saved state".into());
    }
    vmmc_client::start(vm)?;
    let deadline = Instant::now() + TIMEOUT;
    // The new worker's log starts afresh.
    while restored(&log) < 1 || vmmc_client::is_suspended(vm) {
        if Instant::now() > deadline {
            return Err("the suspended VM didn't resume".into());
        }
        thread::sleep(Duration::from_millis(250));
    }
    eprintln!("xtask: snapshot, revert, suspend and resume worked through the worker");
    Ok(())
}

/// Builds the worker, and gives the workers the firmware with the UEFI shell (which runs
/// the test ESP's startup.nsh). Returns the test's empty folder for its VMs.
fn setup(name: &str) -> Result<PathBuf, String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    build_vmmc_vmx(&workspace)?;
    // The worker inherits the variable.
    // SAFETY: no other thread of xtask reads the environment at this point.
    unsafe { std::env::set_var("VMMC_FIRMWARE", native.join("OVMF_CODE.test.fd")) };
    let root = workspace.join("target").join(name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).map_err(|e| format!("can't create {}: {e}", root.display()))?;
    Ok(root)
}

/// A new VM in `root` with the test Linux: booted from an ESP (a read-only disk), its
/// scratch disk the VM's own VMDK, with a TPM.
fn linux_vm(root: &Path, name: &str) -> Result<Vm, String> {
    let mut vm = new_vm(root, name)?;
    let http = crate::serve_http()?;
    let files = firmware::linux_boot_files(&format!(" vmmc.http={http} vmmc.tpm"))?;
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    esp::build(&vm.dir().join("esp.raw"), firmware::ESP_SIZE, &files)?;
    vm.config.disks.insert(
        0,
        Disk {
            path: "esp.raw".into(),
            bus: Bus::Nvme,
            read_only: true,
        },
    );
    vm.config.firmware.tpm = true;
    vm.save().map_err(|e| e.to_string())?;
    Ok(vm)
}

pub fn test() -> Result<(), String> {
    let root = setup("worker-test")?;
    // 1. Linux, booted from an ESP, its scratch disk the VM's own VMDK.
    let vm = linux_vm(&root, "Linux")?;
    boot_linux(&vm)?;

    // 2. Nothing to boot: the firmware waits, until a hard stop.
    idle(&root)?;
    let _ = fs::remove_dir_all(&root);
    eprintln!("worker-test passed");
    Ok(())
}

/// `cargo xtask clone-test`: the test Linux cloned two ways, each clone booting in a
/// sandboxed worker and passing the guest's checks on a disk of its own: a linked clone
/// of a snapshot (its disk a delta on the snapshot's), and a full clone of the VM as it
/// is. The linked clone's snapshot can't be deleted while the clone is there.
pub fn clone_test() -> Result<(), String> {
    let root = setup("clone-test")?;
    let mut vm = linux_vm(&root, "Linux")?;
    let snapshot = vmconfig::snapshots::take(&mut vm, "Clean", "")?;
    let clone = |name, snapshot, linked| {
        vmconfig::clone::clone_vm(
            &vmconfig::clone::Clone {
                source: &vm,
                snapshot,
                name,
                parent_dir: &root,
                linked,
            },
            |_, _| {},
        )
    };
    let linked = clone("Linked", Some(snapshot.id.as_str()), true)?;
    eprintln!("xtask: made a linked clone of {:?}", snapshot.name);
    boot_linux(&linked)?;
    let full = clone("Full", None, false)?;
    eprintln!("xtask: made a full clone");
    boot_linux(&full)?;
    if vmconfig::snapshots::delete(&vm, &snapshot.id).is_ok() {
        return Err("the linked clone's snapshot could be deleted".into());
    }
    eprintln!("clone-test passed");
    Ok(())
}

/// Boots the test Linux of `vm` in a sandboxed worker: the worker shares its screen,
/// and the guest passes its checks and powers off, which ends the worker.
fn boot_linux(vm: &Vm) -> Result<(), String> {
    let log = vm.dir().join(vmmc_client::LOG_FILE);
    let mut connection = vmmc_client::start(vm)?;
    let status = connection.status()?;
    eprintln!(
        "xtask: worker {} runs with {}",
        connection.pid, status.sandbox
    );
    if status.sandbox != "low integrity, restricted token" {
        return Err(format!("the worker isn't sandboxed: {}", status.sandbox));
    }
    let screen = connection.screen()?;
    let view = screen.view();
    let mut seq = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    let picture = loop {
        if let Some(picture) = view.read_picture(&mut seq) {
            break picture;
        }
        if Instant::now() > deadline {
            return Err("the worker shared no picture".into());
        }
        thread::sleep(Duration::from_millis(100));
    };
    eprintln!(
        "xtask: the shared screen shows {}x{}",
        picture.width, picture.height
    );
    start_shell(&mut connection, &log)?;
    expect_in_log(&log, &firmware::LINUX_MARKERS, TIMEOUT, 0)?;
    // The guest powers off after its checks, which ends the worker.
    wait_until_stopped(vm)?;
    expect_in_log(
        &log,
        &["vmmc-vmx: guest powered off"],
        Duration::from_secs(5),
        0,
    )?;
    Ok(())
}

/// A VM with nothing to boot waits in the firmware: a second start is refused, a disc
/// goes in, snapshots work, and a hard stop ends it.
fn idle(root: &Path) -> Result<(), String> {
    let mut idle = new_vm(root, "Idle")?;
    let mut connection = vmmc_client::start(&idle)?;
    let again = vmmc_client::start(&idle).err().unwrap_or_default();
    if !again.contains("running already") {
        return Err(format!("a second start wasn't refused: {again:?}"));
    }
    thread::sleep(Duration::from_secs(3));
    if connection.status()?.state != ipc::State::Running {
        return Err("the idle VM isn't running".into());
    }
    // A disc goes into its (empty) DVD drive while it runs; a drive it hasn't is refused.
    let disc = root.join("disc.iso");
    fs::write(
        &disc,
        media::iso::build("VMMC_TEST", &[("hello.txt", b"vmmc")], None),
    )
    .map_err(|e| e.to_string())?;
    connection.command(&Request::InsertDisc {
        drive: 0,
        path: Some(disc.display().to_string()),
    })?;
    let refused = connection
        .command(&Request::InsertDisc {
            drive: 5,
            path: None,
        })
        .err()
        .unwrap_or_default();
    if !refused.contains("no DVD drive") {
        return Err(format!("a missing drive wasn't refused: {refused:?}"));
    }
    expect_in_log(
        &idle.dir().join(vmmc_client::LOG_FILE),
        &["changed the disc in DVD drive 0"],
        Duration::from_secs(5),
        0,
    )?;
    if let Err(e) = snapshots(&mut idle) {
        if let Ok(mut connection) = vmmc_client::connect(&idle, Duration::from_secs(2)) {
            let _ = connection.command(&Request::Stop);
        }
        return Err(e);
    }
    let mut connection = vmmc_client::connect(&idle, Duration::from_secs(5))?;
    connection.command(&Request::Stop)?;
    wait_until_stopped(&idle)?;
    eprintln!("xtask: a hard stop ended the idle VM");
    Ok(())
}
