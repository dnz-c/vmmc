//! `cargo xtask ubuntu-gpu-test`: Ubuntu Desktop on virtio-gpu, its screen following
//! the sizes the host asks for, as the console asks when its window is resized. It
//! boots ubuntu-dnd-test's image (installed first if it isn't there) with the Tools disc
//! and a test disc, whose script (`gpu-test.sh`) the image runs at logon: it installs
//! vmmc Tools from their disc if the guest's are different (and restarts the guest), and
//! checks their agent started with the session. Once the agent says the desktop is up,
//! the test asks for sizes; the size the guest shows comes back in the pipe's status.
//! Then it suspends the VM and resumes it in a new process: the picture is back as it
//! was, and the desktop follows a size asked for after.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ipc::Request;
use vmmc_client::Connection;

use crate::dnd::{self, UBUNTU};
use crate::drivers;
use crate::firmware::Session;
use crate::workspace;

const NAME: &str = "ubuntu-gpu-test";
const TEST: &str = include_str!("../guest/gpu-test.sh");
const DESKTOP_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the desktop gets to take on a size.
const RESIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// What the guest starts at, then the sizes asked for, in order.
const START: (u32, u32) = (1280, 800);
const SIZES: [(u32, u32); 3] = [(1500, 844), (1024, 768), (1920, 1080)];
/// What's asked for after the resume.
const RESUMED: (u32, u32) = (1280, 720);

/// The machine, the same each time: the Tools disc, the test disc, virtio-gpu, and a
/// state file for Ctrl-A u.
struct Machine {
    tools: PathBuf,
    disc: PathBuf,
    state: PathBuf,
}

impl Machine {
    fn command(&self, id: &str, restore: bool) -> Result<Command, String> {
        let mut command = crate::ubuntu::machine(UBUNTU, "screens-gpu")?;
        command
            .args([
                "--nic",
                "e1000e",
                "--adapter",
                "virtio-gpu",
                "--pipe-id",
                id,
            ])
            .arg("--resolution")
            .arg(format!("{}x{}", START.0, START.1))
            .arg("--cdrom")
            .arg(&self.tools)
            .arg("--cdrom")
            .arg(&self.disc)
            .arg("--save-state")
            .arg(&self.state);
        if restore {
            command.arg("--restore").arg(&self.state);
        }
        eprintln!("$ {command:?}");
        Ok(command)
    }
}

pub fn ubuntu_test() -> Result<(), String> {
    // The Tools disc, which the guest installs from.
    drivers::build(false)?;
    dnd::ubuntu_image()?;
    // The test disc, whose script the image runs at logon.
    let dir = workspace().join("target").join(NAME);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let disc = dir.join("test.iso");
    let script = TEST.replace(char::from(13), "");
    let files: [(&str, &[u8]); 1] = [("vmmc-test.sh", script.as_bytes())];
    fs::write(&disc, media::iso::build("VMMC_TEST", &files, None))
        .map_err(|e| format!("can't write {}: {e}", disc.display()))?;
    let machine = Machine {
        tools: workspace()
            .join("target")
            .join("guest-drivers")
            .join(drivers::TOOLS_ISO),
        disc,
        // Gigabytes, written at once: off the workspace's volume (see image_dir).
        state: crate::image_dir()?.join(format!("{NAME}.state")),
    };
    let _ = fs::remove_file(&machine.state);

    let id = format!("{NAME}-{}", std::process::id());
    let mut vm = Session::spawn(machine.command(&id, false)?)?;
    let result = resize(&mut vm, &id).and_then(|()| suspend(&mut vm, &machine.state));
    report(&vm);
    if let Err(e) = result {
        vm.dump_state();
        shut_down(&mut vm);
        return Err(e);
    }

    let id = format!("{NAME}-{}-resumed", std::process::id());
    let mut vm = Session::spawn(machine.command(&id, true)?)?;
    let result = resumed(&mut vm, &id);
    report(&vm);
    if result.is_err() {
        vm.dump_state();
    }
    shut_down(&mut vm);
    let _ = fs::remove_file(&machine.state);
    result?;
    eprintln!("{NAME} passed");
    Ok(())
}

fn report(vm: &Session) {
    for line in vm
        .text(0, vm.len())
        .lines()
        .filter(|line| line.contains("VMMC-GPU:") || line.contains("vmmc-vmx: agent"))
    {
        eprintln!("{}", line.trim());
    }
}

/// The power button: logind powers off at once (the image says so).
fn shut_down(vm: &mut Session) {
    let _ = vm.send("\x01p");
    if vm.wait(Duration::from_secs(180)).is_none() {
        eprintln!("xtask: Ubuntu didn't shut down; stopped it");
    }
}

/// Follows the logon script until vmmc Tools' agent started with the session, through
/// an install and a restart if the guest's Tools weren't the disc's. Returns where the
/// output of that logon starts.
fn tools_agent(vm: &mut Session) -> Result<usize, String> {
    let (mut at, mut logon) = (0, 0);
    loop {
        at = vm.expect("VMMC-GPU: ", at, DESKTOP_TIMEOUT)?;
        let text = vm.text(at, vm.len());
        let line = text.lines().next().unwrap_or_default().trim().to_owned();
        match line.as_str() {
            "installed-agent running" => return Ok(logon),
            "tools-updated" => eprintln!("xtask: installed the Tools disc's; the guest restarts"),
            "installed-agent not running" => {
                // Its journal lines follow.
                thread::sleep(Duration::from_secs(2));
                return Err("vmmc Tools' agent didn't start with the session".into());
            }
            _ if line.starts_with("logged-on") => logon = at,
            _ if line.starts_with("journal ") => {}
            _ => return Err(format!("the logon script: {line}")),
        }
    }
}

fn screen(pipe: &mut Connection) -> Result<Option<(u32, u32)>, String> {
    Ok(pipe.status()?.screen)
}

/// Asks for `size` and waits for the guest's screen to take it on.
fn follow(
    vm: &mut Session,
    pipe: &mut Connection,
    (width, height): (u32, u32),
) -> Result<(), String> {
    pipe.command(&Request::RequestSize { width, height })?;
    let asked = Instant::now();
    loop {
        let now = screen(pipe)?;
        if now == Some((width, height)) {
            eprintln!(
                "xtask: the guest went to {width}x{height} in {} ms",
                asked.elapsed().as_millis()
            );
            return Ok(());
        }
        if asked.elapsed() > RESIZE_TIMEOUT {
            let _ = vm.send("\x01s");
            return Err(format!(
                "asked for {width}x{height}, the guest stayed at {now:?}"
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn resize(vm: &mut Session, id: &str) -> Result<(), String> {
    let logon = tools_agent(vm)?;
    vm.expect(
        "vmmc-vmx: agent: connected to the desktop",
        logon,
        Duration::from_secs(60),
    )?;
    let mut pipe = vmmc_client::connect_id(id, Duration::from_secs(30))?;
    let now = screen(&mut pipe)?;
    eprintln!("xtask: the desktop is up at {now:?}");
    if now != Some(START) {
        return Err(format!("the desktop came up at {now:?}, not {START:?}"));
    }
    for size in SIZES {
        follow(vm, &mut pipe, size)?;
    }
    // What it looks like, to look at.
    thread::sleep(Duration::from_secs(2));
    let _ = vm.send("\x01s");
    thread::sleep(Duration::from_secs(1));
    Ok(())
}

/// Ctrl-A u: saves the VM's state and stops it.
fn suspend(vm: &mut Session, state: &Path) -> Result<(), String> {
    let started = Instant::now();
    vm.send("\x01u")?;
    if vm.wait(Duration::from_secs(300)).is_none() {
        return Err("vmmc-vmx didn't stop after suspending".into());
    }
    let size = fs::metadata(state)
        .map_err(|e| format!("no saved state at {}: {e}", state.display()))?
        .len();
    eprintln!(
        "xtask: suspended ({} MiB) in {} ms",
        size >> 20,
        started.elapsed().as_millis()
    );
    Ok(())
}

/// The resumed VM shows what it did, and its desktop follows a new size.
fn resumed(vm: &mut Session, id: &str) -> Result<(), String> {
    let mut pipe = vmmc_client::connect_id(id, Duration::from_secs(60))?;
    let last = *SIZES.last().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let now = screen(&mut pipe)?;
        if now == Some(last) {
            eprintln!("xtask: resumed at {last:?}");
            break;
        }
        if Instant::now() > deadline {
            return Err(format!("resumed at {now:?}, not {last:?}"));
        }
        thread::sleep(Duration::from_millis(250));
    }
    follow(vm, &mut pipe, RESUMED)?;
    let _ = vm.send("\x01s");
    thread::sleep(Duration::from_secs(1));
    Ok(())
}
