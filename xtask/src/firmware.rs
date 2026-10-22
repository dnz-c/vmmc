//! Tests that boot the CI-built OVMF on WHP and drive it over the serial console.
//! Current edk2 never boots the UEFI shell by itself (it's an application, not a boot
//! option), so they pick it from the boot manager menu.
//!
//! - **`cargo xtask firmware-test`:** the first boot writes a non-volatile variable in
//!   the shell and warm-resets. vmmc-vmx rebuilds the machine, and the second boot must
//!   read the variable back from `nvram.fd`. Then `reset -s` must power the VM off
//!   through ACPI.
//! - **UEFI Linux ([`linux_test`]):** the shell's `startup.nsh` boots the test Linux
//!   from an EFI system partition on NVMe. Linux must find EFI and the SMBIOS tables,
//!   and do the disk round trip on the second NVMe namespace.
//!
//! The firmware's debug log must not contain an ASSERT.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{build_vmmc_vmx, esp, guests, native, workspace};
use media::iso;

/// From power-on to the shell prompt (a debug build, logging every driver).
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// From the shell starting Linux to Linux powering off.
const LINUX_TIMEOUT: Duration = Duration::from_secs(90);
/// What the test Linux must print when booted through UEFI, in order.
pub(crate) const LINUX_MARKERS: [&str; 9] = [
    "VMMC-TEST: booted",
    "VMMC-TEST: cpus=4",
    "VMMC-TEST: efi=yes",
    "VMMC-TEST: dmi=vmmc Virtual Machine",
    "VMMC-TEST: disk=ok nvme namespace 2",
    "VMMC-TEST: tpm=2 pcr0=measured ppi=1.3",
    "VMMC-TEST: net=ok",
    "VMMC-TEST: dns=ok",
    "VMMC-TEST: done",
];
/// What it must print when booted from CD.
const CD_MARKERS: [&str; 6] = [
    "VMMC-TEST: booted",
    "VMMC-TEST: efi=yes",
    "VMMC-TEST: dmi=vmmc Virtual Machine",
    "VMMC-TEST: cd=ok",
    "VMMC-TEST: disk=ok sda",
    "VMMC-TEST: done",
];
pub(crate) const ESP_SIZE: u64 = 64 << 20;
/// El Torito sizes the boot image in 512-byte sectors, 16 bits' worth: under 32 MiB.
const CD_BOOT_IMAGE_SIZE: u64 = 24 << 20;
const SCRATCH_SIZE: u64 = 64 << 20;
const VARIABLE: &str = "VmmcTest";
const VARIABLE_GUID: &str = "5d2f6e7a-8f1b-4c3e-9a77-766d6d637465";
/// "VMMC", as `setvar` takes it and as it prints it back.
const VALUE_HEX: &str = "564D4D43";
const VALUE_PRINTED: &str = "56 4D 4D 43";
/// What the shell prints when it starts, once per boot.
pub(crate) const SHELL_STARTED: &str = "seconds to skip startup.nsh";
const PROMPT: &str = "Shell>";
pub(crate) const SHELL_OPTION: &str = "EFI Internal Shell";
/// The down-arrow key, as a VT100 terminal sends it.
const DOWN: &str = "\x1b[B";

/// A fresh directory under `target/` for one test's VM state and logs.
fn test_dir(name: &str) -> Result<PathBuf, String> {
    let dir = workspace().join("target").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// vmmc-vmx booting the test firmware with a fresh VM directory in `dir`.
fn firmware_command(dir: &Path, cpus: u8, memory_mib: u32) -> Result<Command, String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.test.fd"))
        .arg("--vars-template")
        .arg(native.join("OVMF_VARS.fd"))
        .arg("--vm-dir")
        .arg(dir.join("vm"))
        .arg("--firmware-log")
        .arg(dir.join("ovmf.log"))
        .arg("--screenshot")
        .arg(dir.join("screens"))
        .args(["--cpus", &cpus.to_string()])
        .args(["--memory", &memory_mib.to_string()]);
    Ok(command)
}

/// Runs `drive` against the VM, then expects it to power off by itself. Checks the
/// firmware log either way.
fn run_session(
    command: Command,
    dir: &Path,
    timeout: Duration,
    drive: impl FnOnce(&mut Session) -> Result<(), String>,
) -> Result<(), String> {
    eprintln!("$ {command:?}");
    let mut vm = Session::spawn(command)?;
    let result = drive(&mut vm);
    let status = match &result {
        Ok(()) => vm.wait(timeout),
        Err(_) => {
            vm.kill();
            None
        }
    };
    check_log(&dir.join("ovmf.log"))?;
    result?;
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("vmmc-vmx exited with {status}")),
        None => Err("the VM didn't power off".into()),
    }
}

pub fn test() -> Result<(), String> {
    let dir = test_dir("firmware-test")?;
    let command = firmware_command(&dir, 2, 512)?;
    run_session(command, &dir, COMMAND_TIMEOUT, drive)?;
    check_screenshot(&dir.join("screens").join("exit.png"))?;
    eprintln!("firmware test passed");
    Ok(())
}

/// The firmware's last screen (the shell, through the GOP on the display adapter) must
/// have a real resolution and show something.
fn check_screenshot(path: &Path) -> Result<(), String> {
    let file =
        fs::File::open(path).map_err(|e| format!("no screenshot at {}: {e}", path.display()))?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut pixels = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut pixels)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if info.width < 640 || info.height < 480 {
        return Err(format!("the screen is only {}x{}", info.width, info.height));
    }
    let lit = pixels[..info.buffer_size()]
        .iter()
        .filter(|&&b| b > 0x40)
        .count();
    if lit < 1000 {
        return Err("the screen is blank".into());
    }
    eprintln!(
        "screenshot: {}x{}, {lit} bright subpixels",
        info.width, info.height
    );
    Ok(())
}

/// The files that boot the test Linux from a FAT file system through the shell.
pub(crate) fn linux_boot_files(
    extra_cmdline: &str,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let guest = guests::prepare(&workspace())?;
    let startup = format!(
        "fs0:\\vmlinuz.efi initrd=\\initramfs.cpio console=ttyS0 panic=-1{extra_cmdline}\r\n"
    );
    Ok(vec![
        ("vmlinuz.efi", guests::read(&guest.kernel)?),
        ("initramfs.cpio", guests::read(&guest.initramfs)?),
        ("startup.nsh", startup.into_bytes()),
    ])
}

/// A blank disk for the guest's round trip, named after the test in `dir`.
fn scratch_disk(dir: &Path) -> Result<PathBuf, String> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("test");
    let scratch = crate::image_dir()?.join(format!("{name}-scratch.vmdk"));
    let _ = fs::remove_file(&scratch);
    disk::create_sparse(&scratch, SCRATCH_SIZE)
        .map_err(|e| format!("can't create {}: {e}", scratch.display()))?;
    Ok(scratch)
}

/// Boots to the shell, which runs startup.nsh, then expects `markers` from Linux.
fn boot_linux(command: Command, dir: &Path, markers: &[&str]) -> Result<(), String> {
    run_session(command, dir, LINUX_TIMEOUT, |vm| {
        // The shell runs startup.nsh once the countdown is skipped.
        let mut at = boot_shell(vm, 0)?;
        for marker in markers {
            at = vm.expect(marker, at, LINUX_TIMEOUT)?;
        }
        Ok(())
    })
}

/// The test Linux from an EFI system partition on NVMe, with the second namespace as
/// its scratch disk, and the network on an e1000e.
pub fn linux_test() -> Result<(), String> {
    let dir = test_dir("uefi-linux-test")?;
    let http = crate::serve_http()?;
    let files = linux_boot_files(&format!(" vmmc.http={http} vmmc.tpm"))?;
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let esp = dir.join("esp.raw");
    esp::build(&esp, ESP_SIZE, &files)?;
    let scratch = scratch_disk(&dir)?;
    let mut command = firmware_command(&dir, 4, 1024)?;
    command
        .arg("--disk")
        .arg(format!("{},nvme", esp.display()))
        .arg("--disk")
        .arg(format!("{},nvme", scratch.display()))
        .args(["--nic", "e1000e"])
        .arg("--tpm");
    boot_linux(command, &dir, &LINUX_MARKERS)?;
    eprintln!("UEFI Linux test passed");
    Ok(())
}

/// The test Linux from a CD: the El Torito EFI image on an ISO in the AHCI DVD drive,
/// with a SATA disk as its scratch disk.
pub fn cd_test() -> Result<(), String> {
    let dir = test_dir("uefi-cd-test")?;
    let files = linux_boot_files(" vmmc.cd")?;
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let boot_image = esp::image(CD_BOOT_IMAGE_SIZE, &files)?;
    let iso_path = dir.join("test.iso");
    let image = iso::build(
        "VMMC_TEST",
        &[("vmmc.txt", b"vmmc cd\n")],
        Some(&boot_image),
    );
    fs::write(&iso_path, image).map_err(|e| format!("can't write {}: {e}", iso_path.display()))?;
    let scratch = scratch_disk(&dir)?;
    let mut command = firmware_command(&dir, 4, 1024)?;
    command
        .arg("--cdrom")
        .arg(&iso_path)
        .arg("--disk")
        .arg(format!("{},sata", scratch.display()));
    boot_linux(command, &dir, &CD_MARKERS)?;
    eprintln!("UEFI CD test passed");
    Ok(())
}

fn drive(vm: &mut Session) -> Result<(), String> {
    // First boot: set the variable, then warm-reset.
    let at = enter_shell(vm, 0)?;
    vm.send(&format!(
        "setvar {VARIABLE} -guid {VARIABLE_GUID} -nv -bs ={VALUE_HEX}\r"
    ))?;
    let at = vm.expect(PROMPT, at, COMMAND_TIMEOUT)?;
    vm.send("reset\r")?;

    // Second boot, on a freshly built machine: the variable must still be there.
    let at = enter_shell(vm, at)?;
    vm.send(&format!("setvar {VARIABLE} -guid {VARIABLE_GUID}\r"))?;
    let at = vm.expect(VALUE_PRINTED, at, COMMAND_TIMEOUT)?;
    vm.expect(PROMPT, at, COMMAND_TIMEOUT)?;
    vm.send("reset -s\r")
}

/// Boots the shell (see [`boot_shell`]) and waits for its prompt. Returns the output
/// offset after it.
fn enter_shell(vm: &mut Session, from: usize) -> Result<usize, String> {
    let at = boot_shell(vm, from)?;
    vm.expect(PROMPT, at, COMMAND_TIMEOUT)
}

/// Waits for the firmware to give up looking for something to boot, picks the shell
/// from the boot manager menu, and skips the startup.nsh countdown (the shell then runs
/// any startup.nsh it finds). Returns the output offset after the countdown.
fn boot_shell(vm: &mut Session, from: usize) -> Result<usize, String> {
    let at = vm.expect(
        "Press any key to enter the Boot Manager Menu",
        from,
        BOOT_TIMEOUT,
    )?;
    vm.send("\r")?;
    let menu = vm.expect("Please select boot device:", at, COMMAND_TIMEOUT)?;
    let end = vm.expect("to move selection", menu, COMMAND_TIMEOUT)?;
    let items = screen_texts(&vm.text(menu, end));
    let index = items
        .iter()
        .position(|item| item == SHELL_OPTION)
        .ok_or_else(|| format!("no {SHELL_OPTION:?} in the boot menu: {items:?}"))?;
    vm.send(&DOWN.repeat(index))?;
    vm.send("\r")?;
    let at = vm.expect(SHELL_STARTED, end, COMMAND_TIMEOUT)?;
    vm.send("\r")?;
    Ok(at)
}

/// The strings a full-screen UEFI menu draws, in order. The session turns each cursor
/// move into a line break, so they're the non-empty lines.
pub(crate) fn screen_texts(screen: &str) -> Vec<String> {
    screen
        .lines()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Turns terminal output into plain text: cursor moves (`ESC [ row ; col H`) become
/// line breaks and every other escape sequence is dropped. Sequences may be split
/// across calls.
#[derive(Default)]
pub(crate) struct PlainText {
    state: Escape,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Escape {
    #[default]
    None,
    /// Just saw ESC.
    Started,
    /// Inside `ESC [`, until the final byte.
    Csi,
}

impl PlainText {
    pub(crate) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (Escape::None, 0x1b) => Escape::Started,
                (Escape::None, _) => {
                    out.push(byte);
                    Escape::None
                }
                (Escape::Started, b'[') => Escape::Csi,
                // A two-byte escape: drop both.
                (Escape::Started, _) => Escape::None,
                (Escape::Csi, b'H') => {
                    out.push(b'\n');
                    Escape::None
                }
                (Escape::Csi, 0x40..=0x7e) => Escape::None,
                (Escape::Csi, _) => Escape::Csi,
            };
        }
        out
    }
}

fn check_log(log: &Path) -> Result<(), String> {
    let text = fs::read(log).map_err(|e| format!("can't read {}: {e}", log.display()))?;
    let text = String::from_utf8_lossy(&text);
    if let Some(line) = text.lines().find(|line| line.contains("ASSERT")) {
        return Err(format!("the firmware asserted: {line}"));
    }
    if !text.contains("InstallQemuFwCfgTables: installed") {
        return Err("the firmware didn't install the ACPI tables from fw_cfg".into());
    }
    Ok(())
}

/// vmmc-vmx with its serial console on pipes. Output is echoed and collected.
pub(crate) struct Session {
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<String>>,
    /// Set once vmmc-vmx's output has ended, which is when it exits.
    ended: Arc<AtomicBool>,
}

impl Session {
    pub(crate) fn spawn(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't start vmmc-vmx: {e}"))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let output = Arc::new(Mutex::new(String::new()));
        let collected = output.clone();
        let ended = Arc::new(AtomicBool::new(false));
        let ending = ended.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            let mut plain = PlainText::default();
            // A character a read cut in two, until the rest comes.
            let mut partial = Vec::new();
            while let Ok(read @ 1..) = stdout.read(&mut chunk) {
                // Flushed, so prompts without a newline show up in the log too.
                let mut echo = std::io::stdout().lock();
                let _ = echo.write_all(&chunk[..read]).and_then(|()| echo.flush());
                drop(echo);
                partial.extend(plain.filter(&chunk[..read]));
                let whole = match std::str::from_utf8(&partial) {
                    Ok(_) => partial.len(),
                    // Cut short at the end: the rest waits for the next read.
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => partial.len(),
                };
                collected
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&partial[..whole]));
                partial.drain(..whole);
            }
            ending.store(true, Ordering::Release);
        });
        Ok(Self {
            child,
            stdin,
            output,
            ended,
        })
    }

    /// Waits for `text` to appear at or after byte `from` of the output. Returns the
    /// offset just past it.
    pub(crate) fn expect(
        &self,
        text: &str,
        from: usize,
        timeout: Duration,
    ) -> Result<usize, String> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let output = self.output.lock().unwrap();
                if let Some(at) = output.get(from..).and_then(|rest| rest.find(text)) {
                    return Ok(from + at + text.len());
                }
            }
            // Checked after the search, so output that came just before the end counts.
            if self.ended.load(Ordering::Acquire) {
                return Err(format!("vmmc-vmx exited before the guest printed {text:?}"));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "the guest didn't print {text:?} within {}s",
                    timeout.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// vmmc-vmx's process ID.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// How much output there is so far, as an offset for [`Session::text`].
    pub(crate) fn len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// The output between two offsets `expect` returned.
    pub(crate) fn text(&self, from: usize, to: usize) -> String {
        self.output.lock().unwrap()[from..to].to_owned()
    }

    /// Types `text` on the serial console.
    pub(crate) fn send(&mut self, text: &str) -> Result<(), String> {
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("can't write to the VM's console: {e}"))
    }

    /// Has vmmc-vmx print every vCPU's registers to its stderr (Ctrl-A r), to show
    /// where a hung guest is stuck, and gives it a moment to.
    pub(crate) fn dump_state(&mut self) {
        if self.send("r").is_ok() {
            thread::sleep(Duration::from_secs(2));
        }
    }

    /// How vmmc-vmx exited, if it has; never waits.
    pub(crate) fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Waits up to `timeout` for vmmc-vmx to exit, and kills it if it doesn't.
    pub(crate) fn wait(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(100));
        }
        self.kill();
        None
    }

    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_items_come_out_in_drawing_order() {
        // As OVMF's boot manager draws its list, then the help line, in two reads.
        let mut plain = PlainText::default();
        let mut screen = plain.filter(b"\x1b[027;104HEFI Firmware Setup\x1b[028;10");
        screen.extend(plain.filter(b"4HEFI Internal Shell\x1b[0m\x1b[37m\x1b[030;108H? and ? "));
        assert_eq!(
            screen_texts(&String::from_utf8(screen).unwrap()),
            ["EFI Firmware Setup", "EFI Internal Shell", "? and ?"]
        );
    }

    #[test]
    fn highlighted_words_read_as_plain_text() {
        let mut plain = PlainText::default();
        let text = plain.filter(b"Press \x1b[1m\x1b[37m\x1b[40mESC\x1b[0m in 5 seconds");
        assert_eq!(text, b"Press ESC in 5 seconds");
    }
}
