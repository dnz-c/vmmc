//! Developer tasks: `cargo xtask <task>`.

mod cpio;
mod drivers;
mod easy_install;
mod esp;
mod firmware;
mod guests;
mod modules;
mod native;
mod perf;
mod snapshot;
mod ubuntu;
mod vmdk_compat;
mod vmware_import;
mod windows;
mod worker;

use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  ci            format check, clippy, tests and cargo-deny, as CI runs them
  fetch-guests  download the test kernel and BusyBox, and build the test initramfs
  fetch-native  download the CI-built OVMF and TPM into target/native (needs
                `gh auth login`)
  boot-test     every WHP test below, plus the hypervisor backend's own; fails if the
                Windows hypervisor isn't running
  linux-test    boot the test Linux directly on WHP (virtio disk)
  firmware-test boot OVMF on WHP to its shell, check variables persist and it powers off
  uefi-linux-test
                boot the test Linux through OVMF from an EFI system partition on NVMe
  uefi-cd-test  boot the test Linux through OVMF from an ISO in the AHCI DVD drive
  win11-test    install Windows 11 unattended from $VMMC_WIN11_ISO (20-60 minutes)
  guest-drivers build vmmc's Windows guest drivers with the WDK and test-sign them
  win-display-test
                install Windows with Secure Boot off and test signing on, install the
                display driver and check that the resolution changes ($VMMC_WIN11_ISO)
  win-perf [RUNS]
                boot the Windows win11-test installed RUNS times (3) each without and
                with Hyper-V enlightenments, in a release build, and compare timings
  ubuntu-desktop-test
                download Ubuntu 26.04 Desktop, install it with autoinstall and boot it
                (15-60 minutes)
  easy-install-test [windows|ubuntu]
                install Windows ($VMMC_WIN11_ISO) and Ubuntu with Easy Install in
                sandboxed workers, as the app does (20-60 minutes)
  worker-test   start VMs in sandboxed workers, as vmmc-cli does, and control them
  snapshot-test save a running Linux guest's whole state, then resume it from it
                through their pipes
  vmdk-compat-test
                check vmmc's VMDKs against VMware's vmware-vdiskmanager, both ways
                (needs VMware Workstation)
  vmware-import-test
                open a VMware VM ($VMMC_VMWARE_VMX) in place, boot it until it takes a
                DHCP address, power it off, and check VMware's files are unchanged
  linux-shell   boot the test Linux guest with a shell on this terminal";

/// How long the Linux test guest gets to boot, test its disk and power off.
const LINUX_TIMEOUT: Duration = Duration::from_secs(90);
const LINUX_CPUS: &str = "4";
/// What the test guest's init prints when everything works, in order.
const LINUX_MARKERS: [&str; 9] = [
    "VMMC-TEST: booted",
    "VMMC-TEST: cpus=4",
    "VMMC-TEST: efi=no",
    "VMMC-TEST: disk=ok vda",
    "VMMC-TEST: usb=tablet,keyboard",
    "VMMC-TEST: keyboard=vmmc ps2",
    "VMMC-TEST: net=ok",
    "VMMC-TEST: dns=ok",
    "VMMC-TEST: done",
];
/// What the Linux test types on the guest's PS/2 keyboard.
const KEYBOARD_LINE: &str = "vmmc ps2";
/// What the host's test web server answers, and the guest checks for.
pub(crate) const HTTP_BODY: &str = "vmmc net ok";

fn main() -> ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("ci") => ci(),
        Some("fetch-guests") => guests::prepare(&workspace()).map(|guest| {
            eprintln!(
                "guest artifacts ready in {}",
                guest.kernel.parent().unwrap().display()
            );
        }),
        Some("fetch-native") => native::fetch(&workspace()).map(|dir| {
            eprintln!("native dependencies ready in {}", dir.display());
        }),
        Some("boot-test") => boot_test(),
        Some("linux-test") => linux_boot_test(),
        Some("firmware-test") => firmware::test(),
        Some("uefi-linux-test") => firmware::linux_test(),
        Some("uefi-cd-test") => firmware::cd_test(),
        Some("win11-test") => windows::test(),
        Some("guest-drivers") => {
            drivers::build(env::args().any(|arg| arg == "--trace")).map(|_| ())
        }
        Some("win-display-test") => drivers::display_test(),
        Some("win-perf") => match env::args().nth(2).map(|runs| runs.parse::<usize>()) {
            None => perf::test(3),
            Some(Ok(runs)) if runs > 0 => perf::test(runs),
            Some(_) => Err("win-perf: RUNS is a number of runs, at least 1".into()),
        },
        Some("ubuntu-desktop-test") => ubuntu::test(),
        Some("vmdk-compat-test") => vmdk_compat::test(),
        Some("worker-test") => worker::test(),
        Some("snapshot-test") => snapshot::test(),
        Some("easy-install-test") => easy_install::test(env::args().nth(2).as_deref()),
        Some("vmware-import-test") => vmware_import::test(),
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

/// Where the disk images guests write to go: `VMMC_TEST_IMAGES`, or a folder in the
/// system temp directory. Some volumes (antivirus or sync filters) rescan a whole file
/// whenever it's modified, which turns a guest's first write into a minutes-long stall,
/// so these stay off the workspace's volume by default.
fn image_dir() -> Result<PathBuf, String> {
    let dir = env::var_os("VMMC_TEST_IMAGES")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("vmmc-test-images"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    Ok(dir)
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
    linux_boot_test()?;
    firmware::test()?;
    firmware::linux_test()?;
    firmware::cd_test()
}

/// Boots the test Linux guest headless and checks what it reports: SMP, the disk, USB,
/// a line typed on the PS/2 keyboard, and the network. Then it powers off.
fn linux_boot_test() -> Result<(), String> {
    let workspace = workspace();
    let guest = guests::prepare(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;

    let http = serve_http()?;
    let mut command = vmmc_vmx_command(
        &exe,
        &guest,
        &format!("console=ttyS0 panic=-1 vmmc.http={http} vmmc.usb vmmc.keyboard"),
    );
    command.args(["--nic", "virtio"]);
    eprintln!("$ {command:?}");
    let mut vm = firmware::Session::spawn(command)?;
    let deadline = Instant::now() + LINUX_TIMEOUT;
    let typed = (|| {
        let at = vm.expect("VMMC-TEST: keyboard-ready", 0, LINUX_TIMEOUT)?;
        // Ctrl-A k switches the console to the PS/2 keyboard, and back.
        vm.send(&format!("\x01k{KEYBOARD_LINE}\r\x01k"))?;
        vm.expect("VMMC-TEST: keyboard=", at, LINUX_TIMEOUT)
    })();
    let status = vm.wait(deadline.saturating_duration_since(Instant::now()));
    let output = vm.text(0, vm.len());
    typed?;
    let Some(status) = status else {
        return Err(format!(
            "the Linux guest didn't power off within {}s",
            LINUX_TIMEOUT.as_secs()
        ));
    };

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

/// A web server on the host's loopback that answers every request with [`HTTP_BODY`],
/// for the guest to reach through the NAT. Returns its port.
pub(crate) fn serve_http() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("can't start the test web server: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{HTTP_BODY}",
                HTTP_BODY.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    Ok(port)
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
    Ok(vmmc_vmx_path(workspace, "debug"))
}

/// Builds vmmc-vmx as users run it, for measuring.
fn build_vmmc_vmx_release(workspace: &Path) -> Result<PathBuf, String> {
    cargo(&["build", "--release", "-p", "vmmc-vmx"])?;
    Ok(vmmc_vmx_path(workspace, "release"))
}

fn vmmc_vmx_path(workspace: &Path, profile: &str) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"))
        .join(profile)
        .join(format!("vmmc-vmx{}", env::consts::EXE_SUFFIX))
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
