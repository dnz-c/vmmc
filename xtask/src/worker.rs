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
use std::path::Path;
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

pub fn test() -> Result<(), String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    build_vmmc_vmx(&workspace)?;
    // The firmware with the UEFI shell, which runs the ESP's startup.nsh; the worker
    // inherits the variable.
    // SAFETY: no other thread of xtask reads the environment at this point.
    unsafe { std::env::set_var("VMMC_FIRMWARE", native.join("OVMF_CODE.test.fd")) };
    let root = workspace.join("target").join("worker-test");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).map_err(|e| format!("can't create {}: {e}", root.display()))?;

    // 1. Linux, booted from an ESP, its scratch disk the VM's own VMDK.
    let mut vm = new_vm(&root, "Linux")?;
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
    let log = vm.dir().join(vmmc_client::LOG_FILE);
    let mut connection = vmmc_client::start(&vm)?;
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
    wait_until_stopped(&vm)?;
    expect_in_log(
        &log,
        &["vmmc-vmx: guest powered off"],
        Duration::from_secs(5),
        0,
    )?;
    drop((connection, screen));

    // 2. Nothing to boot: the firmware waits, until a hard stop.
    let idle = new_vm(&root, "Idle")?;
    let mut connection = vmmc_client::start(&idle)?;
    let again = vmmc_client::start(&idle).err().unwrap_or_default();
    if !again.contains("running already") {
        return Err(format!("a second start wasn't refused: {again:?}"));
    }
    thread::sleep(Duration::from_secs(3));
    if connection.status()?.state != ipc::State::Running {
        return Err("the idle VM isn't running".into());
    }
    connection.command(&Request::Stop)?;
    wait_until_stopped(&idle)?;
    eprintln!("xtask: a hard stop ended the idle VM");

    let _ = fs::remove_dir_all(&root);
    eprintln!("worker-test passed");
    Ok(())
}
