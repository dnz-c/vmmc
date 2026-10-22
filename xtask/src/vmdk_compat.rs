//! `cargo xtask vmdk-compat-test`: vmmc's VMDKs and VMware's agree, as VMware's own
//! `vmware-vdiskmanager` sees them. Needs VMware Workstation (or `VMMC_VDISKMANAGER`
//! naming the tool); the files go in the test image directory and are removed after.
//!
//! 1. VMware makes each kind of disk (sparse, split sparse, flat, split flat); vmmc
//!    writes a pattern into it; VMware finds it consistent (`-R`) and converts it into a
//!    plain disk that holds exactly the pattern.
//! 2. The same the other way round, for a disk vmmc made.
//! 3. A vmmc delta on a VMware disk: VMware finds the chain consistent (`-e`) and reads
//!    the delta's writes on top of the base's.
//! 4. VMware converts vmmc's disk to stream-optimized (as in an OVA); vmmc reads it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use disk::BlockBackend;
use disk::vmdk::Vmdk;

const VDISKMANAGER: &str =
    r"C:\Program Files (x86)\VMware\VMware Workstation\vmware-vdiskmanager.exe";
/// A split sparse extent's size, in bytes, as VMware makes them.
const SPLIT_EXTENT: u64 = 8_323_072 * 512;

fn vdiskmanager() -> Result<PathBuf, String> {
    let path = std::env::var_os("VMMC_VDISKMANAGER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(VDISKMANAGER));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{} not found: install VMware Workstation, or set VMMC_VDISKMANAGER",
            path.display()
        ))
    }
}

/// Runs vdiskmanager, returning what it printed.
fn vd(tool: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| format!("can't run {}: {e}", tool.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(format!(
            "vmware-vdiskmanager {}: {}",
            args.join(" "),
            text.trim()
        ));
    }
    Ok(text)
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// The test's writes for a disk of `size` bytes: at the start, across a grain, a lone
/// sector, the middle, the end, and across the first split extent's end.
fn writes(size: u64, seed: u64) -> Vec<(u64, Vec<u8>)> {
    let mut offsets = vec![
        (0, 4096),
        (63 << 10, 8192),
        ((1 << 20) + 512, 512),
        (size / 2, 64 << 10),
        (size - 4096, 4096),
    ];
    if size > SPLIT_EXTENT {
        offsets.push((SPLIT_EXTENT - 1000, 2000));
    }
    offsets
        .into_iter()
        .enumerate()
        .map(|(i, (offset, len))| (offset, data(len, seed * 100 + i as u64)))
        .collect()
}

/// Checks `disk` holds `writes` (later ones winning) and zeroes at 2 MiB.
fn check(disk: &dyn BlockBackend, writes: &[(u64, Vec<u8>)], what: &str) -> Result<(), String> {
    for (index, (offset, expected)) in writes.iter().enumerate() {
        let mut back = vec![0u8; expected.len()];
        disk.read_at(&mut back, *offset)
            .map_err(|e| format!("{what}: reading at {offset}: {e}"))?;
        // A later write over part of this one wins there.
        let mut expected = expected.clone();
        for (later_offset, later) in &writes[index + 1..] {
            for (i, byte) in later.iter().enumerate() {
                let at = later_offset + i as u64;
                if at >= *offset && at < offset + expected.len() as u64 {
                    expected[(at - offset) as usize] = *byte;
                }
            }
        }
        if back != expected {
            return Err(format!("{what}: wrong data at {offset}"));
        }
    }
    let mut zeroes = vec![0xffu8; 4096];
    disk.read_at(&mut zeroes, 2 << 20)
        .map_err(|e| format!("{what}: {e}"))?;
    if zeroes.iter().any(|&b| b != 0) {
        return Err(format!("{what}: an unwritten area isn't zeroes"));
    }
    Ok(())
}

fn write_all(path: &Path, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
    let disk = Vmdk::open(path, false).map_err(|e| format!("{}: {e}", path.display()))?;
    for (offset, bytes) in writes {
        disk.write_at(bytes, *offset)
            .map_err(|e| format!("{}: writing at {offset}: {e}", path.display()))?;
    }
    disk.flush().map_err(|e| e.to_string())
}

/// VMware's consistency check of a sparse disk; it repairs what it finds broken, and
/// says so.
fn consistent(tool: &Path, path: &Path) -> Result<(), String> {
    let text = vd(tool, &["-R", &path.display().to_string()])?;
    if text.contains("corrupt") {
        return Err(format!(
            "VMware finds {} broken: {}",
            path.display(),
            text.trim()
        ));
    }
    Ok(())
}

/// VMware's copy of `path` as a disk of `kind` (2: one flat file, 0: monolithic
/// sparse), opened by vmmc.
fn vmware_copy(tool: &Path, path: &Path, kind: &str, name: &str) -> Result<Vmdk, String> {
    let copy = path.with_file_name(name);
    vd(
        tool,
        &[
            "-r",
            &path.display().to_string(),
            "-t",
            kind,
            &copy.display().to_string(),
        ],
    )?;
    Vmdk::open(&copy, true).map_err(|e| format!("{}: {e}", copy.display()))
}

fn size_text(bytes: u64) -> String {
    format!("{}MB", bytes >> 20)
}

pub fn test() -> Result<(), String> {
    let tool = vdiskmanager()?;
    let dir = crate::image_dir()?.join("vmdk-compat");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;

    // 1. VMware's disks, written by vmmc. The split sparse one has two extents; it's
    //    checked through a sparse copy, which stays small.
    let small = 64 << 20;
    let big = SPLIT_EXTENT + (1 << 30);
    for (kind, size, name, copy_kind) in [
        ("0", small, "sparse", "2"),
        ("1", big, "split-sparse", "0"),
        ("2", small, "flat", "2"),
        ("3", small, "split-flat", "2"),
    ] {
        let path = dir.join(format!("vmware-{name}.vmdk"));
        vd(
            &tool,
            &[
                "-c",
                "-s",
                &size_text(size),
                "-a",
                "lsilogic",
                "-t",
                kind,
                &path.display().to_string(),
            ],
        )?;
        let writes = writes(size, kind.parse().unwrap());
        write_all(&path, &writes)?;
        check(
            &Vmdk::open(&path, true).map_err(|e| e.to_string())?,
            &writes,
            name,
        )?;
        if kind == "0" || kind == "1" {
            consistent(&tool, &path)?;
        }
        check(
            &vmware_copy(&tool, &path, copy_kind, &format!("check-{name}.vmdk"))?,
            &writes,
            &format!("VMware's copy of {name}"),
        )?;
        eprintln!("xtask: VMware's {name} disk takes vmmc's writes");
    }

    // 2. vmmc's disk, as VMware sees it.
    let ours = dir.join("vmmc.vmdk");
    disk::create_sparse(&ours, small).map_err(|e| e.to_string())?;
    let our_writes = writes(small, 7);
    write_all(&ours, &our_writes)?;
    consistent(&tool, &ours)?;
    check(
        &vmware_copy(&tool, &ours, "2", "check-vmmc.vmdk")?,
        &our_writes,
        "VMware's copy of vmmc's disk",
    )?;
    eprintln!("xtask: VMware reads vmmc's disk");

    // 3. A vmmc delta on VMware's sparse disk.
    let base = dir.join("vmware-sparse.vmdk");
    let delta = dir.join("delta.vmdk");
    disk::create_delta(&delta, &base).map_err(|e| e.to_string())?;
    let delta_writes = writes(small, 9);
    write_all(&delta, &delta_writes[1..3])?;
    let text = vd(&tool, &["-e", &delta.display().to_string()])?;
    if !text.contains("consistent") || text.contains("not consistent") {
        return Err(format!("VMware's chain check: {}", text.trim()));
    }
    let mut both = writes(small, 0);
    both.extend_from_slice(&delta_writes[1..3]);
    check(
        &vmware_copy(&tool, &delta, "2", "check-delta.vmdk")?,
        &both,
        "VMware's copy of vmmc's delta",
    )?;
    eprintln!("xtask: VMware reads vmmc's delta on its disk");

    // 4. A stream-optimized copy of vmmc's disk, made by VMware, read by vmmc.
    check(
        &vmware_copy(&tool, &ours, "5", "stream.vmdk")?,
        &our_writes,
        "VMware's stream-optimized copy",
    )?;
    eprintln!("xtask: vmmc reads VMware's stream-optimized disk");

    // 5. A chain of vmmc's deltas after a snapshot's deletion merged one: the middle
    //    delta goes into vmmc's disk, and the top one moves onto it.
    let middle = dir.join("vmmc-000001.vmdk");
    let top = dir.join("vmmc-000002.vmdk");
    disk::create_delta(&middle, &ours).map_err(|e| e.to_string())?;
    let middle_writes = writes(small, 11);
    write_all(&middle, &middle_writes[..3])?;
    disk::create_delta(&top, &middle).map_err(|e| e.to_string())?;
    let top_writes = writes(small, 12);
    write_all(&top, &top_writes[2..])?;
    disk::merge_into_parent(&middle).map_err(|e| e.to_string())?;
    disk::reparent(&top, &ours).map_err(|e| e.to_string())?;
    fs::remove_file(&middle).map_err(|e| e.to_string())?;
    let text = vd(&tool, &["-e", &top.display().to_string()])?;
    if !text.contains("consistent") || text.contains("not consistent") {
        return Err(format!(
            "VMware's check of the merged chain: {}",
            text.trim()
        ));
    }
    consistent(&tool, &ours)?;
    let mut all = our_writes.clone();
    all.extend_from_slice(&middle_writes[..3]);
    all.extend_from_slice(&top_writes[2..]);
    check(
        &vmware_copy(&tool, &top, "2", "check-merged.vmdk")?,
        &all,
        "VMware's copy of the merged chain",
    )?;
    eprintln!("xtask: VMware reads a chain vmmc merged");

    // Every disk is closed by now (each was opened for one check).
    fs::remove_dir_all(&dir).map_err(|e| format!("can't remove {}: {e}", dir.display()))?;
    eprintln!("vmdk-compat-test passed");
    Ok(())
}
