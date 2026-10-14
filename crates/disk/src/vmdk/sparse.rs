//! Hosted sparse extents: a header, a grain directory pointing at grain tables, and
//! grain tables pointing at grains (64 KiB by default), allocated as they're written.
//! VMware keeps a redundant copy of the directory and tables, which is updated in step.
//! Stream-optimized extents (compressed grains, for OVF) are read-only.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io;
use std::sync::Mutex;

use super::{file_read, file_write, invalid};

pub const MAGIC: u32 = 0x564d_444b; // "KDMV"
pub const SECTOR: u64 = 512;
const HEADER_LEN: usize = 512;
/// A stream-optimized extent's grain directory is in its footer.
const GD_AT_END: u64 = u64::MAX;

/// `flags`: the newline-detection bytes are valid.
pub const FLAG_NEWLINE_TEST: u32 = 1 << 0;
/// `flags`: there is a redundant grain directory and tables.
pub const FLAG_REDUNDANT: u32 = 1 << 1;
/// `flags`: a grain table entry of 1 means a grain of zeroes.
pub const FLAG_ZERO_GRAINS: u32 = 1 << 2;
/// `flags`: grains are compressed (stream-optimized).
pub const FLAG_COMPRESSED: u32 = 1 << 16;
/// `flags`: the extent holds markers (stream-optimized).
pub const FLAG_MARKERS: u32 = 1 << 17;

/// Limits that keep a hostile header from asking for absurd allocations.
const MAX_GRAIN_SECTORS: u64 = 2048;
const MAX_CAPACITY: u64 = 1 << 37; // 64 TiB in sectors
const MAX_GTES: u32 = 4096;

/// The fixed part of an extent file, as on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub flags: u32,
    /// In sectors.
    pub capacity: u64,
    pub grain_sectors: u64,
    pub descriptor_offset: u64,
    pub descriptor_sectors: u64,
    pub gtes_per_gt: u32,
    pub rgd_offset: u64,
    pub gd_offset: u64,
    pub overhead: u64,
    pub unclean_shutdown: bool,
    pub compression: u16,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(invalid("sparse extent header too short"));
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if u32_at(0) != MAGIC {
            return Err(invalid("not a VMDK sparse extent"));
        }
        let header = Header {
            version: u32_at(4),
            flags: u32_at(8),
            capacity: u64_at(12),
            grain_sectors: u64_at(20),
            descriptor_offset: u64_at(28),
            descriptor_sectors: u64_at(36),
            gtes_per_gt: u32_at(44),
            rgd_offset: u64_at(48),
            gd_offset: u64_at(56),
            overhead: u64_at(64),
            unclean_shutdown: bytes[72] != 0,
            compression: u16::from_le_bytes([bytes[77], bytes[78]]),
        };
        if !(1..=3).contains(&header.version) {
            return Err(invalid(&format!(
                "sparse extent version {} isn't supported",
                header.version
            )));
        }
        if header.flags & FLAG_NEWLINE_TEST != 0 && bytes[73..77] != *b"\n \r\n" {
            return Err(invalid(
                "the extent's line endings were changed (copied as text?)",
            ));
        }
        if !header.grain_sectors.is_power_of_two()
            || !(8..=MAX_GRAIN_SECTORS).contains(&header.grain_sectors)
        {
            return Err(invalid("bad grain size"));
        }
        if !header.gtes_per_gt.is_power_of_two() || !(128..=MAX_GTES).contains(&header.gtes_per_gt)
        {
            return Err(invalid("bad grain table size"));
        }
        if header.capacity > MAX_CAPACITY {
            return Err(invalid("extent too large"));
        }
        if header.compression > 1 || (header.compression == 1) != header.compressed() {
            return Err(invalid("unsupported compression"));
        }
        Ok(header)
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.version.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.flags.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.capacity.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.grain_sectors.to_le_bytes());
        bytes[28..36].copy_from_slice(&self.descriptor_offset.to_le_bytes());
        bytes[36..44].copy_from_slice(&self.descriptor_sectors.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.gtes_per_gt.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.rgd_offset.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.gd_offset.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.overhead.to_le_bytes());
        bytes[72] = u8::from(self.unclean_shutdown);
        bytes[73..77].copy_from_slice(b"\n \r\n");
        bytes[77..79].copy_from_slice(&self.compression.to_le_bytes());
        bytes
    }

    pub fn compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }

    pub fn grain_bytes(&self) -> u64 {
        self.grain_sectors * SECTOR
    }

    /// Sectors one grain table covers.
    fn gt_coverage(&self) -> u64 {
        self.grain_sectors * u64::from(self.gtes_per_gt)
    }

    pub fn gd_entries(&self) -> u64 {
        self.capacity.div_ceil(self.gt_coverage())
    }

    /// Sectors a grain table takes.
    pub fn gt_sectors(&self) -> u64 {
        (u64::from(self.gtes_per_gt) * 4).div_ceil(SECTOR)
    }

    /// Sectors a grain directory takes.
    pub fn gd_sectors(&self) -> u64 {
        (self.gd_entries() * 4).div_ceil(SECTOR)
    }
}

/// Where a grain is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grain {
    /// Never written: the parent's data, or zeroes.
    Unallocated,
    /// Written as zeroes.
    Zero,
    /// At this sector of the file.
    At(u64),
}

struct Tables {
    gd: Vec<u32>,
    rgd: Option<Vec<u32>>,
    /// Grain tables read so far, by directory index.
    gts: HashMap<usize, Vec<u32>>,
    /// The first free sector at the end of the file.
    end: u64,
}

pub struct SparseExtent {
    file: File,
    header: Header,
    read_only: bool,
    tables: Mutex<Tables>,
}

impl SparseExtent {
    /// Opens the extent in `file`, whose header has been read. A writable extent is
    /// marked as not shut down cleanly until it's dropped.
    pub fn open(file: File, read_only: bool) -> io::Result<Self> {
        let mut bytes = [0u8; HEADER_LEN];
        file_read(&file, &mut bytes, 0)?;
        let mut header = Header::parse(&bytes)?;
        let len = file.metadata()?.len();
        if header.gd_offset == GD_AT_END {
            // Stream-optimized: the real header is the footer, the sector before the
            // end-of-stream marker.
            if len < 3 * SECTOR {
                return Err(invalid("stream-optimized extent too short"));
            }
            let mut footer = [0u8; HEADER_LEN];
            file_read(&file, &mut footer, len - 2 * SECTOR)?;
            header = Header::parse(&footer)?;
            if header.gd_offset == GD_AT_END {
                return Err(invalid("stream-optimized extent without a grain directory"));
            }
        }
        if header.compressed() && !read_only {
            return Err(invalid(
                "stream-optimized disks are read-only; convert them first",
            ));
        }
        let entries = header.gd_entries() as usize;
        let read_gd = |offset: u64| -> io::Result<Vec<u32>> {
            let mut raw = vec![0u8; entries * 4];
            file_read(&file, &mut raw, sector_offset(offset)?)?;
            Ok(le_u32s(&raw))
        };
        let gd = read_gd(header.gd_offset)?;
        let rgd = if header.flags & FLAG_REDUNDANT != 0 && header.rgd_offset != 0 {
            Some(read_gd(header.rgd_offset)?)
        } else {
            None
        };
        let extent = SparseExtent {
            file,
            read_only,
            tables: Mutex::new(Tables {
                gd,
                rgd,
                gts: HashMap::new(),
                end: len.div_ceil(SECTOR),
            }),
            header,
        };
        if !read_only {
            extent.set_unclean(true)?;
        }
        Ok(extent)
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    fn set_unclean(&self, unclean: bool) -> io::Result<()> {
        file_write(&self.file, &[u8::from(unclean)], 72)
    }

    /// Where grain `index` (counted from the extent's start) is.
    pub fn lookup(&self, index: u64) -> io::Result<Grain> {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        let (table, entry) = self.split(index)?;
        let gt_sector = tables.gd[table];
        if gt_sector == 0 {
            return Ok(Grain::Unallocated);
        }
        let gt = self.load_gt(&mut tables, table, gt_sector)?;
        self.decode(gt[entry])
    }

    fn decode(&self, entry: u32) -> io::Result<Grain> {
        match entry {
            0 => Ok(Grain::Unallocated),
            1 if self.header.flags & FLAG_ZERO_GRAINS != 0 => Ok(Grain::Zero),
            // A grain inside the header and tables would let writes corrupt them.
            sector if u64::from(sector) < self.header.overhead => {
                Err(invalid("grain table points into the extent's metadata"))
            }
            sector => Ok(Grain::At(u64::from(sector))),
        }
    }

    /// The directory and table indices of grain `index`.
    fn split(&self, index: u64) -> io::Result<(usize, usize)> {
        let per_gt = u64::from(self.header.gtes_per_gt);
        let table = (index / per_gt) as usize;
        if index * self.header.grain_sectors >= self.header.capacity {
            return Err(invalid("grain beyond the extent"));
        }
        Ok((table, (index % per_gt) as usize))
    }

    fn load_gt<'a>(
        &self,
        tables: &'a mut Tables,
        table: usize,
        sector: u32,
    ) -> io::Result<&'a mut Vec<u32>> {
        match tables.gts.entry(table) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let mut raw = vec![0u8; self.header.gtes_per_gt as usize * 4];
                file_read(&self.file, &mut raw, sector_offset(u64::from(sector))?)?;
                Ok(entry.insert(le_u32s(&raw)))
            }
        }
    }

    /// Reads `buf.len()` bytes at `offset` into grain `at` (an allocated one).
    pub fn read_grain(&self, at: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.header.compressed() {
            let grain = self.read_compressed(at)?;
            let start = offset as usize;
            buf.copy_from_slice(&grain[start..start + buf.len()]);
            return Ok(());
        }
        file_read(&self.file, buf, sector_offset(at)? + offset)
    }

    /// A stream-optimized grain: a marker (LBA, compressed size) and deflate data.
    fn read_compressed(&self, at: u64) -> io::Result<Vec<u8>> {
        let mut marker = [0u8; 12];
        let start = sector_offset(at)?;
        file_read(&self.file, &mut marker, start)?;
        let size = u32::from_le_bytes(marker[8..12].try_into().unwrap()) as u64;
        let grain = self.header.grain_bytes();
        // Compressed data larger than twice the grain isn't plausible.
        if size == 0 || size > 2 * grain + 1024 {
            return Err(invalid("bad compressed grain"));
        }
        let mut data = vec![0u8; size as usize];
        file_read(&self.file, &mut data, start + 12)?;
        let mut out =
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, grain as usize)
                .map_err(|_| invalid("compressed grain doesn't inflate"))?;
        out.resize(grain as usize, 0);
        Ok(out)
    }

    /// Writes `data` at `offset` into grain `at` (an allocated one).
    pub fn write_grain(&self, at: u64, offset: u64, data: &[u8]) -> io::Result<()> {
        file_write(&self.file, data, sector_offset(at)? + offset)
    }

    /// Allocates grain `index` at the end of the file with `grain` as its contents
    /// (a whole grain), unless another write allocated it first. Returns where it is,
    /// and whether this call allocated it.
    pub fn allocate(&self, index: u64, grain: &[u8]) -> io::Result<(u64, bool)> {
        if self.read_only {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        let (table, entry) = self.split(index)?;
        if tables.gd[table] == 0 {
            self.allocate_gt(&mut tables, table)?;
        }
        let gt_sector = tables.gd[table];
        if let Grain::At(at) = self.decode(self.load_gt(&mut tables, table, gt_sector)?[entry])? {
            return Ok((at, false));
        }
        let at = tables.end;
        let sector = u32::try_from(at).map_err(|_| invalid("extent full"))?;
        file_write(&self.file, grain, sector_offset(at)?)?;
        tables.end += self.header.grain_sectors;
        // The data first, then the tables that point at it.
        let entry_offset = entry as u64 * 4;
        file_write(
            &self.file,
            &sector.to_le_bytes(),
            sector_offset(u64::from(gt_sector))? + entry_offset,
        )?;
        if let Some(rgd) = &tables.rgd {
            let rgt = rgd[table];
            if rgt != 0 {
                file_write(
                    &self.file,
                    &sector.to_le_bytes(),
                    sector_offset(u64::from(rgt))? + entry_offset,
                )?;
            }
        }
        self.load_gt(&mut tables, table, gt_sector)?[entry] = sector;
        Ok((at, true))
    }

    /// Gives directory entry `table` a new, empty grain table (and its redundant copy).
    fn allocate_gt(&self, tables: &mut Tables, table: usize) -> io::Result<()> {
        let sectors = self.header.gt_sectors();
        let empty = vec![0u8; (sectors * SECTOR) as usize];
        let place = |tables: &mut Tables| -> io::Result<u32> {
            let at = tables.end;
            file_write(&self.file, &empty, sector_offset(at)?)?;
            tables.end += sectors;
            u32::try_from(at).map_err(|_| invalid("extent full"))
        };
        let primary = place(tables)?;
        let redundant = match tables.rgd {
            Some(_) => Some(place(tables)?),
            None => None,
        };
        if let (Some(rgd), Some(redundant)) = (&mut tables.rgd, redundant) {
            rgd[table] = redundant;
            file_write(
                &self.file,
                &redundant.to_le_bytes(),
                sector_offset(self.header.rgd_offset)? + table as u64 * 4,
            )?;
        }
        tables.gd[table] = primary;
        file_write(
            &self.file,
            &primary.to_le_bytes(),
            sector_offset(self.header.gd_offset)? + table as u64 * 4,
        )?;
        tables
            .gts
            .insert(table, vec![0; self.header.gtes_per_gt as usize]);
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

impl Drop for SparseExtent {
    fn drop(&mut self) {
        if !self.read_only {
            let _ = self.file.sync_data();
            let _ = self.set_unclean(false);
        }
    }
}

fn le_u32s(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

fn sector_offset(sector: u64) -> io::Result<u64> {
    sector
        .checked_mul(SECTOR)
        .ok_or_else(|| invalid("offset out of range"))
}

/// Lays out a new, empty hosted sparse extent of `capacity` sectors as VMware does:
/// the header, room for an embedded descriptor (if `descriptor_sectors` isn't 0), the
/// redundant directory and its tables, then the directory and its tables, all grain
/// tables allocated up front. Returns the header and the file's initial contents.
pub fn layout(capacity: u64, descriptor_sectors: u64) -> (Header, Vec<u8>) {
    let mut header = Header {
        version: 1,
        flags: FLAG_NEWLINE_TEST | FLAG_REDUNDANT,
        capacity,
        grain_sectors: 128,
        descriptor_offset: if descriptor_sectors == 0 { 0 } else { 1 },
        descriptor_sectors,
        gtes_per_gt: 512,
        rgd_offset: 0,
        gd_offset: 0,
        overhead: 0,
        unclean_shutdown: false,
        compression: 0,
    };
    let entries = header.gd_entries();
    let tables_sectors = header.gd_sectors() + entries * header.gt_sectors();
    header.rgd_offset = 1 + descriptor_sectors;
    header.gd_offset = header.rgd_offset + tables_sectors;
    let end = header.gd_offset + tables_sectors;
    header.overhead = end.div_ceil(header.grain_sectors) * header.grain_sectors;
    let mut bytes = vec![0u8; (header.overhead * SECTOR) as usize];
    bytes[..HEADER_LEN].copy_from_slice(&header.to_bytes());
    for directory in [header.rgd_offset, header.gd_offset] {
        let first_table = directory + header.gd_sectors();
        for entry in 0..entries {
            let table = (first_table + entry * header.gt_sectors()) as u32;
            let at = (directory * SECTOR + entry * 4) as usize;
            bytes[at..at + 4].copy_from_slice(&table.to_le_bytes());
        }
    }
    (header, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lays_out_like_vmware() {
        // What vmware-vdiskmanager makes for a 100 MB monolithic sparse disk.
        let (header, bytes) = layout(204_800, 20);
        assert_eq!(header.rgd_offset, 21);
        assert_eq!(header.gd_offset, 38);
        assert_eq!(header.overhead, 128);
        assert_eq!(bytes.len(), 65_536);
        let entry = |sector: u64, i: usize| {
            let at = sector as usize * 512 + i * 4;
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        assert_eq!(
            (0..4).map(|i| entry(21, i)).collect::<Vec<_>>(),
            [22, 26, 30, 34]
        );
        assert_eq!(
            (0..4).map(|i| entry(38, i)).collect::<Vec<_>>(),
            [39, 43, 47, 51]
        );
        assert_eq!(Header::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn rejects_hostile_headers() {
        let (header, _) = layout(204_800, 20);
        let bytes = header.to_bytes();
        let with = |at: usize, value: &[u8]| {
            let mut copy = bytes;
            copy[at..at + value.len()].copy_from_slice(value);
            Header::parse(&copy)
        };
        assert!(with(0, b"XXXX").is_err());
        assert!(with(4, &9u32.to_le_bytes()).is_err(), "version");
        assert!(
            with(20, &100u64.to_le_bytes()).is_err(),
            "grain not a power of 2"
        );
        assert!(with(20, &(1u64 << 20).to_le_bytes()).is_err(), "huge grain");
        assert!(with(44, &7u32.to_le_bytes()).is_err(), "grain table size");
        assert!(with(12, &u64::MAX.to_le_bytes()).is_err(), "capacity");
        assert!(with(73, b"\r\n").is_err(), "line endings changed");
        assert!(
            with(77, &1u16.to_le_bytes()).is_err(),
            "compression without the flag"
        );
    }
}
