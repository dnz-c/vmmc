//! vmmc-cli: vmmc from the command line.
//!
//! ```text
//! vmmc-cli create "Windows 11" --os windows11 --iso Win11.iso
//! vmmc-cli import "D:\VMs\Windows 10\Windows 10.vmx"
//! vmmc-cli list
//! vmmc-cli start "Windows 11"
//! vmmc-cli status "Windows 11"
//! vmmc-cli stop "Windows 11"
//! vmmc-cli disk create disk.vmdk --size 64G
//! vmmc-cli disk info disk.vmdk
//! vmmc-cli disk convert disk.raw disk.vmdk
//! ```
//!
//! `start` runs a VM in the background, in a sandboxed worker (`vmmc-vmx serve`); the
//! other commands talk to it. A VM is named by its `.vmmc` file or its name in the
//! library. (`vmmc-vmx run --vm FILE` runs one on the terminal instead.)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use vmconfig::create::{EasyInstall, NewVm, create_vm};
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
    Create(Box<CreateArgs>),
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
    /// Start a VM in the background, sandboxed. Its output goes to vmmc-vmx.log in its
    /// folder.
    Start { vm: String },
    /// Shut a VM down: press its power button, or with --hard, stop it at once.
    Stop {
        vm: String,
        #[arg(long)]
        hard: bool,
    },
    /// Reset a VM, as its reset button would.
    Reset { vm: String },
    /// Whether a VM runs, and how.
    Status { vm: String },
    /// Put the vmmc Tools disc (the display driver, for Windows guests) into a running
    /// VM's DVD drive.
    Tools { vm: String },
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
    /// Snapshots: a VM as it was, to go back to.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Save a running VM's state and stop it; its next start resumes it.
    Suspend { vm: String },
    /// Make a new VM from one: of a snapshot of it, or of it as it is (with it off). A
    /// full clone copies its disks; a linked clone (of a snapshot) writes to deltas on
    /// the snapshot's disks, which then can't be deleted while the clone is there. The
    /// clone gets a new UUID, MAC address, UEFI variables and TPM.
    Clone {
        vm: String,
        name: String,
        /// The snapshot to clone, by name or ID.
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(long)]
        linked: bool,
    },
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Take a snapshot: of a running VM with its memory, or of one that's off.
    Create {
        vm: String,
        name: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// A VM's snapshots, as the tree they make.
    List { vm: String },
    /// Go back to a snapshot (by name or ID): what the VM did since is gone.
    Revert { vm: String, snapshot: String },
    /// Delete a snapshot. Its disk changes merge into the disks beneath, at once with the
    /// VM off, or when it stops.
    Delete { vm: String, snapshot: String },
}

/// A VM's snapshots, indented under the one each was taken from.
fn snapshot_tree(tree: &vmconfig::snapshots::Tree) -> String {
    fn walk(
        tree: &vmconfig::snapshots::Tree,
        parent: Option<&str>,
        depth: usize,
        lines: &mut Vec<String>,
    ) {
        for snapshot in tree.children(parent) {
            let mut line = format!(
                "{}{}  ({}{})",
                "  ".repeat(depth),
                snapshot.name,
                snapshot.created,
                if snapshot.running {
                    ", with memory"
                } else {
                    ""
                }
            );
            if tree.current.as_deref() == Some(snapshot.id.as_str()) {
                line += "  <- the VM goes on from here";
            }
            lines.push(line);
            if !snapshot.description.is_empty() {
                lines.push(format!("{}  {}", "  ".repeat(depth), snapshot.description));
            }
            walk(tree, Some(&snapshot.id), depth + 1, lines);
        }
    }
    let mut lines = Vec::new();
    walk(tree, None, 0, &mut lines);
    if lines.is_empty() {
        "no snapshots".into()
    } else {
        lines.join("\n")
    }
}

fn snapshot_command(command: SnapshotCommand) -> Result<(), String> {
    match command {
        SnapshotCommand::List { vm } => {
            let vm = find_vm(&vm)?;
            let tree = vmconfig::snapshots::Tree::load(&vm).map_err(|e| e.to_string())?;
            println!("{}", snapshot_tree(&tree));
        }
        SnapshotCommand::Delete { vm, snapshot } => {
            let mut vm = find_vm(&vm)?;
            println!("{}", vmmc_client::delete_snapshot(&mut vm, &snapshot)?);
        }
        #[cfg(windows)]
        SnapshotCommand::Create {
            vm,
            name,
            description,
        } => {
            let mut vm = find_vm(&vm)?;
            println!(
                "{}",
                vmmc_client::take_snapshot(&mut vm, &name, &description)?
            );
        }
        #[cfg(windows)]
        SnapshotCommand::Revert { vm, snapshot } => {
            let mut vm = find_vm(&vm)?;
            println!("{}", vmmc_client::revert(&mut vm, &snapshot)?);
        }
        #[cfg(not(windows))]
        SnapshotCommand::Create { .. } | SnapshotCommand::Revert { .. } => {
            return Err("snapshots need Windows for now".into());
        }
    }
    Ok(())
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
    /// Install the OS from --iso unattended (Windows and Ubuntu): vmmc makes an answer
    /// disc, and types what the installer's first screens need.
    #[arg(long, requires = "iso", requires = "password")]
    easy_install: bool,
    /// Easy Install's user [default: yours].
    #[arg(long)]
    user: Option<String>,
    /// Easy Install's password for that user.
    #[arg(long)]
    password: Option<String>,
    /// Easy Install's Windows edition, as Setup names it [default: Windows 11 Pro or
    /// Windows 10 Pro].
    #[arg(long)]
    edition: Option<String>,
    /// Easy Install's keyboard layout for Linux, like `us` or `de` [default: yours].
    #[arg(long)]
    keyboard: Option<String>,
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
    /// Make a VM's disk bigger (it's written anew). The guest then has free space at
    /// the end of the disk, for its partition to grow into. The VM must be off, without
    /// snapshots.
    Expand {
        vm: String,
        #[arg(long, value_parser = vmconfig::parse_size)]
        size: u64,
        /// Which of its disks, from 1.
        #[arg(long, default_value_t = 1)]
        disk: usize,
    },
    /// Write a VM's disk anew without the space that holds only zeroes (zero free
    /// space in the guest first, as Windows' `sdelete -z` does). The VM must be off,
    /// without snapshots.
    Compact {
        vm: String,
        /// Which of its disks, from 1.
        #[arg(long, default_value_t = 1)]
        disk: usize,
    },
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
        easy_install: easy_install(args, preset)?,
    })?;
    let mut library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    library.add(&vm.path, args.folder.as_deref());
    library.save().map_err(|e| e.to_string())?;
    Ok(vm.path)
}

/// Easy Install's settings from the arguments, with the host's as defaults.
fn easy_install(
    args: &CreateArgs,
    preset: &vmconfig::presets::Preset,
) -> Result<Option<EasyInstall>, String> {
    use vmmc_client::host;

    if !args.easy_install {
        return Ok(None);
    }
    let user = args.user.clone().unwrap_or_else(host::user_name);
    let password = args.password.clone().unwrap_or_default();
    Ok(Some(match preset.id {
        "windows11" | "windows10" => EasyInstall::Windows {
            edition: args.edition.clone().unwrap_or_else(|| {
                if preset.id == "windows11" {
                    "Windows 11 Pro".into()
                } else {
                    "Windows 10 Pro".into()
                }
            }),
            user,
            password,
            language: None,
            time_zone: host::windows_time_zone().unwrap_or_else(|| "UTC".into()),
            first_logon: Vec::new(),
        },
        "ubuntu" => EasyInstall::Ubuntu {
            real_name: user.clone(),
            user,
            password,
            keyboard: args
                .keyboard
                .clone()
                .unwrap_or_else(|| host::keyboard_layout().into()),
            locale: host::linux_locale(),
            time_zone: host::linux_time_zone(),
        },
        other => {
            return Err(format!(
                "Easy Install knows Windows and Ubuntu, not {other}"
            ));
        }
    }))
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
                let started = if running(&vm) {
                    "running".into()
                } else if vmmc_client::is_suspended(&vm) {
                    "suspended".into()
                } else {
                    vm.config
                        .last_powered_on
                        .as_deref()
                        .map_or("never started".into(), |t| format!("last started {t}"))
                };
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

#[cfg(windows)]
fn running(vm: &vmconfig::Vm) -> bool {
    vmmc_client::is_running(vm)
}

#[cfg(not(windows))]
fn running(_: &vmconfig::Vm) -> bool {
    false
}

/// A VM by its `.vmmc` file, or by its name in the library.
fn find_vm(name: &str) -> Result<vmconfig::Vm, String> {
    let path = Path::new(name);
    if path.is_file() {
        return vmconfig::Vm::load(path).map_err(|e| e.to_string());
    }
    let library = Library::load(&library::default_path()).map_err(|e| e.to_string())?;
    for entry in library.entries() {
        if let Ok(vm) = vmconfig::Vm::load(Path::new(&entry.path))
            && vm.config.name.eq_ignore_ascii_case(name)
        {
            return Ok(vm);
        }
    }
    Err(format!(
        "no VM {name:?}: give its .vmmc file, or a name from `vmmc-cli list`"
    ))
}

#[cfg(windows)]
fn worker_command(command: Command) -> Result<(), String> {
    use std::time::Duration;

    use ipc::Request;

    let connect = |vm: &vmconfig::Vm| vmmc_client::connect(vm, Duration::from_secs(2));
    match command {
        Command::Start { vm } => {
            let vm = find_vm(&vm)?;
            let connection = vmmc_client::start(&vm)?;
            println!("{} is running (worker {})", vm.config.name, connection.pid);
        }
        Command::Stop { vm, hard } => {
            let vm = find_vm(&vm)?;
            let mut connection = connect(&vm)?;
            if hard {
                connection.command(&Request::Stop)?;
                println!("{} stopped", vm.config.name);
            } else {
                connection.command(&Request::PowerButton)?;
                println!("pressed {}'s power button", vm.config.name);
            }
        }
        Command::Reset { vm } => {
            let vm = find_vm(&vm)?;
            connect(&vm)?.command(&Request::Reset)?;
            println!("{} reset", vm.config.name);
        }
        Command::Tools { vm } => {
            let vm = find_vm(&vm)?;
            println!("{}", vmmc_client::insert_tools(&vm)?);
        }
        Command::Suspend { vm } => {
            let vm = find_vm(&vm)?;
            vmmc_client::suspend(&vm)?;
            println!(
                "{} is suspended: `vmmc-cli start` resumes it",
                vm.config.name
            );
        }
        Command::Status { vm } => {
            let vm = find_vm(&vm)?;
            match connect(&vm) {
                Ok(mut connection) => {
                    let status = connection.status()?;
                    let screen = status
                        .screen
                        .map_or("no picture yet".into(), |(w, h)| format!("{w}x{h}"));
                    println!(
                        "{}: {:?}, worker {} ({}), screen {screen}",
                        vm.config.name, status.state, connection.pid, status.sandbox
                    );
                }
                Err(_) => println!("{}: not running", vm.config.name),
            }
        }
        _ => unreachable!("not a worker command"),
    }
    Ok(())
}

#[cfg(not(windows))]
fn worker_command(_: Command) -> Result<(), String> {
    Err("running VMs needs Windows for now".into())
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
        command @ (Command::Start { .. }
        | Command::Stop { .. }
        | Command::Reset { .. }
        | Command::Status { .. }
        | Command::Suspend { .. }
        | Command::Tools { .. }) => worker_command(command)?,
        Command::Snapshot(command) => snapshot_command(command)?,
        Command::Clone {
            vm,
            name,
            snapshot,
            linked,
        } => {
            let vm = find_vm(&vm)?;
            let mut shown = 0;
            let clone =
                vmmc_client::clone_vm(&vm, snapshot.as_deref(), &name, linked, |done, total| {
                    let percent = done * 100 / total.max(1);
                    if percent >= shown + 10 {
                        shown = percent;
                        eprintln!("  copied {percent}%");
                    }
                })?;
            println!(
                "{} {} into {}",
                if linked { "linked" } else { "cloned" },
                vm.config.name,
                clone.path.display()
            );
        }
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
        Command::Disk(DiskCommand::Expand { vm, size, disk }) => {
            let vm = find_vm(&vm)?;
            vmmc_client::expand_disk(&vm, disk, size)?;
            println!(
                "{}'s disk {disk} is {} now: extend its partition in the guest to use it",
                vm.config.name,
                gib(size)
            );
        }
        Command::Disk(DiskCommand::Compact { vm, disk }) => {
            let vm = find_vm(&vm)?;
            let freed = vmmc_client::compact_disk(&vm, disk)?;
            println!(
                "{}'s disk {disk} takes {} MiB less",
                vm.config.name,
                freed >> 20
            );
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
