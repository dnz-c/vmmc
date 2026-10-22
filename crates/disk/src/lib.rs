//! Disk images for vmmc's block devices (virtio-blk, NVMe, AHCI).
//!
//! A [`BlockBackend`] is a flat array of bytes. Devices check every guest-supplied range
//! against [`BlockBackend::len`] before calling in, so backends only see valid ranges.
//! Images are raw files ([`RawFile`]) or VMDKs ([`vmdk`]); [`open`] tells them apart.

mod switchable;
pub mod vmdk;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use switchable::Switchable;
pub use vmdk::{
    compact, create_delta, create_sparse, expand, merge_into_parent, parent_of, reparent,
};

/// Opens a disk image: a VMDK if its name ends in `.vmdk`, otherwise a raw image. The
/// name decides, never the contents: a guest can write anything into a raw image,
/// including a VMDK header whose extents name host files.
pub fn open(path: &Path, read_only: bool) -> io::Result<Arc<dyn BlockBackend>> {
    let is_vmdk = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vmdk"));
    Ok(if is_vmdk {
        vmdk::open(path, read_only)?
    } else {
        Arc::new(RawFile::open(path, read_only)?)
    })
}

/// Where a block device's data lives.
pub trait BlockBackend: Send + Sync {
    /// Size in bytes.
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn is_read_only(&self) -> bool;
    /// Fills `buf` from `offset`. The caller has checked the range is inside the disk.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Writes `buf` at `offset`. The caller has checked the range is inside the disk.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn flush(&self) -> io::Result<()>;

    /// Zeroes `len` bytes at `offset`. The caller has checked the range.
    fn write_zeroes(&self, mut offset: u64, mut len: u64) -> io::Result<()> {
        const CHUNK: u64 = 1 << 20;
        let zeroes = vec![0u8; CHUNK.min(len) as usize];
        while len > 0 {
            let now = CHUNK.min(len);
            self.write_at(&zeroes[..now as usize], offset)?;
            offset += now;
            len -= now;
        }
        Ok(())
    }

    /// Tells the backend `len` bytes at `offset` are no longer needed. Their contents
    /// are undefined afterwards. Backends may ignore this.
    fn discard(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }

    /// For a delta disk, the image at the bottom of its chain: the disk it started as,
    /// which snapshots keep the same. `None` for an image that's no delta.
    fn base_image(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Copies a whole disk into another of at least its size, skipping zeroes so a sparse
/// destination stays sparse. `progress` hears the bytes copied so far.
pub fn copy(
    from: &dyn BlockBackend,
    to: &dyn BlockBackend,
    mut progress: impl FnMut(u64),
) -> io::Result<()> {
    const CHUNK: usize = 1 << 20;
    if to.len() < from.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the destination is smaller than the source",
        ));
    }
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0;
    while offset < from.len() {
        let now = (from.len() - offset).min(CHUNK as u64) as usize;
        from.read_at(&mut buf[..now], offset)?;
        // A new destination reads as zeroes already: only pieces holding data are
        // written, 64 KiB (a VMDK grain) at a time, so sparse images stay sparse.
        for (index, sector) in buf[..now].chunks(64 << 10).enumerate() {
            if sector.iter().any(|&b| b != 0) {
                to.write_at(sector, offset + (index * (64 << 10)) as u64)?;
            }
        }
        offset += now as u64;
        progress(offset);
    }
    to.flush()
}

/// A raw disk image file: byte N of the disk is byte N of the file.
#[derive(Debug)]
pub struct RawFile {
    file: File,
    len: u64,
    read_only: bool,
}

impl RawFile {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            read_only,
        })
    }
}

/// Creates a raw disk image of `len` bytes. On NTFS it's a sparse file, so it takes no
/// space until the guest writes to it.
pub fn create_raw(path: &Path, len: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    set_sparse(&file)?;
    file.set_len(len)
}

#[cfg(windows)]
fn set_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let mut returned = 0u32;
    // SAFETY: the handle is a valid open file for the duration of the call, and
    // FSCTL_SET_SPARSE with no input buffer means "make sparse".
    let result = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    // Not every file system has sparse files (FAT, some network shares); the image just
    // takes its full size there.
    if let Err(e) = result {
        tracing::debug!(%e, "can't make the disk image sparse");
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_sparse(_: &File) -> io::Result<()> {
    // Files are sparse by default on the usual Unix file systems.
    Ok(())
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

impl BlockBackend for RawFile {
    fn len(&self) -> u64 {
        self.len
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match read_at(&self.file, buf, offset)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }

    fn write_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match write_at(&self.file, buf, offset)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        // Nothing to flush, and Windows refuses to flush a handle opened for reading.
        if self.read_only {
            return Ok(());
        }
        self.file.sync_data()
    }
}

/// A disk held in memory, for tests.
#[derive(Debug)]
pub struct MemDisk {
    data: Mutex<Vec<u8>>,
    read_only: bool,
    flushes: Mutex<u32>,
}

impl MemDisk {
    pub fn new(len: usize, read_only: bool) -> Self {
        Self {
            data: Mutex::new(vec![0; len]),
            read_only,
            flushes: Mutex::new(0),
        }
    }

    /// How many times the disk was flushed.
    pub fn flushes(&self) -> u32 {
        *self.flushes.lock().unwrap()
    }

    /// A copy of `len` bytes at `offset`.
    pub fn bytes(&self, offset: usize, len: usize) -> Vec<u8> {
        self.data.lock().unwrap()[offset..offset + len].to_vec()
    }
}

impl BlockBackend for MemDisk {
    fn len(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let data = self.data.lock().unwrap();
        buf.copy_from_slice(&data[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut data = self.data.lock().unwrap();
        data[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_file_backend_round_trips() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4096).unwrap();
        let raw = RawFile::open(file.path(), false).unwrap();
        assert_eq!(raw.len(), 4096);
        raw.write_at(b"vmmc", 1000).unwrap();
        let mut back = [0u8; 4];
        raw.read_at(&mut back, 1000).unwrap();
        assert_eq!(&back, b"vmmc");
        raw.write_zeroes(998, 4).unwrap();
        raw.read_at(&mut back, 1000).unwrap();
        assert_eq!(&back, b"\0\0mc");
        raw.flush().unwrap();
    }

    #[test]
    fn new_images_have_the_size_asked_for_and_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        create_raw(&path, 64 << 30).unwrap();
        let raw = RawFile::open(&path, true).unwrap();
        assert_eq!(raw.len(), 64 << 30);
        let mut tail = [0xffu8; 16];
        raw.read_at(&mut tail, (64 << 30) - 16).unwrap();
        assert_eq!(tail, [0; 16]);
        assert!(create_raw(&path, 1 << 20).is_err(), "existing image kept");
    }

    #[test]
    fn flushing_a_read_only_image_is_a_no_op() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4096).unwrap();
        RawFile::open(file.path(), true).unwrap().flush().unwrap();
    }

    #[test]
    fn write_zeroes_spans_chunks() {
        let disk = MemDisk::new(3 << 20, false);
        disk.write_at(&[1; 3 << 20], 0).unwrap();
        disk.write_zeroes(1, (3 << 20) - 2).unwrap();
        assert_eq!(disk.bytes(0, 2), [1, 0]);
        assert_eq!(disk.bytes((3 << 20) - 2, 2), [0, 1]);
    }
}
