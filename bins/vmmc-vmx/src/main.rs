//! vmmc-vmx: runs one virtual machine, with its serial console on this terminal.
//!
//! ```text
//! vmmc-vmx run --vm "My VM\My VM.vmmc" --display window
//! vmmc-vmx run --kernel bzImage --initrd initramfs.cpio --disk disk.raw --cpus 4
//! vmmc-vmx run --firmware OVMF_CODE.fd --vm-dir my-vm --disk disk.vmdk,nvme
//! vmmc-vmx create-disk disk.vmdk --size 64G
//! ```
//!
//! A guest reset restarts the VM from scratch (unless `--no-reboot`). Exit status: 0
//! when the guest powers off (or the user quits with Ctrl-A x), 2 when it resets with
//! `--no-reboot`, 3 on a triple fault, 1 on any error. Ctrl-A p presses the ACPI power
//! button, Ctrl-A r prints each vCPU's registers, and Ctrl-A m saves guest RAM.

mod console;
#[cfg(windows)]
mod install;
mod memdump;
mod sandbox;
mod screenshot;
#[cfg(windows)]
mod serve;
mod vm_dir;
#[cfg(windows)]
mod window;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Args, Parser, Subcommand, ValueEnum};
use machine::{DiskConfig, DiskInterface, NicConfig, NicModel, RunOutcome, SharedTpm, TpmBackend};

#[derive(Parser)]
#[command(version, about = "Runs one vmmc virtual machine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot UEFI firmware or a Linux kernel, with the serial console on this terminal.
    Run(Box<RunArgs>),
    /// Run a VM (`--vm`) with no console: vmmc's app and `vmmc-cli` control it through
    /// its pipe, `\\.\pipe\vmmc-<uuid>`. vmmc starts its VMs this way, sandboxed.
    Serve(Box<RunArgs>),
    /// Create an empty disk for `run --disk`: a VMDK if the name ends in `.vmdk`
    /// (it grows as the guest writes), otherwise a raw image (a sparse file on NTFS).
    CreateDisk(CreateDiskArgs),
}

#[derive(Args)]
struct CreateDiskArgs {
    /// The image to create (with its directory). An existing file is never replaced.
    path: PathBuf,
    /// Its size: bytes, or with a K, M, G or T suffix (powers of 1024), like 64G.
    #[arg(long, value_parser = vmconfig::parse_size)]
    size: u64,
}

fn create_disk(args: &CreateDiskArgs) -> Result<(), String> {
    if let Some(dir) = args.path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let is_vmdk = args
        .path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vmdk"));
    let created = if is_vmdk {
        disk::create_sparse(&args.path, args.size)
    } else {
        disk::create_raw(&args.path, args.size)
    };
    created.map_err(|e| format!("can't create {}: {e}", args.path.display()))
}

/// Where the guest's screen goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum DisplayMode {
    /// Nowhere (screenshots still work).
    #[default]
    None,
    /// A window, with the guest's keyboard and pointer.
    Window,
}

#[derive(Args)]
struct RunArgs {
    /// A VM's `.vmmc` file, which says what the machine is. Disks, CD-ROMs and NICs
    /// given as options are added to its own; other options override it.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["kernel", "vm_dir"])]
    vm: Option<PathBuf>,
    /// The VM `--vm` names, once loaded.
    #[arg(skip)]
    loaded: Option<vmconfig::Vm>,
    /// Linux kernel (bzImage) to boot directly.
    #[arg(
        long,
        required_unless_present_any = ["firmware", "vm"],
        conflicts_with = "firmware"
    )]
    kernel: Option<PathBuf>,
    /// Initial ramdisk for the kernel.
    #[arg(long, requires = "kernel")]
    initrd: Option<PathBuf>,
    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0", requires = "kernel")]
    cmdline: String,
    /// UEFI firmware code (OVMF_CODE.fd) to boot [default with --vm: OVMF_CODE.fd
    /// next to vmmc-vmx, or in ..\native, as a build has it].
    #[arg(long)]
    firmware: Option<PathBuf>,
    /// Variable store a new VM starts from [default: OVMF_VARS.secboot.fd next to
    /// the firmware (Secure Boot on), or OVMF_VARS.fd if there is none; with --vm,
    /// whichever its Secure Boot setting asks for].
    #[arg(long)]
    vars_template: Option<PathBuf>,
    /// The VM's state directory (variable store, UUID); created if missing.
    #[arg(long, requires = "firmware")]
    vm_dir: Option<PathBuf>,
    /// Give the VM a TPM 2.0, its state kept in the VM directory (tpm-state.bin).
    #[arg(long)]
    tpm: bool,
    /// Write the firmware's debug log (port 0x402) to this file.
    #[arg(long)]
    firmware_log: Option<PathBuf>,
    /// Show the screen in a window, which also takes keyboard and pointer input.
    #[arg(long, value_enum, default_value_t = DisplayMode::None)]
    display: DisplayMode,
    /// The guest's screen resolution, which its firmware boots in and its display
    /// driver prefers [default: with a window, the largest standard resolution that
    /// fits on the screen; without, 1280x800].
    #[arg(long, value_name = "WxH", value_parser = parse_resolution)]
    resolution: Option<(u32, u32)>,
    /// Exit when the guest resets, rather than restarting it.
    #[arg(long)]
    no_reboot: bool,
    /// Hide the Hyper-V enlightenments (reference TSC, synthetic timers, hypercalls)
    /// that the Windows hypervisor implements for guests. With them, an idle Windows
    /// costs the host a fraction of the CPU it does without.
    #[arg(long)]
    no_enlightenments: bool,
    /// Save screenshots (PNG) here: one when the VM stops, and one on each Ctrl-A s.
    #[arg(long, value_name = "DIR")]
    screenshot: Option<PathBuf>,
    /// With --screenshot, also save one every SECS seconds.
    #[arg(long, value_name = "SECS", requires = "screenshot")]
    screenshot_every: Option<u64>,
    /// Raw disk image, with options after commas: `ro` for read-only, and where to
    /// attach it: `nvme`, `sata` or (the default) virtio. Repeat for more disks.
    #[arg(long = "disk", value_name = "PATH[,ro][,nvme|sata]")]
    disks: Vec<String>,
    /// A disc image (ISO) for a DVD-ROM drive. Repeat for more drives.
    #[arg(long = "cdrom", value_name = "ISO")]
    cdroms: Vec<PathBuf>,
    /// A network adapter on user-mode NAT: `virtio` or `e1000e`, optionally with
    /// `,mac=XX:XX:...`.
    /// Repeat for more adapters.
    #[arg(long = "nic", value_name = "MODEL[,mac=MAC]")]
    nics: Vec<String>,
    /// Number of vCPUs [default: the VM's, or 1].
    #[arg(long)]
    cpus: Option<u8>,
    /// Memory in MiB [default: the VM's, or 1024].
    #[arg(long)]
    memory: Option<u64>,
}

/// Where a build keeps the firmware: next to the program, or in `target\native` for
/// `target\debug` and `target\release`. `VMMC_FIRMWARE` names another (the tests'
/// firmware with the UEFI shell, say).
fn default_firmware() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMMC_FIRMWARE") {
        return Some(path.into());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("OVMF_CODE.fd"),
        dir.parent()?.join("native").join("OVMF_CODE.fd"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// Fills in what `--vm`'s file says, where the options don't.
fn with_vm(mut args: RunArgs) -> Result<RunArgs, String> {
    let Some(path) = &args.vm else {
        if args.tpm && args.vm_dir.is_none() {
            return Err("--tpm needs --vm or --vm-dir, where its state is kept".into());
        }
        return Ok(args);
    };
    let mut vm = vmconfig::Vm::load(path).map_err(|e| e.to_string())?;
    let config = &vm.config;
    if args.firmware.is_none() {
        args.firmware =
            Some(default_firmware().ok_or(
                "no firmware found: put OVMF_CODE.fd next to vmmc-vmx, or give --firmware",
            )?);
    }
    if args.vars_template.is_none()
        && let Some(firmware) = &args.firmware
    {
        let name = if config.firmware.secure_boot {
            "OVMF_VARS.secboot.fd"
        } else {
            "OVMF_VARS.fd"
        };
        args.vars_template = Some(firmware.with_file_name(name));
    }
    args.tpm |= config.firmware.tpm;
    args.cpus = args.cpus.or(Some(config.cpus));
    args.memory = args.memory.or(Some(config.memory_mib));
    args.resolution = args.resolution.or(config.resolution());
    args.no_enlightenments |= !config.advanced.enlightenments;
    if let Err(e) = vm.mark_powered_on() {
        tracing::warn!(%e, "can't note when the VM was started");
    }
    args.loaded = Some(vm);
    Ok(args)
}

/// The process's TPM, as the machine's CRB uses it.
struct VmTpm(tpm::Tpm);

impl TpmBackend for VmTpm {
    fn execute(&mut self, locality: u8, command: &[u8]) -> Vec<u8> {
        self.0.execute(locality, command)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// Who drives the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frontend {
    /// This terminal (and a window, with `--display window`).
    Console,
    /// Clients of the VM's pipe.
    Pipe,
}

/// The resolution of a VM without a window.
const HEADLESS_RESOLUTION: (u32, u32) = (1280, 800);

/// `WxH`, within what the display adapter offers.
fn parse_resolution(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{text:?} isn't WIDTHxHEIGHT"))?;
    let parse = |value: &str| {
        value
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("{text:?} isn't WIDTHxHEIGHT"))
    };
    let (width, height) = (parse(width)?, parse(height)?);
    let (min, max) = (machine::MIN_PREFERRED, machine::MAX_PREFERRED);
    if !(min.0..=max.0).contains(&width) || !(min.1..=max.1).contains(&height) {
        return Err(format!(
            "the resolution must be between {}x{} and {}x{}",
            min.0, min.1, max.0, max.1
        ));
    }
    Ok((width, height))
}

/// The MAC address of a directly booted VM's first NIC (QEMU's usual one).
const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// `MODEL[,mac=MAC]`. Without a MAC, NIC `index` gets `default` with its last byte
/// bumped by the index, so several NICs differ.
fn parse_nic(arg: &str, default: [u8; 6], index: usize) -> Result<NicConfig, String> {
    let mut parts = arg.split(',');
    let model = match parts.next() {
        Some("virtio") => NicModel::Virtio,
        Some("e1000e") => NicModel::E1000e,
        Some(other) => {
            return Err(format!("--nic: unknown model {other:?} (virtio or e1000e)"));
        }
        None => return Err("--nic needs a model".into()),
    };
    let mut mac = default;
    mac[5] = mac[5].wrapping_add(index as u8);
    for option in parts {
        let Some(text) = option.strip_prefix("mac=") else {
            return Err(format!("--nic: unknown option {option:?}"));
        };
        mac = parse_mac(text).ok_or_else(|| format!("--nic: bad MAC address {text:?}"))?;
    }
    Ok(NicConfig { model, mac })
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let bytes: Vec<u8> = text
        .split([':', '-'])
        .map(|part| u8::from_str_radix(part, 16).ok())
        .collect::<Option<_>>()?;
    let mac: [u8; 6] = bytes.try_into().ok()?;
    // Unicast only.
    (mac[0] & 1 == 0).then_some(mac)
}

/// `PATH[,ro][,nvme]`: options come off the end, so a path may contain commas.
fn parse_disk(arg: &str) -> DiskConfig {
    let mut path = arg;
    let mut disk = DiskConfig {
        path: PathBuf::new(),
        read_only: false,
        interface: DiskInterface::Virtio,
    };
    loop {
        if let Some(rest) = path.strip_suffix(",ro") {
            disk.read_only = true;
            path = rest;
        } else if let Some(rest) = path.strip_suffix(",nvme") {
            disk.interface = DiskInterface::Nvme;
            path = rest;
        } else if let Some(rest) = path.strip_suffix(",sata") {
            disk.interface = DiskInterface::Sata;
            path = rest;
        } else {
            break;
        }
    }
    disk.path = path.into();
    disk
}

fn exit_code(outcome: &RunOutcome) -> u8 {
    match outcome {
        RunOutcome::PowerOff | RunOutcome::Requested(_) => 0,
        RunOutcome::Reset => 2,
        RunOutcome::TripleFault => 3,
        RunOutcome::Error(_) => 1,
    }
}

fn describe(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::PowerOff => "guest powered off".into(),
        RunOutcome::Reset => "guest reset".into(),
        RunOutcome::TripleFault => "guest triple-faulted".into(),
        RunOutcome::Requested(_) => "stopped".into(),
        RunOutcome::Error(message) => format!("error: {message}"),
    }
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))
}

/// What the VM boots, loaded once and reused for every restart.
enum Boot {
    Linux {
        kernel: Vec<u8>,
        initrd: Option<Vec<u8>>,
        cmdline: String,
    },
    Firmware {
        code: Vec<u8>,
        dir: vm_dir::VmDir,
        log: Option<PathBuf>,
    },
}

impl Boot {
    fn load(args: &RunArgs) -> Result<Self, String> {
        if let Some(kernel) = &args.kernel {
            return Ok(Boot::Linux {
                kernel: read(kernel)?,
                initrd: args.initrd.as_deref().map(read).transpose()?,
                cmdline: args.cmdline.clone(),
            });
        }
        let Some(firmware) = &args.firmware else {
            return Err("give --vm, --kernel, or --firmware with --vm-dir".into());
        };
        if args.loaded.is_none() && args.vm_dir.is_none() {
            return Err("--firmware needs --vm-dir, where the VM's state is kept".into());
        }
        let template = match &args.vars_template {
            Some(template) => template.clone(),
            None => ["OVMF_VARS.secboot.fd", "OVMF_VARS.fd"]
                .map(|name| firmware.with_file_name(name))
                .into_iter()
                .find(|path| path.is_file())
                .unwrap_or_else(|| firmware.with_file_name("OVMF_VARS.fd")),
        };
        let log = args.firmware_log.clone();
        if let Some(log) = &log {
            // Each run of the process starts a fresh log; restarts append to it.
            fs::File::create(log).map_err(|e| format!("can't create {}: {e}", log.display()))?;
        }
        let dir = match (&args.loaded, &args.vm_dir) {
            (Some(vm), _) => vm_dir::open_vm(vm, &template)?,
            (None, Some(dir)) => vm_dir::open(dir, &template)?,
            (None, None) => unreachable!("checked above"),
        };
        Ok(Boot::Firmware {
            code: read(firmware)?,
            dir,
            log,
        })
    }

    fn firmware_config(&self) -> Result<Option<machine::FirmwareConfig>, String> {
        let Boot::Firmware { code, dir, log } = self else {
            return Ok(None);
        };
        let debug_log = match log {
            Some(path) => {
                let file = fs::File::options()
                    .append(true)
                    .open(path)
                    .map_err(|e| format!("can't open {}: {e}", path.display()))?;
                Some(Box::new(std::io::BufWriter::new(file)) as Box<dyn std::io::Write + Send>)
            }
            None => None,
        };
        Ok(Some(machine::FirmwareConfig {
            code: code.clone(),
            vars: dir.nvram.clone(),
            debug_log,
            uuid: dir.uuid,
        }))
    }
}

/// Every disk and disc the VM's file and the arguments name. CD-ROMs come first, so
/// they take the first SATA ports.
fn disks(args: &RunArgs) -> Vec<DiskConfig> {
    let cdrom = |path: PathBuf| DiskConfig {
        path,
        read_only: true,
        interface: DiskInterface::Cdrom,
    };
    let mut cdroms = Vec::new();
    let mut disks = Vec::new();
    if let Some(vm) = &args.loaded {
        // An empty drive has no path (and keeps its place for a disc later).
        cdroms.extend(vm.config.cdroms.iter().map(|drive| {
            cdrom(
                drive
                    .path
                    .as_deref()
                    .map(|path| vm.resolve(path))
                    .unwrap_or_default(),
            )
        }));
        disks.extend(vm.config.disks.iter().map(|disk| DiskConfig {
            path: vm.resolve(&disk.path),
            read_only: disk.read_only,
            interface: match disk.bus {
                vmconfig::Bus::Nvme => DiskInterface::Nvme,
                vmconfig::Bus::Sata => DiskInterface::Sata,
                vmconfig::Bus::Virtio => DiskInterface::Virtio,
            },
        }));
    }
    cdroms.extend(args.cdroms.iter().cloned().map(cdrom));
    disks.extend(args.disks.iter().map(|disk| parse_disk(disk)));
    cdroms.into_iter().chain(disks).collect()
}

/// The VM file's NICs, then the arguments'.
fn nics(args: &RunArgs, default_mac: [u8; 6]) -> Result<Vec<NicConfig>, String> {
    let mut nics = Vec::new();
    if let Some(vm) = &args.loaded {
        for nic in &vm.config.nics {
            nics.push(NicConfig {
                model: match nic.model {
                    vmconfig::NicModel::E1000e => NicModel::E1000e,
                    vmconfig::NicModel::Virtio => NicModel::Virtio,
                },
                mac: vmconfig::parse_mac(&nic.mac).ok_or("bad MAC address in the VM file")?,
            });
        }
    }
    let first = nics.len();
    for (index, nic) in args.nics.iter().enumerate() {
        nics.push(parse_nic(nic, default_mac, first + index)?);
    }
    Ok(nics)
}

/// Saves a screenshot of whichever machine is running every `every` seconds.
#[cfg(windows)]
fn spawn_periodic_screenshots(
    screenshots: Arc<screenshot::Screenshots>,
    targets: console::Targets,
    every: u64,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("screenshots".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(every));
                if let Some(display) = targets.display()
                    && let Err(e) = screenshots.save(&display, "periodic", true)
                {
                    tracing::warn!(%e, "periodic screenshot failed");
                }
            }
        })?;
    Ok(())
}

#[cfg(windows)]
fn run(args: RunArgs, frontend: Frontend) -> Result<RunOutcome, Box<dyn Error>> {
    use machine::layout::MIB;

    let args = with_vm(args)?;
    let boot = Boot::load(&args)?;
    let memory = args
        .memory
        .unwrap_or(1024)
        .checked_mul(MIB)
        .ok_or("memory size is too large")?;
    let whp = hv_whp::Whp::new()?;
    let default_mac = match &boot {
        Boot::Firmware { dir, .. } => dir.mac,
        Boot::Linux { .. } => DEFAULT_MAC,
    };
    let nics = nics(&args, default_mac)?;

    let screenshots = match &args.screenshot {
        Some(dir) => Some(Arc::new(
            screenshot::Screenshots::new(dir)
                .map_err(|e| format!("can't create {}: {e}", dir.display()))?,
        )),
        None => None,
    };
    let targets = console::Targets::new(screenshots.clone());
    let _raw = match frontend {
        Frontend::Console => {
            let raw = console::RawConsole::enable();
            console::spawn_input(targets.clone())?;
            Some(raw)
        }
        Frontend::Pipe => {
            let vm = args.loaded.as_ref().ok_or("serve needs --vm")?;
            serve::start(targets.clone(), &vm.config.name, &vm.config.uuid)?;
            None
        }
    };
    if let (Some(screenshots), Some(every)) = (&screenshots, args.screenshot_every) {
        spawn_periodic_screenshots(screenshots.clone(), targets.clone(), every.max(1))?;
    }
    // The TPM outlives each machine: a guest reset is a platform reset to it.
    let tpm = match (&boot, args.tpm) {
        (Boot::Firmware { dir, .. }, true) => {
            let tpm = tpm::Tpm::open(&dir.tpm_state).map_err(|e| e.to_string())?;
            Some(Arc::new(Mutex::new(VmTpm(tpm))) as SharedTpm)
        }
        _ => None,
    };
    let title = match (&boot, &args.loaded) {
        (_, Some(vm)) => format!("vmmc: {}", vm.config.name),
        (Boot::Firmware { .. }, None) => args
            .vm_dir
            .as_deref()
            .and_then(|dir| dir.file_name())
            .map_or_else(
                || "vmmc".into(),
                |name| format!("vmmc: {}", name.to_string_lossy()),
            ),
        (Boot::Linux { .. }, None) => "vmmc".into(),
    };
    let display = match frontend {
        Frontend::Console => args.display,
        Frontend::Pipe => DisplayMode::None,
    };
    let resolution = match (args.resolution, display) {
        (Some(resolution), _) => resolution,
        (None, DisplayMode::Window) => window::fitting_resolution(),
        (None, DisplayMode::None) => HEADLESS_RESOLUTION,
    };
    let vm_targets = targets.clone();
    let vm_screenshots = screenshots.clone();
    let run_vm = move || -> Result<RunOutcome, String> {
        let mut args = args;
        run_machines(
            &whp,
            &mut args,
            &boot,
            memory,
            resolution,
            &nics,
            tpm.as_ref(),
            &vm_targets,
            vm_screenshots.as_deref(),
        )
        .map_err(|e| e.to_string())
    };
    let outcome = match display {
        DisplayMode::None => run_vm(),
        // The window has the main thread; the VM runs beside it.
        DisplayMode::Window => std::thread::scope(|scope| {
            let vm = scope.spawn(move || {
                let outcome = run_vm();
                window::close();
                outcome
            });
            let shown = window::run(targets.clone(), screenshots.clone(), &title, resolution);
            let outcome = vm
                .join()
                .unwrap_or_else(|_| Err("the VM thread panicked".into()));
            shown.and(outcome)
        }),
    };
    Ok(outcome?)
}

/// Runs the VM, building a new machine each time the guest resets.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_machines(
    whp: &hv_whp::Whp,
    args: &mut RunArgs,
    boot: &Boot,
    memory: u64,
    resolution: (u32, u32),
    nics: &[NicConfig],
    tpm: Option<&SharedTpm>,
    targets: &console::Targets,
    screenshots: Option<&screenshot::Screenshots>,
) -> Result<RunOutcome, Box<dyn Error>> {
    use machine::{Machine, MachineConfig};

    // Easy Install's first boot, if the VM has one to do.
    let installer = args
        .loaded
        .as_ref()
        .and_then(|vm| install::Installer::start(vm, targets.clone()));
    loop {
        // Only one VM may have memory mapped per process, so the previous machine is
        // gone before the next one is built.
        let mut machine = Machine::new(
            whp,
            MachineConfig {
                cpus: args.cpus.unwrap_or(1),
                memory,
                disks: disks(args),
                nics: nics.to_vec(),
                serial_output: match &installer {
                    Some(installer) => installer.tap(Box::new(console::ConsoleOutput)),
                    None => Box::new(console::ConsoleOutput),
                },
                firmware: boot.firmware_config()?,
                tpm: tpm.cloned(),
                enlightenments: !args.no_enlightenments,
                resolution,
            },
        )?;
        if let Boot::Linux {
            kernel,
            initrd,
            cmdline,
        } = &boot
        {
            machine.load_linux(kernel, initrd.as_deref(), cmdline)?;
        }
        targets.set(Some(console::Target {
            serial: machine.serial_input(),
            power: machine.power_button(),
            stop: machine.stop_handle(),
            display: machine.display(),
            memory: machine.memory().clone(),
            ps2: machine.ps2_input(),
            usb: machine.usb_input(),
            discs: machine.disc_drives(),
        }));
        let outcome = machine.run();
        targets.set(None);
        // The end of an install: start again from the disk, the install discs out.
        if let (Some(installer), Ok(ended)) = (&installer, &outcome)
            && !targets.quitting()
            && let Some(vm) = installer.finish(ended)
        {
            args.loaded = Some(vm);
            drop(machine);
            continue;
        }
        let restart =
            matches!(outcome, Ok(RunOutcome::Reset)) && !args.no_reboot && !targets.quitting();
        if !restart
            && let Some(screenshots) = screenshots
            && let Err(e) = screenshots.save(&machine.display(), "exit", false)
        {
            eprint!("\r\nvmmc-vmx: screenshot failed: {e}\r\n");
        }
        drop(machine);
        let outcome = outcome?;
        if !restart {
            return Ok(outcome);
        }
        eprint!("\r\nvmmc-vmx: guest reset; restarting\r\n");
    }
}

#[cfg(not(windows))]
fn run(args: RunArgs, _frontend: Frontend) -> Result<RunOutcome, Box<dyn Error>> {
    // Keep the shared helpers in use on every platform.
    let args = with_vm(args)?;
    let _ = (
        Boot::load(&args)?.firmware_config(),
        disks(&args),
        nics(&args, DEFAULT_MAC),
    );
    Err("no hypervisor backend for this operating system yet".into())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMMC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        // Colours only on a terminal: a log file gets plain text.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let (args, frontend) = match Cli::parse().command {
        Command::Run(args) => (args, Frontend::Console),
        Command::Serve(args) => (args, Frontend::Pipe),
        Command::CreateDisk(args) => return create_disk_command(&args),
    };
    match run(*args, frontend) {
        Ok(outcome) => {
            eprintln!("\r\nvmmc-vmx: {}", describe(&outcome));
            ExitCode::from(exit_code(&outcome))
        }
        Err(e) => {
            eprintln!("vmmc-vmx: {e}");
            ExitCode::FAILURE
        }
    }
}

fn create_disk_command(args: &CreateDiskArgs) -> ExitCode {
    match create_disk(args) {
        Ok(()) => {
            eprintln!(
                "vmmc-vmx: created {} ({} GiB)",
                args.path.display(),
                args.size as f64 / f64::from(1 << 30)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("vmmc-vmx: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_parse_with_optional_suffixes() {
        let disk = parse_disk(r"C:\vms\disk.raw,ro");
        assert_eq!(disk.path, PathBuf::from(r"C:\vms\disk.raw"));
        assert!(disk.read_only);
        assert_eq!(disk.interface, DiskInterface::Virtio);
        assert!(!parse_disk("disk.raw").read_only);
        let disk = parse_disk("a,b.raw,nvme,ro");
        assert_eq!(disk.path, PathBuf::from("a,b.raw"));
        assert!(disk.read_only);
        assert_eq!(disk.interface, DiskInterface::Nvme);
        assert_eq!(parse_disk("d.raw,sata").interface, DiskInterface::Sata);
    }

    #[test]
    fn nics_parse_with_generated_or_given_macs() {
        let nic = parse_nic("virtio", DEFAULT_MAC, 0).unwrap();
        assert_eq!((nic.model, nic.mac), (NicModel::Virtio, DEFAULT_MAC));
        assert_eq!(parse_nic("virtio", DEFAULT_MAC, 1).unwrap().mac[5], 0x57);
        let given = parse_nic("virtio,mac=02:00:00:aa:bb:cc", DEFAULT_MAC, 0).unwrap();
        assert_eq!(given.mac, [2, 0, 0, 0xaa, 0xbb, 0xcc]);
        assert!(
            parse_nic("virtio,mac=01:00:00:00:00:00", DEFAULT_MAC, 0).is_err(),
            "multicast"
        );
        assert!(parse_nic("rtl8139", DEFAULT_MAC, 0).is_err());
    }

    #[test]
    fn cdroms_come_first_and_read_only() {
        let args = parse(&["--kernel", "k", "--disk", "a.raw,sata", "--cdrom", "x.iso"]).unwrap();
        let disks = disks(&args);
        assert_eq!(disks[0].interface, DiskInterface::Cdrom);
        assert!(disks[0].read_only);
        assert_eq!(disks[1].interface, DiskInterface::Sata);
    }

    #[test]
    fn resolutions_parse_within_the_adapter_limits() {
        assert_eq!(parse_resolution("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_resolution("2560X1440"), Ok((2560, 1440)));
        assert!(parse_resolution("1920").is_err());
        assert!(parse_resolution("320x200").is_err());
        assert!(parse_resolution("7680x4320").is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&RunOutcome::PowerOff), 0);
        assert_eq!(exit_code(&RunOutcome::Requested(console::QUIT_CODE)), 0);
        assert_eq!(exit_code(&RunOutcome::Reset), 2);
        assert_eq!(exit_code(&RunOutcome::TripleFault), 3);
        assert_eq!(exit_code(&RunOutcome::Error("x".into())), 1);
    }

    fn parse(args: &[&str]) -> Result<RunArgs, clap::Error> {
        let mut full = vec!["vmmc-vmx", "run"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).map(|cli| match cli.command {
            Command::Run(args) | Command::Serve(args) => *args,
            Command::CreateDisk(_) => unreachable!("parsed a run command"),
        })
    }

    #[test]
    fn create_disk_makes_its_directory_and_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let args = CreateDiskArgs {
            path: dir.path().join("vm").join("disk.raw"),
            size: 1 << 20,
        };
        create_disk(&args).unwrap();
        assert_eq!(fs::metadata(&args.path).unwrap().len(), 1 << 20);
        assert!(create_disk(&args).is_err(), "it exists already");
    }

    #[test]
    fn kernel_command_line_parses() {
        let args = parse(&[
            "--kernel", "k", "--disk", "a.raw", "--disk", "b.raw,ro", "--cpus", "4", "--memory",
            "2048",
        ])
        .unwrap();
        assert_eq!(
            (args.cpus, args.memory, args.disks.len()),
            (Some(4), Some(2048), 2)
        );
        assert_eq!(args.cmdline, "console=ttyS0");
        assert!(!args.no_reboot);
    }

    #[test]
    fn firmware_needs_a_vm_dir_and_excludes_a_kernel() {
        let args = parse(&["--firmware", "OVMF_CODE.fd", "--vm-dir", "vm"]).unwrap();
        assert_eq!(args.vm_dir, Some(PathBuf::from("vm")));
        let alone = parse(&["--firmware", "OVMF_CODE.fd"]).unwrap();
        assert!(Boot::load(&alone).err().unwrap().contains("--vm-dir"));
        assert!(parse(&["--firmware", "f", "--vm-dir", "vm", "--kernel", "k"]).is_err());
        assert!(parse(&["--vm-dir", "vm"]).is_err(), "nothing to boot");
        assert!(parse(&["--vm", "x.vmmc", "--kernel", "k"]).is_err());
        assert!(parse(&["--vm", "x.vmmc", "--vm-dir", "d"]).is_err());
        let tpm = parse(&["--kernel", "k", "--tpm"]).unwrap();
        assert!(with_vm(tpm).err().unwrap().contains("--tpm"));
    }

    #[test]
    fn a_vm_file_supplies_the_machine_and_options_add_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = vmconfig::VmConfig::new("Test VM", "linux", 2, 3072);
        config.disks.push(vmconfig::Disk {
            path: "test.vmdk".into(),
            bus: vmconfig::Bus::Nvme,
            read_only: false,
        });
        config.cdroms.push(vmconfig::Cdrom {
            path: Some("install.iso".into()),
            install_media: false,
        });
        config.firmware.secure_boot = false;
        config.display.resolution = Some("1600x900".into());
        let path = dir.path().join("Test VM.vmmc");
        let vm = vmconfig::Vm {
            path: path.clone(),
            config,
        };
        vm.save().unwrap();
        let path = path.to_str().unwrap();
        let args = parse(&[
            "--vm",
            path,
            "--firmware",
            "fw/OVMF_CODE.fd",
            "--cdrom",
            "answer.iso",
            "--nic",
            "virtio",
            "--cpus",
            "4",
        ])
        .unwrap();
        let args = with_vm(args).unwrap();
        assert_eq!((args.cpus, args.memory), (Some(4), Some(3072)));
        assert!(args.tpm, "the file's TPM");
        assert_eq!(args.resolution, Some((1600, 900)));
        assert_eq!(
            args.vars_template,
            Some(PathBuf::from("fw/OVMF_VARS.fd")),
            "Secure Boot off"
        );
        let disks = disks(&args);
        assert_eq!(disks.len(), 3);
        assert_eq!(disks[0].path, dir.path().join("install.iso"));
        assert_eq!(disks[1].path, PathBuf::from("answer.iso"));
        assert_eq!(disks[2].path, dir.path().join("test.vmdk"));
        assert_eq!(disks[2].interface, DiskInterface::Nvme);
        let nics = nics(&args, DEFAULT_MAC).unwrap();
        assert_eq!(nics.len(), 2);
        assert_eq!(nics[0].model, NicModel::E1000e);
        assert_eq!(nics[1].model, NicModel::Virtio);
        let saved = vmconfig::Vm::load(Path::new(path)).unwrap();
        assert!(saved.config.last_powered_on.is_some(), "start noted");
    }
}
