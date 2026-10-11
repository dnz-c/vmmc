//! Writes ISO 9660 disc images: files in the root directory, with Joliet names, and
//! optionally an El Torito boot entry for UEFI (a FAT image, as UEFI firmware expects).
//!
//! Written from ECMA-119, the Joliet specification and the El Torito specification.
//! Everything is laid out in order: volume descriptors from sector 16, path tables, the
//! two root directories, the boot catalog, then file data.

const SECTOR: usize = 2048;
/// Fixed timestamps keep images reproducible: 2026-01-01 00:00:00 UTC.
const DATE: &[u8; 16] = b"2026010100000000";
const RECORD_DATE: [u8; 7] = [126, 1, 1, 0, 0, 0, 0];

fn both32(value: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&value.to_le_bytes());
    out[4..].copy_from_slice(&value.to_be_bytes());
    out
}

fn both16(value: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&value.to_le_bytes());
    out[2..].copy_from_slice(&value.to_be_bytes());
    out
}

/// A name as ISO 9660 level 1 allows it: upper case, `[A-Z0-9_]`, 8.3, version 1.
fn iso_name(name: &str) -> Vec<u8> {
    let clean = |part: &str, len: usize| -> String {
        part.chars()
            .map(|c| c.to_ascii_uppercase())
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(len)
            .collect()
    };
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    format!("{}.{};1", clean(stem, 8), clean(extension, 3)).into_bytes()
}

/// A Joliet name: UCS-2 big-endian.
fn joliet_name(name: &str) -> Vec<u8> {
    name.encode_utf16()
        .take(64)
        .flat_map(u16::to_be_bytes)
        .collect()
}

fn directory_record(extent: u32, len: u32, directory: bool, name: &[u8]) -> Vec<u8> {
    let mut record = vec![0u8; 33];
    record[2..10].copy_from_slice(&both32(extent));
    record[10..18].copy_from_slice(&both32(len));
    record[18..25].copy_from_slice(&RECORD_DATE);
    record[25] = if directory { 0x02 } else { 0 };
    record[28..32].copy_from_slice(&both16(1));
    record[32] = name.len() as u8;
    record.extend_from_slice(name);
    if record.len() % 2 == 1 {
        record.push(0);
    }
    record[0] = record.len() as u8;
    record
}

/// Fills a text field: ASCII padded with spaces, or UCS-2 spaces for Joliet.
fn text_field(field: &mut [u8], text: &str, joliet: bool) {
    if joliet {
        for (i, byte) in field.iter_mut().enumerate() {
            *byte = if i % 2 == 0 { 0 } else { b' ' };
        }
        let name = joliet_name(text);
        let n = name.len().min(field.len() & !1);
        field[..n].copy_from_slice(&name[..n]);
    } else {
        field.fill(b' ');
        let n = text.len().min(field.len());
        field[..n].copy_from_slice(&text.as_bytes()[..n]);
    }
}

struct Layout {
    total: u32,
    path_tables: [u32; 2],
    root: u32,
}

fn volume_descriptor(kind: u8, joliet: bool, volume_id: &str, layout: &Layout) -> Vec<u8> {
    let mut sector = vec![0u8; SECTOR];
    sector[0] = kind;
    sector[1..6].copy_from_slice(b"CD001");
    sector[6] = 1;
    text_field(&mut sector[8..40], "", joliet);
    text_field(&mut sector[40..72], volume_id, joliet);
    sector[80..88].copy_from_slice(&both32(layout.total));
    if joliet {
        // UCS-2 level 3.
        sector[88..91].copy_from_slice(b"%/E");
    }
    sector[120..124].copy_from_slice(&both16(1));
    sector[124..128].copy_from_slice(&both16(1));
    sector[128..132].copy_from_slice(&both16(SECTOR as u16));
    sector[132..140].copy_from_slice(&both32(10));
    sector[140..144].copy_from_slice(&layout.path_tables[0].to_le_bytes());
    sector[148..152].copy_from_slice(&layout.path_tables[1].to_be_bytes());
    let root = directory_record(layout.root, SECTOR as u32, true, &[0]);
    sector[156..156 + root.len()].copy_from_slice(&root);
    for range in [
        190..318,
        318..446,
        446..574,
        574..702,
        702..739,
        739..776,
        776..813,
    ] {
        text_field(&mut sector[range], "", joliet);
    }
    text_field(&mut sector[574..702], "VMMC XTASK", joliet);
    for at in [813, 830] {
        sector[at..at + 16].copy_from_slice(DATE);
    }
    for at in [847, 864] {
        sector[at..at + 16].fill(b'0');
    }
    sector[881] = 1;
    sector
}

/// A path table holding just the root directory, little- or big-endian.
fn path_table(root: u32, big_endian: bool) -> Vec<u8> {
    let mut table = vec![1u8, 0];
    if big_endian {
        table.extend_from_slice(&root.to_be_bytes());
        table.extend_from_slice(&1u16.to_be_bytes());
    } else {
        table.extend_from_slice(&root.to_le_bytes());
        table.extend_from_slice(&1u16.to_le_bytes());
    }
    table.extend_from_slice(&[0, 0]);
    table
}

/// An image with `files` in the root directory. With `efi_boot`, a FAT image that UEFI
/// firmware boots from (it's also visible as `efiboot.img`).
pub fn build(volume_id: &str, files: &[(&str, &[u8])], efi_boot: Option<&[u8]>) -> Vec<u8> {
    let mut entries: Vec<(&str, &[u8])> = files.to_vec();
    if let Some(image) = efi_boot {
        entries.push(("efiboot.img", image));
    }

    // Sectors, in order.
    let mut next = 16u32;
    let mut take = || {
        let at = next;
        next += 1;
        at
    };
    let primary = take();
    let boot_record = efi_boot.is_some().then(&mut take);
    let joliet = take();
    let terminator = take();
    let iso_tables = [take(), take()];
    let joliet_tables = [take(), take()];
    let iso_root = take();
    let joliet_root = take();
    let catalog = efi_boot.is_some().then(&mut take);
    let mut extents = Vec::with_capacity(entries.len());
    for (_, data) in &entries {
        extents.push(next);
        next += data.len().div_ceil(SECTOR) as u32;
    }
    let total = next;
    let mut image = vec![0u8; total as usize * SECTOR];
    let mut put = |sector: u32, bytes: &[u8]| {
        let at = sector as usize * SECTOR;
        image[at..at + bytes.len()].copy_from_slice(bytes);
    };

    let iso_layout = Layout {
        total,
        path_tables: iso_tables,
        root: iso_root,
    };
    let joliet_layout = Layout {
        total,
        path_tables: joliet_tables,
        root: joliet_root,
    };
    put(
        primary,
        &volume_descriptor(1, false, volume_id, &iso_layout),
    );
    put(
        joliet,
        &volume_descriptor(2, true, volume_id, &joliet_layout),
    );
    let mut end = vec![255u8];
    end.extend_from_slice(b"CD001\x01");
    put(terminator, &end);
    put(iso_tables[0], &path_table(iso_root, false));
    put(iso_tables[1], &path_table(iso_root, true));
    put(joliet_tables[0], &path_table(joliet_root, false));
    put(joliet_tables[1], &path_table(joliet_root, true));

    // Both root directories: ".", "..", then the files sorted by their names.
    for (root, name_of) in [
        (iso_root, iso_name as fn(&str) -> Vec<u8>),
        (joliet_root, joliet_name as fn(&str) -> Vec<u8>),
    ] {
        let mut records = directory_record(root, SECTOR as u32, true, &[0]);
        records.extend(directory_record(root, SECTOR as u32, true, &[1]));
        let mut sorted: Vec<(Vec<u8>, u32, usize)> = entries
            .iter()
            .zip(&extents)
            .map(|((name, data), &extent)| (name_of(name), extent, data.len()))
            .collect();
        sorted.sort();
        for (name, extent, len) in sorted {
            records.extend(directory_record(extent, len as u32, false, &name));
        }
        assert!(
            records.len() <= SECTOR,
            "the root directory fits one sector"
        );
        put(root, &records);
    }

    if let (Some(boot_record), Some(catalog), Some(boot_image)) = (boot_record, catalog, efi_boot) {
        let mut record = vec![0u8; SECTOR];
        record[1..6].copy_from_slice(b"CD001");
        record[6] = 1;
        record[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
        record[71..75].copy_from_slice(&catalog.to_le_bytes());
        put(boot_record, &record);

        // Validation entry for the EFI platform, then the default entry: no emulation,
        // the FAT image, its size in 512-byte sectors.
        let mut entries = vec![0u8; 64];
        entries[0] = 1;
        entries[1] = 0xef;
        entries[4..8].copy_from_slice(b"VMMC");
        entries[30] = 0x55;
        entries[31] = 0xaa;
        let sum = entries[..32].chunks(2).fold(0u16, |sum, word| {
            sum.wrapping_add(u16::from_le_bytes([word[0], word[1]]))
        });
        entries[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
        entries[32] = 0x88;
        let sectors = boot_image.len().div_ceil(512).min(0xffff) as u16;
        entries[38..40].copy_from_slice(&sectors.to_le_bytes());
        let image_extent = *extents.last().expect("the boot image is a file");
        entries[40..44].copy_from_slice(&image_extent.to_le_bytes());
        put(catalog, &entries);
    }

    for ((_, data), &extent) in entries.iter().zip(&extents) {
        put(extent, data);
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_le(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    /// The root directory's (name, extent, length) entries, skipping "." and "..".
    fn list(image: &[u8], descriptor: usize) -> Vec<(Vec<u8>, u32, u32)> {
        let pvd = &image[descriptor * SECTOR..];
        let root = u32_le(&pvd[156 + 2..]) as usize;
        let mut at = root * SECTOR;
        let mut out = Vec::new();
        while image[at] != 0 {
            let len = image[at] as usize;
            let name_len = image[at + 32] as usize;
            let name = image[at + 33..at + 33 + name_len].to_vec();
            if name != [0] && name != [1] {
                out.push((name, u32_le(&image[at + 2..]), u32_le(&image[at + 10..])));
            }
            at += len;
        }
        out
    }

    #[test]
    fn files_are_found_through_both_directories() {
        let image = build("VMMC_TEST", &[("vmmc.txt", b"vmmc cd\n")], None);
        assert_eq!(&image[16 * SECTOR + 1..16 * SECTOR + 6], b"CD001");
        assert_eq!(image[16 * SECTOR], 1, "primary");
        assert_eq!(image[17 * SECTOR], 2, "Joliet");
        assert_eq!(&image[17 * SECTOR + 88..17 * SECTOR + 91], b"%/E");
        assert_eq!(image[18 * SECTOR], 255, "terminator");
        assert_eq!(
            u32_le(&image[16 * SECTOR + 80..]) as usize * SECTOR,
            image.len()
        );

        let iso = list(&image, 16);
        assert_eq!(iso.len(), 1);
        assert_eq!(iso[0].0, b"VMMC.TXT;1");
        let (extent, len) = (iso[0].1 as usize, iso[0].2 as usize);
        assert_eq!(&image[extent * SECTOR..extent * SECTOR + len], b"vmmc cd\n");
        let joliet = list(&image, 17);
        assert_eq!(joliet[0].0, joliet_name("vmmc.txt"));
        assert_eq!(joliet[0].1, iso[0].1, "same data");
    }

    #[test]
    fn el_torito_points_at_the_efi_image() {
        let fat = vec![0xeb; 3 * 512 + 1];
        let image = build("VMMC_BOOT", &[], Some(&fat));
        let record = &image[17 * SECTOR..];
        assert_eq!(&record[7..30], b"EL TORITO SPECIFICATION");
        let catalog = &image[u32_le(&record[71..]) as usize * SECTOR..];
        assert_eq!(
            (catalog[0], catalog[1], catalog[30], catalog[31]),
            (1, 0xef, 0x55, 0xaa)
        );
        let sum = catalog[..32].chunks(2).fold(0u16, |sum, w| {
            sum.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
        });
        assert_eq!(sum, 0, "validation entry checksum");
        assert_eq!(catalog[32], 0x88, "bootable");
        assert_eq!(catalog[33], 0, "no emulation");
        assert_eq!(
            u16::from_le_bytes([catalog[38], catalog[39]]),
            4,
            "512-byte sectors"
        );
        let extent = u32_le(&catalog[40..]) as usize;
        assert_eq!(
            &image[extent * SECTOR..extent * SECTOR + fat.len()],
            &fat[..]
        );
    }

    #[test]
    fn long_names_fold_to_8_3() {
        assert_eq!(iso_name("autounattend.xml"), b"AUTOUNAT.XML;1");
        assert_eq!(iso_name("a-b.tx"), b"A_B.TX;1");
    }
}
