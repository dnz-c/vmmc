//! A VM's snapshots: the tree, in `snapshots/snapshots.toml` in the VM's folder, and
//! each snapshot's files beside it.
//!
//! - A snapshot keeps the VM as it was: its config (`<id>.vmmc`, whose disks name the
//!   images frozen when it was taken), its UEFI variables and TPM (`<id>.nvram.fd`,
//!   `<id>.tpm-state.bin`) and, if the VM was running, its memory and device state
//!   (`<id>.state`).
//! - **Taking one** moves the VM's writes to new delta disks on top of its images,
//!   which from then on belong to the snapshot. A running VM's worker does that while
//!   the VM is paused ([`begin`], then [`Pending::commit`]); a VM that's off just gets
//!   the deltas ([`take`]).
//! - **Reverting** ([`revert`]) gives each disk a new delta on top of the snapshot's
//!   images, puts back its config, UEFI variables and TPM, and removes the deltas that
//!   held what came since. A snapshot of a running VM then starts the VM from its
//!   state.
//! - **Deleting** one ([`delete`]) forgets it and its files. Its disk images stay, at
//!   the bottom of the chains above them, until [`consolidate`] merges into each
//!   image the one delta on top of it, with the VM off.
//!
//! ```toml
//! current = "3f2a9c1e-..."   # the snapshot the VM goes on from
//!
//! [[snapshot]]
//! id = "3f2a9c1e-..."
//! name = "Before the update"
//! created = "2026-10-16T19:30:00Z"
//! running = true
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Vm, VmConfig};

/// The folder in a VM's folder that holds its snapshots.
pub const SNAPSHOTS_DIR: &str = "snapshots";
const TREE_FILE: &str = "snapshots.toml";
/// Where a suspended VM's state waits for it to resume.
pub const SUSPENDED_FILE: &str = "suspended.state";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    /// The snapshot the VM's state goes on from: the parent of the next one taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, rename = "snapshot")]
    pub snapshots: Vec<Snapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Taken while the VM ran: it has the VM's memory and device state.
    #[serde(default)]
    pub running: bool,
    /// The linked clones made from it (their `.vmmc` files), whose disks are deltas on
    /// its images: it can't be deleted while they're there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clones: Vec<String>,
}

impl Snapshot {
    /// The linked clones made from it that are still there.
    pub fn live_clones(&self) -> impl Iterator<Item = &str> {
        self.clones
            .iter()
            .map(String::as_str)
            .filter(|clone| Path::new(clone).is_file())
    }
}

/// The folder a VM's snapshots are in.
pub fn dir(vm: &Vm) -> PathBuf {
    vm.dir().join(SNAPSHOTS_DIR)
}

/// A snapshot's memory and device state.
pub fn state_file(vm: &Vm, id: &str) -> PathBuf {
    dir(vm).join(format!("{id}.state"))
}

fn config_file(vm: &Vm, id: &str) -> PathBuf {
    dir(vm).join(format!("{id}.vmmc"))
}

fn nvram_file(vm: &Vm, id: &str) -> PathBuf {
    dir(vm).join(format!("{id}.nvram.fd"))
}

fn tpm_file(vm: &Vm, id: &str) -> PathBuf {
    dir(vm).join(format!("{id}.tpm-state.bin"))
}

/// Where the VM's state goes when it's suspended.
pub fn suspended_file(vm: &Vm) -> PathBuf {
    vm.dir().join(SUSPENDED_FILE)
}

fn write_error(path: &Path, source: std::io::Error) -> Error {
    Error::Write {
        path: path.to_owned(),
        source,
    }
}

impl Tree {
    /// The VM's tree: empty if it has no snapshots.
    pub fn load(vm: &Vm) -> Result<Self, Error> {
        let path = dir(vm).join(TREE_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(Error::Read { path, source }),
        };
        toml::from_str(&text).map_err(|e| Error::Invalid {
            path,
            message: e.to_string(),
        })
    }

    pub fn save(&self, vm: &Vm) -> Result<(), Error> {
        let dir = dir(vm);
        fs::create_dir_all(&dir).map_err(|e| write_error(&dir, e))?;
        let path = dir.join(TREE_FILE);
        let text = toml::to_string(self)
            .map_err(|e| write_error(&path, std::io::Error::other(e.to_string())))?;
        let partial = path.with_extension("toml-partial");
        fs::write(&partial, text).map_err(|e| write_error(&partial, e))?;
        fs::rename(&partial, &path).map_err(|e| write_error(&path, e))
    }

    pub fn get(&self, id: &str) -> Option<&Snapshot> {
        self.snapshots.iter().find(|snapshot| snapshot.id == id)
    }

    /// The snapshot with this ID, or the one with this name (if just one has it).
    pub fn find(&self, id_or_name: &str) -> Option<&Snapshot> {
        self.get(id_or_name).or_else(|| {
            let mut named = self.snapshots.iter().filter(|s| s.name == id_or_name);
            match (named.next(), named.next()) {
                (Some(snapshot), None) => Some(snapshot),
                _ => None,
            }
        })
    }

    /// The snapshots taken from `id` (from none: the first ones).
    pub fn children<'a>(&'a self, id: Option<&'a str>) -> impl Iterator<Item = &'a Snapshot> {
        self.snapshots
            .iter()
            .filter(move |snapshot| snapshot.parent.as_deref() == id)
    }
}

/// The next free delta name for `image`: `NAME-000001.vmdk`, or one up from the
/// number `image` has, like VMware's.
pub fn next_delta(image: &Path) -> PathBuf {
    let stem = image
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = match stem.rsplit_once('-') {
        Some((base, number)) if number.len() == 6 && number.bytes().all(|b| b.is_ascii_digit()) => {
            base.to_owned()
        }
        _ => stem,
    };
    let dir = image.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| dir.join(format!("{base}-{n:06}.vmdk")))
        .find(|candidate| !candidate.exists())
        .expect("a free name")
}

/// A disk path as the config keeps it: relative to the VM's folder when it's inside.
fn config_path(vm_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(vm_dir) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// A snapshot being taken.
#[derive(Debug)]
pub struct Pending {
    pub id: String,
    name: String,
    description: String,
    /// The delta each writable disk (in the config's order) moves to.
    pub deltas: Vec<PathBuf>,
    /// Where the memory and device state goes, for a running VM.
    pub state: PathBuf,
    /// The config as it is now: the snapshot's.
    frozen: VmConfig,
}

/// Starts a snapshot: says where each writable disk's delta goes and where the state
/// goes, and makes the folder for it.
pub fn begin(vm: &Vm, name: &str, description: &str) -> Result<Pending, String> {
    if name.trim().is_empty() {
        return Err("a snapshot needs a name".into());
    }
    let snapshots = dir(vm);
    fs::create_dir_all(&snapshots)
        .map_err(|e| format!("can't create {}: {e}", snapshots.display()))?;
    let id = crate::format_uuid(&crate::random_uuid());
    let mut deltas = Vec::new();
    for disk in vm.config.disks.iter().filter(|disk| !disk.read_only) {
        let path = vm.resolve(&disk.path);
        if !is_vmdk(&path) {
            return Err(format!(
                "{} isn't a VMDK: snapshots need VMDK disks (vmmc-cli disk convert makes one)",
                path.display()
            ));
        }
        deltas.push(next_delta(&path));
    }
    Ok(Pending {
        state: state_file(vm, &id),
        id,
        name: name.trim().to_owned(),
        description: description.to_owned(),
        deltas,
        frozen: vm.config.clone(),
    })
}

impl Pending {
    /// Makes the snapshot part of the VM, once its disks write to the deltas (and, for
    /// a running VM, its state is saved): keeps its config, UEFI variables and TPM,
    /// points the VM's disks at the deltas, and adds it to the tree under the current
    /// snapshot.
    pub fn commit(self, vm: &mut Vm, running: bool) -> Result<Snapshot, Error> {
        let dir = dir(vm);
        fs::create_dir_all(&dir).map_err(|e| write_error(&dir, e))?;
        let config = config_file(vm, &self.id);
        let text = toml::to_string(&self.frozen)
            .map_err(|e| write_error(&config, std::io::Error::other(e.to_string())))?;
        fs::write(&config, text).map_err(|e| write_error(&config, e))?;
        for (from, to) in [
            (vm.nvram(), nvram_file(vm, &self.id)),
            (vm.tpm_state(), tpm_file(vm, &self.id)),
        ] {
            if from.is_file() {
                fs::copy(&from, &to).map_err(|e| write_error(&to, e))?;
            }
        }
        let mut deltas = self.deltas.iter();
        let vm_dir = &vm.dir().to_owned();
        for disk in vm.config.disks.iter_mut().filter(|disk| !disk.read_only) {
            if let Some(delta) = deltas.next() {
                disk.path = config_path(vm_dir, delta);
            }
        }
        vm.save()?;
        let mut tree = Tree::load(vm)?;
        let snapshot = Snapshot {
            id: self.id,
            name: self.name,
            description: self.description,
            created: crate::now(),
            parent: tree.current.clone(),
            running,
            clones: Vec::new(),
        };
        tree.current = Some(snapshot.id.clone());
        tree.snapshots.push(snapshot.clone());
        tree.save(vm)?;
        Ok(snapshot)
    }
}

/// Takes a snapshot of a VM that isn't running: its disks get new deltas.
pub fn take(vm: &mut Vm, name: &str, description: &str) -> Result<Snapshot, String> {
    let pending = begin(vm, name, description)?;
    let images: Vec<PathBuf> = vm
        .config
        .disks
        .iter()
        .filter(|disk| !disk.read_only)
        .map(|disk| vm.resolve(&disk.path))
        .collect();
    let mut made = Vec::new();
    for (image, delta) in images.iter().zip(&pending.deltas) {
        if let Err(e) = disk::create_delta(delta, image) {
            for delta in made {
                let _ = fs::remove_file(delta);
            }
            return Err(format!("can't make {}: {e}", delta.display()));
        }
        made.push(delta.clone());
    }
    pending.commit(vm, false).map_err(|e| e.to_string())
}

/// Every disk image some snapshot keeps.
fn kept_images(vm: &Vm, tree: &Tree) -> Vec<PathBuf> {
    tree.snapshots
        .iter()
        .filter_map(|snapshot| snapshot_config(vm, &snapshot.id).ok())
        .flat_map(|config| {
            config
                .disks
                .into_iter()
                .map(|disk| vm.resolve(&disk.path))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The config a snapshot keeps.
pub fn snapshot_config(vm: &Vm, id: &str) -> Result<VmConfig, Error> {
    let path = config_file(vm, id);
    let text = fs::read_to_string(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    toml::from_str(&text).map_err(|e| Error::Invalid {
        path,
        message: e.to_string(),
    })
}

/// Reverts a VM that isn't running to snapshot `id`: what the VM did since is gone.
/// Returns the state to start it from, for a snapshot of a running VM.
pub fn revert(vm: &mut Vm, id: &str) -> Result<Option<PathBuf>, String> {
    let mut tree = Tree::load(vm).map_err(|e| e.to_string())?;
    let snapshot = tree
        .get(id)
        .cloned()
        .ok_or_else(|| format!("{} has no snapshot {id}", vm.config.name))?;
    let frozen = snapshot_config(vm, id).map_err(|e| e.to_string())?;
    let state = state_file(vm, id);
    if snapshot.running && !state.is_file() {
        return Err(format!(
            "the snapshot's state {} is missing",
            state.display()
        ));
    }

    // New deltas on the snapshot's images.
    let mut config = frozen;
    let mut made = Vec::new();
    for disk in config.disks.iter_mut().filter(|disk| !disk.read_only) {
        let image = vm.resolve(&disk.path);
        let delta = next_delta(&image);
        if let Err(e) = disk::create_delta(&delta, &image) {
            for delta in made {
                let _ = fs::remove_file(delta);
            }
            return Err(format!("can't make {}: {e}", delta.display()));
        }
        disk.path = config_path(vm.dir(), &delta);
        made.push(delta);
    }
    // What came since: the VM's current deltas, unless a snapshot keeps them, and never
    // an image vmmc didn't make (a VMware VM's own disks).
    let kept = kept_images(vm, &tree);
    let discarded: Vec<PathBuf> = vm
        .config
        .disks
        .iter()
        .filter(|disk| !disk.read_only)
        .map(|disk| vm.resolve(&disk.path))
        .filter(|path| !kept.contains(path) && is_delta_name(path))
        .collect();

    // The snapshot's hardware, under the VM's own name and history.
    let mut reverted = config;
    reverted.name = vm.config.name.clone();
    reverted.uuid = vm.config.uuid.clone();
    reverted.notes = vm.config.notes.clone();
    reverted.created = vm.config.created.clone();
    reverted.last_powered_on = vm.config.last_powered_on.clone();
    reverted.vmware = vm.config.vmware.clone();
    reverted.install = None;
    vm.config = reverted;
    for (from, to) in [
        (nvram_file(vm, id), vm.nvram()),
        (tpm_file(vm, id), vm.tpm_state()),
    ] {
        if from.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("can't put back {}: {e}", to.display()))?;
        }
    }
    vm.save().map_err(|e| e.to_string())?;
    tree.current = Some(id.to_owned());
    tree.save(vm).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(suspended_file(vm));
    for path in discarded {
        if let Err(e) = fs::remove_file(&path) {
            eprintln!("vmmc: can't remove {}: {e}", path.display());
        }
    }
    Ok(snapshot.running.then_some(state))
}

/// A file named like a delta [`next_delta`] makes.
fn is_delta_name(path: &Path) -> bool {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .and_then(|stem| {
            stem.rsplit_once('-')
                .map(|(_, number)| number.len() == 6 && number.bytes().all(|b| b.is_ascii_digit()))
        })
        .unwrap_or(false)
}

/// Deletes snapshot `id`: forgets it and removes its files. The snapshots taken from it
/// hang from its parent instead. Its disk images stay, for the disks above them, until
/// [`consolidate`] merges them.
pub fn delete(vm: &Vm, id: &str) -> Result<(), String> {
    let mut tree = Tree::load(vm).map_err(|e| e.to_string())?;
    let snapshot = tree
        .get(id)
        .cloned()
        .ok_or_else(|| format!("{} has no snapshot {id}", vm.config.name))?;
    let clones: Vec<&str> = snapshot.live_clones().collect();
    if !clones.is_empty() {
        return Err(format!(
            "“{}” is what the linked clone {} is made from: remove that first",
            snapshot.name,
            clones.join(", ")
        ));
    }
    tree.snapshots.retain(|other| other.id != id);
    for other in &mut tree.snapshots {
        if other.parent.as_deref() == Some(id) {
            other.parent = snapshot.parent.clone();
        }
    }
    if tree.current.as_deref() == Some(id) {
        tree.current = snapshot.parent.clone();
    }
    tree.save(vm).map_err(|e| e.to_string())?;
    for path in [
        state_file(vm, id),
        config_file(vm, id),
        nvram_file(vm, id),
        tpm_file(vm, id),
    ] {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

/// A merge [`consolidate`] makes: `child`'s data goes into `image`, which takes its
/// place, and the deltas on `child` (`above`) move onto `image`.
struct Merge {
    image: PathBuf,
    child: PathBuf,
    above: Vec<PathBuf>,
}

/// A path to compare with others: absolute, without `.` and `..`.
fn normal(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part.as_os_str()),
        }
    }
    out
}

fn is_vmdk(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vmdk"))
}

/// The images `config`'s writable disks read and write.
fn writable_images(vm: &Vm, config: &VmConfig) -> Vec<PathBuf> {
    config
        .disks
        .iter()
        .filter(|disk| !disk.read_only)
        .map(|disk| normal(&vm.resolve(&disk.path)))
        .collect()
}

/// The next merge, if there is one: an image in the VM's folder that neither the VM nor
/// a snapshot uses (and isn't a VMware VM's own disk), with just one disk on top, a
/// delta of vmmc's.
fn next_merge(vm: &Vm, tree: &Tree) -> Result<Option<Merge>, String> {
    let mut used = writable_images(vm, &vm.config);
    for snapshot in &tree.snapshots {
        let config = snapshot_config(vm, &snapshot.id).map_err(|e| e.to_string())?;
        used.extend(writable_images(vm, &config));
    }
    // Each image's parent, down every chain. An image that can't be read stops it all:
    // one with a disk on top left out could look like it has just one.
    let mut parents: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut queue = used.clone();
    while let Some(image) = queue.pop() {
        if !is_vmdk(&image) || !seen.insert(image.clone()) {
            continue;
        }
        let parent = disk::parent_of(&image).map_err(|e| format!("{}: {e}", image.display()))?;
        if let Some(parent) = parent {
            let parent = normal(&parent);
            parents.insert(image, parent.clone());
            queue.push(parent);
        }
    }
    let vm_dir = normal(vm.dir());
    let vmware: Vec<PathBuf> = vm
        .config
        .vmware
        .iter()
        .flat_map(|origin| &origin.base_disks)
        .map(|disk| normal(&vm.resolve(disk)))
        .collect();
    let on = |image: &PathBuf| -> Vec<PathBuf> {
        parents
            .iter()
            .filter(|(_, parent)| *parent == image)
            .map(|(child, _)| child.clone())
            .collect()
    };
    for image in &seen {
        if used.contains(image) || !image.starts_with(&vm_dir) || vmware.contains(image) {
            continue;
        }
        let [child] = &on(image)[..] else {
            continue;
        };
        if child.starts_with(&vm_dir) && is_delta_name(child) {
            return Ok(Some(Merge {
                image: image.clone(),
                above: on(child),
                child: child.clone(),
            }));
        }
    }
    Ok(None)
}

/// Writes a snapshot's config back.
fn save_snapshot_config(vm: &Vm, id: &str, config: &VmConfig) -> Result<(), String> {
    let path = config_file(vm, id);
    let text = toml::to_string(config).map_err(|e| e.to_string())?;
    let partial = path.with_extension("vmmc-partial");
    fs::write(&partial, text).map_err(|e| format!("can't write {}: {e}", partial.display()))?;
    fs::rename(&partial, &path).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// Merges disk images no snapshot needs any more, in a VM that isn't running: an image
/// in the VM's folder that neither the VM nor a snapshot uses, with just one delta on
/// top of it, takes the delta's data and its place (as deleting a snapshot in VMware
/// does). Images that more than one disk is on stay, and a VMware VM's own disks are
/// never written. Returns what was merged, for the user.
pub fn consolidate(vm: &mut Vm) -> Result<Vec<String>, String> {
    let mut merged = Vec::new();
    loop {
        let tree = Tree::load(vm).map_err(|e| e.to_string())?;
        let Some(merge) = next_merge(vm, &tree)? else {
            return Ok(merged);
        };
        disk::merge_into_parent(&merge.child).map_err(|e| {
            format!(
                "can't merge {} into {}: {e}",
                merge.child.display(),
                merge.image.display()
            )
        })?;
        for above in &merge.above {
            disk::reparent(above, &merge.image).map_err(|e| {
                format!(
                    "can't move {} onto {}: {e}",
                    above.display(),
                    merge.image.display()
                )
            })?;
        }
        // Whatever named the delta names the image now.
        let image = config_path(&normal(vm.dir()), &merge.image);
        let renamed = |config: &mut VmConfig| {
            let mut changed = false;
            for disk in config.disks.iter_mut().filter(|disk| !disk.read_only) {
                if normal(&vm.resolve(&disk.path)) == merge.child {
                    disk.path = image.clone();
                    changed = true;
                }
            }
            changed
        };
        for snapshot in &tree.snapshots {
            let mut config = snapshot_config(vm, &snapshot.id).map_err(|e| e.to_string())?;
            if renamed(&mut config) {
                save_snapshot_config(vm, &snapshot.id, &config)?;
            }
        }
        let mut config = vm.config.clone();
        if renamed(&mut config) {
            vm.config = config;
            vm.save().map_err(|e| e.to_string())?;
        }
        fs::remove_file(&merge.child)
            .map_err(|e| format!("can't remove {}: {e}", merge.child.display()))?;
        let name = |path: &Path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        merged.push(format!(
            "{} into {}",
            name(&merge.child),
            name(&merge.image)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(dir: &Path) -> Vm {
        let mut config = VmConfig::new("Snappy", "linux", 1, 256);
        config.disks.push(crate::Disk {
            path: "disk.vmdk".into(),
            bus: crate::Bus::Nvme,
            read_only: false,
        });
        disk::create_sparse(&dir.join("disk.vmdk"), 4 << 20).unwrap();
        let vm = Vm {
            path: dir.join("Snappy.vmmc"),
            config,
        };
        vm.save().unwrap();
        vm
    }

    fn write(vm: &Vm, data: &[u8]) {
        let disk = disk::open(&vm.resolve(&vm.config.disks[0].path), false).unwrap();
        disk.write_at(data, 0).unwrap();
        disk.flush().unwrap();
    }

    fn read(vm: &Vm) -> Vec<u8> {
        let disk = disk::open(&vm.resolve(&vm.config.disks[0].path), true).unwrap();
        let mut data = vec![0u8; 5];
        disk.read_at(&mut data, 0).unwrap();
        data
    }

    #[test]
    fn deltas_count_up() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("Windows 11.vmdk");
        assert_eq!(next_delta(&disk), dir.path().join("Windows 11-000001.vmdk"));
        fs::write(dir.path().join("Windows 11-000001.vmdk"), b"").unwrap();
        assert_eq!(next_delta(&disk), dir.path().join("Windows 11-000002.vmdk"));
        assert_eq!(
            next_delta(&dir.path().join("Windows 11-000001.vmdk")),
            dir.path().join("Windows 11-000002.vmdk")
        );
        assert!(is_delta_name(&dir.path().join("a-000003.vmdk")));
        assert!(!is_delta_name(&dir.path().join("a.vmdk")));
    }

    #[test]
    fn take_revert_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = vm(dir.path());
        write(&vm, b"one..");
        let first = take(&mut vm, "First", "").unwrap();
        assert_eq!(vm.config.disks[0].path, "disk-000001.vmdk");
        write(&vm, b"two..");
        vm.config.memory_mib = 512;
        vm.save().unwrap();
        let second = take(&mut vm, "Second", "after two").unwrap();
        assert_eq!(second.parent.as_deref(), Some(first.id.as_str()));
        write(&vm, b"three");

        // Back to the first: its disk, its memory size; what came since is gone.
        assert_eq!(revert(&mut vm, &first.id).unwrap(), None);
        assert_eq!(read(&vm), b"one..");
        assert_eq!(vm.config.memory_mib, 256);
        assert_eq!(vm.config.name, "Snappy");
        let tree = Tree::load(&vm).unwrap();
        assert_eq!(tree.current.as_deref(), Some(first.id.as_str()));
        assert_eq!(
            tree.find("Second").map(|s| s.id.as_str()),
            Some(second.id.as_str())
        );
        // The delta that held "three" is gone; the second snapshot's image stays.
        assert!(!dir.path().join("disk-000002.vmdk").exists());
        assert!(dir.path().join("disk-000001.vmdk").exists());

        // Then to the second: "two".
        revert(&mut vm, &second.id).unwrap();
        assert_eq!(read(&vm), b"two..");
        assert_eq!(vm.config.memory_mib, 512);

        delete(&vm, &first.id).unwrap();
        let tree = Tree::load(&vm).unwrap();
        assert_eq!(tree.snapshots.len(), 1);
        assert_eq!(
            tree.snapshots[0].parent, None,
            "it hangs from the first's parent"
        );
        assert_eq!(read(&vm), b"two..", "the disks are untouched");
    }

    #[test]
    fn deleting_snapshots_merges_the_deltas_nothing_else_needs() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = vm(dir.path());
        write(&vm, b"one..");
        let first = take(&mut vm, "First", "").unwrap();
        write(&vm, b"two..");
        let second = take(&mut vm, "Second", "").unwrap();
        write(&vm, b"three");
        // disk.vmdk (First's) <- disk-000001 (Second's) <- disk-000002 (the VM's).
        assert_eq!(
            consolidate(&mut vm).unwrap(),
            Vec::<String>::new(),
            "all in use"
        );

        delete(&vm, &first.id).unwrap();
        assert_eq!(
            consolidate(&mut vm).unwrap(),
            ["disk-000001.vmdk into disk.vmdk"]
        );
        assert!(!dir.path().join("disk-000001.vmdk").exists());
        assert_eq!(
            snapshot_config(&vm, &second.id).unwrap().disks[0].path,
            "disk.vmdk",
            "the second snapshot's image is the merged one"
        );
        assert_eq!(read(&vm), b"three", "the VM's delta moved onto it");
        revert(&mut vm, &second.id).unwrap();
        assert_eq!(read(&vm), b"two..");

        // The last snapshot: its delta, the VM's, merges too.
        delete(&vm, &second.id).unwrap();
        assert_eq!(consolidate(&mut vm).unwrap().len(), 1);
        assert_eq!(vm.config.disks[0].path, "disk.vmdk");
        assert_eq!(read(&vm), b"two..");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".vmdk"))
            .collect();
        assert_eq!(left, ["disk.vmdk"]);
    }

    #[test]
    fn an_image_two_disks_are_on_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = vm(dir.path());
        let first = take(&mut vm, "First", "").unwrap();
        write(&vm, b"left.");
        take(&mut vm, "Left", "").unwrap();
        revert(&mut vm, &first.id).unwrap();
        write(&vm, b"right");
        delete(&vm, &first.id).unwrap();
        // disk.vmdk has Left's image and the VM's disk on it.
        assert_eq!(consolidate(&mut vm).unwrap(), Vec::<String>::new());
        assert_eq!(read(&vm), b"right");
    }

    #[test]
    fn snapshots_need_vmdk_disks() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = vm(dir.path());
        vm.config.disks[0].path = "disk.raw".into();
        assert!(begin(&vm, "x", "").unwrap_err().contains("VMDK"));
        assert!(begin(&vm, " ", "").is_err(), "a name is needed");
    }
}
