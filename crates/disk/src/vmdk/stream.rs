//! Writing stream-optimized VMDKs, the kind OVF packages carry. The file is written front
//! to back, as VMware's Virtual Disk Format 5.0 lays it out: the header (its grain
//! directory "at the end") and the descriptor; each grain that isn't all zeroes,
//! compressed (zlib) behind a marker with its LBA and size; after each grain table's
//! grains, that table behind a marker; then the grain directory, a footer (the header
//! again, with the directory's place), and the end-of-stream marker.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use super::descriptor::{Access, Descriptor, ExtentLine, ExtentType, NO_PARENT};
use super::sparse::{self, Header, SECTOR};
use super::{invalid, new_ddb, random_u128};
use crate::BlockBackend;

const MARKER_EOS: u32 = 0;
const MARKER_GT: u32 = 1;
const MARKER_GD: u32 = 2;
const MARKER_FOOTER: u32 = 3;
/// Where the grains start, as in VMware's stream-optimized disks.
const OVERHEAD: u64 = 128;

/// A sector-counting writer.
struct Out {
    file: BufWriter<File>,
    /// The next sector.
    at: u64,
}

impl Out {
    /// Writes `bytes`, padded with zeroes to whole sectors; returns the sector they
    /// started at.
    fn sectors(&mut self, bytes: &[u8]) -> io::Result<u64> {
        let start = self.at;
        self.file.write_all(bytes)?;
        let padding = bytes.len().next_multiple_of(SECTOR as usize) - bytes.len();
        self.file.write_all(&vec![0u8; padding])?;
        self.at += (bytes.len() + padding) as u64 / SECTOR;
        Ok(start)
    }

    /// A metadata marker: `sectors` of `kind` follow.
    fn marker(&mut self, sectors: u64, kind: u32) -> io::Result<()> {
        let mut marker = [0u8; SECTOR as usize];
        marker[0..8].copy_from_slice(&sectors.to_le_bytes());
        marker[12..16].copy_from_slice(&kind.to_le_bytes());
        self.sectors(&marker).map(|_| ())
    }
}

fn table_bytes(entries: &[u32]) -> Vec<u8> {
    entries
        .iter()
        .flat_map(|entry| entry.to_le_bytes())
        .collect()
}

/// Writes `from` (any disk) as a new stream-optimized VMDK at `path`, whose descriptor
/// names its extent by the file's name. `progress` hears the bytes read so far. An
/// existing file is never replaced.
pub fn write_stream_optimized(
    from: &dyn BlockBackend,
    path: &Path,
    mut progress: impl FnMut(u64),
) -> io::Result<()> {
    let len = from.len();
    if len == 0 || !len.is_multiple_of(SECTOR) {
        return Err(invalid("a disk is a whole number of 512-byte sectors"));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("the disk's file name isn't valid Unicode"))?;
    let capacity = len / SECTOR;
    let mut descriptor = Descriptor {
        other: Vec::new(),
        cid: NO_PARENT,
        parent_cid: NO_PARENT,
        create_type: "streamOptimized".into(),
        parent_hint: None,
        extents: vec![ExtentLine {
            access: Access::ReadWrite,
            sectors: capacity,
            kind: ExtentType::Sparse,
            file: name.to_owned(),
            offset: 0,
        }],
        ddb: new_ddb(capacity),
    };
    descriptor.new_content_id(random_u128());
    let text = descriptor.to_text();
    let descriptor_sectors = (text.len() as u64).div_ceil(SECTOR);
    if 1 + descriptor_sectors > OVERHEAD {
        return Err(invalid("the disk's descriptor is too long"));
    }
    let mut header = Header {
        version: 3,
        flags: sparse::FLAG_NEWLINE_TEST | sparse::FLAG_COMPRESSED | sparse::FLAG_MARKERS,
        capacity,
        grain_sectors: 128,
        descriptor_offset: 1,
        descriptor_sectors,
        gtes_per_gt: 512,
        rgd_offset: 0,
        gd_offset: u64::MAX,
        overhead: OVERHEAD,
        unclean_shutdown: false,
        compression: 1,
    };
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut out = Out {
        file: BufWriter::with_capacity(1 << 20, file),
        at: 0,
    };
    out.sectors(&header.to_bytes())?;
    out.sectors(text.as_bytes())?;
    out.sectors(&vec![0u8; ((OVERHEAD - out.at) * SECTOR) as usize])?;

    let grain = header.grain_bytes();
    let per_table = u64::from(header.gtes_per_gt);
    let mut directory = vec![0u32; header.gd_entries() as usize];
    let mut buf = vec![0u8; grain as usize];
    for (table, gd_entry) in directory.iter_mut().enumerate() {
        let mut entries = vec![0u32; per_table as usize];
        for (index, entry) in entries.iter_mut().enumerate() {
            let offset = (table as u64 * per_table + index as u64) * grain;
            if offset >= len {
                break;
            }
            let piece = grain.min(len - offset) as usize;
            buf[piece..].fill(0);
            from.read_at(&mut buf[..piece], offset)?;
            if buf.iter().any(|&b| b != 0) {
                let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&buf, 6);
                let mut record = Vec::with_capacity(12 + compressed.len());
                record.extend_from_slice(&(offset / SECTOR).to_le_bytes());
                record.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
                record.extend_from_slice(&compressed);
                let at = out.sectors(&record)?;
                *entry = u32::try_from(at).map_err(|_| invalid("the stream is too big"))?;
            }
            progress(offset + piece as u64);
        }
        if entries.iter().any(|&entry| entry != 0) {
            out.marker(header.gt_sectors(), MARKER_GT)?;
            let at = out.sectors(&table_bytes(&entries))?;
            *gd_entry = u32::try_from(at).map_err(|_| invalid("the stream is too big"))?;
        }
    }
    out.marker(header.gd_sectors(), MARKER_GD)?;
    header.gd_offset = out.sectors(&table_bytes(&directory))?;
    out.marker(1, MARKER_FOOTER)?;
    out.sectors(&header.to_bytes())?;
    out.marker(0, MARKER_EOS)?;
    let file = out.file.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()
}
