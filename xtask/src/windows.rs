//! `cargo xtask win11-test`: installs Windows 11 unattended from your own ISO, onto a
//! 64 GiB NVMe disk, and checks what the installed system reports.
//!
//! - **Inputs:** `VMMC_WIN11_ISO` names the ISO. `VMMC_WIN11_IMAGE` picks the edition
//!   (default "Windows 11 Pro", installed with Microsoft's public generic key, which
//!   installs but doesn't activate). Setup runs in the ISO's language, read from its
//!   `sources/lang.ini`, or the one `VMMC_WIN11_LANG` names (such as `en-US`).
//! - **Answer disc:** a second ISO with `autounattend.xml` and a first-logon script that
//!   writes `VMMC-WIN:` markers to COM1 and shuts down.
//! - **Progress:** a screenshot every 30 s in `target/win11-test/screens`.
//! - **Enlightenments:** `VMMC_ENLIGHTENMENTS=0` hides the Hyper-V enlightenments
//!   (`--no-enlightenments`), for comparing.
//!
//! It takes 20 to 60 minutes.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::{build_vmmc_vmx, native, workspace};
use media::iso;

const ISO_VAR: &str = "VMMC_WIN11_ISO";
const IMAGE_VAR: &str = "VMMC_WIN11_IMAGE";
const LANGUAGE_VAR: &str = "VMMC_WIN11_LANG";
const ENLIGHTENMENTS_VAR: &str = "VMMC_ENLIGHTENMENTS";
const DEFAULT_IMAGE: &str = "Windows 11 Pro";
const FIRST_LOGON: &str = include_str!("../guest/firstlogon.ps1");
/// Runs the first-logon script from the answer disc, whichever letter it got.
const FIRST_LOGON_COMMAND: &str = r"cmd /c for %d in (D E F G H I J) do @if exist %d:\vmmc-firstlogon.ps1 powershell -NoProfile -ExecutionPolicy Bypass -File %d:\vmmc-firstlogon.ps1";
const BOOT_REPORT: &str = include_str!("../guest/boot-report.ps1");
const DISK_SIZE: u64 = 64 << 30;
pub(crate) const MEMORY_MIB: &str = "8192";
pub(crate) const CPUS: &str = "4";
const CD_PROMPT: &str = "Press any key to boot from CD or DVD";
const CD_PROMPT_TIMEOUT: Duration = Duration::from_secs(180);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(90 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);
/// What the first-logon script must report, in order.
const MARKERS: [&str; 10] = [
    "VMMC-WIN: booted",
    "VMMC-WIN: cpus=4",
    "VMMC-WIN: secureboot=True",
    "VMMC-WIN: tpm=True",
    "VMMC-WIN: usb-tablet=True",
    "VMMC-WIN: screen=1920x1080",
    KEYBOARD_READY,
    "VMMC-WIN: keyboard=vmmc ps2",
    "VMMC-WIN: net=ok",
    "VMMC-WIN: done",
];
/// The first-logon script waits for a line on the keyboard after this.
const KEYBOARD_READY: &str = "VMMC-WIN: keyboard-ready";
/// The resolution the display's EDID asks for, and Windows should end up in.
pub(crate) const RESOLUTION: &str = "1920x1080";

/// Boots the Windows a test named `name` installed, from its disk alone, with the
/// debug console saved to `log`.
pub(crate) fn boot_installed(name: &str, log: &Path) -> Result<Session, String> {
    boot_installed_with(name, log, &[])
}

/// [`boot_installed`], with more of vmmc-vmx's options.
pub(crate) fn boot_installed_with(
    name: &str,
    log: &Path,
    extra: &[std::ffi::OsString],
) -> Result<Session, String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join(name);
    let disk = crate::image_dir()?.join(format!("{name}-disk.vmdk"));
    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.fd"))
        .arg("--tpm")
        .arg("--vm-dir")
        .arg(dir.join("vm"))
        .arg("--disk")
        .arg(format!("{},nvme", disk.display()))
        .args(["--cpus", CPUS, "--memory", MEMORY_MIB])
        .args(["--nic", "e1000e"])
        .args(["--resolution", RESOLUTION])
        .arg("--screenshot")
        .arg(dir.join("screens"))
        .arg("--firmware-log")
        .arg(log)
        .args(extra);
    eprintln!("$ {command:?}");
    Session::spawn(command)
}

/// `text` with Windows line ends, whatever the checkout has.
pub(crate) fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

pub fn test() -> Result<(), String> {
    // The guest fetches this through the NAT's gateway.
    let http = crate::serve_http()?;
    let first_logon = crlf(
        &FIRST_LOGON
            .replace("@@HTTP_PORT@@", &http.to_string())
            .replace("@@HTTP_BODY@@", crate::HTTP_BODY),
    );
    install(
        &Install {
            name: "win11-test",
            vars: "OVMF_VARS.secboot.fd",
            first_logon,
            files: vec![(
                "vmmc-boot-report.ps1".into(),
                crlf(BOOT_REPORT).into_bytes(),
            )],
            specialize: &[],
            markers: &MARKERS,
        },
        |marker, vm, _| {
            if marker == KEYBOARD_READY {
                // Typed on the PS/2 keyboard: Ctrl-A k switches the console to it,
                // and back.
                vm.send("\x01kvmmc ps2\r\x01k")?;
            }
            Ok(())
        },
    )
}

/// One unattended install, and what its first logon must report.
pub(crate) struct Install<'a> {
    /// Names its directory in `target` and its disk image.
    pub name: &'a str,
    /// The variable store the VM starts from, in `target/native`: Secure Boot on or off.
    pub vars: &'a str,
    /// The first-logon script, with Windows line ends.
    pub first_logon: String,
    /// More files for the answer disc, all in its root.
    pub files: Vec<(String, Vec<u8>)>,
    /// Commands for Setup's specialize pass, after the installed system's first boot:
    /// (description, command line).
    pub specialize: &'a [(&'a str, &'a str)],
    /// What the first-logon script reports, in order. The first follows logon.
    pub markers: &'a [&'a str],
}

/// Installs Windows from `VMMC_WIN11_ISO` onto a new disk as `install` says, then
/// expects its markers. `act` runs as each marker is seen, given the VM and its
/// directory. Passes once Windows powers off.
pub(crate) fn install(
    install: &Install,
    mut act: impl FnMut(&str, &mut Session, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let windows_iso = PathBuf::from(env::var_os(ISO_VAR).ok_or_else(|| {
        format!("set {ISO_VAR} to the path of a Windows 11 ISO (x64, any recent release)")
    })?);
    if !windows_iso.is_file() {
        return Err(format!("{ISO_VAR}: no file at {}", windows_iso.display()));
    }
    let image = env::var(IMAGE_VAR).unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    let key = media::windows::generic_key(&image).ok_or_else(|| {
        let editions: Vec<_> = media::windows::EDITIONS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        format!("{IMAGE_VAR}: no generic key for {image:?}; use one of {editions:?}")
    })?;

    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join(install.name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;

    let answer = dir.join("answer.iso");
    let language = match env::var(LANGUAGE_VAR) {
        Ok(language) => language,
        Err(_) => media::windows::iso_language(&windows_iso)
            .map_err(|e| format!("{e}; set {LANGUAGE_VAR} to Setup's language"))?,
    };
    eprintln!("xtask: Setup language {language}");
    let answer_xml = media::windows::answer_file(&media::windows::Setup {
        image: &image,
        product_key: key,
        language: &language,
        computer_name: "VMMC-TEST",
        time_zone: "UTC",
        user: "vmmc",
        password: "vmmc",
        auto_logons: 1,
        specialize: install.specialize,
        first_logon: &[("Report to the host and shut down", FIRST_LOGON_COMMAND)],
    });
    let mut files: Vec<(&str, &[u8])> = vec![
        ("autounattend.xml", answer_xml.as_bytes()),
        ("vmmc-firstlogon.ps1", install.first_logon.as_bytes()),
    ];
    files.extend(
        install
            .files
            .iter()
            .map(|(name, data)| (name.as_str(), data.as_slice())),
    );
    let disc = iso::build("VMMC_ANSWER", &files, None);
    fs::write(&answer, disc).map_err(|e| format!("can't write {}: {e}", answer.display()))?;
    let disk = crate::image_dir()?.join(format!("{}-disk.vmdk", install.name));
    let _ = fs::remove_file(&disk);
    disk::create_sparse(&disk, DISK_SIZE)
        .map_err(|e| format!("can't create {}: {e}", disk.display()))?;

    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.fd"))
        .arg("--vars-template")
        .arg(native.join(install.vars))
        .arg("--tpm")
        .arg("--vm-dir")
        .arg(dir.join("vm"))
        .arg("--cdrom")
        .arg(&windows_iso)
        .arg("--cdrom")
        .arg(&answer)
        .arg("--disk")
        .arg(format!("{},nvme", disk.display()))
        .args(["--cpus", CPUS, "--memory", MEMORY_MIB])
        .args(["--nic", "e1000e"])
        .args(["--resolution", RESOLUTION])
        .arg("--screenshot")
        .arg(dir.join("screens"))
        .args(["--screenshot-every", "30"])
        // The firmware's debug output, and anything else on the debug console.
        .arg("--firmware-log")
        .arg(dir.join("debug.log"));
    if env::var(ENLIGHTENMENTS_VAR).is_ok_and(|value| value == "0") {
        command.arg("--no-enlightenments");
    }
    eprintln!("$ {command:?}");
    eprintln!(
        "installing {image}; progress screenshots in {}",
        dir.join("screens").display()
    );
    let started = Instant::now();
    let mut vm = Session::spawn(command)?;

    let result = (|| {
        // The disc's boot loader waits a few seconds for a key, then gives up.
        let at = match vm.expect(CD_PROMPT, 0, CD_PROMPT_TIMEOUT) {
            Ok(at) => {
                vm.send("\r")?;
                at
            }
            Err(_) => {
                eprintln!("xtask: no \"{CD_PROMPT}\" prompt seen; carrying on");
                // Twice, so a guest that's spinning shows whether it moves.
                vm.dump_state();
                vm.dump_state();
                0
            }
        };
        let mut at = at;
        for (index, marker) in install.markers.iter().enumerate() {
            at = vm.expect(marker, at, INSTALL_TIMEOUT)?;
            if index == 0 {
                eprintln!(
                    "xtask: installed and logged on after {}s",
                    started.elapsed().as_secs()
                );
            }
            act(marker, &mut vm, &dir)?;
        }
        Ok::<usize, String>(at)
    })();
    // Whatever happened, show what Windows reported: the lines with the markers' prefix.
    let prefix = install.markers[0]
        .split_once(':')
        .map_or(install.markers[0], |(prefix, _)| prefix);
    let reported = vm.text(0, vm.len());
    for line in reported.lines().filter(|line| line.contains(prefix)) {
        eprintln!("{}", line.trim());
    }
    let status = match &result {
        Ok(_) => vm.wait(SHUTDOWN_TIMEOUT),
        Err(_) => {
            vm.dump_state();
            vm.kill();
            None
        }
    };
    result?;
    match status {
        Some(status) if status.success() => {
            eprintln!(
                "{} passed in {}s",
                install.name,
                started.elapsed().as_secs()
            );
            Ok(())
        }
        Some(status) => Err(format!("vmmc-vmx exited with {status}")),
        None => Err("Windows didn't power off after the first-logon script".into()),
    }
}
