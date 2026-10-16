//! `cargo xtask win-snapshot-test`: a running Windows suspended, then resumed in a new
//! process.
//!
//! It boots the Windows that `win11-test` installed (Secure Boot, a TPM, the Hyper-V
//! enlightenments), on a delta disk over that test's disk so the installation stays as
//! it was. Once Windows has asked vmmc's NAT for an address and settled at its logon
//! screen, Ctrl-A u suspends it, saving its whole state, and a new vmmc-vmx resumes
//! it. The power button must then shut it down cleanly: that needs its timers (the
//! synthetic ones the enlightenments give it), its interrupts, its disk and ACPI to
//! work after the restore.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::windows::{CPUS, MEMORY_MIB, RESOLUTION};
use crate::{build_vmmc_vmx, native, workspace};

/// How long Windows gets to ask for an address.
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long it then gets to settle at its logon screen.
const SETTLE: Duration = Duration::from_secs(60);
/// How long a suspend or a restore may take.
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
    let vm_dir = dir.join("vm");
    fs::create_dir_all(&vm_dir).map_err(|e| format!("can't create {}: {e}", vm_dir.display()))?;
    // Its UEFI variables, TPM, UUID and MAC address, copied.
    for name in ["nvram.fd", "tpm-state.bin", "uuid", "mac"] {
        let from = installed_vm.join(name);
        if from.is_file() {
            fs::copy(&from, vm_dir.join(name))
                .map_err(|e| format!("can't copy {}: {e}", from.display()))?;
        }
    }
    let delta = crate::image_dir()?.join("win-snapshot-test-delta.vmdk");
    let _ = fs::remove_file(&delta);
    disk::create_delta(&delta, &installed)
        .map_err(|e| format!("can't create {}: {e}", delta.display()))?;
    // On the drive with the test disks: a few GiB, written while Windows is paused.
    let state = crate::image_dir()?.join("win-snapshot-test.state");
    let _ = fs::remove_file(&state);

    let start = |run: u32, restore: bool| -> Result<(Session, std::path::PathBuf), String> {
        let log = dir.join(format!("vmmc-vmx-{run}.log"));
        let log_file =
            fs::File::create(&log).map_err(|e| format!("can't create {}: {e}", log.display()))?;
        let mut command = Command::new(&exe);
        command
            .arg("run")
            .arg("--firmware")
            .arg(native.join("OVMF_CODE.fd"))
            .arg("--tpm")
            .arg("--vm-dir")
            .arg(&vm_dir)
            .arg("--disk")
            .arg(format!("{},nvme", delta.display()))
            .args(["--cpus", CPUS, "--memory", MEMORY_MIB])
            .args(["--nic", "e1000e"])
            .args(["--resolution", RESOLUTION])
            .arg("--screenshot")
            .arg(dir.join("screens"))
            .arg("--save-state")
            .arg(&state)
            // The NAT's DHCP server says when Windows asks for an address.
            .env("VMMC_LOG", "net::dhcp=debug")
            .stderr(Stdio::from(log_file));
        if restore {
            command.arg("--restore").arg(&state);
        }
        eprintln!("$ {command:?}");
        Ok((Session::spawn(command)?, log))
    };

    // Windows boots, settles, and is suspended.
    let (mut vm, log) = start(1, false)?;
    let started = Instant::now();
    let suspended = (|| {
        // A DHCP acknowledgement (reply type 5).
        wait_for_log(&mut vm, &log, "reply=5", BOOT_TIMEOUT)?;
        eprintln!(
            "xtask: Windows took a DHCP address after {}s",
            started.elapsed().as_secs()
        );
        thread::sleep(SETTLE);
        vm.send("\x01s")?;
        thread::sleep(Duration::from_secs(3));
        vm.send("\x01u")?;
        wait_for_log(&mut vm, &log, "saved the VM's state", SAVE_TIMEOUT)?;
        match vm.wait(Duration::from_secs(60)) {
            Some(status) if status.success() => Ok(()),
            Some(status) => Err(format!("vmmc-vmx ended with {status} after suspending")),
            None => Err("vmmc-vmx didn't stop after suspending".into()),
        }
    })();
    if suspended.is_err() {
        let _ = vm.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        vm.kill();
    }
    suspended?;
    let saved = log_text(&log);
    let line = saved
        .lines()
        .find(|line| line.contains("saved the VM's state"))
        .unwrap_or_default();
    eprintln!(
        "xtask: suspended Windows ({} MiB): {}",
        fs::metadata(&state).map_or(0, |m| m.len() >> 20),
        line.trim()
    );

    // Resumed, it shuts down on the power button.
    let (mut vm, log) = start(2, true)?;
    let resumed = (|| {
        wait_for_log(&mut vm, &log, "restored the VM's state", SAVE_TIMEOUT)?;
        eprintln!("xtask: resumed Windows");
        thread::sleep(Duration::from_secs(20));
        vm.send("\x01s")?;
        thread::sleep(Duration::from_secs(3));
        vm.send("\x01p")?;
        match vm.wait(SHUTDOWN_TIMEOUT) {
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
        let _ = vm.send("\x01r");
        thread::sleep(Duration::from_secs(2));
        let _ = vm.send("\x01s");
        thread::sleep(Duration::from_secs(2));
        vm.kill();
    }
    resumed.map_err(|e| {
        format!(
            "{e}; see {} and the screenshots in {}",
            log.display(),
            dir.join("screens").display()
        )
    })?;
    eprintln!("xtask: the resumed Windows shut down on the power button");
    let _ = fs::remove_file(&delta);
    let _ = fs::remove_file(&state);
    eprintln!("win-snapshot-test passed");
    Ok(())
}
