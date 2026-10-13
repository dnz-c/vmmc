//! vmmc-vmx: runs one virtual machine, with its serial console on this terminal.
//!
//! ```text
//! vmmc-vmx run --kernel bzImage --initrd initramfs.cpio --disk disk.raw --cpus 4
//! vmmc-vmx run --firmware OVMF_CODE.fd --vm-dir my-vm --disk disk.raw
//! vmmc-vmx create-disk disk.raw --size 64G
//! ```
//!
//! A guest reset restarts the VM from scratch (unless `--no-reboot`). Exit status: 0
//! when the guest powers off (or the user quits with Ctrl-A x), 2 when it resets with
//! `--no-reboot`, 3 on a triple fault, 1 on any error. Ctrl-A p presses the ACPI power
//! button, Ctrl-A r prints each vCPU's registers, and Ctrl-A m saves guest RAM.

mod console;
mod memdump;
mod screenshot;
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
    /// Create an empty raw disk image for `run --disk`. On NTFS it's a sparse file,
    /// which takes no space until the guest writes to it.
    CreateDisk(CreateDiskArgs),
}

#[derive(Args)]
struct CreateDiskArgs {
    /// The image to create (with its directory). An existing file is never replaced.
    path: PathBuf,
    /// Its size: bytes, or with a K, M, G or T suffix (powers of 1024), like 64G.
    #[arg(long, value_parser = parse_size)]
    size: u64,
}

/// A disk size like `64G`, `500M` or `1T` (powers of 1024), in whole sectors.
fn parse_size(text: &str) -> Result<u64, String> {
    let invalid = || format!("{text:?} isn't a size like 64G or 500M");
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let unit = unit.trim().to_ascii_uppercase();
    let unit = unit
        .strip_suffix("IB")
        .or_else(|| unit.strip_suffix('B'))
        .unwrap_or(&unit);
    let shift = match unit {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(invalid()),
    };
    let size = digits
        .parse::<u64>()
        .map_err(|_| invalid())?
        .checked_mul(1 << shift)
        .ok_or_else(invalid)?;
    if size == 0 || !size.is_multiple_of(512) {
        return Err(format!(
            "{text:?}: a disk is a whole number of 512-byte sectors"
        ));
    }
    Ok(size)
}

fn create_disk(args: &CreateDiskArgs) -> Result<(), String> {
    if let Some(dir) = args.path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    disk::create_raw(&args.path, args.size)
        .map_err(|e| format!("can't create {}: {e}", args.path.display()))
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
    /// Linux kernel (bzImage) to boot directly.
    #[arg(
        long,
        required_unless_present = "firmware",
        conflicts_with = "firmware"
    )]
    kernel: Option<PathBuf>,
    /// Initial ramdisk for the kernel.
    #[arg(long, requires = "kernel")]
    initrd: Option<PathBuf>,
    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0", requires = "kernel")]
    cmdline: String,
    /// UEFI firmware code (OVMF_CODE.fd) to boot.
    #[arg(long, requires = "vm_dir")]
    firmware: Option<PathBuf>,
    /// Variable store a new VM starts from [default: OVMF_VARS.secboot.fd next to
    /// the firmware (Secure Boot on), or OVMF_VARS.fd if there is none].
    #[arg(long, requires = "firmware")]
    vars_template: Option<PathBuf>,
    /// The VM's state directory (variable store, UUID); created if missing.
    #[arg(long, requires = "firmware")]
    vm_dir: Option<PathBuf>,
    /// Give the VM a TPM 2.0, its state kept in the VM directory (tpm-state.bin).
    #[arg(long, requires = "vm_dir")]
    tpm: bool,
    /// Write the firmware's debug log (port 0x402) to this file.
    #[arg(long, requires = "firmware")]
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
    /// Expose Hyper-V enlightenments (reference TSC, synthetic timers, hypercalls),
    /// which the Windows hypervisor implements itself.
    #[arg(long)]
    enlightenments: bool,
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
    /// Number of vCPUs.
    #[arg(long, default_value_t = 1)]
    cpus: u8,
    /// Memory in MiB.
    #[arg(long, default_value_t = 1024)]
    memory: u64,
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
        let (Some(firmware), Some(dir)) = (&args.firmware, &args.vm_dir) else {
            return Err("give either --kernel, or --firmware with --vm-dir".into());
        };
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
        Ok(Boot::Firmware {
            code: read(firmware)?,
            dir: vm_dir::open(dir, &template)?,
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

/// Every disk and disc the arguments name. CD-ROMs come first, so they take the first
/// SATA ports.
fn disks(args: &RunArgs) -> Vec<DiskConfig> {
    let cdroms = args.cdroms.iter().map(|path| DiskConfig {
        path: path.clone(),
        read_only: true,
        interface: DiskInterface::Cdrom,
    });
    cdroms
        .chain(args.disks.iter().map(|disk| parse_disk(disk)))
        .collect()
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
fn run(args: RunArgs) -> Result<RunOutcome, Box<dyn Error>> {
    use machine::layout::MIB;

    let boot = Boot::load(&args)?;
    let memory = args
        .memory
        .checked_mul(MIB)
        .ok_or("memory size is too large")?;
    let whp = hv_whp::Whp::new()?;
    let default_mac = match &boot {
        Boot::Firmware { dir, .. } => dir.mac,
        Boot::Linux { .. } => DEFAULT_MAC,
    };
    let nics = args
        .nics
        .iter()
        .enumerate()
        .map(|(index, nic)| parse_nic(nic, default_mac, index))
        .collect::<Result<Vec<_>, _>>()?;

    let screenshots = match &args.screenshot {
        Some(dir) => Some(Arc::new(
            screenshot::Screenshots::new(dir)
                .map_err(|e| format!("can't create {}: {e}", dir.display()))?,
        )),
        None => None,
    };
    let targets = console::Targets::new(screenshots.clone());
    let _raw = console::RawConsole::enable();
    console::spawn_input(targets.clone())?;
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
    let title = match &boot {
        Boot::Firmware { .. } => args
            .vm_dir
            .as_deref()
            .and_then(|dir| dir.file_name())
            .map_or_else(
                || "vmmc".into(),
                |name| format!("vmmc: {}", name.to_string_lossy()),
            ),
        Boot::Linux { .. } => "vmmc".into(),
    };
    let display = args.display;
    let resolution = match (args.resolution, display) {
        (Some(resolution), _) => resolution,
        (None, DisplayMode::Window) => window::fitting_resolution(),
        (None, DisplayMode::None) => HEADLESS_RESOLUTION,
    };
    let vm_targets = targets.clone();
    let vm_screenshots = screenshots.clone();
    let run_vm = move || -> Result<RunOutcome, String> {
        run_machines(
            &whp,
            &args,
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
    args: &RunArgs,
    boot: &Boot,
    memory: u64,
    resolution: (u32, u32),
    nics: &[NicConfig],
    tpm: Option<&SharedTpm>,
    targets: &console::Targets,
    screenshots: Option<&screenshot::Screenshots>,
) -> Result<RunOutcome, Box<dyn Error>> {
    use machine::{Machine, MachineConfig};

    loop {
        // Only one VM may have memory mapped per process, so the previous machine is
        // gone before the next one is built.
        let mut machine = Machine::new(
            whp,
            MachineConfig {
                cpus: args.cpus,
                memory,
                disks: disks(args),
                nics: nics.to_vec(),
                serial_output: Box::new(console::ConsoleOutput),
                firmware: boot.firmware_config()?,
                tpm: tpm.cloned(),
                enlightenments: args.enlightenments,
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
        }));
        let outcome = machine.run();
        targets.set(None);
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
fn run(args: RunArgs) -> Result<RunOutcome, Box<dyn Error>> {
    // Keep the shared helpers in use on every platform.
    let _ = (Boot::load(&args)?.firmware_config(), disks(&args));
    Err("no hypervisor backend for this operating system yet".into())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMMC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Run(args) => match run(*args) {
            Ok(outcome) => {
                eprintln!("\r\nvmmc-vmx: {}", describe(&outcome));
                ExitCode::from(exit_code(&outcome))
            }
            Err(e) => {
                eprintln!("vmmc-vmx: {e}");
                ExitCode::FAILURE
            }
        },
        Command::CreateDisk(args) => match create_disk(&args) {
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
        },
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
            Command::Run(args) => *args,
            Command::CreateDisk(_) => unreachable!("parsed a run command"),
        })
    }

    #[test]
    fn disk_sizes_parse_in_powers_of_1024() {
        assert_eq!(parse_size("64G"), Ok(64 << 30));
        assert_eq!(parse_size("64GiB"), Ok(64 << 30));
        assert_eq!(parse_size("500m"), Ok(500 << 20));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert!(parse_size("64X").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("0").is_err());
        assert!(parse_size("1000").is_err(), "not whole sectors");
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
        assert_eq!((args.cpus, args.memory, args.disks.len()), (4, 2048, 2));
        assert_eq!(args.cmdline, "console=ttyS0");
        assert!(!args.no_reboot);
    }

    #[test]
    fn firmware_needs_a_vm_dir_and_excludes_a_kernel() {
        let args = parse(&["--firmware", "OVMF_CODE.fd", "--vm-dir", "vm"]).unwrap();
        assert_eq!(args.vm_dir, Some(PathBuf::from("vm")));
        assert!(parse(&["--firmware", "OVMF_CODE.fd"]).is_err());
        assert!(parse(&["--firmware", "f", "--vm-dir", "vm", "--kernel", "k"]).is_err());
        assert!(parse(&["--vm-dir", "vm"]).is_err(), "nothing to boot");
        assert!(parse(&["--kernel", "k", "--firmware-log", "log"]).is_err());
    }
}
