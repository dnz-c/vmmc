//! `cargo xtask snapshot-test`: a running Linux guest saved, and started again from its
//! saved state.
//!
//! 1. The test Linux boots with a shell on its serial console (and networking). A
//!    marker goes into its RAM (a tmpfs file), and Ctrl-A v saves the VM's whole state
//!    while it runs on. The marker changes, and the VM quits.
//! 2. A new vmmc-vmx starts from the saved state: the shell carries on, with the first
//!    marker. Its timers, its virtio disk and its network (through the new process's
//!    NAT) work.
//! 3. Ctrl-A u suspends it, a third vmmc-vmx resumes it, and the same checks pass
//!    again. Then the guest powers off.
//!
//! The guest doesn't write its disk after the save: a state needs the disks as they
//! were when it was saved (snapshots keep them so with delta disks).

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::{HTTP_BODY, build_vmmc_vmx, guests, serve_http, vmmc_vmx_command, workspace};

const TIMEOUT: Duration = Duration::from_secs(90);
const SAVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `command` in the guest's shell and returns what it printed. A marker computed
/// by the shell (so not the echo of what was typed) says it's done.
fn shell(vm: &mut Session, command: &str, step: &mut u32) -> Result<String, String> {
    *step += 1;
    let n = *step;
    let from = vm.len();
    vm.send(&format!(
        "{command}; echo VMMC-SNAP: done-$(({n}*1000+{n}))\r"
    ))?;
    let end = vm.expect(&format!("VMMC-SNAP: done-{}", n * 1000 + n), from, TIMEOUT)?;
    Ok(vm.text(from, end))
}

/// Waits until the guest's shell answers. Until vmmc-vmx has restored the machine,
/// what's typed is dropped, so ask again until it is.
fn wait_for_shell(vm: &mut Session) -> Result<(), String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let from = vm.len();
        vm.send("echo VMMC-SNAP: ready-$((6*7))\r")?;
        if vm
            .expect("VMMC-SNAP: ready-42", from, Duration::from_secs(1))
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() > deadline || vm.exited().is_some() {
            return Err("the restored guest's shell doesn't answer".into());
        }
    }
}

/// Waits for `path` to appear: a save renames the finished state into place.
fn wait_for_file(path: &Path, vm: &mut Session) -> Result<(), String> {
    let deadline = Instant::now() + SAVE_TIMEOUT;
    while !path.is_file() {
        if vm.exited().is_some() && !path.is_file() {
            return Err("vmmc-vmx exited without saving the VM's state".into());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the VM's state wasn't saved within {}s",
                SAVE_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// What a restored guest must still do: have the first marker, sleep for as long as
/// it's told, read its disk, and fetch from the host.
fn check_restored(vm: &mut Session, http: u16, step: &mut u32) -> Result<(), String> {
    wait_for_shell(vm)?;
    let marker = shell(vm, "cat /tmp/marker", step)?;
    if !marker.contains("marker-one") || marker.contains("marker-two") {
        return Err(format!("the guest's RAM isn't as it was saved: {marker:?}"));
    }
    let started = Instant::now();
    shell(vm, "sleep 2", step)?;
    let slept = started.elapsed();
    if slept < Duration::from_secs(2) || slept > Duration::from_secs(20) {
        return Err(format!("a 2 s sleep took {} ms", slept.as_millis()));
    }
    let disk = shell(
        vm,
        "dd if=/dev/vda bs=64k count=64 2>/dev/null | md5sum",
        step,
    )?;
    if !disk
        .split_whitespace()
        .any(|word| word.len() == 32 && word.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(format!("reading the disk failed: {disk:?}"));
    }
    let page = shell(
        vm,
        &format!("wget -q -T 10 -O - http://10.0.2.2:{http}/vmmc"),
        step,
    )?;
    if !page.contains(HTTP_BODY) {
        return Err(format!(
            "the network doesn't work after the restore: {page:?}"
        ));
    }
    eprintln!(
        "xtask: the restored guest has its RAM, timers, disk and network (a 2 s sleep took {} ms)",
        slept.as_millis()
    );
    Ok(())
}

pub fn test() -> Result<(), String> {
    let workspace = workspace();
    let guest = guests::prepare(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join("snapshot-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let state = dir.join("vm.state");
    let http = serve_http()?;
    let cmdline = format!("console=ttyS0 panic=-1 vmmc.interactive vmmc.http={http}");
    let start = |restore: bool| -> Result<Session, String> {
        let mut command = vmmc_vmx_command(&exe, &guest, &cmdline);
        command
            .args(["--nic", "virtio"])
            .arg("--save-state")
            .arg(&state);
        if restore {
            command.arg("--restore").arg(&state);
        }
        eprintln!("$ {command:?}");
        Session::spawn(command)
    };
    let mut step = 0;

    // A running guest, saved.
    let mut vm = start(false)?;
    let result = (|| {
        vm.expect("vmmc test shell", 0, TIMEOUT)?;
        shell(&mut vm, "echo marker-one >/tmp/marker", &mut step)?;
        let save = Instant::now();
        vm.send("\x01v")?;
        wait_for_file(&state, &mut vm)?;
        eprintln!(
            "xtask: saved the running VM ({} MiB) in {} ms",
            fs::metadata(&state).map_or(0, |m| m.len() >> 20),
            save.elapsed().as_millis()
        );
        // It runs on.
        shell(&mut vm, "echo marker-two >/tmp/marker", &mut step)?;
        vm.send("\x01x")?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        vm.dump_state();
    }
    if vm.wait(Duration::from_secs(30)).is_none() {
        return result.and(Err("vmmc-vmx didn't quit".into()));
    }
    result?;

    // Started again from the saved state.
    let mut vm = start(true)?;
    let result = (|| {
        check_restored(&mut vm, http, &mut step)?;
        // Suspended, then resumed.
        fs::remove_file(&state).map_err(|e| format!("can't remove the old state: {e}"))?;
        vm.send("\x01u")?;
        wait_for_file(&state, &mut vm)?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        vm.dump_state();
        vm.kill();
    }
    result?;
    if vm.wait(Duration::from_secs(30)).is_none() {
        return Err("vmmc-vmx didn't stop after suspending".into());
    }
    eprintln!("xtask: suspended");

    let mut vm = start(true)?;
    let result = (|| {
        check_restored(&mut vm, http, &mut step)?;
        vm.send("poweroff -f\r")?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        vm.dump_state();
        vm.kill();
    }
    result?;
    match vm.wait(Duration::from_secs(30)) {
        Some(status) if status.success() => {
            eprintln!("snapshot test passed");
            Ok(())
        }
        Some(status) => Err(format!("vmmc-vmx exited with {status}")),
        None => Err("the resumed guest didn't power off".into()),
    }
}
