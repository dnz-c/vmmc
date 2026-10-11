//! CFI parallel flash (Intel command set, 8 bits wide), as OVMF uses for its UEFI
//! variable store.
//!
//! In read-array mode the contents are mapped into the guest as read-only RAM, so
//! reads cost nothing; writes still exit here as commands. In every other mode the
//! mapping is removed so reads come here too and return status, ID or query data.
//! Programs and erases are written through to the backing file, so variables persist.

use std::fs::File;
use std::sync::Arc;

use vm_core::{BusDevice, MemoryAccess, RamMapper, RamRegion};

/// Erase block size; OVMF's variable store uses 4 KiB blocks.
pub const BLOCK_SIZE: usize = 0x1000;

const CMD_READ_ARRAY: u8 = 0xff;
const CMD_READ_ARRAY_ALT: u8 = 0x00;
const CMD_PROGRAM: u8 = 0x40;
const CMD_PROGRAM_ALT: u8 = 0x10;
const CMD_ERASE_SETUP: u8 = 0x20;
const CMD_ERASE_CONFIRM: u8 = 0xd0;
const CMD_CLEAR_STATUS: u8 = 0x50;
const CMD_READ_STATUS: u8 = 0x70;
const CMD_READ_ID: u8 = 0x90;
const CMD_CFI_QUERY: u8 = 0x98;

const STATUS_READY: u8 = 0x80;
/// What the status register holds after "clear status". QEMU's flash reads 0 here
/// rather than "ready", and OVMF's flash detection depends on it: anything else and
/// it treats the variable store as ROM.
const STATUS_CLEARED: u8 = 0x00;
const STATUS_ERASE_ERROR: u8 = 0x20;
const STATUS_PROGRAM_ERROR: u8 = 0x10;

/// Intel's manufacturer ID, and a 28F-series device ID.
const MANUFACTURER_ID: u8 = 0x89;
const DEVICE_ID: u8 = 0x18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    ReadArray,
    ReadStatus,
    ReadId,
    CfiQuery,
    /// The next write is the byte to program.
    Program,
    /// The next write must confirm the erase.
    EraseSetup,
}

pub struct Pflash {
    gpa: u64,
    contents: Arc<RamRegion>,
    mapper: RamMapper,
    backing: Option<File>,
    /// Read-only flash ignores every command and stays in read-array mode.
    writable: bool,
    mode: Mode,
    status: u8,
    mapped: bool,
}

impl Pflash {
    /// Flash at guest-physical `gpa` holding `contents`, which is padded with 0xff to
    /// whole pages. Changes are written through to `backing` if given.
    pub fn new(
        gpa: u64,
        contents: &[u8],
        mapper: RamMapper,
        backing: Option<File>,
    ) -> std::io::Result<Self> {
        let len = contents.len().next_multiple_of(BLOCK_SIZE);
        let region = RamRegion::new(len)?;
        region.write(0, contents);
        if len > contents.len() {
            region.write(contents.len(), &vec![0xff; len - contents.len()]);
        }
        let mut flash = Self {
            gpa,
            contents: region,
            mapper,
            backing,
            writable: true,
            mode: Mode::ReadArray,
            status: STATUS_READY,
            mapped: false,
        };
        flash.set_mode(Mode::ReadArray);
        Ok(flash)
    }

    /// Read-only flash at `gpa`, like firmware code: guest writes are ignored.
    pub fn read_only(gpa: u64, contents: &[u8], mapper: RamMapper) -> std::io::Result<Self> {
        let mut flash = Self::new(gpa, contents, mapper, None)?;
        flash.writable = false;
        Ok(flash)
    }

    pub fn len(&self) -> usize {
        self.contents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Switches mode, mapping the contents as RAM only in read-array mode.
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        let want_mapped = mode == Mode::ReadArray;
        if want_mapped == self.mapped {
            return;
        }
        let result = if want_mapped {
            self.mapper.map(
                self.gpa,
                self.contents.clone(),
                MemoryAccess::READ | MemoryAccess::EXECUTE,
            )
        } else {
            self.mapper.unmap(self.gpa)
        };
        match result {
            Ok(()) => self.mapped = want_mapped,
            // Without the mapping every read just comes here, which still works.
            Err(e) => tracing::debug!(%e, "pflash: mapping change failed; serving reads as MMIO"),
        }
    }

    fn program(&mut self, offset: usize, value: u8) {
        let mut old = [0u8];
        if !self.contents.read(offset, &mut old) {
            self.status |= STATUS_PROGRAM_ERROR;
            return;
        }
        // Programming can only clear bits.
        let new = old[0] & value;
        self.contents.write(offset, &[new]);
        self.persist(offset, &[new]);
    }

    fn erase(&mut self, offset: usize) {
        let start = offset - offset % BLOCK_SIZE;
        if start + BLOCK_SIZE > self.len() {
            self.status |= STATUS_ERASE_ERROR;
            return;
        }
        let ones = [0xff; BLOCK_SIZE];
        self.contents.write(start, &ones);
        self.persist(start, &ones);
    }

    fn persist(&mut self, offset: usize, data: &[u8]) {
        let Some(file) = &self.backing else { return };
        if let Err(e) = write_at(file, data, offset as u64) {
            tracing::error!(%e, "pflash: can't write the variable store back to disk");
        }
    }

    fn command(&mut self, offset: usize, value: u8) {
        if !self.writable {
            tracing::trace!(offset, value, "pflash: write to read-only flash ignored");
            return;
        }
        match self.mode {
            Mode::Program => {
                self.program(offset, value);
                self.status |= STATUS_READY;
                self.set_mode(Mode::ReadStatus);
            }
            Mode::EraseSetup => {
                if value == CMD_ERASE_CONFIRM {
                    self.erase(offset);
                } else {
                    self.status |= STATUS_ERASE_ERROR | STATUS_PROGRAM_ERROR;
                }
                self.status |= STATUS_READY;
                self.set_mode(Mode::ReadStatus);
            }
            _ => match value {
                CMD_READ_ARRAY | CMD_READ_ARRAY_ALT => self.set_mode(Mode::ReadArray),
                CMD_PROGRAM | CMD_PROGRAM_ALT => self.set_mode(Mode::Program),
                CMD_ERASE_SETUP => self.set_mode(Mode::EraseSetup),
                CMD_CLEAR_STATUS => {
                    self.status = STATUS_CLEARED;
                    self.set_mode(Mode::ReadArray);
                }
                CMD_READ_STATUS => self.set_mode(Mode::ReadStatus),
                CMD_READ_ID => self.set_mode(Mode::ReadId),
                CMD_CFI_QUERY => self.set_mode(Mode::CfiQuery),
                _ => {
                    tracing::debug!(value, "pflash: unsupported command");
                    self.set_mode(Mode::ReadArray);
                }
            },
        }
    }

    fn read_byte(&self, offset: usize) -> u8 {
        match self.mode {
            Mode::ReadArray | Mode::Program | Mode::EraseSetup => {
                let mut byte = [0xff];
                self.contents.read(offset, &mut byte);
                byte[0]
            }
            Mode::ReadStatus => self.status,
            Mode::ReadId => match offset & 0xff {
                0 => MANUFACTURER_ID,
                1 => DEVICE_ID,
                _ => 0,
            },
            Mode::CfiQuery => self.cfi_byte(offset & 0xff),
        }
    }

    /// The parts of the CFI query table a probe looks at.
    fn cfi_byte(&self, index: usize) -> u8 {
        let blocks = (self.len() / BLOCK_SIZE - 1) as u16;
        match index {
            0x10 => b'Q',
            0x11 => b'R',
            0x12 => b'Y',
            0x13 => 0x01, // Intel/Sharp command set
            // Device size as a power of two.
            0x27 => self.len().trailing_zeros() as u8,
            0x2c => 1, // one erase block region
            0x2d => blocks as u8,
            0x2e => (blocks >> 8) as u8,
            0x2f => ((BLOCK_SIZE >> 8) & 0xff) as u8,
            0x30 => (BLOCK_SIZE >> 16) as u8,
            _ => 0,
        }
    }
}

impl BusDevice for Pflash {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.read_byte(offset as usize + i);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        // The flash is 8 bits wide; wider writes carry the command in their low byte.
        self.command(offset as usize, data[0]);
    }
}

#[cfg(windows)]
fn write_at(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < data.len() {
        done += file.seek_write(&data[done..], offset + done as u64)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flash(contents: &[u8], backing: Option<File>) -> Pflash {
        Pflash::new(0xffc0_0000, contents, RamMapper::detached(), backing).unwrap()
    }

    fn byte(flash: &mut Pflash, offset: u64) -> u8 {
        let mut data = [0u8];
        flash.read(offset, &mut data);
        data[0]
    }

    #[test]
    fn reads_array_and_pads_to_whole_blocks() {
        let mut flash = flash(&[1, 2, 3], None);
        assert_eq!(flash.len(), BLOCK_SIZE);
        assert_eq!(byte(&mut flash, 2), 3);
        assert_eq!(byte(&mut flash, 3), 0xff);
    }

    #[test]
    fn ovmf_probe_sees_flash_not_rom_or_ram() {
        // OvmfPkg's QemuFlashDetected: "clear status" must leave the array readable,
        // then "read status" must return exactly 0 (not the contents, and not the
        // command byte, which would mean RAM).
        let mut flash = flash(&[0x5a; 16], None);
        flash.write(0, &[CMD_CLEAR_STATUS]);
        assert_eq!(byte(&mut flash, 0), 0x5a, "still reading the array");
        flash.write(0, &[CMD_READ_STATUS]);
        assert_eq!(byte(&mut flash, 0), STATUS_CLEARED);
        flash.write(0, &[CMD_READ_ARRAY]);
        assert_eq!(byte(&mut flash, 0), 0x5a);
    }

    #[test]
    fn program_clears_bits_and_erase_restores_ones() {
        let mut flash = flash(&[0xff; 2 * BLOCK_SIZE], None);
        flash.write(0x1010, &[CMD_PROGRAM_ALT]);
        flash.write(0x1010, &[0xa5]);
        assert_eq!(
            byte(&mut flash, 0x1010),
            STATUS_READY,
            "status after program"
        );
        flash.write(0, &[CMD_READ_ARRAY]);
        assert_eq!(byte(&mut flash, 0x1010), 0xa5);

        // A second program can only clear more bits.
        flash.write(0x1010, &[CMD_PROGRAM]);
        flash.write(0x1010, &[0x0f]);
        flash.write(0, &[CMD_READ_ARRAY]);
        assert_eq!(byte(&mut flash, 0x1010), 0x05);

        flash.write(0x1fff, &[CMD_ERASE_SETUP]);
        flash.write(0x1fff, &[CMD_ERASE_CONFIRM]);
        flash.write(0, &[CMD_READ_ARRAY]);
        assert_eq!(byte(&mut flash, 0x1010), 0xff, "whole block erased");
    }

    #[test]
    fn bad_erase_confirm_sets_error_bits() {
        let mut flash = flash(&[0; BLOCK_SIZE], None);
        flash.write(0, &[CMD_ERASE_SETUP]);
        flash.write(0, &[0x42]);
        assert_ne!(byte(&mut flash, 0) & STATUS_ERASE_ERROR, 0);
        flash.write(0, &[CMD_CLEAR_STATUS]);
        flash.write(0, &[CMD_READ_STATUS]);
        assert_eq!(byte(&mut flash, 0), STATUS_CLEARED);
    }

    #[test]
    fn id_and_cfi_query() {
        let mut flash = flash(&[0; 4 * BLOCK_SIZE], None);
        flash.write(0, &[CMD_READ_ID]);
        assert_eq!(
            (byte(&mut flash, 0), byte(&mut flash, 1)),
            (MANUFACTURER_ID, DEVICE_ID)
        );
        flash.write(0, &[CMD_CFI_QUERY]);
        assert_eq!(
            [
                byte(&mut flash, 0x10),
                byte(&mut flash, 0x11),
                byte(&mut flash, 0x12)
            ],
            *b"QRY"
        );
        assert_eq!(byte(&mut flash, 0x27), 14, "16 KiB");
    }

    #[test]
    fn read_only_flash_ignores_commands() {
        let mut flash = Pflash::read_only(0xffc0_0000, &[0x5a; 16], RamMapper::detached()).unwrap();
        flash.write(0, &[CMD_PROGRAM]);
        flash.write(0, &[0x00]);
        flash.write(0, &[CMD_READ_STATUS]);
        assert_eq!(byte(&mut flash, 0), 0x5a, "still the array, unchanged");
    }

    #[test]
    fn programs_are_written_through_to_the_backing_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), vec![0xff; BLOCK_SIZE]).unwrap();
        let backing = File::options().write(true).open(file.path()).unwrap();
        let mut flash = flash(&[0xff; BLOCK_SIZE], Some(backing));
        flash.write(0x20, &[CMD_PROGRAM]);
        flash.write(0x20, &[0x12]);
        drop(flash);
        assert_eq!(std::fs::read(file.path()).unwrap()[0x20], 0x12);
    }
}
