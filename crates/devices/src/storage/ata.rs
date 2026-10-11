//! ATA command set pieces shared by SATA devices: IDENTIFY data, and the disk commands.

use std::sync::Arc;

use disk::BlockBackend;

pub const SECTOR_SIZE: u64 = 512;

// Status register bits.
pub const STATUS_ERR: u8 = 0x01;
pub const STATUS_DRQ: u8 = 0x08;
pub const STATUS_DSC: u8 = 0x10;
pub const STATUS_DRDY: u8 = 0x40;
pub const STATUS_BSY: u8 = 0x80;
/// Error register: command aborted.
pub const ERROR_ABRT: u8 = 0x04;
/// Error register: uncorrectable data error.
pub const ERROR_UNC: u8 = 0x40;
/// Error register: ID not found (LBA out of range).
pub const ERROR_IDNF: u8 = 0x10;

/// ATA strings are space-padded and byte-swapped within each 16-bit word.
fn put_string(words: &mut [u16], text: &str) {
    let mut bytes = text.as_bytes().to_vec();
    bytes.resize(words.len() * 2, b' ');
    for (word, pair) in words.iter_mut().zip(bytes.chunks(2)) {
        *word = (u16::from(pair[0]) << 8) | u16::from(pair[1]);
    }
}

fn to_bytes(words: &mut [u16; 256]) -> Vec<u8> {
    // Word 255: signature 0xa5 and a checksum that makes all 512 bytes sum to zero.
    words[255] = 0x00a5;
    let sum = words[..255]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .chain([0xa5])
        .fold(0u8, |sum, byte| sum.wrapping_add(byte));
    words[255] |= u16::from(0u8.wrapping_sub(sum)) << 8;
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

/// IDENTIFY DEVICE data for a disk of `sectors` 512-byte sectors.
pub fn identify_disk(sectors: u64, serial: &str, trim: bool) -> Vec<u8> {
    let mut words = [0u16; 256];
    words[0] = 0x0040; // fixed, non-removable
    // Legacy geometry, capped as ATA specifies for large disks.
    let cylinders = (sectors / (16 * 63)).clamp(1, 16_383) as u16;
    words[1] = cylinders;
    words[3] = 16;
    words[6] = 63;
    put_string(&mut words[10..20], serial);
    put_string(&mut words[23..27], "1.0");
    put_string(&mut words[27..47], "vmmc SATA Disk");
    words[47] = 0x8010; // up to 16 sectors per READ/WRITE MULTIPLE block
    words[49] = 0x0300; // LBA and DMA
    words[53] = 0x0006; // words 64-70 and 88 are valid
    words[54] = cylinders;
    words[55] = 16;
    words[56] = 63;
    let chs = u32::from(cylinders) * 16 * 63;
    words[57] = chs as u16;
    words[58] = (chs >> 16) as u16;
    words[59] = 0x0110; // multiple sector setting: 16, valid
    let lba28 = sectors.min(0x0fff_ffff) as u32;
    words[60] = lba28 as u16;
    words[61] = (lba28 >> 16) as u16;
    words[63] = 0x0407; // multiword DMA 0-2, mode 2 selected
    words[64] = 0x0003; // PIO 3 and 4
    words[65..69].fill(120); // cycle times, ns
    words[76] = 0x0006; // SATA 1.5 and 3 Gb/s
    words[80] = 0x01f0; // ATA/ATAPI-4 to ATA8-ACS
    words[82] = 0x4060; // NOP, write cache, look-ahead
    words[83] = 0x7400; // 48-bit, FLUSH CACHE and its EXT form
    words[84] = 0x4000;
    words[85] = 0x4060;
    words[86] = 0x3400;
    words[87] = 0x4000;
    words[88] = 0x203f; // UDMA 0-5, mode 5 selected
    for (i, word) in words[100..104].iter_mut().enumerate() {
        *word = (sectors >> (16 * i)) as u16;
    }
    words[106] = 0x4000; // one logical sector per physical, valid
    if trim {
        words[169] = 0x0001; // DATA SET MANAGEMENT with TRIM
    }
    words[217] = 0x0001; // non-rotating medium (an SSD)
    to_bytes(&mut words)
}

/// IDENTIFY PACKET DEVICE data for a CD/DVD drive.
pub fn identify_cdrom(serial: &str) -> Vec<u8> {
    let mut words = [0u16; 256];
    // ATAPI, CD/DVD device, removable, 50 µs DRQ, 12-byte packets.
    words[0] = 0x85c0;
    put_string(&mut words[10..20], serial);
    put_string(&mut words[23..27], "1.0");
    put_string(&mut words[27..47], "vmmc DVD-ROM");
    words[49] = 0x0300; // LBA and DMA
    words[53] = 0x0006;
    words[63] = 0x0407;
    words[64] = 0x0003;
    words[65..69].fill(120);
    words[76] = 0x0006;
    words[80] = 0x01f0;
    words[82] = 0x4010; // NOP, PACKET
    words[83] = 0x4000;
    words[84] = 0x4000;
    words[85] = 0x4010;
    words[87] = 0x4000;
    words[88] = 0x203f;
    to_bytes(&mut words)
}

/// A fixed disk on the ATA command set.
pub struct AtaDisk {
    pub backend: Arc<dyn BlockBackend>,
    pub serial: String,
}

/// What an ATA command does with data.
#[derive(Debug, PartialEq, Eq)]
pub enum Transfer {
    None,
    /// Data for the host; `pio` if the command uses PIO protocol.
    In {
        data: Vec<u8>,
        pio: bool,
    },
    /// Data from the host: `len` bytes, handed to [`AtaDisk::write`].
    Out {
        len: usize,
    },
}

/// An ATA command decoded from a register FIS.
#[derive(Clone, Copy, Debug, Default)]
pub struct Taskfile {
    pub command: u8,
    pub features: u16,
    pub lba: u64,
    pub count: u16,
    pub device: u8,
}

/// Why a command failed: the error register value.
pub type AtaError = u8;

impl AtaDisk {
    fn sectors(&self) -> u64 {
        self.backend.len() / SECTOR_SIZE
    }

    /// The byte range a read or write covers, checked against the disk.
    fn range(&self, tf: &Taskfile, lba48: bool) -> Result<(u64, u64), AtaError> {
        let (lba, count) = if lba48 {
            (
                tf.lba & 0xffff_ffff_ffff,
                if tf.count == 0 {
                    65_536
                } else {
                    tf.count.into()
                },
            )
        } else {
            let lba = (tf.lba & 0x00ff_ffff) | (u64::from(tf.device & 0x0f) << 24);
            (
                lba,
                if tf.count & 0xff == 0 {
                    256
                } else {
                    u64::from(tf.count & 0xff)
                },
            )
        };
        match lba.checked_add(count) {
            Some(end) if end <= self.sectors() => Ok((lba * SECTOR_SIZE, count * SECTOR_SIZE)),
            _ => Err(ERROR_IDNF | ERROR_ABRT),
        }
    }

    /// Runs a command. One that takes data from the host returns how much to fetch;
    /// the caller then hands that data to [`AtaDisk::write`].
    pub fn command(&self, tf: &Taskfile) -> Result<Transfer, AtaError> {
        match tf.command {
            // IDENTIFY DEVICE.
            0xec => Ok(Transfer::In {
                data: identify_disk(self.sectors(), &self.serial, true),
                pio: true,
            }),
            // READ DMA EXT, READ SECTORS EXT, READ MULTIPLE EXT; then the 28-bit forms.
            0x25 | 0x24 | 0x29 | 0xc8 | 0x20 | 0xc4 => {
                let lba48 = matches!(tf.command, 0x25 | 0x24 | 0x29);
                let (offset, len) = self.range(tf, lba48)?;
                let mut data = vec![0u8; len as usize];
                self.backend.read_at(&mut data, offset).map_err(|e| {
                    tracing::warn!(%e, "SATA disk read failed");
                    ERROR_UNC
                })?;
                let pio = !matches!(tf.command, 0x25 | 0xc8);
                Ok(Transfer::In { data, pio })
            }
            // WRITE DMA EXT, WRITE SECTORS EXT, WRITE MULTIPLE EXT; then the 28-bit forms.
            0x35 | 0x34 | 0x39 | 0xca | 0x30 | 0xc5 => {
                let lba48 = matches!(tf.command, 0x35 | 0x34 | 0x39);
                let (_, len) = self.range(tf, lba48)?;
                if self.backend.is_read_only() {
                    return Err(ERROR_ABRT);
                }
                Ok(Transfer::Out { len: len as usize })
            }
            // DATA SET MANAGEMENT (TRIM): `count` 512-byte blocks of ranges follow.
            0x06 if tf.features & 1 != 0 => {
                if self.backend.is_read_only() || tf.count == 0 {
                    return Err(ERROR_ABRT);
                }
                Ok(Transfer::Out {
                    len: usize::from(tf.count) * SECTOR_SIZE as usize,
                })
            }
            // FLUSH CACHE (EXT).
            0xe7 | 0xea => {
                self.backend.flush().map_err(|_| ERROR_ABRT)?;
                Ok(Transfer::None)
            }
            // READ VERIFY SECTORS (EXT): just the range check.
            0x40 | 0x42 => {
                self.range(tf, tf.command == 0x42)?;
                Ok(Transfer::None)
            }
            // SET FEATURES, SET MULTIPLE MODE, INITIALIZE DEVICE PARAMETERS, and the
            // power-management commands: nothing to do.
            0xef | 0xc6 | 0x91 | 0xe0 | 0xe1 | 0xe2 | 0xe3 | 0xe6 | 0xe5 => Ok(Transfer::None),
            _ => {
                tracing::debug!(command = tf.command, "SATA disk: unsupported command");
                Err(ERROR_ABRT)
            }
        }
    }

    /// Finishes a command that took data from the host.
    pub fn write(&self, tf: &Taskfile, data: &[u8]) -> Result<(), AtaError> {
        if tf.command == 0x06 {
            for range in data.chunks(8) {
                let entry = u64::from_le_bytes(range.try_into().unwrap());
                let (lba, count) = (entry & 0xffff_ffff_ffff, entry >> 48);
                if count == 0 {
                    continue;
                }
                match lba.checked_add(count) {
                    Some(end) if end <= self.sectors() => {}
                    _ => return Err(ERROR_ABRT),
                }
                self.backend
                    .discard(lba * SECTOR_SIZE, count * SECTOR_SIZE)
                    .map_err(|_| ERROR_ABRT)?;
            }
            return Ok(());
        }
        let lba48 = matches!(tf.command, 0x35 | 0x34 | 0x39);
        let (offset, _) = self.range(tf, lba48)?;
        self.backend.write_at(data, offset).map_err(|e| {
            tracing::warn!(%e, "SATA disk write failed");
            ERROR_ABRT
        })
    }
}

#[cfg(test)]
mod tests {
    use disk::MemDisk;

    use super::*;

    fn word(data: &[u8], index: usize) -> u16 {
        u16::from_le_bytes([data[2 * index], data[2 * index + 1]])
    }

    #[test]
    fn identify_data_is_checksummed_and_byte_swapped() {
        let data = identify_disk(1 << 30, "VMMC0001", true);
        assert_eq!(data.len(), 512);
        assert_eq!(data.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)), 0);
        assert_eq!(data[510], 0xa5);
        // "VMMC0001" stored as "MVCM0010" byte-wise.
        assert_eq!(&data[20..28], b"MVCM0010");
        let lba48 = (0..4).fold(0u64, |acc, i| {
            acc | (u64::from(word(&data, 100 + i)) << (16 * i))
        });
        assert_eq!(lba48, 1 << 30);
        assert_eq!(word(&data, 60), 0xffff, "LBA28 capacity saturates");
        assert_eq!(word(&data, 217), 1, "solid state");
        let cd = identify_cdrom("VMMCCD");
        assert_eq!(word(&cd, 0) >> 14, 0b10, "ATAPI");
        assert_eq!((word(&cd, 0) >> 8) & 0x1f, 5, "CD/DVD");
    }

    fn disk(sectors: usize) -> AtaDisk {
        AtaDisk {
            backend: Arc::new(MemDisk::new(sectors * 512, false)),
            serial: "VMMC0001".into(),
        }
    }

    #[test]
    fn reads_and_writes_check_the_lba_range() {
        let disk = disk(64);
        let read = Taskfile {
            command: 0x25,
            lba: 60,
            count: 4,
            ..Taskfile::default()
        };
        assert!(matches!(
            disk.command(&read),
            Ok(Transfer::In { pio: false, .. })
        ));
        let too_far = Taskfile { count: 5, ..read };
        assert_eq!(disk.command(&too_far), Err(ERROR_IDNF | ERROR_ABRT));

        // 28-bit: LBA bits 24-27 come from the device register.
        let write = Taskfile {
            command: 0xca,
            lba: 2,
            count: 1,
            device: 0xe0,
            ..Taskfile::default()
        };
        assert_eq!(disk.command(&write), Ok(Transfer::Out { len: 512 }));
        disk.write(&write, &[0x5a; 512]).unwrap();
        let mut back = [0u8; 512];
        disk.backend.read_at(&mut back, 1024).unwrap();
        assert_eq!(back, [0x5a; 512]);
        let high = Taskfile {
            device: 0xe1,
            ..write
        };
        assert_eq!(disk.command(&high), Err(ERROR_IDNF | ERROR_ABRT));
    }

    #[test]
    fn trim_ranges_are_checked() {
        let disk = disk(64);
        let trim = Taskfile {
            command: 0x06,
            features: 1,
            count: 1,
            ..Taskfile::default()
        };
        assert_eq!(disk.command(&trim), Ok(Transfer::Out { len: 512 }));
        let mut ranges = vec![0u8; 512];
        ranges[..8].copy_from_slice(&((8u64 << 48) | 16).to_le_bytes());
        assert_eq!(disk.write(&trim, &ranges), Ok(()));
        ranges[..8].copy_from_slice(&((8u64 << 48) | 60).to_le_bytes());
        assert_eq!(disk.write(&trim, &ranges), Err(ERROR_ABRT));
        assert_eq!(
            disk.command(&Taskfile {
                command: 0xb0,
                ..trim
            }),
            Err(ERROR_ABRT)
        );
    }
}
