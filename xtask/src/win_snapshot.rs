//! `cargo xtask win-snapshot-test`: a running Windows reverted to a snapshot, then
//! suspended and resumed in a new process, all through the worker's pipe as the app does
//! it.
//!
//! It boots the Windows that `win11-test` installed (Secure Boot, a TPM, the Hyper-V
//! enlightenments) as a VM file whose disk is a delta over that test's disk, so the
//! installation stays as it was. Once Windows has asked vmmc's NAT for an address and
//! settled at its logon screen, a snapshot is taken; Windows runs on, and is reverted to
//! it (the worker starts a new machine from the snapshot's state). Then it's suspended,
//! and a new vmmc-vmx resumes it. The power button must then shut it down cleanly: that
//! needs its timers (the synthetic ones the enlightenments give it), its interrupts, its
//! disk and ACPI to work after both restores.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ipc::{Request, Response};
use vmconfig::{Bus, Disk, Nic, NicModel, Vm, VmConfig};

use crate::firmware::Session;
use crate::windows::{CPUS, MEMORY_MIB, RESOLUTION};
use crate::{build_vmmc_vmx, native, workspace};

/// How long Windows gets to ask for an address.
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long it then gets to settle at its logon screen.
const SETTLE: Duration = Duration::from_secs(60);
/// How long Windows runs on between the snapshot and the revert, and after each restore.
const RUN_ON: Duration = Duration::from_secs(30);
/// How long a snapshot, a suspend or a restore may take.
const SAVE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the resumed Windows gets to shut down.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);

fn log_text(path: &Path) -> String {
    String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned()
}

/// Waits until vmmc-vmx's log has `text`, or it exits, or `timeout` passes.
fn wait_for_log(vm: &mut Session, log: &Path, text: &str, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if log_text(log).contains(text) {
            return Ok(());
        }
        if let Some(status) = vm.exited() {
            return Err(format!(
                "vmmc-vmx ended ({status}) before its log said {text:?}"
            ));
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "vmmc-vmx's log didn't say {text:?} within {}s",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// The installed Windows as a VM file in `dir`: its UEFI variables, TPM, UUID and MAC
/// address copied, its disk a delta over the installed one.
fn make_vm(installed: &Path, installed_vm: &Path, dir: &Path) -> Result<Vm, String> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    for name in ["nvram.fd", "tpm-state.bin"] {
        let from = installed_vm.join(name);
        fs::copy(&from, dir.join(name))
            .map_err(|e| format!("can't copy {}: {e}", from.display()))?;
    }
    let read = |name: &str| {
        fs::read_to_string(installed_vm.join(name))
            .map(|text| text.trim().to_owned())
            .map_err(|e| format!("can't read {name} of the installed VM: {e}"))
    };
    let disk = dir.join("Windows.vmdk");
    disk::create_delta(&disk, installed)
        .map_err(|e| format!("can't create {}: {e}", disk.display()))?;
    let mut config = VmConfig::new(
        "Windows",
        "windows11",
        CPUS.parse().unwrap(),
        MEMORY_MIB.parse().unwrap(),
    );
    config.uuid = read("uuid")?;
    config.nics = vec![Nic {
        model: NicModel::E1000e,
        mac: read("mac")?,
    }];
    config.firmware.secure_boot = true;
    config.firmware.tpm = true;
    config.display.resolution = Some(RESOLUTION.into());
    config.disks = vec![Disk {
        path: "Windows.vmdk".into(),
        bus: Bus::Nvme,
        read_only: false,
    }];
    let vm = Vm {
        path: dir.join("Windows.vmmc"),
        config,
    };
    vm.save().map_err(|e| e.to_string())?;
    Ok(vm)
}

pub fn test() -> Result<(), String> {
    let workspace = workspace();
    let installed = crate::image_dir()?.join("win11-test-disk.vmdk");
    let installed_vm = workspace.join("target").join("win11-test").join("vm");
    if !installed.is_file() || !installed_vm.join("nvram.fd").is_file() {
        return Err(format!(
            "no installed Windows at {}: run `cargo xtask win11-test` first",
            installed.display()
        ));
    }
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join("win-snapshot-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    // On the drive with the test disks: its snapshot and suspended states are a few GiB
    // each, written while Windows is paused.
    let vm_dir = crate::image_dir()?.join("win-snapshot-test");
    let vm = make_vm(&installed, &installed_vm, &vm_dir)?;
    let id = format!("win-snapshot-test-{}", std::process::id());

    let start = |run: u32| -> Result<(Session, PathBuf), String> {
        let log = dir.join(format!("vmmc-vmx-{run}.log"));
        let log_file =
            fs::File::create(&log).map_err(|e| format!("can't create {}: {e}", log.display()))?;
        let mut command = Command::new(&exe);
        command
            .arg("run")
            .arg("--vm")
            .arg(&vm.path)
            .arg("--firmware")
            .arg(native.join("OVMF_CODE.fd"))
            .args(["--pipe-id", &id])
            .arg("--screenshot")
            .arg(dir.join("screens"))
            // The NAT's DHCP server says when Windows asks for an address.
            .env("VMMC_LOG", "net::dhcp=debug")
            .stderr(Stdio::from(log_file));
        eprintln!("$ {command:?}");
        Ok((Session::spawn(command)?, log))
    };
    let pipe = || vmmc_client::connect_id(&id, Duration::from_secs(10));

    // Windows boots and settles; a snapshot, Windows runs on, a revert to it; then it's
    // suspended.
    let (mut session, log) = start(1)?;
    let started = Instant::now();
    let suspended = (|| {
        // A DHCP acknowledgement (reply type 5).
        wait_for_log(&mut session, &log, "reply=5", BOOT_TIMEOUT)?;
        eprintln!(
            "xtask: Windows took a DHCP address after {}s",
            started.elapsed().as_secs()
        );
        thread::sleep(SETTLE);
        let mut pipe = pipe()?;
        let snapshot = match pipe.call(&Request::Snapshot {
            name: "At the logon screen".into(),
            description: String::new(),
        })? {
            Response::Snapshot { id, paused_ms } => {
                eprintln!("xtask: took a snapshot of the running Windows (paused {paused_ms} ms)");
                id
            }
            other => return Err(format!("unexpected answer to the snapshot: {other:?}")),
        };
        thread::sleep(RUN_ON);
        session.send("\x01s")?;
        pipe.command(&Request::Revert { snapshot })?;
        wait_for_log(&mut session, &log, "restored the VM's state", SAVE_TIMEOUT)?;
        eprintln!("xtask: reverted Windows to the snapshot");
        thread::sleep(RUN_ON);
        session.send("\x01s")?;
        thread::sleep(Duration::from_secs(3));
        // The worker ends as it answers, so the answer may not come.
        let _ = pipe.command(&Request::Suspend);
        match session.wait(SAVE_TIMEOUT) {
            Some(status) if status.success() => {}
            Some(status) => return Err(format!("vmmc-vmx ended with {status} after suspending")),
            None => return Err("vmmc-vmx didn't stop after suspending".into()),
        }
        let state = vmconfig::snapshots::suspended_file(&vm);
        if !state.is_file() {
            return Err(format!("no suspended state at {}", state.display()));
        }
        eprintln!(
            "xtask: suspended Windows ({} MiB)",
            fs::metadata(&state).map_or(0, |m| m.len() >> 20)
        );
        Ok(())
    })();
    if suspended.is_err() {
        let _ = session.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        session.kill();
    }
    suspended.map_err(|e| {
        format!(
            "{e}; see {} and the screenshots in {}",
            log.display(),
            dir.join("screens").display()
        )
    })?;

    // Started again, it resumes, and shuts down on the power button.
    let (mut session, log) = start(2)?;
    let resumed = (|| {
        wait_for_log(&mut session, &log, "restored the VM's state", SAVE_TIMEOUT)?;
        eprintln!("xtask: resumed Windows");
        thread::sleep(RUN_ON);
        session.send("\x01s")?;
        thread::sleep(Duration::from_secs(3));
        session.send("\x01p")?;
        match session.wait(SHUTDOWN_TIMEOUT) {
            Some(status) if status.success() && log_text(&log).contains("guest powered off") => {
                Ok(())
            }
            Some(status) => Err(format!("vmmc-vmx ended with {status}")),
            None => Err(format!(
                "the resumed Windows didn't shut down within {}s of the power button",
                SHUTDOWN_TIMEOUT.as_secs()
            )),
        }
    })();
    if resumed.is_err() {
        let _ = session.send("\x01r");
        thread::sleep(Duration::from_secs(2));
        let _ = session.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        session.kill();
    }
    resumed.map_err(|e| {
        format!(
            "{e}; see {} and the screenshots in {}",
            log.display(),
            dir.join("screens").display()
        )
    })?;
    eprintln!("xtask: the resumed Windows shut down on the power button");
    let _ = fs::remove_dir_all(&vm_dir);
    eprintln!("win-snapshot-test passed");
    Ok(())
}
