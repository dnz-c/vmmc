//! Cloning a VM, as it is (with it off) or as a snapshot keeps it.
//!
//! - **A full clone** has disks of its own: each chain of delta disks is copied into one
//!   new monolithic sparse disk, zeroes left out.
//! - **A linked clone** has a new delta on each of a snapshot's images, which take its
//!   writes: it takes little space and no time, and the snapshot keeps its images for
//!   it (it can't be deleted while the clone is there).
//!
//! Either way the clone is a new machine: a new UUID and MAC address, a new UEFI variable
//! store and TPM (made at its first start), no snapshots, and no memory: a snapshot taken
//! while the VM ran gives a clone that boots from its disks as after a power cut.
//! Read-only disks and DVD images aren't copied: the clone names the same files.

use std::fs;
use std::path::{Path, PathBuf};

use crate::snapshots::{self, Tree};
use crate::{EXTENSION, Nic, Vm, format_mac, format_uuid, random_uuid};

/// A clone to make.
pub struct Clone<'a> {
    pub source: &'a Vm,
    /// The snapshot to clone (by ID), or the VM as it is now, which must be off then.
    pub snapshot: Option<&'a str>,
    pub name: &'a str,
    /// Where the clone's folder goes.
    pub parent_dir: &'a Path,
    pub linked: bool,
}

/// Makes the clone in a new folder, and returns it. `progress` hears the bytes copied
/// so far and how many there are, for a full clone.
pub fn clone_vm(clone: &Clone, mut progress: impl FnMut(u64, u64)) -> Result<Vm, String> {
    let source = clone.source;
    let name = clone.name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(format!("{name:?} can't be a VM's name"));
    }
    let mut tree = Tree::load(source).map_err(|e| e.to_string())?;
    let config = match clone.snapshot {
        Some(id) => snapshots::snapshot_config(source, id).map_err(|e| e.to_string())?,
        None if clone.linked => {
            return Err("a linked clone is made from a snapshot: take one first".into());
        }
        None => source.config.clone(),
    };
    let dir = clone.parent_dir.join(name);
    if dir.exists() {
        return Err(format!("{} is there already", dir.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let made = (|| {
        let mut config = config;
        let uuid = random_uuid();
        config.name = name.to_owned();
        config.uuid = format_uuid(&uuid);
        for (index, nic) in config.nics.iter_mut().enumerate() {
            *nic = Nic {
                model: nic.model,
                mac: format_mac(&[0x52, 0x54, 0x00, uuid[13], uuid[14], uuid[15] ^ index as u8]),
            };
        }
        config.created = Some(crate::now());
        config.last_powered_on = None;
        config.vmware = None;
        config.install = None;

        // The source's disks, from its folder.
        let absolute = |path: &str| source.resolve(path).display().to_string();
        let writable = config.disks.iter().filter(|disk| !disk.read_only).count();
        let images: Vec<(usize, PathBuf)> = config
            .disks
            .iter()
            .enumerate()
            .filter(|(_, disk)| !disk.read_only)
            .map(|(index, disk)| (index, source.resolve(&disk.path)))
            .collect();
        let disk_name = |n: usize| {
            if writable <= 1 || n == 0 {
                format!("{name}.vmdk")
            } else {
                format!("{name}-{}.vmdk", n + 1)
            }
        };
        if clone.linked {
            for (n, (index, image)) in images.iter().enumerate() {
                let delta = dir.join(disk_name(n));
                disk::create_delta(&delta, image).map_err(|e| {
                    format!("can't make {} on {}: {e}", delta.display(), image.display())
                })?;
                config.disks[*index].path = disk_name(n);
            }
        } else {
            let opened = images
                .iter()
                .map(|(_, image)| {
                    disk::open(image, true).map_err(|e| format!("{}: {e}", image.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let total: u64 = opened.iter().map(|disk| disk.len()).sum();
            let mut before = 0;
            for (n, ((index, _), from)) in images.iter().zip(&opened).enumerate() {
                let path = dir.join(disk_name(n));
                disk::create_sparse(&path, from.len())
                    .map_err(|e| format!("can't make {}: {e}", path.display()))?;
                let to = disk::open(&path, false).map_err(|e| e.to_string())?;
                disk::copy(from.as_ref(), to.as_ref(), |done| {
                    progress(before + done, total)
                })
                .map_err(|e| format!("can't copy into {}: {e}", path.display()))?;
                before += from.len();
                config.disks[*index].path = disk_name(n);
            }
        }
        for disk in config.disks.iter_mut().filter(|disk| disk.read_only) {
            disk.path = absolute(&disk.path);
        }
        for cdrom in &mut config.cdroms {
            cdrom.path = cdrom.path.as_deref().map(absolute);
            cdrom.install_media = false;
        }
        let vm = Vm {
            path: dir.join(format!("{name}.{EXTENSION}")),
            config,
        };
        vm.save().map_err(|e| e.to_string())?;
        Ok(vm)
    })();
    let vm = match made {
        Ok(vm) => vm,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    // The snapshot keeps its images for the linked clone.
    if clone.linked
        && let Some(id) = clone.snapshot
        && let Some(snapshot) = tree.snapshots.iter_mut().find(|s| s.id == id)
    {
        snapshot.clones.push(vm.path.display().to_string());
        tree.save(source).map_err(|e| e.to_string())?;
    }
    Ok(vm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VmConfig;

    fn vm(dir: &Path) -> Vm {
        let dir = dir.join("Source");
        fs::create_dir_all(&dir).unwrap();
        let mut config = VmConfig::new("Source", "linux", 1, 256);
        config.disks.push(crate::Disk {
            path: "Source.vmdk".into(),
            bus: crate::Bus::Nvme,
            read_only: false,
        });
        disk::create_sparse(&dir.join("Source.vmdk"), 4 << 20).unwrap();
        let vm = Vm {
            path: dir.join("Source.vmmc"),
            config,
        };
        vm.save().unwrap();
        vm
    }

    fn write(vm: &Vm, data: &[u8]) {
        let disk = disk::open(&vm.resolve(&vm.config.disks[0].path), false).unwrap();
        disk.write_at(data, 1 << 20).unwrap();
        disk.flush().unwrap();
    }

    fn read(vm: &Vm) -> Vec<u8> {
        let disk = disk::open(&vm.resolve(&vm.config.disks[0].path), true).unwrap();
        let mut data = vec![0u8; 5];
        disk.read_at(&mut data, 1 << 20).unwrap();
        data
    }

    #[test]
    fn a_full_clone_has_disks_of_its_own() {
        let root = tempfile::tempdir().unwrap();
        let mut source = vm(root.path());
        write(&source, b"one..");
        let first = snapshots::take(&mut source, "First", "").unwrap();
        write(&source, b"two..");
        let clone = |name, snapshot| Clone {
            source: &source,
            snapshot,
            name,
            parent_dir: root.path(),
            linked: false,
        };
        let now = clone_vm(&clone("Now", None), |_, _| {}).unwrap();
        assert_eq!(read(&now), b"two..");
        assert_eq!(now.config.disks[0].path, "Now.vmdk");
        assert_eq!(
            disk::parent_of(&now.resolve("Now.vmdk")).unwrap(),
            None,
            "the chain is one disk"
        );
        assert_ne!(now.config.uuid, source.config.uuid);
        assert_ne!(now.config.nics[0].mac, source.config.nics[0].mac);
        let then = clone_vm(&clone("Then", Some(&first.id)), |_, _| {}).unwrap();
        assert_eq!(read(&then), b"one..");
        assert!(clone_vm(&clone("Then", None), |_, _| {}).is_err(), "taken");
        // The snapshot is free to go: a full clone needs nothing of it.
        snapshots::delete(&source, &first.id).unwrap();
    }

    #[test]
    fn a_linked_clone_is_a_delta_its_snapshot_keeps() {
        let root = tempfile::tempdir().unwrap();
        let mut source = vm(root.path());
        write(&source, b"base.");
        let no_snapshot = Clone {
            source: &source,
            snapshot: None,
            name: "Linked",
            parent_dir: root.path(),
            linked: true,
        };
        assert!(
            clone_vm(&no_snapshot, |_, _| {}).is_err(),
            "needs a snapshot"
        );
        let base = snapshots::take(&mut source, "Base", "").unwrap();
        let linked = Clone {
            source: &source,
            snapshot: Some(&base.id),
            name: "Linked",
            parent_dir: root.path(),
            linked: true,
        };
        let clone = clone_vm(&linked, |_, _| {}).unwrap();
        assert_eq!(read(&clone), b"base.");
        write(&clone, b"clone");
        write(&source, b"mine.");
        assert_eq!(read(&clone), b"clone");
        assert_eq!(read(&source), b"mine.");
        assert!(
            snapshots::delete(&source, &base.id)
                .unwrap_err()
                .contains("linked clone"),
            "its snapshot stays while it's there"
        );
        fs::remove_dir_all(clone.dir()).unwrap();
        snapshots::delete(&source, &base.id).unwrap();
    }
}
