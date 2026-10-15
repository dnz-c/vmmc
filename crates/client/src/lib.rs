//! Starting VMs and talking to them, for vmmc's app and command line.
//!
//! A running VM is a worker process, `vmmc-vmx serve --vm FILE`, which outlives
//! whoever started it. [`start`] launches one in a sandbox (see [`sandbox`]) with its
//! output in `vmmc-vmx.log` in the VM's folder; [`connect`] finds a running one by its
//! pipe.

pub mod host;
#[cfg(windows)]
pub mod sandbox;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ipc::{Request, Response, Status, VERSION};
use vmconfig::Vm;

/// The worker's log, in the VM's folder.
pub const LOG_FILE: &str = "vmmc-vmx.log";
/// How long a new worker gets to open its pipe.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// A connection to a running VM's worker.
pub struct Connection {
    pipe: File,
    pub name: String,
    pub uuid: String,
    /// The worker's process ID.
    pub pid: u32,
}

impl Connection {
    pub fn call(&mut self, request: &Request) -> Result<Response, String> {
        match ipc::call(&mut self.pipe, request) {
            Ok(Response::Error(e)) => Err(e),
            Ok(response) => Ok(response),
            Err(e) => Err(format!("the VM's worker stopped answering: {e}")),
        }
    }

    /// Sends a request that's answered with [`Response::Done`].
    pub fn command(&mut self, request: &Request) -> Result<(), String> {
        match self.call(request)? {
            Response::Done => Ok(()),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    pub fn status(&mut self) -> Result<Status, String> {
        match self.call(&Request::Status)? {
            Response::Status(status) => Ok(status),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    /// The worker's shared screen, mapped into this process.
    #[cfg(windows)]
    pub fn screen(&mut self) -> Result<ipc::pipe::Section, String> {
        match self.call(&Request::Screen)? {
            Response::Screen { handle, .. } => ipc::pipe::Section::open(self.pid, handle)
                .map_err(|e| format!("can't map the VM's screen: {e}")),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }
}

/// Connects to the worker of `vm`, if it's running, waiting up to `timeout`.
#[cfg(windows)]
pub fn connect(vm: &Vm, timeout: Duration) -> Result<Connection, String> {
    let path = ipc::pipe::pipe_path(&vm.config.uuid);
    let pipe = ipc::pipe::connect(&path, timeout)
        .map_err(|e| format!("{} isn't running ({e})", vm.config.name))?;
    hello(pipe)
}

fn hello(mut pipe: File) -> Result<Connection, String> {
    match ipc::call(&mut pipe, &Request::Hello { version: VERSION }) {
        Ok(Response::Hello {
            name, uuid, pid, ..
        }) => Ok(Connection {
            pipe,
            name,
            uuid,
            pid,
        }),
        Ok(Response::Error(e)) => Err(e),
        Ok(other) => Err(format!("unexpected answer {other:?}")),
        Err(e) => Err(format!("the VM's worker didn't answer: {e}")),
    }
}

/// Whether `vm` is running: its worker answers.
#[cfg(windows)]
pub fn is_running(vm: &Vm) -> bool {
    connect(vm, Duration::ZERO).is_ok()
}

/// The worker program: `VMMC_VMX`, or `vmmc-vmx` next to this program.
pub fn worker_exe() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("VMMC_VMX") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let worker = exe.with_file_name(format!("vmmc-vmx{}", std::env::consts::EXE_SUFFIX));
    if worker.is_file() {
        Ok(worker)
    } else {
        Err(format!("{} not found", worker.display()))
    }
}

/// The vmmc Tools disc: `VMMC_TOOLS_ISO`, or `vmmc-tools.iso` next to this program,
/// or (in a build) in `target\guest-drivers`, where `cargo xtask guest-drivers` puts
/// it.
pub fn tools_iso() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMMC_TOOLS_ISO") {
        return Some(path.into());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join(TOOLS_ISO),
        dir.parent()?.join("guest-drivers").join(TOOLS_ISO),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// The vmmc Tools disc's file name.
pub const TOOLS_ISO: &str = "vmmc-tools.iso";

/// Puts the vmmc Tools disc into the running VM's first DVD drive. Returns what the
/// user should know next.
#[cfg(windows)]
pub fn insert_tools(vm: &Vm) -> Result<String, String> {
    let iso = tools_iso().ok_or(
        "the vmmc Tools disc (vmmc-tools.iso) isn't there: build it with `cargo xtask guest-drivers`",
    )?;
    let mut connection = connect(vm, Duration::from_secs(2))?;
    connection
        .command(&Request::InsertDisc {
            drive: 0,
            path: Some(iso.display().to_string()),
        })
        .map_err(|e| {
            if e.contains("no DVD drive") {
                "this VM has no DVD drive: shut it down, put the vmmc Tools disc (vmmc-tools.iso) in its DVD drive in its settings, and start it again".to_owned()
            } else {
                e
            }
        })?;
    let mut text = "The vmmc Tools disc is in the VM's DVD drive. In Windows, open it (AutoPlay offers it, or open the drive in Explorer) and run setup; it asks for administrator rights.".to_owned();
    if vm.config.firmware.secure_boot {
        text += "\n\nSecure Boot is on in this VM, and vmmc's display driver isn't signed by Microsoft yet, so Windows won't load it: setup will say so. To use it now, shut the VM down, turn Secure Boot off in its settings, start it, and run setup again.";
    } else {
        text += " Windows restarts once afterwards, the first time.";
    }
    Ok(text)
}

/// Whether the sandbox is off (`VMMC_NO_SANDBOX=1`), for finding out if it's in the
/// way.
pub fn sandbox_disabled() -> bool {
    std::env::var_os("VMMC_NO_SANDBOX").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Disks the sandboxed worker couldn't write: writable ones outside the VM's folder.
/// The sandbox only lets the worker write inside its folder.
pub fn disks_outside(vm: &Vm) -> Vec<PathBuf> {
    let dir = std::path::absolute(vm.dir()).unwrap_or_else(|_| vm.dir().to_owned());
    vm.config
        .disks
        .iter()
        .filter(|disk| !disk.read_only)
        .map(|disk| {
            std::path::absolute(vm.resolve(&disk.path)).unwrap_or_else(|_| vm.resolve(&disk.path))
        })
        .filter(|path| !path.starts_with(&dir))
        .collect()
}

/// Starts `vm`'s worker (sandboxed unless [`sandbox_disabled`]) and connects to it.
#[cfg(windows)]
pub fn start(vm: &Vm) -> Result<Connection, String> {
    if is_running(vm) {
        return Err(format!("{} is running already", vm.config.name));
    }
    let sandboxed = !sandbox_disabled();
    if sandboxed && !sandbox::interactive() {
        return Err(
            "a sandboxed VM can only start from the desktop, not from a service or SSH session (or set VMMC_NO_SANDBOX=1)"
                .into(),
        );
    }
    if sandboxed && let Some(outside) = disks_outside(vm).first() {
        return Err(format!(
            "{} is outside the VM's folder, where the sandboxed VM can't write: move it into {} (or run with VMMC_NO_SANDBOX=1)",
            outside.display(),
            vm.dir().display()
        ));
    }
    let exe = worker_exe()?;
    let path = std::path::absolute(&vm.path).map_err(|e| e.to_string())?;
    if sandboxed {
        sandbox::allow_writes(vm.dir())?;
    }
    let log = vm.dir().join(LOG_FILE);
    let args = [
        "serve".to_owned(),
        "--vm".to_owned(),
        path.display().to_string(),
    ];
    let mut process = sandbox::spawn(&exe, &args, &log, sandboxed)?;
    // Wait for the pipe, unless the worker gives up first.
    let pipe_path = ipc::pipe::pipe_path(&vm.config.uuid);
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(pipe) = ipc::pipe::connect(&pipe_path, Duration::from_millis(200)) {
            return hello(pipe);
        }
        if let Some(code) = process.exit_code() {
            if code == sandbox::DLL_INIT_FAILED {
                return Err("Windows didn't let the sandboxed VM start in this session (its desktop doesn't admit sandboxed programs); set VMMC_NO_SANDBOX=1 to run it unsandboxed".into());
            }
            return Err(format!(
                "the VM stopped (exit code {code}): {}",
                log_tail(&log)
            ));
        }
        if Instant::now() > deadline {
            return Err(format!("the VM didn't start: {}", log_tail(&log)));
        }
    }
}

/// The end of a worker's log, for an error message.
pub fn log_tail(log: &Path) -> String {
    let text = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
    if tail.is_empty() {
        format!("see {}", log.display())
    } else {
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_writable_disks_outside_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = vmconfig::VmConfig::new("x", "linux", 1, 512);
        for (path, read_only) in [
            ("x.vmdk", false),
            (r"C:\elsewhere\data.vmdk", false),
            (r"C:\elsewhere\base.vmdk", true),
        ] {
            config.disks.push(vmconfig::Disk {
                path: path.into(),
                bus: vmconfig::Bus::Nvme,
                read_only,
            });
        }
        let vm = Vm {
            path: dir.path().join("x.vmmc"),
            config,
        };
        let outside = disks_outside(&vm);
        if cfg!(windows) {
            assert_eq!(outside, [PathBuf::from(r"C:\elsewhere\data.vmdk")]);
        }
    }
}
