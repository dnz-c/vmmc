//! Developer tasks: `cargo xtask <task>`.

mod cpio;
mod guests;

use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  ci            format check, clippy, tests and cargo-deny, as CI runs them
  fetch-guests  download the test kernel and BusyBox, and build the test initramfs
  boot-test     boot the test guests on WHP; fails if the Windows hypervisor isn't running
  linux-shell   boot the test Linux guest with a shell on this terminal";

/// How long the Linux test guest gets to boot, test its disk and power off.
const LINUX_TIMEOUT: Duration = Duration::from_secs(90);
const LINUX_CPUS: &str = "4";
/// What the test guest's init prints when everything works, in order.
const LINUX_MARKERS: [&str; 4] = [
    "VMMC-TEST: booted",
    "VMMC-TEST: cpus=4",
    "VMMC-TEST: disk=ok",
    "VMMC-TEST: done",
];

fn main() -> ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("ci") => ci(),
        Some("fetch-guests") => guests::prepare(&workspace()).map(|guest| {
            eprintln!(
                "guest artifacts ready in {}",
                guest.kernel.parent().unwrap().display()
            );
        }),
        Some("boot-test") => boot_test(),
        Some("linux-shell") => linux_shell(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is in the workspace")
        .to_owned()
}

fn ci() -> Result<(), String> {
    cargo(&["fmt", "--all", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace"])?;
    cargo(&["deny", "check"])
}

fn boot_test() -> Result<(), String> {
    run(cargo_command()
        .args([
            "test",
            "-p",
            "hv-whp",
            "--test",
            "guests",
            "--",
            "--nocapture",
        ])
        .env("VMMC_REQUIRE_WHP", "1"))?;
    linux_boot_test()
}

/// Boots the test Linux guest headless and checks it reports a working SMP boot and
/// disk, then powers off cleanly.
fn linux_boot_test() -> Result<(), String> {
    let workspace = workspace();
    let guest = guests::prepare(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;

    let mut command = vmmc_vmx_command(&exe, &guest, "console=ttyS0 panic=-1");
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    eprintln!("$ {command:?}");
    let mut child = command
        .spawn()
        .map_err(|e| format!("can't start {}: {e}", exe.display()))?;

    // Echo the guest console while collecting it.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = thread::spawn(move || {
        let mut all = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(read @ 1..) = stdout.read(&mut chunk) {
            let _ = std::io::stdout().write_all(&chunk[..read]);
            all.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&all).into_owned()
    });

    let deadline = Instant::now() + LINUX_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the Linux guest didn't power off within {}s",
                LINUX_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(100));
    };
    let output = reader.join().map_err(|_| "console reader panicked")?;

    let mut rest = output.as_str();
    for marker in LINUX_MARKERS {
        match rest.find(marker) {
            Some(at) => rest = &rest[at + marker.len()..],
            None => return Err(format!("the Linux guest never printed {marker:?}")),
        }
    }
    if !status.success() {
        return Err(format!("vmmc-vmx exited with {status}"));
    }
    eprintln!("Linux boot test passed");
    Ok(())
}

fn linux_shell() -> Result<(), String> {
    let workspace = workspace();
    let guest = guests::prepare(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    run(&mut vmmc_vmx_command(
        &exe,
        &guest,
        "console=ttyS0 vmmc.interactive",
    ))
}

fn vmmc_vmx_command(exe: &Path, guest: &guests::Guest, cmdline: &str) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("run")
        .arg("--kernel")
        .arg(&guest.kernel)
        .arg("--initrd")
        .arg(&guest.initramfs)
        .arg("--disk")
        .arg(&guest.disk)
        .args([
            "--cpus",
            LINUX_CPUS,
            "--memory",
            "1024",
            "--cmdline",
            cmdline,
        ]);
    command
}

fn build_vmmc_vmx(workspace: &Path) -> Result<PathBuf, String> {
    cargo(&["build", "-p", "vmmc-vmx"])?;
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    Ok(target
        .join("debug")
        .join(format!("vmmc-vmx{}", env::consts::EXE_SUFFIX)))
}

fn cargo(args: &[&str]) -> Result<(), String> {
    run(cargo_command().args(args))
}

/// A cargo command that runs in the workspace root, whatever the current directory is.
fn cargo_command() -> Command {
    let mut command = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    command.current_dir(workspace());
    command
}

fn run(command: &mut Command) -> Result<(), String> {
    let line = format!("{command:?}");
    eprintln!("$ {line}");
    let status = command
        .status()
        .map_err(|e| format!("could not run {line}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{line} failed ({status})"))
    }
}
