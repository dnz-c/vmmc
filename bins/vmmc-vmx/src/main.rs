//! vmmc-vmx: runs one virtual machine, with its serial console on this terminal.
//!
//! ```text
//! vmmc-vmx run --kernel bzImage --initrd initramfs.cpio --disk disk.raw --cpus 4
//! ```
//!
//! Exit status: 0 when the guest powers off (or the user quits with Ctrl-A x), 2 when it
//! resets, 3 on a triple fault, 1 on any error. Ctrl-A p presses the ACPI power button.

mod console;

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use machine::{DiskConfig, RunOutcome};

#[derive(Parser)]
#[command(version, about = "Runs one vmmc virtual machine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot a Linux kernel directly, with the serial console on this terminal.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Linux kernel (bzImage) to boot.
    #[arg(long)]
    kernel: PathBuf,
    /// Initial ramdisk.
    #[arg(long)]
    initrd: Option<PathBuf>,
    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0")]
    cmdline: String,
    /// Raw disk image, as PATH or PATH,ro. Repeat for more disks.
    #[arg(long = "disk", value_name = "PATH[,ro]")]
    disks: Vec<String>,
    /// Number of vCPUs.
    #[arg(long, default_value_t = 1)]
    cpus: u8,
    /// Memory in MiB.
    #[arg(long, default_value_t = 1024)]
    memory: u64,
}

fn parse_disk(arg: &str) -> DiskConfig {
    match arg.strip_suffix(",ro") {
        Some(path) => DiskConfig {
            path: path.into(),
            read_only: true,
        },
        None => DiskConfig {
            path: arg.into(),
            read_only: false,
        },
    }
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
        RunOutcome::Reset => "guest reset (restarting isn't supported yet)".into(),
        RunOutcome::TripleFault => "guest triple-faulted".into(),
        RunOutcome::Requested(_) => "stopped".into(),
        RunOutcome::Error(message) => format!("error: {message}"),
    }
}

#[cfg(windows)]
fn run(args: RunArgs) -> Result<RunOutcome, Box<dyn Error>> {
    use machine::{Machine, MachineConfig, layout::MIB};

    let kernel =
        fs::read(&args.kernel).map_err(|e| format!("can't read {}: {e}", args.kernel.display()))?;
    let initrd = match &args.initrd {
        Some(path) => {
            Some(fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?)
        }
        None => None,
    };
    let memory = args
        .memory
        .checked_mul(MIB)
        .ok_or("memory size is too large")?;

    let whp = hv_whp::Whp::new()?;
    let mut machine = Machine::new(
        &whp,
        MachineConfig {
            cpus: args.cpus,
            memory,
            disks: args.disks.iter().map(|disk| parse_disk(disk)).collect(),
            serial_output: Box::new(console::ConsoleOutput),
        },
    )?;
    machine.load_linux(&kernel, initrd.as_deref(), &args.cmdline)?;

    let raw = console::RawConsole::enable();
    console::spawn_input(
        machine.serial_input(),
        machine.power_button(),
        machine.stop_handle(),
    )?;
    let outcome = machine.run();
    drop(raw);
    Ok(outcome?)
}

#[cfg(not(windows))]
fn run(_: RunArgs) -> Result<RunOutcome, Box<dyn Error>> {
    // Keep the shared helpers in use on every platform.
    let _ = (fs::read::<&str>, parse_disk);
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

    let Command::Run(args) = Cli::parse().command;
    match run(args) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_parse_with_optional_read_only_suffix() {
        let disk = parse_disk(r"C:\vms\disk.raw,ro");
        assert_eq!(disk.path, PathBuf::from(r"C:\vms\disk.raw"));
        assert!(disk.read_only);
        assert!(!parse_disk("disk.raw").read_only);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&RunOutcome::PowerOff), 0);
        assert_eq!(exit_code(&RunOutcome::Requested(console::QUIT_CODE)), 0);
        assert_eq!(exit_code(&RunOutcome::Reset), 2);
        assert_eq!(exit_code(&RunOutcome::TripleFault), 3);
        assert_eq!(exit_code(&RunOutcome::Error("x".into())), 1);
    }

    #[test]
    fn command_line_parses() {
        let cli = Cli::try_parse_from([
            "vmmc-vmx", "run", "--kernel", "k", "--disk", "a.raw", "--disk", "b.raw,ro", "--cpus",
            "4", "--memory", "2048",
        ])
        .unwrap();
        let Command::Run(args) = cli.command;
        assert_eq!((args.cpus, args.memory, args.disks.len()), (4, 2048, 2));
        assert_eq!(args.cmdline, "console=ttyS0");
    }
}
