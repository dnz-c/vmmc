//! vmmc-cli: vmmc from the command line.
//!
//! ```text
//! vmmc-cli create "Windows 11" --os windows11 --iso Win11.iso
//! vmmc-cli import "D:\VMs\Windows 10\Windows 10.vmx"
//! vmmc-cli list
//! vmmc-cli disk create disk.vmdk --size 64G
//! vmmc-cli disk info disk.vmdk
//! vmmc-cli disk convert disk.raw disk.vmdk
//! ```
//!
//! A VM made here runs with `vmmc-vmx run --vm FILE`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use vmconfig::create::{NewVm, create_vm};
use vmconfig::library::{self, Library};
use vmconfig::presets::{self, PRESETS};

#[derive(Parser)]
#[command(
    version,
    about = "vmmc from the command line: VMs, the library and disks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Make a new VM: a folder with its config (.vmmc) and an empty disk, added to the
    /// library.
    Create(CreateArgs),
    /// Open a VMware VM in place. vmmc's files go in a `vmmc` folder beside the .vmx,
    /// and its writes go to delta disks there: VMware's files are never changed, and
    /// deleting that folder leaves the VMware VM as it was.
    Import {
        vmx: PathBuf,
        /// Put vmmc's files here instead.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The library folder to put it in.
        #[arg(long)]
        folder: Option<String>,
    },
    /// List the library's VMs.
    List,
    /// Add an existing VM (its .vmmc file) to the library.
    Add {
        file: PathBuf,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Take a VM out of the library. Its files stay.
    Remove { file: PathBuf },
    /// Disk images.
    #[command(subcommand)]
    Disk(DiskCommand),
}

#[derive(Args)]
struct CreateArgs {
    name: String,
    /// What the VM is for, which sets its hardware: windows11, windows10, ubuntu,
    /// linux or other.
    #[arg(long, default_value = "windows11")]
    os: String,
    /// Where the VM's folder goes [default: Documents\vmmc VMs].
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    cpus: Option<u8>,
    /// Memory in MiB.
    #[arg(long)]
    memory: Option<u64>,
    /// The disk's size, like 64G.
    #[arg(long, value_parser = vmconfig::parse_size)]
    disk_size: Option<u64>,
    /// Use this disk image rather than making one.
    #[arg(long, conflicts_with = "disk_size")]
    disk: Option<PathBuf>,
    /// An installation disc for the DVD drive.
    #[arg(long)]
    iso: Option<PathBuf>,
    /// Take over a state directory of `vmmc-vmx run --vm-dir`: its UUID, MAC, UEFI
    /// variables and TPM.
    #[arg(long)]
    from_vm_dir: Option<PathBuf>,
    /// The library folder to put it in.
    #[arg(long)]
    folder: Option<String>,
}

#[derive(Subcommand)]
enum DiskCommand {
    /// Make an empty disk: a VMDK if the name ends in .vmdk (it grows as it's
    /// written), otherwise a raw image.
    Create {
        path: PathBuf,
        #[arg(long, value_parser = vmconfig::parse_size)]
        size: u64,
    },
    /// What a disk is: its format, size, space used, and the disks beneath it.
    Info { path: PathBuf },
    /// Copy a disk into a new one, in the format its name says (.vmdk: VMDK, otherwise
    /// raw). Reads any VMDK VMware makes; writes monolithic sparse VMDKs.
    Convert { from: PathBuf, to: PathBuf },
}

fn is_vmdk(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vmdk"))
}

fn create_disk(path: &Path, size: u64) -> Result<(), String> {
    let created = if is_vmdk(path) {
        disk::create_sparse(path, size)
    } else {
        disk::create_raw(path, size)
    };
    created.map_err(|e| format!("can't create {}: {e}", path.display()))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1 << 30))
}

fn disk_info(path: &Path) -> Result<String, String> {
    use std::fmt::Write as _;

    if !is_vmdk(path) {
        let disk = disk::open(path, true).map_err(|e| e.to_string())?;
        return Ok(format!("raw image, {}", gib(disk.len())));
    }
    let vmdk = disk::vmdk::Vmdk::open(path, true).map_err(|e| e.to_string())?;
    let mut text = String::new();
    let mut layer = Some(&vmdk);
    let mut depth = 0;
    while let Some(disk) = layer {
        let descriptor = disk.descriptor();
        let indent = "  ".repeat(depth);
        let _ = writeln!(
            text,
            "{indent}{}: {}, {} ({} used), CID {:08x}",
            disk.path().display(),
            descriptor.create_type,
            gib(disk::BlockBackend::len(disk)),
            gib(disk.allocated()),
            descriptor.cid
        );
        if depth == 0 && disk.parent().is_some() {
            let _ = writeln!(text, "{indent}on top of:");
        }
        layer = disk.parent();
        depth += 1;
    }
    Ok(text.trim_end().to_owned())
}

fn convert(from: &Path, to: &Path) -> Result<(), String> {
    let source = disk::open(from, true).map_err(|e| format!("{}: {e}", from.display()))?;
    create_disk(to, source.len())?;
    let target = disk::open(to, false).map_err(|e| format!("{}: {e}", to.display()))?;
    let total = source.len();
    let mut shown = 0;
    disk::copy(&*source, &*target, |done| {
        let percent = done * 100 / total.max(1);
        if percent != shown {
            shown = percent;
            eprint!("\r{percent}%");
            let _ = std::io::stderr().flush();
        }
    })
    .map_err(|e| format!("copying failed: {e}"))?;
    eprintln!();
    Ok(())
}

fn create(args: &CreateArgs) -> Result<PathBuf, String> {
    let preset = presets::find(&args.os).ok_or_else(|| {
        let ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        format!("--os {:?}: one of {}", args.os, ids.join(", "))
    })?;
    let parent = args.dir.clone().unwrap_or_else(library::default_vm_root);
    let vm = create_vm(&NewVm {
        name: &args.name,
        parent_dir: &parent,
        preset,
        cpus: args.cpus,
        memory_mib: args.memory,
        disk_bytes: args.disk_size,
        iso: args.iso.as_deref(),
        adopt: args.from_vm_dir.as_deref(),
        existing_disk: args.disk.as_deref(),
    })?;
    let mut library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    library.add(&vm.path, args.folder.as_deref());
    library.save().map_err(|e| e.to_string())?;
    Ok(vm.path)
}

fn list() -> Result<String, String> {
    let library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    if library.entries().is_empty() {
        return Ok("no VMs yet: make one with `vmmc-cli create`".into());
    }
    let mut lines = Vec::new();
    for entry in library.entries() {
        let path = Path::new(&entry.path);
        let folder = entry
            .folder
            .as_deref()
            .map(|f| format!("{f}/"))
            .unwrap_or_default();
        match vmconfig::Vm::load(path) {
            Ok(vm) => {
                let started = vm
                    .config
                    .last_powered_on
                    .as_deref()
                    .map_or("never started".into(), |t| format!("last started {t}"));
                lines.push(format!(
                    "{folder}{}  ({} vCPUs, {} MiB, {started})\n    {}",
                    vm.config.name,
                    vm.config.cpus,
                    vm.config.memory_mib,
                    path.display()
                ));
            }
            Err(e) => lines.push(format!("{folder}?  {e}")),
        }
    }
    Ok(lines.join("\n"))
}

fn library_change(change: impl FnOnce(&mut Library) -> Result<(), String>) -> Result<(), String> {
    let mut library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    change(&mut library)?;
    library.save().map_err(|e| e.to_string())
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Create(args) => {
            let path = create(&args)?;
            println!("created {}", path.display());
            println!(
                "run it with: vmmc-vmx run --vm \"{}\" --display window",
                path.display()
            );
        }
        Command::Import {
            vmx,
            state_dir,
            folder,
        } => {
            let imported = vmconfig::import::import(&vmx, state_dir.as_deref())?;
            library_change(|library| {
                library.add(&imported.vm.path, folder.as_deref());
                Ok(())
            })?;
            println!("imported {}", imported.vm.path.display());
            for warning in &imported.warnings {
                println!("  note: {warning}");
            }
            println!(
                "run it with: vmmc-vmx run --vm \"{}\" --display window",
                imported.vm.path.display()
            );
        }
        Command::List => println!("{}", list()?),
        Command::Add { file, folder } => {
            let file = std::path::absolute(&file).map_err(|e| e.to_string())?;
            vmconfig::Vm::load(&file).map_err(|e| e.to_string())?;
            library_change(|library| {
                library.add(&file, folder.as_deref());
                Ok(())
            })?;
        }
        Command::Remove { file } => {
            let file = std::path::absolute(&file).map_err(|e| e.to_string())?;
            library_change(|library| {
                if library.remove(&file) {
                    Ok(())
                } else {
                    Err(format!("{} isn't in the library", file.display()))
                }
            })?;
        }
        Command::Disk(DiskCommand::Create { path, size }) => {
            create_disk(&path, size)?;
            println!("created {} ({})", path.display(), gib(size));
        }
        Command::Disk(DiskCommand::Info { path }) => println!("{}", disk_info(&path)?),
        Command::Disk(DiskCommand::Convert { from, to }) => {
            convert(&from, &to)?;
            println!("converted {} to {}", from.display(), to.display());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmmc-cli: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn converts_raw_to_vmdk_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("a.raw");
        create_disk(&raw, 8 << 20).unwrap();
        {
            let disk = disk::open(&raw, false).unwrap();
            disk.write_at(b"start", 0).unwrap();
            disk.write_at(b"end", (8 << 20) - 3).unwrap();
        }
        let vmdk = dir.path().join("a.vmdk");
        convert(&raw, &vmdk).unwrap();
        let info = disk_info(&vmdk).unwrap();
        assert!(info.contains("monolithicSparse"), "{info}");
        // Only the two grains with data take space.
        let used = fs::metadata(&vmdk).unwrap().len();
        assert!(used < 300 << 10, "{used}");
        let back = dir.path().join("b.raw");
        convert(&vmdk, &back).unwrap();
        assert_eq!(fs::read(&raw).unwrap(), fs::read(&back).unwrap());
        assert!(convert(&raw, &vmdk).is_err(), "never replaces a file");
    }

    #[test]
    fn info_shows_a_delta_s_parents() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.vmdk");
        let delta = dir.path().join("delta.vmdk");
        create_disk(&base, 1 << 20).unwrap();
        disk::create_delta(&delta, &base).unwrap();
        let info = disk_info(&delta).unwrap();
        assert!(info.contains("on top of:"), "{info}");
        assert!(info.contains("base.vmdk"), "{info}");
    }

    #[test]
    fn the_command_line_parses() {
        let cli = Cli::try_parse_from([
            "vmmc-cli",
            "create",
            "My VM",
            "--os",
            "ubuntu",
            "--disk-size",
            "32G",
        ])
        .unwrap();
        let Command::Create(args) = cli.command else {
            panic!("not create");
        };
        assert_eq!(args.disk_size, Some(32 << 30));
        assert!(
            Cli::try_parse_from([
                "vmmc-cli",
                "create",
                "x",
                "--disk",
                "d",
                "--disk-size",
                "1G"
            ])
            .is_err()
        );
    }
}
