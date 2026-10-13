//! EDID 1.4 base blocks (VESA E-EDID): how a monitor describes itself. The display
//! adapter offers one so the firmware and the guest's driver choose the resolution the
//! VMM asks for: OVMF boots in the first detailed timing's mode, and Linux's bochs-drm
//! prefers it.
//!
//! The preferred mode's timing follows CVT with reduced blanking at 60 Hz. Nothing
//! scans it out, but drivers check that it adds up.

/// Bytes in an EDID base block.
pub const EDID_SIZE: usize = 128;

/// The manufacturer ID, three letters packed into five bits each.
const MANUFACTURER: [u8; 3] = *b"VMC";
const PRODUCT: u16 = 0x0001;
/// Assumed pixel density for the physical size the EDID reports.
const DPI: u32 = 96;
const REFRESH_HZ: u32 = 60;

/// CVT reduced blanking: fixed horizontal blanking, front porch and sync width.
const H_BLANK: u32 = 160;
const H_FRONT: u32 = 48;
const H_SYNC: u32 = 32;
const V_FRONT: u32 = 3;
const MIN_V_BACK: u32 = 6;
/// The shortest vertical blanking interval, in microseconds.
const MIN_V_BLANK_US: u32 = 460;

/// sRGB's chromaticity: red, green, blue and white (x, y), in 1/1024ths.
const SRGB: [(u32, u32); 4] = [(655, 338), (307, 614), (154, 61), (320, 337)];

/// An EDID for a monitor whose preferred (and native) mode is `width` x `height` at
/// 60 Hz. Both must fit 12 bits; the adapter keeps them to its own maximum.
pub fn edid(width: u32, height: u32) -> [u8; EDID_SIZE] {
    let mut edid = [0u8; EDID_SIZE];
    edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    let letter = |c: u8| u16::from(c - b'A' + 1);
    let manufacturer =
        (letter(MANUFACTURER[0]) << 10) | (letter(MANUFACTURER[1]) << 5) | letter(MANUFACTURER[2]);
    edid[8..10].copy_from_slice(&manufacturer.to_be_bytes());
    edid[10..12].copy_from_slice(&PRODUCT.to_le_bytes());
    // Serial number 0 and week 0 (unspecified), model year 2026.
    edid[17] = (2026 - 1990) as u8;
    // EDID 1.4.
    edid[18] = 1;
    edid[19] = 4;
    // Digital input, 8 bits per colour, DisplayPort.
    edid[20] = 0xa5;
    edid[21] = (width * 254 / DPI).div_ceil(100) as u8;
    edid[22] = (height * 254 / DPI).div_ceil(100) as u8;
    // Gamma 2.2.
    edid[23] = 120;
    // RGB 4:4:4, sRGB is the default colour space, the preferred timing is native.
    edid[24] = 0x06;
    let [(rx, ry), (gx, gy), (bx, by), (wx, wy)] = SRGB;
    edid[25] = (((rx & 3) << 6) | ((ry & 3) << 4) | ((gx & 3) << 2) | (gy & 3)) as u8;
    edid[26] = (((bx & 3) << 6) | ((by & 3) << 4) | ((wx & 3) << 2) | (wy & 3)) as u8;
    for (i, value) in [rx, ry, gx, gy, bx, by, wx, wy].into_iter().enumerate() {
        edid[27 + i] = (value >> 2) as u8;
    }
    // Established timings: 640x480, 800x600 and 1024x768 at 60 Hz.
    edid[35] = 0x21;
    edid[36] = 0x08;
    // No standard timings.
    for slot in edid[38..54].chunks_mut(2) {
        slot.copy_from_slice(&[0x01, 0x01]);
    }
    edid[54..72].copy_from_slice(&detailed_timing(width, height));
    edid[72..90].copy_from_slice(&range_limits());
    edid[90..108].copy_from_slice(&text_descriptor(0xfc, "vmmc"));
    // A dummy descriptor fills the last slot.
    edid[108..126].copy_from_slice(&[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    // No extension blocks; the checksum makes the block sum to 0.
    let sum = edid[..127]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    edid[127] = sum.wrapping_neg();
    edid
}

/// CVT's vertical sync width for the aspect ratio.
fn v_sync(width: u32, height: u32) -> u32 {
    match (width, height) {
        _ if width * 3 == height * 4 => 4,
        _ if width * 9 == height * 16 => 5,
        _ if width * 10 == height * 16 => 6,
        _ if width * 4 == height * 5 => 7,
        _ => 10,
    }
}

/// An 18-byte detailed timing descriptor for `width` x `height` at 60 Hz.
fn detailed_timing(width: u32, height: u32) -> [u8; 18] {
    let v_sync = v_sync(width, height);
    // Lines in the minimum blanking time, at this many lines per frame.
    let frame_ns = 1_000_000_000 / REFRESH_HZ;
    let line_ns = (frame_ns - MIN_V_BLANK_US * 1000) / height;
    let v_blank = (MIN_V_BLANK_US * 1000)
        .div_ceil(line_ns)
        .max(V_FRONT + v_sync + MIN_V_BACK);
    let (h_total, v_total) = (width + H_BLANK, height + v_blank);
    // In 10 kHz units, rounded down to CVT's 0.25 MHz steps.
    let clock =
        (u64::from(h_total) * u64::from(v_total) * u64::from(REFRESH_HZ) / 250_000 * 25) as u16;
    let (width_mm, height_mm) = (width * 254 / DPI / 10, height * 254 / DPI / 10);
    let low = |value: u32| (value & 0xff) as u8;
    let high = |value: u32| ((value >> 8) & 0xf) as u8;
    let mut descriptor = [0u8; 18];
    descriptor[..2].copy_from_slice(&clock.to_le_bytes());
    descriptor[2] = low(width);
    descriptor[3] = low(H_BLANK);
    descriptor[4] = (high(width) << 4) | high(H_BLANK);
    descriptor[5] = low(height);
    descriptor[6] = low(v_blank);
    descriptor[7] = (high(height) << 4) | high(v_blank);
    descriptor[8] = low(H_FRONT);
    descriptor[9] = low(H_SYNC);
    descriptor[10] = (((V_FRONT & 0xf) << 4) | (v_sync & 0xf)) as u8;
    descriptor[11] = (((H_FRONT >> 8) & 3) << 6 | ((H_SYNC >> 8) & 3) << 4) as u8;
    descriptor[12] = low(width_mm);
    descriptor[13] = low(height_mm);
    descriptor[14] = (high(width_mm) << 4) | high(height_mm);
    // Digital separate sync: horizontal positive, vertical negative (CVT-RB).
    descriptor[17] = 0x1a;
    descriptor
}

/// The display range limits: 50-75 Hz vertical, 30-160 kHz horizontal, up to
/// 600 MHz, enough for 3840x2160 at 60 Hz.
fn range_limits() -> [u8; 18] {
    [
        0, 0, 0, 0xfd, 0, 50, 75, 30, 160, 60, 0x01, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
    ]
}

/// A display descriptor holding `text` (at most 13 bytes), such as the monitor name.
fn text_descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut descriptor = [0x20u8; 18];
    descriptor[..5].copy_from_slice(&[0, 0, 0, tag, 0]);
    let text = &text.as_bytes()[..text.len().min(12)];
    descriptor[5..5 + text.len()].copy_from_slice(text);
    descriptor[5 + text.len()] = 0x0a;
    descriptor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preferred resolution, read the way OVMF's QemuVideoDxe reads it.
    fn preferred(edid: &[u8; EDID_SIZE]) -> (u32, u32) {
        (
            u32::from(edid[56]) | (u32::from(edid[58] & 0xf0) << 4),
            u32::from(edid[59]) | (u32::from(edid[61] & 0xf0) << 4),
        )
    }

    #[test]
    fn a_valid_block_with_the_preferred_mode_first() {
        for (width, height) in [(1280, 800), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let edid = edid(width, height);
            assert_eq!(&edid[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
            assert_eq!(
                edid.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                0,
                "checksum"
            );
            assert_eq!((edid[18], edid[19]), (1, 4), "EDID 1.4");
            assert_eq!(preferred(&edid), (width, height));
            assert!(edid.windows(4).any(|window| window == b"vmmc"));
        }
    }

    #[test]
    fn the_timing_is_cvt_reduced_blanking_at_60_hz() {
        // 1920x1080 under CVT-RB is 138.50 MHz, with 31 lines of vertical blanking
        // (VESA's published 1920x1080R timing).
        let edid = edid(1920, 1080);
        assert_eq!(u16::from_le_bytes([edid[54], edid[55]]), 13_850);
        let v_blank = u32::from(edid[60]) | (u32::from(edid[61] & 0x0f) << 8);
        assert_eq!(v_blank, 31);
        let h_blank = u32::from(edid[57]) | (u32::from(edid[58] & 0x0f) << 8);
        assert_eq!(h_blank, 160);
    }

    #[test]
    fn the_manufacturer_packs_three_letters() {
        let edid = edid(1280, 800);
        let id = u16::from_be_bytes([edid[8], edid[9]]);
        let letter = |shift: u16| (b'A' - 1 + ((id >> shift) & 0x1f) as u8) as char;
        assert_eq!([letter(10), letter(5), letter(0)], ['V', 'M', 'C']);
    }
}
