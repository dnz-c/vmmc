//! The Bochs/QEMU "bochs-display" adapter (1234:1111, class "other display"): a linear
//! framebuffer and the VBE DISPI mode registers, with no legacy VGA. OVMF drives it
//! (QemuVideoDxe) and hands the framebuffer to the OS as GOP, which is all Windows'
//! basic display driver needs; Linux has a DRM driver for it.
//!
//! - **BAR 0:** the framebuffer, 16 MiB, mapped into the guest as plain RAM so drawing
//!   never exits. The device remaps it whenever the guest moves the BAR.
//! - **BAR 2:** MMIO registers: the DISPI registers at 0x500 (16 bits each) and QEMU's
//!   extension registers at 0x600 (framebuffer byte order).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use vm_core::{MemoryAccess, RamMapper, RamRegion};

use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice};

pub const FRAMEBUFFER_BAR: u8 = 0;
pub const FRAMEBUFFER_SIZE: u64 = 16 << 20;
pub const MMIO_BAR: u8 = 2;
pub const MMIO_SIZE: u64 = 0x1000;

const DISPI: u64 = 0x500;
const QEXT: u64 = 0x600;
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
    /// The picture now, or `None` while no graphics mode is enabled.
    pub fn snapshot(&self) -> Option<Frame> {
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
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        let mut row = vec![0u8; row_bytes as usize];
        for y in 0..u64::from(height) {
            self.framebuffer
                .read((start + y * stride) as usize, &mut row);
            for pixel in row.chunks(bytes_per_pixel as usize) {
                match bytes_per_pixel {
                    // Little-endian XRGB and RGB: blue first.
                    4 | 3 => rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]),
                    _ => {
                        let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                        let (r, g, b) = (value >> 11, (value >> 5) & 0x3f, value & 0x1f);
                        rgb.extend_from_slice(&[
                            (r << 3 | r >> 2) as u8,
                            (g << 2 | g >> 4) as u8,
                            (b << 3 | b >> 2) as u8,
                        ]);
                    }
                }
            }
        }
        Some(Frame { width, height, rgb })
    }
}

pub struct BochsDisplay {
    config: ConfigSpace,
    display: Display,
    mapper: RamMapper,
    /// Where the framebuffer is mapped into the guest now.
    mapped_at: Option<u64>,
}

impl BochsDisplay {
    pub fn new(mapper: RamMapper) -> std::io::Result<Self> {
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
                        // No EDID: firmware picks its default resolution.
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::config;

    fn adapter() -> BochsDisplay {
        BochsDisplay::new(RamMapper::detached()).unwrap()
    }

    fn write16(d: &mut BochsDisplay, index: usize, value: u16) {
        d.bar_write(MMIO_BAR, DISPI + 2 * index as u64, &value.to_le_bytes());
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
        assert_eq!(read16(&mut d, VIDEO_MEMORY_64K), 256, "16 MiB");
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
