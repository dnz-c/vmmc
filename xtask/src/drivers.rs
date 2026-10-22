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
//!   Secure Boot off and test signing on, installs the display driver at first logon
//!   with the vmmc Tools disc's setup,
//!   switches to 1600x900 and checks that the screen followed, then asks for 1500x844
//!   as resizing the window does and checks the desktop follows. A second boot checks
//!   that Windows hands the driver its pointer and that the helper starts at logon.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::windows::{self, Install};
use crate::{run, workspace};

const CERTIFICATE_SUBJECT: &str = "CN=vmmc test driver signing";
const KITS: &str = r"C:\Program Files (x86)\Windows Kits\10";
const DISPLAY_TEST: &str = include_str!("../guest/display-test.ps1");
/// What the display test's first logon reports, in order.
const DISPLAY_MARKERS: [&str; 14] = [
    "VMMC-DISP: booted",
    "VMMC-DISP: testsigning=True",
    "VMMC-DISP: setup=ok",
    "VMMC-DISP: agent=True",
    "VMMC-DISP: clipboard-copied",
    "VMMC-DISP: clipboard=vmmc clipboard from the host",
    "VMMC-DISP: driver=vmmcdisp status=OK",
    "VMMC-DISP: set=0 now=1600x900",
    "VMMC-DISP: mode-set",
    "VMMC-DISP: service=Running helper=True",
    "VMMC-DISP: helper-started",
    "VMMC-DISP: fitted=1500x844",
    "VMMC-DISP: fit-checked",
    "VMMC-DISP: done",
];
/// The resolution the test switches to, then the window size the host asks for.
const TEST_MODE: (u32, u32) = (1600, 900);
const FIT_MODE: (u32, u32) = (1500, 844);
/// Screenshots the screen checks take before giving up, 3 s apart.
const SCREEN_TRIES: u32 = 5;
/// How long the second boot gets to reach the desktop, with its pointer and the helper.
const CURSOR_TIMEOUT: Duration = Duration::from_secs(240);
/// What the second boot's logon script reports, followed by True or False.
const LOGON_HELPER: &str = "VMMC-DISP: logon helper=";
/// The version stamped into the INF.
const DRIVER_VERSION: &str = "0.2.0.0";
/// The vmmc Tools disc `guest-drivers` makes, beside the package: the driver package,
/// the test certificate, and setup (`guest-drivers/windows/tools`).
pub const TOOLS_ISO: &str = "vmmc-tools.iso";
/// The display driver's package.
const PACKAGE: [&str; 4] = [
    "vmmcdisp.inf",
    "vmmcdisp.sys",
    "vmmcdisplay.exe",
    "vmmcdisp.cat",
];

/// A built and signed driver package, and the certificate that signed it.
pub struct Drivers {
    /// The display driver's package: `vmmcdisp.inf`, `.sys` and `.cat`.
    pub display: PathBuf,
    /// The agent that shares the clipboard and drags files (`vmmcagent.exe`), which
    /// needs no driver.
    pub agent: PathBuf,
    /// The Linux agent (`vmmc-agent`), static.
    pub linux_agent: PathBuf,
    pub certificate: PathBuf,
}

/// The Rust target the Linux agent is built for: static, so it runs on any distribution.
const LINUX_TARGET: &str = "x86_64-unknown-linux-musl";

/// Builds the Linux agent (`guest-drivers/linux/agent`, a workspace of its own), linked
/// with rust-lld, which Rust brings along.
pub fn build_linux_agent() -> Result<PathBuf, String> {
    let dir = workspace()
        .join("guest-drivers")
        .join("linux")
        .join("agent");
    run(Command::new(env!("CARGO"))
        .current_dir(&dir)
        .args(["build", "--release", "--target", LINUX_TARGET])
        .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld"))?;
    let agent = dir
        .join("target")
        .join(LINUX_TARGET)
        .join("release")
        .join("vmmc-agent");
    if agent.is_file() {
        Ok(agent)
    } else {
        Err(format!("no {} after the build", agent.display()))
    }
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
    // The helper, a user-mode program, with the CRT linked in.
    let msvc_root = tools
        .msvc
        .ancestors()
        .nth(3)
        .ok_or("MSVC's bin directory isn't where it should be")?
        .to_owned();
    let libs = tools.kits("Lib");
    run(Command::new(tools.msvc.join("cl.exe"))
        .args([
            "/nologo",
            "/O2",
            "/W4",
            "/WX",
            "/MT",
            "/GR-",
            "/EHs-c-",
            "/std:c++17",
        ])
        .args(["/DUNICODE", "/D_UNICODE", "/D_WIN32_WINNT=0x0A00"])
        .arg(format!("/I{}", msvc_root.join("include").display()))
        .arg(format!("/I{}", include.join("ucrt").display()))
        .arg(format!("/I{}", include.join("um").display()))
        .arg(format!("/I{}", include.join("shared").display()))
        .arg(format!("/Fo{}", obj.join("vmmcdisplay.obj").display()))
        .arg(format!("/Fe{}", display.join("vmmcdisplay.exe").display()))
        .arg(source.join("vmmcdisplay.cpp"))
        .args(["/link", "/SUBSYSTEM:WINDOWS"])
        .args([
            "user32.lib",
            "gdi32.lib",
            "advapi32.lib",
            "wtsapi32.lib",
            "userenv.lib",
        ])
        .arg(format!("/LIBPATH:{}", msvc_root.join(r"lib\x64").display()))
        .arg(format!("/LIBPATH:{}", libs.join(r"um\x64").display()))
        .arg(format!("/LIBPATH:{}", libs.join(r"ucrt\x64").display())))?;
    // The agent, a user-mode program too, built the same way.
    let agent = out.join("windows").join("agent").join("vmmcagent.exe");
    fs::create_dir_all(agent.parent().unwrap())
        .map_err(|e| format!("can't create {}: {e}", agent.display()))?;
    run(Command::new(tools.msvc.join("cl.exe"))
        .args(["/nologo", "/O2", "/W4", "/WX", "/MT", "/EHsc", "/std:c++17"])
        .args(["/DUNICODE", "/D_UNICODE", "/D_WIN32_WINNT=0x0A00"])
        .arg(format!("/I{}", msvc_root.join("include").display()))
        .arg(format!("/I{}", include.join("ucrt").display()))
        .arg(format!("/I{}", include.join("um").display()))
        .arg(format!("/I{}", include.join("shared").display()))
        .arg(format!("/Fo{}", obj.join("vmmcagent.obj").display()))
        .arg(format!("/Fe{}", agent.display()))
        .arg(source.with_file_name("agent").join("vmmcagent.cpp"))
        .args(["/link", "/SUBSYSTEM:WINDOWS"])
        .args([
            "user32.lib",
            "kernel32.lib",
            "gdi32.lib",
            "ole32.lib",
            "shell32.lib",
            "ws2_32.lib",
        ])
        .arg(format!("/LIBPATH:{}", msvc_root.join(r"lib\x64").display()))
        .arg(format!("/LIBPATH:{}", libs.join(r"um\x64").display()))
        .arg(format!("/LIBPATH:{}", libs.join(r"ucrt\x64").display())))?;
    let inf = display.join("vmmcdisp.inf");
    fs::copy(source.join("vmmcdisp.inf"), &inf).map_err(|e| format!("can't copy the INF: {e}"))?;
    run(Command::new(tools.tool("x64", "stampinf.exe"))
        .arg("-f")
        .arg(&inf)
        .args(["-d", &utc_date(), "-a", "amd64", "-v", DRIVER_VERSION]))?;
    // The INF rules for drivers Microsoft signs ("Windows Driver": the package keeps to
    // its own registry keys and files, so no Run key for the helper, say).
    run(Command::new(tools.kits("Tools").join(r"x64\infverif.exe"))
        .arg("/w")
        .arg(&inf))?;
    let _ = fs::remove_file(display.join("vmmcdisp.cat"));
    run(Command::new(tools.tool("x86", "Inf2Cat.exe"))
        .arg(format!("/driver:{}", display.display()))
        .arg("/os:10_X64"))?;

    let linux_agent = build_linux_agent()?;
    let certificate = out.join("vmmc-test-signing.cer");
    let thumbprint = test_certificate(&certificate)?;
    run(Command::new(tools.tool("x64", "signtool.exe"))
        .args(["sign", "/fd", "sha256", "/s", "My", "/sha1", &thumbprint])
        .arg(display.join("vmmcdisp.sys"))
        .arg(display.join("vmmcdisplay.exe"))
        .arg(display.join("vmmcdisp.cat"))
        .arg(&agent))?;
    eprintln!(
        "xtask: drivers in {}, test-signed with {}",
        display.display(),
        certificate.display()
    );
    let tools = out.join(TOOLS_ISO);
    write_tools_iso(&display, &agent, &linux_agent, &certificate, &tools)?;
    eprintln!("xtask: vmmc Tools disc in {}", tools.display());
    Ok(Drivers {
        display,
        agent,
        linux_agent,
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
    for file in PACKAGE {
        files.push((file.to_owned(), read(&drivers.display.join(file))?));
    }
    files.push(("setup.ps1".to_owned(), tools_file("setup.ps1")?));
    files.push(("vmmcagent.exe".to_owned(), read(&drivers.agent)?));
    let mut modes = String::new();
    windows::install(
        &Install {
            name: "win-display-test",
            vars: "OVMF_VARS.fd",
            first_logon: windows::crlf(DISPLAY_TEST),
            files,
            // Before the first logon, so the driver loads without the restart setup
            // asks for when it turns test signing on. (Setup's pnputil ran without
            // effect; the first logon's works.)
            specialize: &[(
                "Load test-signed drivers",
                "bcdedit /set {current} testsigning on",
            )],
            markers: &DISPLAY_MARKERS,
        },
        |marker, vm, dir| {
            if marker == "VMMC-DISP: clipboard-copied" {
                // The guest's copy reached the host (its length, as vmmc-vmx reports
                // it); the host answers.
                vm.expect(
                    "vmmc-vmx: the guest copied 29 characters",
                    0,
                    Duration::from_secs(30),
                )?;
                vm.send("\x01cvmmc clipboard from the host\r")?;
            }
            if marker == "VMMC-DISP: mode-set" {
                modes = vm.text(0, vm.len());
                check_screen(vm, dir, TEST_MODE)?;
            }
            if marker == "VMMC-DISP: helper-started" {
                // What resizing the window does, from the console.
                vm.send(&format!("\x01z{}x{}\r", FIT_MODE.0, FIT_MODE.1))?;
            }
            if marker == "VMMC-DISP: fit-checked" {
                check_screen(vm, dir, FIT_MODE)?;
            }
            Ok(())
        },
    )?;
    // A second boot, which logs on by itself: on the desktop, once a logon script has
    // moved the mouse (until then Windows hides the pointer), Windows hands the driver
    // its pointer, as a hardware cursor the window draws. (The first logon ends before
    // the desktop shows.) The script also reports whether the driver's service started
    // the helper at this logon.
    let log = workspace()
        .join("target")
        .join("win-display-test")
        .join("second-boot.log");
    let mut vm = windows::boot_installed("win-display-test", &log)?;
    let deadline = Instant::now() + CURSOR_TIMEOUT;
    let (shape, helper) = loop {
        let trace = fs::read_to_string(&log).unwrap_or_default();
        let shape = trace
            .lines()
            .find(|line| line.contains("vmmcdisp: pointer shape"))
            .map(|line| line.trim().to_owned());
        let helper = vm
            .text(0, vm.len())
            .lines()
            .find_map(|line| line.split_once(LOGON_HELPER))
            .map(|(_, started)| started.trim() == "True");
        if (shape.is_some() && helper.is_some()) || Instant::now() > deadline {
            break (shape, helper);
        }
        thread::sleep(Duration::from_secs(1));
    };
    // The power button: Windows shuts down.
    let _ = vm.send("\x01p");
    if vm.wait(Duration::from_secs(180)).is_none() {
        eprintln!("xtask: Windows didn't shut down after the second boot; stopped it");
    }
    match shape {
        Some(line) => eprintln!("xtask: hardware cursor: {line}"),
        None => return Err("Windows gave the driver no cursor image".into()),
    }
    match helper {
        Some(true) => eprintln!("xtask: the service started the helper at logon"),
        Some(false) => return Err("the service didn't start the helper at logon".into()),
        None => return Err("the second logon didn't report".into()),
    }
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

/// The vmmc Tools disc, all in the root: Windows' setup (with Windows line ends), the
/// driver package, the certificate it's signed with and the agent; Linux's install
/// script and agent.
fn write_tools_iso(
    package: &Path,
    agent: &Path,
    linux_agent: &Path,
    certificate: &Path,
    iso: &Path,
) -> Result<(), String> {
    let mut files = Vec::new();
    for name in ["setup.cmd", "setup.ps1", "autorun.inf"] {
        files.push((name.to_owned(), tools_file(name)?));
    }
    for name in PACKAGE {
        files.push((name.to_owned(), read(&package.join(name))?));
    }
    files.push(("vmmcagent.exe".to_owned(), read(agent)?));
    files.push(("vmmc-test-signing.cer".to_owned(), read(certificate)?));
    files.push((
        "install-linux.sh".to_owned(),
        linux_tools_file("install-linux.sh")?,
    ));
    files.push(("vmmc-agent".to_owned(), read(linux_agent)?));
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    fs::write(iso, media::iso::build("VMMC_TOOLS", &files, None))
        .map_err(|e| format!("can't write {}: {e}", iso.display()))
}

/// A file of the Tools disc's setup (`guest-drivers/windows/tools`), with Windows line
/// ends.
pub(crate) fn tools_file(name: &str) -> Result<Vec<u8>, String> {
    let path = workspace()
        .join("guest-drivers")
        .join("windows")
        .join("tools")
        .join(name);
    let text = String::from_utf8(read(&path)?).map_err(|_| format!("{name} isn't UTF-8"))?;
    Ok(windows::crlf(&text).into_bytes())
}

/// A file of the Tools disc's Linux part (`guest-drivers/linux/tools`), with Unix line
/// ends, whatever the checkout has.
pub(crate) fn linux_tools_file(name: &str) -> Result<Vec<u8>, String> {
    let path = workspace()
        .join("guest-drivers")
        .join("linux")
        .join("tools")
        .join(name);
    let text = String::from_utf8(read(&path)?).map_err(|_| format!("{name} isn't UTF-8"))?;
    Ok(text.replace(char::from(13), "").into_bytes())
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))
}

/// Takes a screenshot (Ctrl-A s) and checks it's the test's resolution, with a picture.
/// Takes screenshots (Ctrl-A s) until one is `expected`'s size with a picture: the
/// guest may still be switching, or showing a plain colour for a moment.
fn check_screen(vm: &mut Session, dir: &Path, expected: (u32, u32)) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..SCREEN_TRIES {
        vm.send("\x01s")?;
        thread::sleep(Duration::from_secs(3));
        match screen(dir, expected) {
            Ok(shot) => {
                eprintln!(
                    "xtask: the screen is {}x{}, with a picture ({})",
                    expected.0,
                    expected.1,
                    shot.display()
                );
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The newest screenshot, if it's `expected`'s size and not one colour.
fn screen(dir: &Path, expected: (u32, u32)) -> Result<PathBuf, String> {
    let screens = dir.join("screens");
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
    if (width, height) != expected {
        return Err(format!(
            "the screen is {width}x{height}, not {}x{} ({})",
            expected.0,
            expected.1,
            shot.display()
        ));
    }
    let first = &pixels[..3];
    if pixels.chunks(3).all(|pixel| pixel == first) {
        return Err(format!("the screen is one colour ({})", shot.display()));
    }
    Ok(shot)
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
