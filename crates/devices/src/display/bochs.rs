//! The Bochs/QEMU "bochs-display" adapter (1234:1111, class "other display"): a linear
//! framebuffer and the VBE DISPI mode registers, with no legacy VGA. OVMF drives it
//! (QemuVideoDxe) and hands the framebuffer to the OS as GOP, which is all Windows'
//! basic display driver needs; Linux has a DRM driver for it.
//!
//! - **BAR 0:** the framebuffer, 32 MiB (enough for 3840x2160), mapped into the guest
//!   as plain RAM so drawing never exits. The device remaps it whenever the guest moves
//!   the BAR.
//! - **BAR 2:** MMIO registers: an EDID at 0x000 naming the preferred resolution (OVMF
//!   boots in it, Linux's driver prefers it, and Windows keeps the firmware's mode),
//!   the DISPI registers at 0x500 (16 bits each), QEMU's extension registers at
//!   0x600 (framebuffer byte order), and vmmc's own at 0x700 (32 bits each):
//!   - 0x00 "VMMC", 0x04 features (bit 0 size requests, bit 1 a hardware cursor);
//!   - 0x08 the size the host would like the screen to be (its window's; width in
//!     the low 16 bits) and 0x0c a count of such requests, read-only;
//!   - 0x10 cursor control (bit 0 shown), 0x14 cursor size and 0x18 hot spot (x in
//!     the low 16 bits), 0x1c a count the guest bumps after writing a new image.
//!
//!   The cursor's image sits at the end of the framebuffer ([`CURSOR_OFFSET`]), as
//!   rows of 128 straight-alpha BGRA pixels. vmmc's guest driver uses all this: Windows
//!   switches to the window's size, and the window draws the guest's cursor itself.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use vm_core::{MemoryAccess, RamMapper, RamRegion};

use super::edid::{EDID_SIZE, edid};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice};

pub const FRAMEBUFFER_BAR: u8 = 0;
pub const FRAMEBUFFER_SIZE: u64 = 32 << 20;
/// The largest preferred resolution the EDID offers: what fits the framebuffer.
pub const MAX_PREFERRED: (u32, u32) = (3840, 2160);
/// The smallest: what UEFI's graphics console needs.
pub const MIN_PREFERRED: (u32, u32) = (640, 480);
pub const MMIO_BAR: u8 = 2;
pub const MMIO_SIZE: u64 = 0x1000;

const DISPI: u64 = 0x500;
const QEXT: u64 = 0x600;
const VMMC: u64 = 0x700;
const VMMC_END: u64 = VMMC + 0x20;
/// "VMMC": vmmc's registers are there (QEMU's adapter has none).
const VMMC_MAGIC: u32 = u32::from_le_bytes(*b"VMMC");
/// Size requests and a hardware cursor.
const VMMC_FEATURES: u32 = 0b11;
// vmmc register offsets.
const REQUEST_SIZE: u64 = 0x08;
const REQUEST_COUNT: u64 = 0x0c;
const CURSOR_CONTROL: u64 = 0x10;
const CURSOR_SIZE: u64 = 0x14;
const CURSOR_HOTSPOT: u64 = 0x18;
const CURSOR_COUNT: u64 = 0x1c;
const CURSOR_SHOWN: u32 = 1;
/// The largest cursor, in pixels each way.
pub const CURSOR_MAX: u32 = 128;
/// Where the cursor's image is in the framebuffer: its last 64 KiB, which no mode
/// reaches (3840x2160 ends below it).
pub const CURSOR_OFFSET: u64 = FRAMEBUFFER_SIZE - (CURSOR_MAX * CURSOR_MAX * 4) as u64;
/// QEMU extension registers: their size, then the framebuffer byte order.
const QEXT_SIZE: u32 = 8;
const LITTLE_ENDIAN: u32 = 0x1e1e_1e1e;

// DISPI register indexes.
const ID: usize = 0;
const XRES: usize = 1;
const YRES: usize = 2;
const BPP: usize = 3;
const ENABLE: usize = 4;
const BANK: usize = 5;
const VIRT_WIDTH: usize = 6;
const VIRT_HEIGHT: usize = 7;
const X_OFFSET: usize = 8;
const Y_OFFSET: usize = 9;
const VIDEO_MEMORY_64K: usize = 10;
const REGISTERS: usize = 11;

/// The newest interface version, which firmware checks for.
const ID5: u16 = 0xb0c5;
const ENABLED: u16 = 0x01;
const GETCAPS: u16 = 0x02;
const NOCLEARMEM: u16 = 0x80;
const MAX_XRES: u16 = 7680;
const MAX_YRES: u16 = 4320;
const MAX_BPP: u16 = 32;

/// One captured frame, as packed 8-bit RGB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

struct State {
    registers: [u16; REGISTERS],
    /// The last ENABLE write asked for the maximum values instead of the current ones.
    getcaps: bool,
    edid: [u8; EDID_SIZE],
    /// The size the host asked for, and how many times it asked.
    request: (u32, u32),
    request_count: u32,
    cursor: CursorRegisters,
}

/// The cursor registers, as the guest set them.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct CursorRegisters {
    control: u32,
    size: u32,
    hotspot: u32,
    count: u32,
}

/// The guest's hardware cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// How many images the guest has given; 0 while it draws the cursor itself.
    pub count: u32,
    pub shown: bool,
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    /// Straight-alpha RGBA, `width` x `height`.
    pub rgba: Vec<u8>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads what the adapter shows, from any thread.
#[derive(Clone)]
pub struct Display {
    state: Arc<Mutex<State>>,
    framebuffer: Arc<RamRegion>,
}

impl Display {
    /// Asks the guest for a `width` x `height` screen: what the host's window has room
    /// for. A guest driver that knows vmmc's registers switches to it; the EDID prefers
    /// it from the next boot. Returns the size asked for, after keeping it within what
    /// the adapter can show.
    pub fn request_size(&self, width: u32, height: u32) -> (u32, u32) {
        let width = width.clamp(MIN_PREFERRED.0, MAX_PREFERRED.0);
        let mut height = height.clamp(MIN_PREFERRED.1, MAX_PREFERRED.1);
        // The picture has to end before the cursor's image.
        let rows = (CURSOR_OFFSET / (u64::from(width) * 4)) as u32;
        height = height.min(rows);
        let mut state = lock(&self.state);
        if state.request != (width, height) {
            state.request = (width, height);
            state.request_count = state.request_count.wrapping_add(1);
            state.edid = edid(width, height);
        }
        (width, height)
    }

    /// The cursor's state; its image only when it changed since `last_count`.
    pub fn cursor(&self, last_count: u32) -> (u32, bool, Option<Cursor>) {
        let registers = lock(&self.state).cursor;
        let shown = registers.control & CURSOR_SHOWN != 0;
        if registers.count == last_count || registers.count == 0 {
            return (registers.count, shown, None);
        }
        let width = (registers.size & 0xffff).min(CURSOR_MAX);
        let height = (registers.size >> 16).min(CURSOR_MAX);
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for (y, row) in rgba
            .chunks_exact_mut((width * 4).max(1) as usize)
            .enumerate()
        {
            let at = CURSOR_OFFSET as usize + y * (CURSOR_MAX * 4) as usize;
            self.framebuffer.read(at, row);
            // BGRA to RGBA.
            for pixel in row.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let cursor = Cursor {
            count: registers.count,
            shown,
            width,
            height,
            hotspot: (registers.hotspot & 0xffff, registers.hotspot >> 16),
            rgba,
        };
        (registers.count, shown, Some(cursor))
    }

    /// The picture now, or `None` while no graphics mode is enabled.
    pub fn snapshot(&self) -> Option<Frame> {
        self.capture(&mut Capture::default())
    }

    /// The picture, if it changed since `last` saw it; `None` when it didn't, or
    /// while no graphics mode is enabled. Only a changed picture is converted, so
    /// watching a still screen costs a copy and a compare.
    pub fn capture(&self, last: &mut Capture) -> Option<Frame> {
        let registers = lock(&self.state).registers;
        if registers[ENABLE] & ENABLED == 0 {
            return None;
        }
        let (width, height) = (u32::from(registers[XRES]), u32::from(registers[YRES]));
        let bytes_per_pixel = match registers[BPP] {
            32 => 4,
            24 => 3,
            16 => 2,
            _ => return None,
        };
        let stride = u64::from(registers[VIRT_WIDTH].max(registers[XRES])) * bytes_per_pixel;
        let start = u64::from(registers[Y_OFFSET]) * stride
            + u64::from(registers[X_OFFSET]) * bytes_per_pixel;
        let row_bytes = u64::from(width) * bytes_per_pixel;
        if width == 0
            || height == 0
            || start + stride * u64::from(height - 1) + row_bytes > FRAMEBUFFER_SIZE
        {
            return None;
        }
        let geometry = [
            u64::from(width),
            u64::from(height),
            bytes_per_pixel,
            stride,
            start,
        ];
        let row_bytes = row_bytes as usize;
        let mut raw = std::mem::take(&mut last.spare);
        raw.resize(row_bytes * height as usize, 0);
        for (y, row) in raw.chunks_exact_mut(row_bytes).enumerate() {
            self.framebuffer
                .read((start + y as u64 * stride) as usize, row);
        }
        if last.geometry == Some(geometry) && last.raw == raw {
            last.spare = raw;
            return None;
        }
        let mut rgb = vec![0u8; (width * height * 3) as usize];
        match bytes_per_pixel {
            // Little-endian XRGB and RGB: blue first.
            4 | 3 => {
                let pixels = raw.chunks_exact(bytes_per_pixel as usize);
                for (out, pixel) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(pixels) {
                    *out = [pixel[2], pixel[1], pixel[0]];
                }
            }
            _ => {
                for (out, pixel) in rgb
                    .as_chunks_mut::<3>()
                    .0
                    .iter_mut()
                    .zip(raw.as_chunks::<2>().0)
                {
                    let value = u16::from_le_bytes(*pixel);
                    let (r, g, b) = (value >> 11, (value >> 5) & 0x3f, value & 0x1f);
                    *out = [
                        (r << 3 | r >> 2) as u8,
                        (g << 2 | g >> 4) as u8,
                        (b << 3 | b >> 2) as u8,
                    ];
                }
            }
        }
        last.geometry = Some(geometry);
        last.spare = std::mem::replace(&mut last.raw, raw);
        Some(Frame { width, height, rgb })
    }
}

/// What [`Display::capture`] saw last.
#[derive(Default)]
pub struct Capture {
    /// Width, height, bytes per pixel, stride and start of the last picture.
    geometry: Option<[u64; 5]>,
    /// Its pixels as the framebuffer held them.
    raw: Vec<u8>,
    /// A buffer to read the next picture into.
    spare: Vec<u8>,
}

pub struct BochsDisplay {
    config: ConfigSpace,
    display: Display,
    mapper: RamMapper,
    /// Where the framebuffer is mapped into the guest now.
    mapped_at: Option<u64>,
}

impl BochsDisplay {
    /// An adapter whose EDID prefers `resolution`, kept between [`MIN_PREFERRED`] and
    /// [`MAX_PREFERRED`].
    pub fn new(mapper: RamMapper, resolution: (u32, u32)) -> std::io::Result<Self> {
        let width = resolution.0.clamp(MIN_PREFERRED.0, MAX_PREFERRED.0);
        let height = resolution.1.clamp(MIN_PREFERRED.1, MAX_PREFERRED.1);
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x1234,
            device_id: 0x1111,
            revision: 2,
            // Display controller, other.
            class: 0x03_80_00,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 0x1100,
        });
        config.add_bar(
            FRAMEBUFFER_BAR.into(),
            BarKind::Memory32,
            FRAMEBUFFER_SIZE,
            true,
        );
        config.add_bar(MMIO_BAR.into(), BarKind::Memory32, MMIO_SIZE, false);
        let mut registers = [0u16; REGISTERS];
        registers[ID] = ID5;
        registers[VIDEO_MEMORY_64K] = (FRAMEBUFFER_SIZE >> 16) as u16;
        Ok(Self {
            config,
            display: Display {
                state: Arc::new(Mutex::new(State {
                    registers,
                    getcaps: false,
                    edid: edid(width, height),
                    request: (width, height),
                    request_count: 0,
                    cursor: CursorRegisters::default(),
                })),
                framebuffer: RamRegion::new(FRAMEBUFFER_SIZE as usize)?,
            },
            mapper,
            mapped_at: None,
        })
    }

    /// A handle for reading the picture.
    pub fn display(&self) -> Display {
        self.display.clone()
    }

    /// Keeps the framebuffer's RAM mapping where BAR 0 decodes.
    fn sync_mapping(&mut self) {
        let wanted = self
            .config
            .bar_mappings()
            .into_iter()
            .find(|mapping| mapping.bar == FRAMEBUFFER_BAR)
            .map(|mapping| mapping.address);
        if wanted == self.mapped_at {
            return;
        }
        if let Some(old) = self.mapped_at.take()
            && let Err(e) = self.mapper.unmap(old)
        {
            tracing::warn!(%e, "bochs-display: can't unmap the framebuffer");
        }
        if let Some(address) = wanted {
            match self.mapper.map(
                address,
                self.display.framebuffer.clone(),
                MemoryAccess::READ | MemoryAccess::WRITE,
            ) {
                Ok(()) => self.mapped_at = Some(address),
                // The BAR region still works, just through exits.
                Err(e) => tracing::warn!(%e, address, "bochs-display: can't map the framebuffer"),
            }
        }
    }

    fn read_vmmc(&self, offset: u64) -> u32 {
        let state = lock(&self.display.state);
        match offset {
            0x00 => VMMC_MAGIC,
            0x04 => VMMC_FEATURES,
            REQUEST_SIZE => state.request.0 | (state.request.1 << 16),
            REQUEST_COUNT => state.request_count,
            CURSOR_CONTROL => state.cursor.control,
            CURSOR_SIZE => state.cursor.size,
            CURSOR_HOTSPOT => state.cursor.hotspot,
            CURSOR_COUNT => state.cursor.count,
            _ => 0,
        }
    }

    fn write_vmmc(&self, offset: u64, value: u32) {
        let mut state = lock(&self.display.state);
        match offset {
            CURSOR_CONTROL => state.cursor.control = value & CURSOR_SHOWN,
            CURSOR_SIZE => state.cursor.size = value,
            CURSOR_HOTSPOT => state.cursor.hotspot = value,
            CURSOR_COUNT => state.cursor.count = value,
            _ => {}
        }
    }

    fn read_dispi(&self, index: usize) -> u16 {
        let state = lock(&self.display.state);
        if state.getcaps {
            match index {
                XRES => return MAX_XRES,
                YRES => return MAX_YRES,
                BPP => return MAX_BPP,
                _ => {}
            }
        }
        state.registers.get(index).copied().unwrap_or(0)
    }

    fn write_dispi(&self, index: usize, value: u16) {
        let mut state = lock(&self.display.state);
        match index {
            ID => {
                // The guest names the version it wants; anything we know is fine.
                if (0xb0c0..=ID5).contains(&value) {
                    state.registers[ID] = value;
                }
            }
            XRES => state.registers[XRES] = value.min(MAX_XRES),
            YRES => state.registers[YRES] = value.min(MAX_YRES),
            BPP => {
                state.registers[BPP] = match value {
                    0 => 8,
                    8 | 15 | 16 | 24 | 32 => value,
                    _ => state.registers[BPP],
                }
            }
            ENABLE => {
                state.getcaps = value & GETCAPS != 0;
                let enabling = value & ENABLED != 0 && state.registers[ENABLE] & ENABLED == 0;
                state.registers[ENABLE] = value & !GETCAPS;
                if enabling {
                    let registers = &mut state.registers;
                    registers[VIRT_WIDTH] = registers[VIRT_WIDTH].max(registers[XRES]);
                    let bytes_per_pixel = u64::from(registers[BPP].div_ceil(8).max(1));
                    let stride = u64::from(registers[VIRT_WIDTH]).max(1) * bytes_per_pixel;
                    registers[VIRT_HEIGHT] =
                        (FRAMEBUFFER_SIZE / stride).min(u64::from(u16::MAX)) as u16;
                    registers[X_OFFSET] = 0;
                    registers[Y_OFFSET] = 0;
                    if value & NOCLEARMEM == 0 {
                        let visible = (stride * u64::from(registers[YRES])).min(FRAMEBUFFER_SIZE);
                        drop(state);
                        let zeroes = vec![0u8; 1 << 16];
                        let mut at = 0;
                        while at < visible as usize {
                            let len = zeroes.len().min(visible as usize - at);
                            self.display.framebuffer.write(at, &zeroes[..len]);
                            at += len;
                        }
                    }
                }
            }
            BANK => state.registers[BANK] = value,
            VIRT_WIDTH => state.registers[VIRT_WIDTH] = value,
            VIRT_HEIGHT => state.registers[VIRT_HEIGHT] = value,
            X_OFFSET => state.registers[X_OFFSET] = value,
            Y_OFFSET => state.registers[Y_OFFSET] = value,
            _ => {}
        }
    }
}

impl Drop for BochsDisplay {
    fn drop(&mut self) {
        if let Some(address) = self.mapped_at.take() {
            let _ = self.mapper.unmap(address);
        }
    }
}

impl PciDevice for BochsDisplay {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        self.sync_mapping();
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        match bar {
            // Only while the RAM mapping is missing.
            FRAMEBUFFER_BAR => {
                if !self.display.framebuffer.read(offset as usize, data) {
                    data.fill(0xff);
                }
            }
            MMIO_BAR => {
                for (i, byte) in data.iter_mut().enumerate() {
                    let at = offset + i as u64;
                    *byte = match at {
                        DISPI..QEXT => {
                            let register = self.read_dispi(((at - DISPI) / 2) as usize);
                            register.to_le_bytes()[(at % 2) as usize]
                        }
                        QEXT..0x604 => QEXT_SIZE.to_le_bytes()[(at - QEXT) as usize],
                        0x604..0x608 => LITTLE_ENDIAN.to_le_bytes()[(at - 0x604) as usize],
                        VMMC..VMMC_END => {
                            let register = self.read_vmmc((at - VMMC) & !3);
                            register.to_le_bytes()[(at % 4) as usize]
                        }
                        at if at < EDID_SIZE as u64 => lock(&self.display.state).edid[at as usize],
                        _ => 0,
                    };
                }
            }
            _ => data.fill(0xff),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        match bar {
            FRAMEBUFFER_BAR => {
                self.display.framebuffer.write(offset as usize, data);
            }
            MMIO_BAR if (DISPI..QEXT).contains(&offset) => {
                // 16-bit registers; a 32-bit write sets two.
                for (i, pair) in data.chunks(2).enumerate() {
                    let at = offset + 2 * i as u64;
                    if !at.is_multiple_of(2) || pair.len() != 2 {
                        continue;
                    }
                    self.write_dispi(
                        ((at - DISPI) / 2) as usize,
                        u16::from_le_bytes([pair[0], pair[1]]),
                    );
                }
            }
            // 32-bit registers, written whole.
            MMIO_BAR if (VMMC..VMMC_END).contains(&offset) => {
                if let Ok(bytes) = <[u8; 4]>::try_from(data)
                    && offset.is_multiple_of(4)
                {
                    self.write_vmmc(offset - VMMC, u32::from_le_bytes(bytes));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::config;

    fn adapter() -> BochsDisplay {
        BochsDisplay::new(RamMapper::detached(), (1280, 800)).unwrap()
    }

    fn write16(d: &mut BochsDisplay, index: usize, value: u16) {
        d.bar_write(MMIO_BAR, DISPI + 2 * index as u64, &value.to_le_bytes());
    }

    fn read32(d: &mut BochsDisplay, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        d.bar_read(MMIO_BAR, VMMC + offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(d: &mut BochsDisplay, offset: u64, value: u32) {
        d.bar_write(MMIO_BAR, VMMC + offset, &value.to_le_bytes());
    }

    #[test]
    fn the_host_asks_for_screen_sizes_through_vmmc_registers() {
        let mut d = adapter();
        let display = d.display();
        assert_eq!(read32(&mut d, 0), VMMC_MAGIC);
        assert_eq!(
            read32(&mut d, REQUEST_SIZE),
            1280 | (800 << 16),
            "the EDID's at first"
        );
        assert_eq!(read32(&mut d, REQUEST_COUNT), 0);
        assert_eq!(display.request_size(1500, 844), (1500, 844));
        assert_eq!(read32(&mut d, REQUEST_SIZE), 1500 | (844 << 16));
        assert_eq!(read32(&mut d, REQUEST_COUNT), 1);
        // The EDID prefers it from the next boot.
        let mut edid = [0u8; EDID_SIZE];
        d.bar_read(MMIO_BAR, 0, &mut edid);
        assert_eq!(
            u32::from(edid[56]) | (u32::from(edid[58] & 0xf0) << 4),
            1500
        );
        // Kept within what the adapter shows; the same size again isn't a new request.
        assert_eq!(display.request_size(100, 100), MIN_PREFERRED);
        assert_eq!(display.request_size(9000, 9000), MAX_PREFERRED);
        display.request_size(9000, 9000);
        assert_eq!(read32(&mut d, REQUEST_COUNT), 3);
    }

    #[test]
    fn the_guests_cursor_comes_out_as_rgba() {
        let mut d = adapter();
        let display = d.display();
        assert_eq!(
            display.cursor(0),
            (0, false, None),
            "none until the guest gives one"
        );
        // 2x1: opaque blue, then half-transparent red, as BGRA.
        d.bar_write(
            FRAMEBUFFER_BAR,
            CURSOR_OFFSET,
            &[0xff, 0, 0, 0xff, 0, 0, 0xff, 0x80],
        );
        write32(&mut d, CURSOR_SIZE, 2 | (1 << 16));
        write32(&mut d, CURSOR_HOTSPOT, 1);
        write32(&mut d, CURSOR_CONTROL, 1);
        write32(&mut d, CURSOR_COUNT, 1);
        let (count, shown, cursor) = display.cursor(0);
        assert_eq!((count, shown), (1, true));
        let cursor = cursor.expect("a new image");
        assert_eq!(
            (cursor.width, cursor.height, cursor.hotspot),
            (2, 1, (1, 0))
        );
        assert_eq!(cursor.rgba, [0, 0, 0xff, 0xff, 0xff, 0, 0, 0x80]);
        assert_eq!(display.cursor(1).2, None, "unchanged since");
        assert_eq!(read32(&mut d, CURSOR_COUNT), 1);
    }

    fn read16(d: &mut BochsDisplay, index: usize) -> u16 {
        let mut data = [0u8; 2];
        d.bar_read(MMIO_BAR, DISPI + 2 * index as u64, &mut data);
        u16::from_le_bytes(data)
    }

    /// What OVMF's QemuVideoDxe does to set a mode.
    fn set_mode(d: &mut BochsDisplay, width: u16, height: u16, bpp: u16) {
        write16(d, ENABLE, 0);
        write16(d, BANK, 0);
        write16(d, BPP, bpp);
        write16(d, XRES, width);
        write16(d, YRES, height);
        write16(d, VIRT_WIDTH, width);
        write16(d, VIRT_HEIGHT, height);
        write16(d, X_OFFSET, 0);
        write16(d, Y_OFFSET, 0);
        write16(d, ENABLE, ENABLED | 0x40);
    }

    #[test]
    fn identifies_itself_the_way_firmware_checks() {
        let mut d = adapter();
        assert_eq!(read16(&mut d, ID) & 0xfff0, 0xb0c0);
        assert_eq!(read16(&mut d, VIDEO_MEMORY_64K), 512, "32 MiB");
        let mut class = [0u8; 4];
        d.config_read(config::REVISION, &mut class);
        assert_eq!(u32::from_le_bytes(class) >> 8, 0x03_80_00);
        let mut qext = [0u8; 8];
        d.bar_read(MMIO_BAR, QEXT, &mut qext);
        assert_eq!(qext, [8, 0, 0, 0, 0x1e, 0x1e, 0x1e, 0x1e]);
        // GETCAPS shows the limits.
        write16(&mut d, ENABLE, GETCAPS);
        assert_eq!(read16(&mut d, XRES), MAX_XRES);
        write16(&mut d, ENABLE, 0);
        assert_eq!(read16(&mut d, XRES), 0);
    }

    #[test]
    fn capture_reports_only_changes() {
        let mut d = adapter();
        let display = d.display();
        set_mode(&mut d, 4, 2, 32);
        let mut last = Capture::default();
        assert!(display.capture(&mut last).is_some(), "the first picture");
        assert!(display.capture(&mut last).is_none(), "unchanged");
        d.bar_write(FRAMEBUFFER_BAR, 0, &0x00ff_0000u32.to_le_bytes());
        let frame = display.capture(&mut last).expect("a pixel changed");
        assert_eq!(&frame.rgb[..3], &[0xff, 0, 0]);
        assert!(display.capture(&mut last).is_none());
        set_mode(&mut d, 2, 2, 32);
        assert!(display.capture(&mut last).is_some(), "a new mode");
    }

    #[test]
    fn the_edid_names_the_preferred_resolution() {
        let mut d = BochsDisplay::new(RamMapper::detached(), (1920, 1080)).unwrap();
        let mut edid = [0u8; EDID_SIZE];
        d.bar_read(MMIO_BAR, 0, &mut edid);
        assert_eq!(&edid[..2], &[0x00, 0xff], "where OVMF looks");
        // As QemuVideoDxe reads it.
        let width = u32::from(edid[56]) | (u32::from(edid[58] & 0xf0) << 4);
        let height = u32::from(edid[59]) | (u32::from(edid[61] & 0xf0) << 4);
        assert_eq!((width, height), (1920, 1080));
        // Out of range: kept to what fits.
        let mut d = BochsDisplay::new(RamMapper::detached(), (8000, 100)).unwrap();
        d.bar_read(MMIO_BAR, 0, &mut edid);
        assert_eq!(
            u32::from(edid[56]) | (u32::from(edid[58] & 0xf0) << 4),
            3840
        );
        assert_eq!(u32::from(edid[59]) | (u32::from(edid[61] & 0xf0) << 4), 480);
    }

    #[test]
    fn snapshots_follow_the_mode_and_pixel_format() {
        let mut d = adapter();
        let display = d.display();
        assert_eq!(display.snapshot(), None, "no mode yet");
        set_mode(&mut d, 4, 2, 32);
        // Pixel (1, 1): XRGB 0x00112233.
        d.bar_write(FRAMEBUFFER_BAR, (4 + 1) * 4, &0x0011_2233u32.to_le_bytes());
        let frame = display.snapshot().unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(&frame.rgb[(4 + 1) * 3..(4 + 2) * 3], &[0x11, 0x22, 0x33]);
        assert_eq!(&frame.rgb[..3], &[0, 0, 0]);

        set_mode(&mut d, 2, 1, 16);
        d.bar_write(FRAMEBUFFER_BAR, 2, &0xf800u16.to_le_bytes());
        let frame = display.snapshot().unwrap();
        assert_eq!(frame.rgb, [0, 0, 0, 0xff, 0, 0], "RGB565 red");
    }

    #[test]
    fn enabling_clears_the_screen_unless_asked_not_to() {
        let mut d = adapter();
        set_mode(&mut d, 4, 4, 32);
        d.bar_write(FRAMEBUFFER_BAR, 0, &[0xff; 4]);
        write16(&mut d, ENABLE, 0);
        write16(&mut d, ENABLE, ENABLED | NOCLEARMEM);
        let mut pixel = [0u8; 4];
        d.bar_read(FRAMEBUFFER_BAR, 0, &mut pixel);
        assert_eq!(pixel, [0xff; 4]);
        set_mode(&mut d, 4, 4, 32);
        d.bar_read(FRAMEBUFFER_BAR, 0, &mut pixel);
        assert_eq!(pixel, [0; 4]);
    }
}
