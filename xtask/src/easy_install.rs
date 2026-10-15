//! `cargo xtask easy-install-test [windows|ubuntu]`: Easy Install end to end, as the
//! app and `vmmc-cli create --easy-install` do it: `create_vm` with an `EasyInstall`,
//! then a sandboxed worker (`vmmc_client::start`), which types the installer's first
//! steps, ejects the install discs at the end and starts the guest from its disk.
//!
//! - **Windows** (`VMMC_WIN11_ISO`, edition `VMMC_WIN11_IMAGE`): a first-logon command
//!   reports to COM1 and shuts down, so the test sees Setup finished, logged on, and
//!   reached the desktop.
//! - **Ubuntu** (26.04 Desktop, downloaded as for `ubuntu-desktop-test`): after the
//!   install powers off, the discs come out and the installed system boots; the test
//!   sees it ask the NAT for an address, then stops it.
//!
//! Both run at once, in the test image directory; a picture of each screen is saved
//! every minute in `target/easy-install-test`. 20 to 60 minutes.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ipc::Request;
use vmconfig::create::{EasyInstall, NewVm, create_vm};
use vmconfig::{Vm, presets};

use crate::{build_vmmc_vmx, native, workspace};

const TIMEOUT: Duration = Duration::from_secs(75 * 60);
const SCREEN_EVERY: Duration = Duration::from_secs(60);
const DONE: &str = "vmmc-vmx: Easy Install is done";
/// What the Windows guest's first logon writes to COM1.
const LOGGED_ON: &str = "VMMC-EASY: logged on";

struct Run {
    vm: Vm,
    /// What says the guest got there, after the install discs came out.
    finished: &'static str,
}

fn windows_vm(root: &Path) -> Result<Run, String> {
    let iso = PathBuf::from(env::var_os("VMMC_WIN11_ISO").ok_or("set VMMC_WIN11_ISO")?);
    let edition = env::var("VMMC_WIN11_IMAGE").unwrap_or_else(|_| "Windows 11 Pro".into());
    let preset = if edition.contains("10") {
        "windows10"
    } else {
        "windows11"
    };
    let report = format!(
        "powershell -NoProfile -Command \"$p = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $p.Open(); $p.WriteLine('{LOGGED_ON}'); $p.Close(); shutdown /s /t 0\""
    );
    let vm = create_vm(&NewVm {
        name: "Easy Windows",
        parent_dir: root,
        preset: presets::find(preset).unwrap(),
        cpus: None,
        memory_mib: None,
        disk_bytes: None,
        iso: Some(&iso),
        adopt: None,
        existing_disk: None,
        easy_install: Some(EasyInstall::Windows {
            edition,
            user: "vmmc".into(),
            password: "vmmc easy".into(),
            language: env::var("VMMC_WIN11_LANG").ok(),
            time_zone: "UTC".into(),
            first_logon: vec![("Report to the host and shut down".into(), report)],
        }),
    })?;
    Ok(Run {
        vm,
        finished: LOGGED_ON,
    })
}

fn ubuntu_vm(root: &Path) -> Result<Run, String> {
    let iso = crate::ubuntu::fetch_iso()?;
    let vm = create_vm(&NewVm {
        name: "Easy Ubuntu",
        parent_dir: root,
        preset: presets::find("ubuntu").unwrap(),
        cpus: Some(4),
        memory_mib: Some(8192),
        disk_bytes: Some(32 << 30),
        iso: Some(&iso),
        adopt: None,
        existing_disk: None,
        easy_install: Some(EasyInstall::Ubuntu {
            user: "vmmc".into(),
            real_name: "vmmc".into(),
            password: "vmmc easy".into(),
            keyboard: "us".into(),
            locale: "en_US.UTF-8".into(),
            time_zone: "Etc/UTC".into(),
        }),
    })?;
    // The installed system asks the NAT for an address (the NAT's debug log, which
    // the worker inherits VMMC_LOG for).
    Ok(Run {
        vm,
        finished: "reply=5",
    })
}

/// Saves the VM's screen as a PNG.
fn save_screen(connection: &mut vmmc_client::Connection, path: &Path) {
    let Ok(section) = connection.screen() else {
        return;
    };
    let mut seq = 0;
    let Some(picture) = section.view().read_picture(&mut seq) else {
        return;
    };
    let Ok(file) = fs::File::create(path) else {
        return;
    };
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), picture.width, picture.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(&picture.rgb);
    }
}

/// Watches one VM until `finished` shows in its log after the discs came out.
fn watch(run: &Run, screens: &Path) -> Result<(), String> {
    let name = &run.vm.config.name;
    let log = run.vm.dir().join(vmmc_client::LOG_FILE);
    let mut connection = vmmc_client::start(&run.vm)?;
    eprintln!("xtask: {name}: installing (worker {})", connection.pid);
    let started = Instant::now();
    let mut next_screen = Instant::now();
    let mut shot = 0;
    let mut steps_seen = 0;
    loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        let steps = text.matches("Easy Install: step").count();
        if steps != steps_seen {
            steps_seen = steps;
            eprintln!(
                "xtask: {name}: step {steps} typed after {}s",
                started.elapsed().as_secs()
            );
        }
        if let Some(done) = text.find(DONE)
            && text[done..].contains(run.finished)
        {
            eprintln!(
                "xtask: {name}: installed and running from its disk after {}s",
                started.elapsed().as_secs()
            );
            break;
        }
        if !vmmc_client::is_running(&run.vm) {
            return Err(format!(
                "{name}: the VM stopped before it finished: {}",
                vmmc_client::log_tail(&log)
            ));
        }
        if started.elapsed() > TIMEOUT {
            let _ = connection.command(&Request::Stop);
            return Err(format!(
                "{name}: not installed after {}s",
                TIMEOUT.as_secs()
            ));
        }
        if Instant::now() >= next_screen {
            shot += 1;
            save_screen(
                &mut connection,
                &screens.join(format!("screen-{shot:03}.png")),
            );
            next_screen += SCREEN_EVERY;
        }
        thread::sleep(Duration::from_secs(2));
    }
    // Windows shuts itself down; Ubuntu is stopped.
    let deadline = Instant::now() + Duration::from_secs(300);
    while vmmc_client::is_running(&run.vm) {
        if Instant::now() > deadline {
            let _ = connection.command(&Request::Stop);
            break;
        }
        if run.finished != LOGGED_ON {
            let _ = connection.command(&Request::Stop);
        }
        thread::sleep(Duration::from_secs(2));
    }
    let config = Vm::load(&run.vm.path).map_err(|e| e.to_string())?.config;
    if config.install.is_some() || config.cdroms.iter().any(|cd| cd.install_media) {
        return Err(format!("{name}: the install is still in its config"));
    }
    if run.vm.dir().join(vmconfig::create::ANSWER_DISC).exists() {
        return Err(format!(
            "{name}: the answer disc (with the password) is still there"
        ));
    }
    Ok(())
}

pub fn test(only: Option<&str>) -> Result<(), String> {
    let workspace = workspace();
    native::fetch(&workspace)?;
    build_vmmc_vmx(&workspace)?;
    // SAFETY: no other thread of xtask reads the environment at this point.
    unsafe { env::set_var("VMMC_LOG", "net::dhcp=debug") };
    let root = crate::image_dir()?.join("easy-install");
    let screens = workspace.join("target").join("easy-install-test");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&screens);
    fs::create_dir_all(&root).map_err(|e| format!("can't create {}: {e}", root.display()))?;
    let mut runs = Vec::new();
    if only != Some("ubuntu") {
        runs.push(windows_vm(&root)?);
    }
    if only != Some("windows") {
        runs.push(ubuntu_vm(&root)?);
    }
    let results: Vec<(String, Result<(), String>)> = thread::scope(|scope| {
        let handles: Vec<_> = runs
            .iter()
            .map(|run| {
                let dir = screens.join(run.vm.config.name.replace(' ', "-"));
                let _ = fs::create_dir_all(&dir);
                scope.spawn(move || watch(run, &dir))
            })
            .collect();
        runs.iter()
            .zip(handles)
            .map(|(run, handle)| {
                (
                    run.vm.config.name.clone(),
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("the watcher panicked".into())),
                )
            })
            .collect()
    });
    let mut failed = Vec::new();
    for (name, result) in results {
        match result {
            Ok(()) => eprintln!("xtask: {name}: passed"),
            Err(e) => {
                eprintln!("xtask: {e}");
                failed.push(name);
            }
        }
    }
    if !failed.is_empty() {
        return Err(format!("Easy Install failed for {}", failed.join(", ")));
    }
    let _ = fs::remove_dir_all(&root);
    eprintln!("easy-install-test passed");
    Ok(())
}
