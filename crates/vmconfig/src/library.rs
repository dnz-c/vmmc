//! The library: the VMs the app shows, in folders, and the tabs it had open. VMs live
//! wherever their folders are; the library only points at their `.vmmc` files.
//!
//! It's `%APPDATA%\vmmc\library.toml`, or the file `VMMC_LIBRARY` names.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryData {
    #[serde(default)]
    pub format: u32,
    /// Folder names, in the order the app shows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// The `.vmmc` files of the open tabs, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<String>,
    #[serde(default, rename = "vm", skip_serializing_if = "Vec::is_empty")]
    pub vms: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The VM's `.vmmc` file.
    pub path: String,
    /// The folder it's in; none for the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

pub struct Library {
    pub path: PathBuf,
    pub data: LibraryData,
}

/// Where the library is kept.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VMMC_LIBRARY") {
        return path.into();
    }
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("vmmc").join("library.toml")
}

/// Where new VMs go by default: a `vmmc VMs` folder in the user's documents.
pub fn default_vm_root() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("vmmc VMs")
}

impl Library {
    /// The library at `path`, or an empty one if there's none yet.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let data = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| Error::Invalid {
                path: path.to_owned(),
                message: e.to_string(),
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => LibraryData {
                format: crate::FORMAT,
                ..LibraryData::default()
            },
            Err(source) => {
                return Err(Error::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Ok(Self {
            path: path.to_owned(),
            data,
        })
    }

    pub fn save(&self) -> Result<(), Error> {
        let write = |source| Error::Write {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(write)?;
        }
        let text = toml::to_string(&self.data)
            .map_err(|e| write(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        let partial = self.path.with_extension("toml-partial");
        fs::write(&partial, text).map_err(write)?;
        fs::rename(&partial, &self.path).map_err(write)
    }

    /// Adds a VM (once), in `folder`, which is made if it's new.
    pub fn add(&mut self, vm: &Path, folder: Option<&str>) {
        let path = vm.display().to_string();
        if let Some(folder) = folder
            && !self.data.folders.iter().any(|f| f == folder)
        {
            self.data.folders.push(folder.to_owned());
        }
        match self.data.vms.iter_mut().find(|e| same_path(&e.path, &path)) {
            Some(entry) => entry.folder = folder.map(str::to_owned),
            None => self.data.vms.push(Entry {
                path,
                folder: folder.map(str::to_owned),
            }),
        }
    }

    /// Takes a VM out of the library (its files stay). Returns whether it was there.
    pub fn remove(&mut self, vm: &Path) -> bool {
        let path = vm.display().to_string();
        let before = self.data.vms.len();
        self.data.vms.retain(|e| !same_path(&e.path, &path));
        self.data.tabs.retain(|t| !same_path(t, &path));
        self.data.vms.len() != before
    }

    pub fn entries(&self) -> &[Entry] {
        &self.data.vms
    }
}

/// Windows paths are case-insensitive.
fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_vms_in_folders_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vmmc").join("library.toml");
        let mut library = Library::load(&path).unwrap();
        assert!(library.entries().is_empty());
        library.add(Path::new(r"C:\VMs\a\a.vmmc"), Some("Work"));
        library.add(Path::new(r"C:\VMs\b\b.vmmc"), None);
        library.add(Path::new(r"C:\VMs\a\a.vmmc"), Some("Test"));
        library.data.tabs.push(r"C:\VMs\b\b.vmmc".into());
        library.save().unwrap();
        let mut back = Library::load(&path).unwrap();
        assert_eq!(back.data, library.data);
        assert_eq!(back.data.folders, ["Work", "Test"]);
        assert_eq!(back.entries().len(), 2);
        assert_eq!(back.entries()[0].folder.as_deref(), Some("Test"));
        assert!(back.remove(Path::new(r"C:\VMs\b\b.vmmc")));
        assert!(back.data.tabs.is_empty(), "its tab closed too");
        assert!(!back.remove(Path::new(r"C:\VMs\b\b.vmmc")));
    }
}
