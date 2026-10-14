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
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::{build_vmmc_vmx, iso, native, workspace};

const ISO_VAR: &str = "VMMC_WIN11_ISO";
const IMAGE_VAR: &str = "VMMC_WIN11_IMAGE";
const LANGUAGE_VAR: &str = "VMMC_WIN11_LANG";
const ENLIGHTENMENTS_VAR: &str = "VMMC_ENLIGHTENMENTS";
/// How `sources/lang.ini` on a Windows ISO starts.
const LANG_INI: &[u8] = b"[Available UI Languages]";
const DEFAULT_IMAGE: &str = "Windows 11 Pro";
/// Microsoft's published generic installation keys: they install, never activate.
/// Windows 10 and 11 share them.
const GENERIC_KEYS: [(&str, &str); 6] = [
    ("Windows 11 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
    ("Windows 11 Home", "YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
    ("Windows 11 Enterprise", "XGVPP-NMH47-7TTHJ-W3FW7-8HV2C"),
    ("Windows 10 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
    ("Windows 10 Home", "YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
    ("Windows 10 Enterprise", "XGVPP-NMH47-7TTHJ-W3FW7-8HV2C"),
];
const AUTOUNATTEND: &str = include_str!("../guest/autounattend.xml");
const FIRST_LOGON: &str = include_str!("../guest/firstlogon.ps1");
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
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let exe = build_vmmc_vmx(&workspace)?;
    let dir = workspace.join("target").join(name);
    let disk = crate::image_dir()?.join(format!("{name}-disk.raw"));
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
        .arg(log);
    eprintln!("$ {command:?}");
    Session::spawn(command)
}

/// `text` as XML character data.
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `text` with Windows line ends, whatever the checkout has.
pub(crate) fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The first language `lang.ini` lists, as a tag like `de-DE`.
fn parse_lang_ini(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    if lines.next()? != "[Available UI Languages]" {
        return None;
    }
    let tag = lines
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (tag, index) = line.split_once('=')?;
            index.trim().parse::<u32>().ok()?;
            Some(tag.trim())
        })
        .filter(|tag| !tag.is_empty())?;
    Some(match tag.split_once('-') {
        Some((language, region)) if region.len() == 2 => {
            format!("{}-{}", language.to_lowercase(), region.to_uppercase())
        }
        _ => tag.to_owned(),
    })
}

/// The language of a Windows ISO's Setup, from its `sources/lang.ini`. The ISO's files
/// are on a UDF file system, and a file that small sits inside its file entry, so
/// rather than read UDF, look for the file's contents.
fn iso_language(iso: &Path) -> Result<String, String> {
    let mut file = File::open(iso).map_err(|e| format!("can't open {}: {e}", iso.display()))?;
    // Each read keeps the end of the last one, so a match can straddle two reads.
    const KEEP: usize = 512;
    let mut buffer = vec![0u8; KEEP + (8 << 20)];
    let mut kept = 0;
    loop {
        let read = file
            .read(&mut buffer[kept..])
            .map_err(|e| format!("can't read {}: {e}", iso.display()))?;
        if read == 0 {
            return Err(format!(
                "{}: no sources/lang.ini found; set {LANGUAGE_VAR} to Setup's language",
                iso.display()
            ));
        }
        let filled = kept + read;
        let mut from = 0;
        while let Some(at) = buffer[from..filled]
            .windows(LANG_INI.len())
            .position(|window| window == LANG_INI)
        {
            let start = from + at;
            let text = String::from_utf8_lossy(&buffer[start..filled.min(start + KEEP)]);
            if let Some(language) = parse_lang_ini(&text) {
                return Ok(language);
            }
            from = start + 1;
        }
        kept = KEEP.min(filled);
        buffer.copy_within(filled - kept..filled, 0);
    }
}

/// The answer file for `image` in `language`, with `specialize` commands (description,
/// command line) added to that pass. Windows 11's hardware checks run as they would on
/// a PC: vmmc has the TPM 2.0 and Secure Boot they ask for.
fn answer_file(image: &str, key: &str, language: &str, specialize: &[(&str, &str)]) -> String {
    // After BypassNRO, which is command 1.
    let commands: String = specialize
        .iter()
        .enumerate()
        .map(|(index, (description, command))| {
            format!(
                "        <RunSynchronousCommand wcm:action=\"add\">\n          <Order>{}</Order>\n          <Description>{}</Description>\n          <Path>{}</Path>\n        </RunSynchronousCommand>\n",
                index + 2,
                xml_text(description),
                xml_text(command)
            )
        })
        .collect();
    AUTOUNATTEND
        .replace("\r\n", "\n")
        .replace("@@IMAGE@@", image)
        .replace("@@KEY@@", key)
        .replace("@@LANGUAGE@@", language)
        .replace("@@SPECIALIZE@@", &commands)
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
    let key = GENERIC_KEYS
        .iter()
        .find(|(name, _)| *name == image)
        .map(|(_, key)| *key)
        .ok_or_else(|| {
            format!("{IMAGE_VAR}: no generic key for {image:?}; use one of {GENERIC_KEYS:?}")
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
        Err(_) => iso_language(&windows_iso)?,
    };
    eprintln!("xtask: Setup language {language}");
    let answer_xml = answer_file(&image, key, &language, install.specialize);
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
    let disk = crate::image_dir()?.join(format!("{}-disk.raw", install.name));
    let _ = fs::remove_file(&disk);
    disk::create_raw(&disk, DISK_SIZE)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_language_from_lang_ini() {
        let ini = "[Available UI Languages]\r\nde-de = 3\r\n\r\n[Fallback Languages]\r\nde-de = en-us\r\n";
        assert_eq!(parse_lang_ini(ini).as_deref(), Some("de-DE"));
        assert_eq!(
            parse_lang_ini("[Available UI Languages]\nsr-latn-rs = 1\n").as_deref(),
            Some("sr-latn-rs")
        );
        assert_eq!(
            parse_lang_ini("[Available UI Languages]\n[Other]\nx = 1"),
            None
        );
    }

    #[test]
    fn specialize_commands_follow_bypass_nro() {
        let xml = answer_file(
            "Windows 10 Pro",
            "key",
            "en-US",
            &[("Test signing", "bcdedit /set testsigning on")],
        );
        assert_eq!(xml.matches("<RunSynchronousCommand").count(), 2);
        let at = xml
            .find("<Order>2</Order>\n          <Description>Test signing</Description>\n          <Path>bcdedit /set testsigning on</Path>")
            .expect("the second command");
        assert!(xml.find("BypassNRO").unwrap() < at);
        let xml = answer_file("Windows 10 Pro", "key", "en-US", &[("a & b", "x & y > z")]);
        assert!(xml.contains("<Description>a &amp; b</Description>"));
        assert!(xml.contains("<Path>x &amp; y &gt; z</Path>"));
    }

    #[test]
    fn answer_file_fills_every_placeholder() {
        let xml = answer_file(
            "Windows 11 Pro",
            "VK7JG-NPHTM-C97JM-9MPGT-3V66T",
            "de-DE",
            &[],
        );
        assert_eq!(xml.matches("de-DE").count(), 9);
        assert!(!xml.contains("@@"));
        assert!(xml.contains("<Value>Windows 11 Pro</Value>"));
        assert!(!xml.contains("LabConfig"), "no hardware check bypasses");
        assert_eq!(
            xml.matches("<RunSynchronousCommand").count(),
            1,
            "just BypassNRO"
        );
        // Well-formed enough: every opened element is closed.
        for tag in [
            "settings",
            "component",
            "RunSynchronous",
            "DiskConfiguration",
        ] {
            assert_eq!(
                xml.matches(&format!("<{tag}>")).count() + xml.matches(&format!("<{tag} ")).count(),
                xml.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
    }
}
