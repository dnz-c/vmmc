//! A disk whose image can change under a running VM: a snapshot moves the VM's writes
//! to a new delta disk, and the image it was writing becomes the snapshot's.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use crate::BlockBackend;

/// A [`BlockBackend`] whose image can be switched. Change it only while nothing does
/// I/O on it: the machine switches disks with its vCPUs paused and its devices
/// quiesced.
pub struct Switchable {
    image: RwLock<Image>,
}

struct Image {
    path: PathBuf,
    backend: Arc<dyn BlockBackend>,
}

impl Switchable {
    /// The image at `path`, opened as `backend`.
    pub fn new(path: &Path, backend: Arc<dyn BlockBackend>) -> Arc<Self> {
        Arc::new(Self {
            image: RwLock::new(Image {
                path: path.to_owned(),
                backend,
            }),
        })
    }

    /// The image the disk reads and writes now.
    pub fn path(&self) -> PathBuf {
        self.image
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .path
            .clone()
    }

    fn backend(&self) -> Arc<dyn BlockBackend> {
        self.image
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .backend
            .clone()
    }

    /// Moves the disk's writes to a new delta at `delta`, on top of the image it has
    /// now: that image is flushed, and from then on read-only, beneath the delta.
    /// Only for VMDK images. If it fails, the disk goes on with the image it had.
    pub fn switch_to_delta(&self, delta: &Path) -> io::Result<()> {
        let mut image = self.image.write().unwrap_or_else(PoisonError::into_inner);
        if !is_vmdk(&image.path) {
            return Err(io::Error::other(format!(
                "{} isn't a VMDK: snapshots need VMDK disks (vmmc-cli disk convert makes one)",
                image.path.display()
            )));
        }
        if image.backend.is_read_only() {
            return Err(io::Error::other(format!(
                "{} is read-only",
                image.path.display()
            )));
        }
        image.backend.flush()?;
        // The image is opened again read-only beneath the delta, which its own lock
        // allows only once this handle is closed.
        let parent = image.path.clone();
        image.backend = Arc::new(Closed(image.backend.len()));
        let switched = crate::vmdk::create_delta(delta, &parent)
            .and_then(|()| crate::open(delta, false))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(delta);
            });
        match switched {
            Ok(backend) => {
                *image = Image {
                    path: delta.to_owned(),
                    backend,
                };
                Ok(())
            }
            Err(e) => {
                image.backend = crate::open(&parent, false)?;
                Err(e)
            }
        }
    }

    /// Makes the disk read and write `path` instead, opened read-write (a revert, to a
    /// new delta on top of a snapshot's image).
    pub fn replace(&self, path: &Path) -> io::Result<()> {
        let mut image = self.image.write().unwrap_or_else(PoisonError::into_inner);
        let len = image.backend.len();
        image.backend = Arc::new(Closed(len));
        let backend = crate::open(path, false)?;
        if backend.len() != len {
            return Err(io::Error::other(format!(
                "{} is {} bytes, not {len}",
                path.display(),
                backend.len()
            )));
        }
        *image = Image {
            path: path.to_owned(),
            backend,
        };
        Ok(())
    }
}

fn is_vmdk(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vmdk"))
}

impl BlockBackend for Switchable {
    fn len(&self) -> u64 {
        self.backend().len()
    }

    fn is_read_only(&self) -> bool {
        self.backend().is_read_only()
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.backend().read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.backend().write_at(buf, offset)
    }

    fn flush(&self) -> io::Result<()> {
        self.backend().flush()
    }

    fn write_zeroes(&self, offset: u64, len: u64) -> io::Result<()> {
        self.backend().write_zeroes(offset, len)
    }

    fn discard(&self, offset: u64, len: u64) -> io::Result<()> {
        self.backend().discard(offset, len)
    }

    fn base_image(&self) -> Option<PathBuf> {
        self.backend().base_image()
    }
}

/// A disk that's being switched: never seen by I/O, which is paused meanwhile.
struct Closed(u64);

impl BlockBackend for Closed {
    fn len(&self) -> u64 {
        self.0
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn read_at(&self, _: &mut [u8], _: u64) -> io::Result<()> {
        Err(io::Error::other("the disk is being switched"))
    }

    fn write_at(&self, _: &[u8], _: u64) -> io::Result<()> {
        Err(io::Error::other("the disk is being switched"))
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_move_to_the_delta_and_the_image_below_keeps_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("disk.vmdk");
        crate::create_sparse(&base, 4 << 20).unwrap();
        let disk = Switchable::new(&base, crate::open(&base, false).unwrap());
        disk.write_at(b"before", 0).unwrap();

        let delta = dir.path().join("disk-000001.vmdk");
        disk.switch_to_delta(&delta).unwrap();
        assert_eq!(disk.path(), delta);
        disk.write_at(b"after!", 0).unwrap();
        let mut data = [0u8; 6];
        disk.read_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"after!");
        disk.flush().unwrap();

        // The snapshot's image still has what was written before the switch.
        drop(disk);
        let frozen = crate::open(&base, true).unwrap();
        frozen.read_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"before");

        // Reverting: a new delta on the snapshot's image, which the disk switches to.
        drop(frozen);
        let disk = Switchable::new(&delta, crate::open(&delta, false).unwrap());
        let reverted = dir.path().join("disk-000002.vmdk");
        disk.replace(&base).unwrap();
        disk.switch_to_delta(&reverted).unwrap();
        disk.read_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"before");
    }

    #[test]
    fn raw_images_are_refused_and_keep_working() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("disk.raw");
        crate::create_raw(&raw, 1 << 20).unwrap();
        let disk = Switchable::new(&raw, crate::open(&raw, false).unwrap());
        assert!(disk.switch_to_delta(&dir.path().join("x.vmdk")).is_err());
        disk.write_at(b"still", 0).unwrap();
    }
}
