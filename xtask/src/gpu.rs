//! `cargo xtask ubuntu-gpu-test`: Ubuntu Desktop on virtio-gpu, its screen following
//! the sizes the host asks for, as the console asks when its window is resized. It
//! boots ubuntu-dnd-test's image (installed first if it isn't there) with the Tools disc
//! and a test disc, whose script (`gpu-test.sh`) the image runs at logon: it installs
//! vmmc Tools from their disc if the guest's are different (and restarts the guest), and
//! checks their agent started with the session. Once the agent says the desktop is up,
//! the test asks for sizes; the size the guest shows comes back in the pipe's status.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use ipc::Request;

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

pub fn ubuntu_test() -> Result<(), String> {
    // The Tools disc, which the guest installs from.
    drivers::build(false)?;
    dnd::ubuntu_image()?;
    let tools = workspace()
        .join("target")
        .join("guest-drivers")
        .join(drivers::TOOLS_ISO);
    // The test disc, whose script the image runs at logon.
    let dir = workspace().join("target").join(NAME);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let disc = dir.join("test.iso");
    let script = TEST.replace(char::from(13), "");
    let files: [(&str, &[u8]); 1] = [("vmmc-test.sh", script.as_bytes())];
    fs::write(&disc, media::iso::build("VMMC_TEST", &files, None))
        .map_err(|e| format!("can't write {}: {e}", disc.display()))?;
    let id = format!("{NAME}-{}", std::process::id());
    let mut command = crate::ubuntu::machine(UBUNTU, "screens-gpu")?;
    command
        .args([
            "--nic",
            "e1000e",
            "--adapter",
            "virtio-gpu",
            "--pipe-id",
            &id,
        ])
        .arg("--resolution")
        .arg(format!("{}x{}", START.0, START.1))
        .arg("--cdrom")
        .arg(&tools)
        .arg("--cdrom")
        .arg(&disc);
    eprintln!("$ {command:?}");
    let mut vm = Session::spawn(command)?;
    let result = resize(&mut vm, &id);
    for line in vm
        .text(0, vm.len())
        .lines()
        .filter(|line| line.contains("VMMC-GPU:") || line.contains("vmmc-vmx: agent"))
    {
        eprintln!("{}", line.trim());
    }
    if result.is_err() {
        vm.dump_state();
    }
    let _ = vm.send("\x01p");
    if vm.wait(Duration::from_secs(180)).is_none() {
        eprintln!("xtask: Ubuntu didn't shut down; stopped it");
    }
    result?;
    eprintln!("{NAME} passed");
    Ok(())
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

fn resize(vm: &mut Session, id: &str) -> Result<(), String> {
    let logon = tools_agent(vm)?;
    vm.expect(
        "vmmc-vmx: agent: connected to the desktop",
        logon,
        Duration::from_secs(60),
    )?;
    let mut pipe = vmmc_client::connect_id(id, Duration::from_secs(30))?;
    let screen =
        |pipe: &mut vmmc_client::Connection| -> Result<_, String> { Ok(pipe.status()?.screen) };
    let now = screen(&mut pipe)?;
    eprintln!("xtask: the desktop is up at {now:?}");
    if now != Some(START) {
        return Err(format!("the desktop came up at {now:?}, not {START:?}"));
    }
    for (width, height) in SIZES {
        pipe.command(&Request::RequestSize { width, height })?;
        let asked = Instant::now();
        loop {
            let now = screen(&mut pipe)?;
            if now == Some((width, height)) {
                eprintln!(
                    "xtask: the guest went to {width}x{height} in {} ms",
                    asked.elapsed().as_millis()
                );
                break;
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
    // What it looks like, to look at.
    thread::sleep(Duration::from_secs(2));
    let _ = vm.send("\x01s");
    thread::sleep(Duration::from_secs(1));
    Ok(())
}
