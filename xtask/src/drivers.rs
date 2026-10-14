//! vmmc's Windows guest drivers (`guest-drivers/windows`), and their test.
//!
//! - `cargo xtask guest-drivers` builds them into `target/guest-drivers/windows` with
//!   MSVC (from Visual Studio or its Build Tools) and the Windows Driver Kit's headers,
//!   libraries and tools, called directly rather than through MSBuild, whose WDK
//!   integration depends on the Visual Studio version. It signs them with a test
//!   certificate it makes once in your certificate store ("vmmc test driver signing").
//!   Windows loads test-signed drivers only with test signing on, which Secure Boot
//!   rules out: until Microsoft signs them, they're for VMs with Secure Boot off.
//! - `cargo xtask win-display-test` installs Windows as `win11-test` does, but with
//!   Secure Boot off and test signing on, installs the display driver at first logon,
//!   switches to 1600x900 and checks that the screen followed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::firmware::Session;
use crate::windows::{self, Install};
use crate::{run, workspace};

const CERTIFICATE_SUBJECT: &str = "CN=vmmc test driver signing";
const KITS: &str = r"C:\Program Files (x86)\Windows Kits\10";
const DISPLAY_TEST: &str = include_str!("../guest/display-test.ps1");
/// What the display test's first logon reports, in order.
const DISPLAY_MARKERS: [&str; 6] = [
    "VMMC-DISP: booted",
    "VMMC-DISP: testsigning=True",
    "VMMC-DISP: driver=vmmcdisp status=OK",
    "VMMC-DISP: set=0 now=1600x900",
    "VMMC-DISP: mode-set",
    "VMMC-DISP: done",
];
/// The resolution the test switches to.
const TEST_MODE: (u32, u32) = (1600, 900);
/// The version stamped into the INF.
const DRIVER_VERSION: &str = "0.1.0.0";

/// A built and signed driver package, and the certificate that signed it.
pub struct Drivers {
    /// The display driver's package: `vmmcdisp.inf`, `.sys` and `.cat`.
    pub display: PathBuf,
    pub certificate: PathBuf,
}

/// The tools a build uses.
struct Toolchain {
    /// MSVC's `bin\Hostx64\x64`, with cl.exe and link.exe.
    msvc: PathBuf,
    /// The WDK's version directory name, such as `10.0.28000.0`.
    kit: String,
}

impl Toolchain {
    fn find() -> Result<Self, String> {
        // The newest MSVC of any Visual Studio or Build Tools.
        let vswhere =
            Path::new(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
        let output = Command::new(vswhere)
            .args([
                "-all",
                "-products",
                "*",
                "-find",
                r"VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe",
            ])
            .output()
            .map_err(|e| format!("can't run {}: {e}", vswhere.display()))?;
        let mut compilers: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| PathBuf::from(line.trim()))
            .filter(|path| path.is_file())
            .collect();
        compilers.sort_by_key(|path| msvc_version(path));
        let msvc = compilers
            .pop()
            .and_then(|cl| cl.parent().map(Path::to_owned))
            .ok_or("no MSVC: install Visual Studio or its Build Tools with the C++ workload")?;
        // The newest Windows Driver Kit: one whose headers have the display driver
        // interface.
        let include = Path::new(KITS).join("Include");
        let mut kits: Vec<String> = fs::read_dir(&include)
            .map_err(|e| format!("no Windows Kits in {}: {e}", include.display()))?
            .flatten()
            .filter(|entry| entry.path().join(r"km\dispmprt.h").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        kits.sort_by_key(|version| {
            version
                .split('.')
                .map(|part| part.parse::<u32>().unwrap_or(0))
                .collect::<Vec<_>>()
        });
        let kit = kits.pop().ok_or("no Windows Driver Kit: install the WDK")?;
        Ok(Self { msvc, kit })
    }

    fn kits(&self, area: &str) -> PathBuf {
        Path::new(KITS).join(area).join(&self.kit)
    }

    fn tool(&self, arch: &str, name: &str) -> PathBuf {
        self.kits("bin").join(arch).join(name)
    }
}

/// Today in UTC as `MM/DD/YYYY`, for the INF's DriverVer: Inf2Cat refuses a date
/// that's still in the future in UTC, which the local date can be.
fn utc_date() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{month:02}/{day:02}/{year}")
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// MSVC's version from a path like `...\MSVC\14.51.36231\bin\...`, for sorting.
fn msvc_version(cl: &Path) -> Vec<u32> {
    cl.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .skip_while(|part| part != "MSVC")
        .nth(1)
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Builds the drivers; `trace` makes them trace to the debug console (I/O port 0x402).
pub fn build(trace: bool) -> Result<Drivers, String> {
    let workspace = workspace();
    let tools = Toolchain::find()?;
    let source = workspace
        .join("guest-drivers")
        .join("windows")
        .join("display");
    let out = workspace.join("target").join("guest-drivers");
    let obj = out.join("obj").join("display");
    let display = out.join("windows").join("vmmcdisp");
    for dir in [&obj, &display] {
        fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let include = tools.kits("Include");
    let object = obj.join("vmmcdisp.obj");
    run(Command::new(tools.msvc.join("cl.exe"))
        .args([
            "/nologo", "/c", "/kernel", "/O2", "/GS", "/Gy", "/GR-", "/Zi", "/W4", "/WX",
        ])
        .args(["/std:c++17", "/Zc:wchar_t", "/Zc:inline", "/Zp8"])
        // Nameless unions and bit fields of enums, all over the WDK's headers.
        .args(["/wd4201", "/wd4214"])
        .args(["/D_AMD64_", "/DAMD64", "/D_WIN64"])
        // Windows 10 2004 (ExAllocatePool2), and the display-only driver interface as
        // Windows 8 has it: all this driver needs.
        .args([
            "/D_WIN32_WINNT=0x0A00",
            "/DWINVER=0x0A00",
            "/DNTDDI_VERSION=0x0A000008",
        ])
        .arg("/DDXGKDDI_INTERFACE_VERSION=DXGKDDI_INTERFACE_VERSION_WIN8")
        .args(trace.then_some("/DVMMC_TRACE"))
        .arg(format!("/I{}", include.join(r"km\crt").display()))
        .arg(format!("/I{}", include.join("km").display()))
        .arg(format!("/I{}", include.join("shared").display()))
        .arg(format!("/Fd{}", obj.join("vmmcdisp.pdb").display()))
        .arg(format!("/Fo{}", object.display()))
        .arg(source.join("vmmcdisp.cpp")))?;
    let libs = tools.kits("Lib").join(r"km\x64");
    run(Command::new(tools.msvc.join("link.exe"))
        .args([
            "/nologo",
            "/DRIVER",
            "/SUBSYSTEM:NATIVE,10.00",
            "/ENTRY:GsDriverEntry",
        ])
        .args([
            "/NODEFAULTLIB",
            "/MACHINE:X64",
            "/RELEASE",
            "/OPT:REF",
            "/OPT:ICF",
            "/DEBUG",
        ])
        .args(["/MERGE:_TEXT=.text", "/MERGE:_PAGE=PAGE", "/SECTION:INIT,d"])
        .arg(format!("/PDB:{}", obj.join("vmmcdisp.sys.pdb").display()))
        .arg(format!("/OUT:{}", display.join("vmmcdisp.sys").display()))
        .arg(&object)
        .args([
            "ntoskrnl.lib",
            "hal.lib",
            "displib.lib",
            "BufferOverflowFastFailK.lib",
        ])
        .arg(format!("/LIBPATH:{}", libs.display())))?;
    let inf = display.join("vmmcdisp.inf");
    fs::copy(source.join("vmmcdisp.inf"), &inf).map_err(|e| format!("can't copy the INF: {e}"))?;
    run(Command::new(tools.tool("x64", "stampinf.exe"))
        .arg("-f")
        .arg(&inf)
        .args(["-d", &utc_date(), "-a", "amd64", "-v", DRIVER_VERSION]))?;
    let _ = fs::remove_file(display.join("vmmcdisp.cat"));
    run(Command::new(tools.tool("x86", "Inf2Cat.exe"))
        .arg(format!("/driver:{}", display.display()))
        .arg("/os:10_X64"))?;

    let certificate = out.join("vmmc-test-signing.cer");
    let thumbprint = test_certificate(&certificate)?;
    run(Command::new(tools.tool("x64", "signtool.exe"))
        .args(["sign", "/fd", "sha256", "/s", "My", "/sha1", &thumbprint])
        .arg(display.join("vmmcdisp.sys"))
        .arg(display.join("vmmcdisp.cat")))?;
    eprintln!(
        "xtask: drivers in {}, test-signed with {}",
        display.display(),
        certificate.display()
    );
    Ok(Drivers {
        display,
        certificate,
    })
}

/// The thumbprint of the test certificate in the user's store, made if there's none
/// with some life left, and its public part exported to `export`.
fn test_certificate(export: &Path) -> Result<String, String> {
    let script = format!(
        "$ErrorActionPreference = 'Stop'
         $cert = Get-ChildItem Cert:\\CurrentUser\\My |
             Where-Object {{ $_.Subject -eq '{CERTIFICATE_SUBJECT}' -and $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date).AddDays(30) }} |
             Select-Object -First 1
         if (-not $cert) {{
             $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject '{CERTIFICATE_SUBJECT}' `
                 -CertStoreLocation Cert:\\CurrentUser\\My -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(5)
         }}
         Export-Certificate -Cert $cert -FilePath '{}' | Out-Null
         $cert.Thumbprint",
        export.display()
    );
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .map_err(|e| format!("can't run PowerShell: {e}"))?;
    let thumbprint = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || thumbprint.len() != 40 {
        return Err(format!(
            "can't make or export the test certificate: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(thumbprint)
}

pub fn display_test() -> Result<(), String> {
    let drivers = build(true)?;
    let mut files = vec![(
        "vmmc-test-signing.cer".to_owned(),
        read(&drivers.certificate)?,
    )];
    for file in ["vmmcdisp.inf", "vmmcdisp.sys", "vmmcdisp.cat"] {
        files.push((file.to_owned(), read(&drivers.display.join(file))?));
    }
    let mut modes = String::new();
    windows::install(
        &Install {
            name: "win-display-test",
            vars: "OVMF_VARS.fd",
            first_logon: windows::crlf(DISPLAY_TEST),
            files,
            // As SYSTEM, before the first logon, which installs the driver. (Setup's
            // pnputil ran without effect; the first logon's works. Importing the
            // certificate there was denied.)
            specialize: &[
                (
                    "Load test-signed drivers",
                    "bcdedit /set {current} testsigning on",
                ),
                (
                    "Trust the test driver certificate",
                    r"cmd /c for %d in (D E F G H I J) do @if exist %d:\vmmc-test-signing.cer (certutil -addstore -f Root %d:\vmmc-test-signing.cer & certutil -addstore -f TrustedPublisher %d:\vmmc-test-signing.cer)",
                ),
            ],
            markers: &DISPLAY_MARKERS,
        },
        |marker, vm, dir| {
            if marker == "VMMC-DISP: mode-set" {
                modes = vm.text(0, vm.len());
                check_screen(vm, dir)?;
            }
            Ok(())
        },
    )?;
    // Windows offers the standard modes too, not just the firmware's.
    let offered = modes
        .lines()
        .find_map(|line| line.split_once("VMMC-DISP: modes="))
        .map(|(_, modes)| modes.trim().to_owned())
        .unwrap_or_default();
    for mode in ["1024x768", "1600x900", "1920x1080"] {
        if !offered.split(',').any(|offered| offered == mode) {
            return Err(format!("Windows doesn't offer {mode}: {offered}"));
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))
}

/// Takes a screenshot (Ctrl-A s) and checks it's the test's resolution, with a picture.
fn check_screen(vm: &mut Session, dir: &Path) -> Result<(), String> {
    let screens = dir.join("screens");
    vm.send("\x01s")?;
    thread::sleep(Duration::from_secs(3));
    let mut shots: Vec<PathBuf> = fs::read_dir(&screens)
        .map_err(|e| format!("can't list {}: {e}", screens.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("screenshot"))
        })
        .collect();
    shots.sort();
    let shot = shots.pop().ok_or("no screenshot was saved")?;
    let decoder = png::Decoder::new(std::io::BufReader::new(
        fs::File::open(&shot).map_err(|e| format!("can't open {}: {e}", shot.display()))?,
    ));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", shot.display()))?;
    let (width, height) = (reader.info().width, reader.info().height);
    let mut pixels = vec![0; reader.output_buffer_size().unwrap_or(0)];
    reader
        .next_frame(&mut pixels)
        .map_err(|e| format!("{}: {e}", shot.display()))?;
    if (width, height) != TEST_MODE {
        return Err(format!(
            "the screen is {width}x{height}, not {}x{} ({})",
            TEST_MODE.0,
            TEST_MODE.1,
            shot.display()
        ));
    }
    let first = &pixels[..3];
    if pixels.chunks(3).all(|pixel| pixel == first) {
        return Err(format!("the screen is one colour ({})", shot.display()));
    }
    eprintln!(
        "xtask: the screen is {width}x{height}, with a picture ({})",
        shot.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_become_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_740), (2026, 10, 14));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn msvc_versions_sort_numerically() {
        let old = msvc_version(Path::new(
            r"C:\VS\VC\Tools\MSVC\14.9.1\bin\Hostx64\x64\cl.exe",
        ));
        let new = msvc_version(Path::new(
            r"C:\VS\VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64\cl.exe",
        ));
        assert_eq!(new, [14, 51, 36231]);
        assert!(old < new);
    }
}
