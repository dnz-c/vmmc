//! `cargo xtask win11-test`: installs Windows 11 unattended from your own ISO, onto a
//! 64 GiB NVMe disk, and checks what the installed system reports.
//!
//! - **Inputs:** `VMMC_WIN11_ISO` names the ISO. `VMMC_WIN11_IMAGE` picks the edition
//!   (default "Windows 11 Pro", installed with Microsoft's public generic key, which
//!   installs but doesn't activate).
//! - **Answer disc:** a second ISO with `autounattend.xml` and a first-logon script that
//!   writes `VMMC-WIN:` markers to COM1 and shuts down.
//! - **Progress:** a screenshot every 30 s in `target/win11-test/screens`.
//!
//! It takes 20 to 60 minutes.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::firmware::Session;
use crate::{build_vmmc_vmx, iso, native, workspace};

const ISO_VAR: &str = "VMMC_WIN11_ISO";
const IMAGE_VAR: &str = "VMMC_WIN11_IMAGE";
const DEFAULT_IMAGE: &str = "Windows 11 Pro";
/// Microsoft's published generic installation keys: they install, never activate.
const GENERIC_KEYS: [(&str, &str); 3] = [
    ("Windows 11 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
    ("Windows 11 Home", "YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
    ("Windows 11 Enterprise", "XGVPP-NMH47-7TTHJ-W3FW7-8HV2C"),
];
const AUTOUNATTEND: &str = include_str!("../guest/autounattend.xml");
const FIRST_LOGON: &str = include_str!("../guest/firstlogon.ps1");
/// Checks Windows Setup skips while vmmc has no TPM or enrolled Secure Boot keys.
const BYPASSES: [&str; 3] = ["BypassTPMCheck", "BypassSecureBootCheck", "BypassCPUCheck"];
const DISK_SIZE: u64 = 64 << 30;
const MEMORY_MIB: &str = "8192";
const CPUS: &str = "4";
const CD_PROMPT: &str = "Press any key to boot from CD or DVD";
const CD_PROMPT_TIMEOUT: Duration = Duration::from_secs(180);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(90 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);
/// What the first-logon script must report, in order.
const MARKERS: [&str; 5] = [
    "VMMC-WIN: booted",
    "VMMC-WIN: cpus=4",
    "VMMC-WIN: secureboot=",
    "VMMC-WIN: tpm=",
    "VMMC-WIN: done",
];

/// The answer file for `image`, with the check bypasses while vmmc needs them.
fn answer_file(image: &str, key: &str) -> String {
    let commands: String = BYPASSES
        .iter()
        .enumerate()
        .map(|(i, value)| {
            format!(
                "        <RunSynchronousCommand wcm:action=\"add\">\n          \
                 <Order>{}</Order>\n          \
                 <Path>reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {value} /t REG_DWORD /d 1 /f</Path>\n        \
                 </RunSynchronousCommand>\n",
                i + 1
            )
        })
        .collect();
    let labconfig = format!("      <RunSynchronous>\n{commands}      </RunSynchronous>");
    AUTOUNATTEND
        .replace("\r\n", "\n")
        .replace("@@LABCONFIG@@", &labconfig)
        .replace("@@IMAGE@@", image)
        .replace("@@KEY@@", key)
}

pub fn test() -> Result<(), String> {
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
    let dir = workspace.join("target").join("win11-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;

    let answer = dir.join("answer.iso");
    let answer_xml = answer_file(&image, key);
    let script = FIRST_LOGON.replace("\r\n", "\n").replace('\n', "\r\n");
    let disc = iso::build(
        "VMMC_ANSWER",
        &[
            ("autounattend.xml", answer_xml.as_bytes()),
            ("vmmc-firstlogon.ps1", script.as_bytes()),
        ],
        None,
    );
    fs::write(&answer, disc).map_err(|e| format!("can't write {}: {e}", answer.display()))?;
    let disk = crate::image_dir()?.join("win11-test-disk.raw");
    let _ = fs::remove_file(&disk);
    disk::create_raw(&disk, DISK_SIZE)
        .map_err(|e| format!("can't create {}: {e}", disk.display()))?;

    let mut command = Command::new(&exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.fd"))
        .arg("--vars-template")
        .arg(native.join("OVMF_VARS.fd"))
        .arg("--vm-dir")
        .arg(dir.join("vm"))
        .arg("--cdrom")
        .arg(&windows_iso)
        .arg("--cdrom")
        .arg(&answer)
        .arg("--disk")
        .arg(format!("{},nvme", disk.display()))
        .args(["--cpus", CPUS, "--memory", MEMORY_MIB])
        .arg("--screenshot")
        .arg(dir.join("screens"))
        .args(["--screenshot-every", "30"]);
    eprintln!("$ {command:?}");
    eprintln!(
        "installing {image}; progress screenshots in {}",
        dir.join("screens").display()
    );
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
                0
            }
        };
        let mut at = at;
        for marker in MARKERS {
            at = vm.expect(marker, at, INSTALL_TIMEOUT)?;
        }
        Ok::<usize, String>(at)
    })();
    // Whatever happened, show what Windows reported.
    let reported = vm.text(0, vm.len());
    for line in reported.lines().filter(|line| line.contains("VMMC-WIN:")) {
        eprintln!("{}", line.trim());
    }
    let status = match &result {
        Ok(_) => vm.wait(SHUTDOWN_TIMEOUT),
        Err(_) => {
            vm.kill();
            None
        }
    };
    result?;
    match status {
        Some(status) if status.success() => {
            eprintln!("Windows 11 install test passed");
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
    fn answer_file_fills_every_placeholder() {
        let xml = answer_file("Windows 11 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T");
        assert!(!xml.contains("@@"));
        assert!(xml.contains("<Value>Windows 11 Pro</Value>"));
        assert!(xml.contains("BypassTPMCheck"));
        assert_eq!(
            xml.matches("<RunSynchronousCommand").count(),
            4,
            "3 bypasses + BypassNRO"
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
