//! VMware's virtual disk format (VMDK), written from VMware's published Virtual Disk
//! Format specification: a descriptor listing extents (hosted sparse, flat or zero
//! data), optionally on top of a parent disk, which is how VMware's snapshots work.
//!
//! - **Sparse extents** allocate 64 KiB grains as the guest writes; [`create_sparse`]
//!   makes the monolithic kind, vmmc's own disk format.
//! - **Delta disks** ([`create_delta`]) hold only what changed since their parent, which
//!   they name in `parentFileNameHint` and check by its content ID (CID): a parent that
//!   changed after the delta was made is refused, as VMware refuses it.
//! - **The CID** changes when a disk is first written after opening.
//! - **Locking:** extent files are opened without sharing write access, so VMware can't
//!   write to a disk vmmc has open, and vmmc can't open one VMware is using.

pub mod descriptor;
pub mod sparse;

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use self::descriptor::{Access, Descriptor, ExtentLine, ExtentType, NO_PARENT};
use self::sparse::{Grain, SECTOR, SparseExtent};
use crate::BlockBackend;

/// The deepest chain of delta disks opened; deeper is surely a loop.
const MAX_CHAIN: usize = 64;
/// Sectors set aside for the descriptor in a new monolithic sparse disk, as VMware does.
const DESCRIPTOR_SECTORS: u64 = 20;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub(crate) fn file_read(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match crate::read_at(file, buf, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

pub(crate) fn file_write(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match crate::write_at(file, buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Opens an extent or descriptor file, sharing it only for reading.
fn open_file(path: &Path, read_only: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(!read_only);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path).map_err(|e| {
        if e.raw_os_error() == Some(32) {
            // ERROR_SHARING_VIOLATION
            io::Error::new(
                e.kind(),
                format!(
                    "{} is in use by another program (VMware, or another vmmc VM)",
                    path.display()
                ),
            )
        } else {
            io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        }
    })
}

fn random_u128() -> u128 {
    let state = RandomState::new();
    u128::from(state.hash_one(1u8)) << 64 | u128::from(state.hash_one(2u8))
}

enum ExtentData {
    Sparse(SparseExtent),
    Flat { file: File, offset: u64 },
    Zero,
}

struct Extent {
    /// Where it starts on the disk, in sectors.
    start: u64,
    sectors: u64,
    access: Access,
    data: ExtentData,
}

/// Where the descriptor lives, for writing it back with a new CID.
enum DescriptorHome {
    /// A file of its own.
    File(PathBuf),
    /// Embedded in the first extent's file, at this sector, with room for this many.
    Embedded { sector: u64, sectors: u64 },
}

/// An open VMDK, with its chain of parents.
pub struct Vmdk {
    path: PathBuf,
    descriptor: Mutex<Descriptor>,
    home: DescriptorHome,
    extents: Vec<Extent>,
    parent: Option<Box<Vmdk>>,
    len: u64,
    read_only: bool,
    /// The CID has been changed since opening.
    written: AtomicBool,
}

/// What a file holds, from its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A sparse extent with its descriptor inside (monolithic or stream-optimized).
    Sparse,
    /// A descriptor file.
    Descriptor,
    /// Anything else: a raw image.
    Raw,
}

pub fn detect(file: &File) -> io::Result<Kind> {
    let mut start = [0u8; 32];
    let len = file.metadata()?.len().min(32) as usize;
    file_read(file, &mut start[..len], 0)?;
    if start[..4] == sparse::MAGIC.to_le_bytes() {
        Ok(Kind::Sparse)
    } else if start[..len].starts_with(b"# Disk DescriptorFile") {
        Ok(Kind::Descriptor)
    } else {
        Ok(Kind::Raw)
    }
}

impl Vmdk {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        Self::open_chain(path, read_only, 0)
    }

    fn open_chain(path: &Path, read_only: bool, depth: usize) -> io::Result<Self> {
        if depth > MAX_CHAIN {
            return Err(invalid("chain of delta disks too long (a loop?)"));
        }
        let first = open_file(path, read_only)?;
        let (descriptor, home) = match detect(&first)? {
            Kind::Sparse => {
                let mut bytes = [0u8; 512];
                file_read(&first, &mut bytes, 0)?;
                let header = sparse::Header::parse(&bytes)?;
                if header.descriptor_offset == 0 || header.descriptor_sectors == 0 {
                    return Err(invalid(
                        "this is one extent of a split disk: open its descriptor .vmdk",
                    ));
                }
                let len = header.descriptor_sectors * SECTOR;
                if len > descriptor::MAX_LEN as u64 {
                    return Err(invalid("embedded descriptor too large"));
                }
                let mut text = vec![0u8; len as usize];
                file_read(&first, &mut text, header.descriptor_offset * SECTOR)?;
                (
                    Descriptor::parse(&text).map_err(|e| invalid(&e))?,
                    DescriptorHome::Embedded {
                        sector: header.descriptor_offset,
                        sectors: header.descriptor_sectors,
                    },
                )
            }
            Kind::Descriptor => {
                let len = first.metadata()?.len();
                if len > descriptor::MAX_LEN as u64 {
                    return Err(invalid("descriptor file too large"));
                }
                let mut text = vec![0u8; len as usize];
                file_read(&first, &mut text, 0)?;
                (
                    Descriptor::parse(&text).map_err(|e| invalid(&e))?,
                    DescriptorHome::File(path.to_owned()),
                )
            }
            Kind::Raw => return Err(invalid("not a VMDK")),
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut first = Some(first);
        let mut extents = Vec::new();
        let mut start = 0u64;
        for (index, line) in descriptor.extents.iter().enumerate() {
            let data = match line.kind {
                ExtentType::Zero => ExtentData::Zero,
                kind => {
                    let extent_path = dir.join(&line.file);
                    let writable = !read_only && line.access == Access::ReadWrite;
                    // A monolithic disk's only extent is the file already open.
                    let file = match (&home, index) {
                        (DescriptorHome::Embedded { .. }, 0) => first.take().unwrap(),
                        _ => open_file(&extent_path, !writable)?,
                    };
                    match kind {
                        ExtentType::Sparse => {
                            let extent = SparseExtent::open(file, !writable)?;
                            if extent.header().capacity != line.sectors {
                                return Err(invalid(&format!(
                                    "{}: its size doesn't match the descriptor",
                                    extent_path.display()
                                )));
                            }
                            ExtentData::Sparse(extent)
                        }
                        _ => ExtentData::Flat {
                            file,
                            offset: line
                                .offset
                                .checked_mul(SECTOR)
                                .ok_or_else(|| invalid("flat extent offset too large"))?,
                        },
                    }
                }
            };
            extents.push(Extent {
                start,
                sectors: line.sectors,
                access: line.access,
                data,
            });
            start = start
                .checked_add(line.sectors)
                .filter(|&s| s <= 1 << 37)
                .ok_or_else(|| invalid("disk too large"))?;
        }
        let parent = if descriptor.parent_cid != NO_PARENT {
            let hint = descriptor
                .parent_hint
                .as_deref()
                .ok_or_else(|| invalid("delta disk without parentFileNameHint"))?;
            let parent_path = dir.join(hint);
            let parent = Self::open_chain(&parent_path, true, depth + 1)?;
            let parent_cid = parent
                .descriptor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cid;
            if parent_cid != descriptor.parent_cid {
                return Err(invalid(&format!(
                    "{} changed after the delta disk {} was made on it (its content ID is {parent_cid:08x}, not {:08x})",
                    parent_path.display(),
                    path.display(),
                    descriptor.parent_cid
                )));
            }
            Some(Box::new(parent))
        } else {
            None
        };
        Ok(Vmdk {
            path: path.to_owned(),
            descriptor: Mutex::new(descriptor),
            home,
            extents,
            parent,
            len: start * SECTOR,
            read_only,
            written: AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn descriptor(&self) -> Descriptor {
        self.descriptor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn parent(&self) -> Option<&Vmdk> {
        self.parent.as_deref()
    }

    /// Bytes the disk's own files take (not its parents').
    pub fn allocated(&self) -> u64 {
        self.extents
            .iter()
            .map(|extent| match &extent.data {
                ExtentData::Sparse(sparse) => sparse.file().metadata().map_or(0, |m| m.len()),
                ExtentData::Flat { .. } => extent.sectors * SECTOR,
                ExtentData::Zero => 0,
            })
            .sum()
    }

    /// The first write after opening gives the disk a new CID, which tells its
    /// children (and VMware) its contents changed.
    fn note_write(&self) -> io::Result<()> {
        if self.written.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut descriptor = self.descriptor.lock().unwrap_or_else(|e| e.into_inner());
        descriptor.new_content_id(random_u128());
        self.write_descriptor(&descriptor)
    }

    fn write_descriptor(&self, descriptor: &Descriptor) -> io::Result<()> {
        let text = descriptor.to_text();
        match &self.home {
            DescriptorHome::File(path) => {
                let partial = path.with_extension("vmdk-partial");
                fs::write(&partial, text.as_bytes())?;
                fs::rename(&partial, path)
            }
            DescriptorHome::Embedded { sector, sectors } => {
                let room = (sectors * SECTOR) as usize;
                if text.len() > room {
                    return Err(invalid("descriptor doesn't fit in its extent"));
                }
                let mut bytes = text.into_bytes();
                bytes.resize(room, 0);
                let ExtentData::Sparse(extent) = &self.extents[0].data else {
                    unreachable!("an embedded descriptor is in a sparse extent");
                };
                file_write(extent.file(), &bytes, sector * SECTOR)
            }
        }
    }

    /// Calls `f(extent, offset within the extent, bytes, offset within buf)` for the
    /// pieces of `len` bytes at `offset`.
    fn pieces(
        &self,
        offset: u64,
        len: usize,
        mut f: impl FnMut(&Extent, u64, usize, usize) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut done = 0usize;
        while done < len {
            let at = offset + done as u64;
            let extent = self
                .extents
                .iter()
                .find(|e| at < (e.start + e.sectors) * SECTOR)
                .ok_or_else(|| invalid("beyond the disk"))?;
            let within = at - extent.start * SECTOR;
            let room = (extent.sectors * SECTOR - within) as usize;
            let now = room.min(len - done);
            f(extent, within, now, done)?;
            done += now;
        }
        Ok(())
    }

    /// Reads from the parent, or zeroes without one (or past its end).
    fn read_below(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        match &self.parent {
            Some(parent) if offset < parent.len => {
                let inside = ((parent.len - offset) as usize).min(buf.len());
                parent.read_at(&mut buf[..inside], offset)?;
                buf[inside..].fill(0);
                Ok(())
            }
            _ => {
                buf.fill(0);
                Ok(())
            }
        }
    }

    fn read_sparse(
        &self,
        sparse: &SparseExtent,
        extent_start: u64,
        mut within: u64,
        buf: &mut [u8],
    ) -> io::Result<()> {
        let grain = sparse.header().grain_bytes();
        let mut done = 0;
        while done < buf.len() {
            let index = within / grain;
            let in_grain = within % grain;
            let now = ((grain - in_grain) as usize).min(buf.len() - done);
            let piece = &mut buf[done..done + now];
            match sparse.lookup(index)? {
                Grain::At(at) => sparse.read_grain(at, in_grain, piece)?,
                Grain::Zero => piece.fill(0),
                Grain::Unallocated => self.read_below(piece, extent_start + within)?,
            }
            done += now;
            within += now as u64;
        }
        Ok(())
    }

    fn write_sparse(
        &self,
        sparse: &SparseExtent,
        extent_start: u64,
        mut within: u64,
        data: &[u8],
        zeroes: bool,
    ) -> io::Result<()> {
        let grain = sparse.header().grain_bytes();
        let mut done = 0;
        while done < data.len() {
            let index = within / grain;
            let in_grain = within % grain;
            let now = ((grain - in_grain) as usize).min(data.len() - done);
            let piece = &data[done..done + now];
            match sparse.lookup(index)? {
                Grain::At(at) => sparse.write_grain(at, in_grain, piece)?,
                // Zeroes over a grain that already reads as zeroes change nothing.
                Grain::Unallocated if zeroes && self.parent.is_none() => {}
                Grain::Zero if zeroes => {}
                state => {
                    // A new grain: what's there now, with the write on top.
                    let grain_start = within - in_grain;
                    let mut whole = vec![0u8; grain as usize];
                    if state == Grain::Unallocated {
                        let disk_len =
                            (sparse.header().capacity * SECTOR - grain_start).min(grain) as usize;
                        self.read_below(&mut whole[..disk_len], extent_start + grain_start)?;
                    }
                    whole[in_grain as usize..in_grain as usize + now].copy_from_slice(piece);
                    let (at, allocated) = sparse.allocate(index, &whole)?;
                    if !allocated {
                        // Another write allocated it in the meantime.
                        sparse.write_grain(at, in_grain, piece)?;
                    }
                }
            }
            done += now;
            within += now as u64;
        }
        Ok(())
    }

    fn write(&self, buf: &[u8], offset: u64, zeroes: bool) -> io::Result<()> {
        if self.read_only {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.note_write()?;
        self.pieces(offset, buf.len(), |extent, within, len, at| {
            let data = &buf[at..at + len];
            if extent.access != Access::ReadWrite {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            match &extent.data {
                ExtentData::Sparse(sparse) => {
                    self.write_sparse(sparse, extent.start * SECTOR, within, data, zeroes)
                }
                ExtentData::Flat { file, offset } => file_write(file, data, offset + within),
                ExtentData::Zero => Err(io::ErrorKind::PermissionDenied.into()),
            }
        })
    }
}

impl BlockBackend for Vmdk {
    fn len(&self) -> u64 {
        self.len
    }

    fn base_image(&self) -> Option<PathBuf> {
        let mut base = self.parent.as_deref()?;
        while let Some(parent) = base.parent.as_deref() {
            base = parent;
        }
        Some(base.path.clone())
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let len = buf.len();
        self.pieces(offset, len, |extent, within, len, at| {
            let piece = &mut buf[at..at + len];
            match &extent.data {
                _ if extent.access == Access::NoAccess => {
                    Err(io::ErrorKind::PermissionDenied.into())
                }
                ExtentData::Sparse(sparse) => {
                    self.read_sparse(sparse, extent.start * SECTOR, within, piece)
                }
                ExtentData::Flat { file, offset } => file_read(file, piece, offset + within),
                ExtentData::Zero => {
                    piece.fill(0);
                    Ok(())
                }
            }
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.write(buf, offset, false)
    }

    fn write_zeroes(&self, mut offset: u64, mut len: u64) -> io::Result<()> {
        const CHUNK: u64 = 1 << 20;
        let zeroes = vec![0u8; CHUNK.min(len) as usize];
        while len > 0 {
            let now = CHUNK.min(len);
            self.write(&zeroes[..now as usize], offset, true)?;
            offset += now;
            len -= now;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        // Windows refuses to flush handles opened for reading, and they hold nothing
        // to flush.
        if self.read_only {
            return Ok(());
        }
        for extent in self
            .extents
            .iter()
            .filter(|e| e.access == Access::ReadWrite)
        {
            match &extent.data {
                ExtentData::Sparse(sparse) => sparse.flush()?,
                ExtentData::Flat { file, .. } => file.sync_data()?,
                ExtentData::Zero => {}
            }
        }
        Ok(())
    }
}

/// A descriptor's DDB for a new disk of `sectors`: the adapter type and a geometry, as
/// VMware writes them.
fn new_ddb(sectors: u64) -> Vec<(String, String)> {
    let (heads, per_track) = if sectors < 2 << 20 {
        (64, 32)
    } else {
        (255, 63)
    };
    let cylinders = (sectors / (heads * per_track)).clamp(1, 65_535);
    vec![
        ("ddb.adapterType".into(), "lsilogic".into()),
        ("ddb.geometry.cylinders".into(), cylinders.to_string()),
        ("ddb.geometry.heads".into(), heads.to_string()),
        ("ddb.geometry.sectors".into(), per_track.to_string()),
        ("ddb.longContentID".into(), String::new()),
        ("ddb.virtualHWVersion".into(), "13".into()),
    ]
}

/// Writes a new monolithic sparse disk of `len` bytes (whole sectors) with `descriptor`
/// (its extent filled in). An existing file is never replaced.
fn write_sparse_file(path: &Path, len: u64, mut descriptor: Descriptor) -> io::Result<()> {
    if len == 0 || !len.is_multiple_of(SECTOR) {
        return Err(invalid("a disk is a whole number of 512-byte sectors"));
    }
    let sectors = len / SECTOR;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("the disk's file name isn't valid Unicode"))?;
    descriptor.extents = vec![ExtentLine {
        access: Access::ReadWrite,
        sectors,
        kind: ExtentType::Sparse,
        file: name.to_owned(),
        offset: 0,
    }];
    descriptor.new_content_id(random_u128());
    let text = descriptor.to_text();
    if text.len() as u64 > DESCRIPTOR_SECTORS * SECTOR {
        return Err(invalid("the disk's name or its parent's path is too long"));
    }
    let (_, mut bytes) = sparse::layout(sectors, DESCRIPTOR_SECTORS);
    let at = SECTOR as usize;
    bytes[at..at + text.len()].copy_from_slice(text.as_bytes());
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file_write(&file, &bytes, 0)?;
    file.sync_all()
}

/// Creates a monolithic sparse VMDK of `len` bytes, which takes about a megabyte per
/// 16 GB of size until the guest writes to it. An existing file is never replaced.
pub fn create_sparse(path: &Path, len: u64) -> io::Result<()> {
    let descriptor = Descriptor {
        other: Vec::new(),
        cid: NO_PARENT,
        parent_cid: NO_PARENT,
        create_type: "monolithicSparse".into(),
        parent_hint: None,
        extents: Vec::new(),
        ddb: new_ddb(len / SECTOR),
    };
    write_sparse_file(path, len, descriptor)
}

/// Creates a delta disk at `path` on top of `parent`, which from then on must not
/// change (it's opened read-only beneath the delta). The delta names the parent by a
/// path relative to its own directory when they share a drive.
pub fn create_delta(path: &Path, parent: &Path) -> io::Result<()> {
    let base = Vmdk::open(parent, true)?;
    let base_descriptor = base.descriptor();
    let dir = path.parent().unwrap_or(Path::new("."));
    let hint = relative_path(dir, parent);
    let mut ddb = base_descriptor.ddb.clone();
    ddb.retain(|(key, _)| !key.starts_with("ddb.uuid") && !key.starts_with("ddb.deletable"));
    let descriptor = Descriptor {
        other: Vec::new(),
        cid: NO_PARENT,
        parent_cid: base_descriptor.cid,
        create_type: "monolithicSparse".into(),
        parent_hint: Some(hint),
        extents: Vec::new(),
        ddb,
    };
    write_sparse_file(path, base.len(), descriptor)
}

/// `target` as seen from `dir`: relative when both are absolute on one drive (or
/// both relative), absolute otherwise. Uses `\` on Windows, as VMware does.
fn relative_path(dir: &Path, target: &Path) -> String {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
    let target = std::path::absolute(target).unwrap_or_else(|_| target.to_owned());
    let dir_parts: Vec<_> = dir.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let common = dir_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let separator = std::path::MAIN_SEPARATOR_STR;
    if common == 0 {
        return target.display().to_string();
    }
    let mut parts: Vec<String> = vec!["..".into(); dir_parts.len() - common];
    parts.extend(
        target_parts[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join(separator)
}

/// Opens `path` as a disk with its parents, sharing it.
pub fn open(path: &Path, read_only: bool) -> io::Result<Arc<Vmdk>> {
    Vmdk::open(path, read_only).map(Arc::new)
}

#[cfg(test)]
mod tests;
