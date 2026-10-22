//! Writes ISO 9660 disc images: files in the root directory, with Joliet names (for
//! Windows) and Rock Ridge names and modes (for Linux), and optionally an El Torito boot
//! entry for UEFI (a FAT image, as UEFI firmware expects).
//!
//! Written from ECMA-119, the Joliet specification, SUSP and RRIP 1.10, and the El
//! Torito specification. Everything is laid out in order: volume descriptors from sector
//! 16, path tables, the two root directories, the boot catalog, then file data.

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

/// A name as ISO 9660 level 1 allows it: upper case, `[A-Z0-9_]`, 8.3, version 1. With
/// `number`, it ends the stem, to tell apart names that fold to the same.
fn iso_name(name: &str, number: Option<usize>) -> Vec<u8> {
    let clean = |part: &str, len: usize| -> String {
        part.chars()
            .map(|c| c.to_ascii_uppercase())
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(len)
            .collect()
    };
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let stem = match number {
        Some(n) => {
            let n = n.to_string();
            format!("{}{n}", clean(stem, 8 - n.len()))
        }
        None => clean(stem, 8),
    };
    format!("{stem}.{};1", clean(extension, 3)).into_bytes()
}

/// The files' names in the primary directory, each one different.
fn iso_names(entries: &[(&str, &[u8])]) -> Vec<Vec<u8>> {
    let mut used = std::collections::HashSet::new();
    entries
        .iter()
        .map(|(name, _)| {
            let mut id = iso_name(name, None);
            let mut number = 1;
            while !used.insert(id.clone()) {
                id = iso_name(name, Some(number));
                number += 1;
            }
            id
        })
        .collect()
}

/// A Joliet name: UCS-2 big-endian.
fn joliet_name(name: &str) -> Vec<u8> {
    name.encode_utf16()
        .take(64)
        .flat_map(u16::to_be_bytes)
        .collect()
}

/// A directory record, its System Use field (Rock Ridge's entries) after the name.
fn directory_record(
    extent: u32,
    len: u32,
    directory: bool,
    name: &[u8],
    system_use: &[u8],
) -> Vec<u8> {
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
    record.extend_from_slice(system_use);
    if record.len() % 2 == 1 {
        record.push(0);
    }
    record[0] = record.len() as u8;
    record
}

/// A SUSP entry: signature, length, version 1, data.
fn susp(signature: &[u8; 2], data: &[u8]) -> Vec<u8> {
    let mut entry = signature.to_vec();
    entry.push((4 + data.len()) as u8);
    entry.push(1);
    entry.extend_from_slice(data);
    entry
}

/// Rock Ridge's POSIX attributes: the mode, links, and root as owner.
fn rock_ridge_px(mode: u32) -> Vec<u8> {
    let links = if mode & 0o040000 != 0 { 2 } else { 1 };
    let mut data = both32(mode).to_vec();
    data.extend_from_slice(&both32(links));
    data.extend_from_slice(&both32(0));
    data.extend_from_slice(&both32(0));
    susp(b"PX", &data)
}

/// The root's "." entries: SUSP is in use (SP), and the extension is Rock Ridge (ER),
/// which is what makes Linux read the primary directory with its names.
fn rock_ridge_root() -> Vec<u8> {
    let mut entries = susp(b"SP", &[0xbe, 0xef, 0]);
    let (id, descriptor, source): (&[u8], &[u8], &[u8]) =
        (b"RRIP_1991A", b"POSIX FILE SYSTEM SEMANTICS", b"IEEE P1282");
    let mut data = vec![
        id.len() as u8,
        descriptor.len() as u8,
        source.len() as u8,
        1,
    ];
    data.extend_from_slice(id);
    data.extend_from_slice(descriptor);
    data.extend_from_slice(source);
    entries.extend(susp(b"ER", &data));
    entries.extend(rock_ridge_px(DIRECTORY_MODE));
    entries
}

/// Read and search for everyone.
const DIRECTORY_MODE: u32 = 0o040555;

/// A file's mode: read-only for everyone, and runnable if it's a script (`#!`) or an
/// ELF program, so a Linux guest can run it from the disc.
fn file_mode(data: &[u8]) -> u32 {
    if data.starts_with(b"#!") || data.starts_with(b"\x7fELF") {
        0o100555
    } else {
        0o100444
    }
}

/// A file's Rock Ridge entries: its mode and its name as it is.
fn rock_ridge_file(name: &str, data: &[u8]) -> Vec<u8> {
    let mut entries = rock_ridge_px(file_mode(data));
    let mut nm = vec![0u8];
    nm.extend_from_slice(name.as_bytes());
    entries.extend(susp(b"NM", &nm));
    entries
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
    /// The root directory's length, whole sectors.
    root_len: u32,
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
    let root = directory_record(layout.root, layout.root_len, true, &[0], &[]);
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

/// A root directory at sector `root`: ".", "..", then the files sorted by their names,
/// in whole sectors (a record doesn't cross into the next one). The primary one carries
/// Rock Ridge; the Joliet one has Joliet names.
fn root_directory(
    root: u32,
    entries: &[(&str, &[u8])],
    extents: &[u32],
    rock_ridge: bool,
) -> Vec<u8> {
    let ids = if rock_ridge {
        iso_names(entries)
    } else {
        entries.iter().map(|(name, _)| joliet_name(name)).collect()
    };
    let mut files: Vec<(Vec<u8>, Vec<u8>)> = entries
        .iter()
        .zip(extents)
        .zip(ids)
        .map(|(((name, data), &extent), id)| {
            let system_use = if rock_ridge {
                rock_ridge_file(name, data)
            } else {
                Vec::new()
            };
            let record = directory_record(extent, data.len() as u32, false, &id, &system_use);
            (id, record)
        })
        .collect();
    files.sort();
    let (dot, dot_dot) = if rock_ridge {
        (rock_ridge_root(), rock_ridge_px(DIRECTORY_MODE))
    } else {
        (Vec::new(), Vec::new())
    };
    let directory = |len: u32| {
        let mut out = Vec::new();
        let records = [
            directory_record(root, len, true, &[0], &dot),
            directory_record(root, len, true, &[1], &dot_dot),
        ];
        for record in records.iter().chain(files.iter().map(|(_, record)| record)) {
            if record.len() > SECTOR - out.len() % SECTOR {
                out.resize(out.len().next_multiple_of(SECTOR), 0);
            }
            out.extend_from_slice(record);
        }
        out.resize(out.len().next_multiple_of(SECTOR), 0);
        out
    };
    // The records' lengths don't depend on the directory's.
    directory(directory(0).len() as u32)
}

/// An image with `files` in the root directory. With `efi_boot`, a FAT image that UEFI
/// firmware boots from (it's also visible as `efiboot.img`).
pub fn build(volume_id: &str, files: &[(&str, &[u8])], efi_boot: Option<&[u8]>) -> Vec<u8> {
    let mut entries: Vec<(&str, &[u8])> = files.to_vec();
    if let Some(image) = efi_boot {
        entries.push(("efiboot.img", image));
    }

    // The root directories' lengths, which their contents don't change.
    let unplaced = vec![0; entries.len()];
    let iso_root_len = root_directory(0, &entries, &unplaced, true).len() as u32;
    let joliet_root_len = root_directory(0, &entries, &unplaced, false).len() as u32;

    // Sectors, in order.
    let mut next = 16u32;
    let mut take = |sectors: u32| {
        let at = next;
        next += sectors;
        at
    };
    let primary = take(1);
    let boot_record = efi_boot.is_some().then(|| take(1));
    let joliet = take(1);
    let terminator = take(1);
    let iso_tables = [take(1), take(1)];
    let joliet_tables = [take(1), take(1)];
    let iso_root = take(iso_root_len / SECTOR as u32);
    let joliet_root = take(joliet_root_len / SECTOR as u32);
    let catalog = efi_boot.is_some().then(|| take(1));
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
        root_len: iso_root_len,
    };
    let joliet_layout = Layout {
        total,
        path_tables: joliet_tables,
        root: joliet_root,
        root_len: joliet_root_len,
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

    put(
        iso_root,
        &root_directory(iso_root, &entries, &extents, true),
    );
    put(
        joliet_root,
        &root_directory(joliet_root, &entries, &extents, false),
    );

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

    /// The root directory's records, as the volume descriptor's root record has it: a
    /// zero after a record means the rest of that sector is empty.
    fn records(image: &[u8], descriptor: usize) -> Vec<&[u8]> {
        let pvd = &image[descriptor * SECTOR..];
        let root = u32_le(&pvd[156 + 2..]) as usize * SECTOR;
        let end = root + u32_le(&pvd[156 + 10..]) as usize;
        let mut at = root;
        let mut out = Vec::new();
        while at < end {
            let len = image[at] as usize;
            if len == 0 {
                at = (at / SECTOR + 1) * SECTOR;
                continue;
            }
            assert_eq!(
                at / SECTOR,
                (at + len - 1) / SECTOR,
                "a record within a sector"
            );
            out.push(&image[at..at + len]);
            at += len;
        }
        out
    }

    /// The root directory's (name, extent, length) entries, skipping "." and "..".
    fn list(image: &[u8], descriptor: usize) -> Vec<(Vec<u8>, u32, u32)> {
        records(image, descriptor)
            .into_iter()
            .filter_map(|record| {
                let name = record[33..33 + record[32] as usize].to_vec();
                (name != [0] && name != [1])
                    .then(|| (name, u32_le(&record[2..]), u32_le(&record[10..])))
            })
            .collect()
    }

    /// A record's SUSP entries by signature.
    fn system_use(record: &[u8]) -> Vec<([u8; 2], Vec<u8>)> {
        let name_len = record[32] as usize;
        let mut at = 33 + name_len + (1 - name_len % 2);
        let mut out = Vec::new();
        while at + 4 <= record.len() {
            let len = record[at + 2] as usize;
            out.push((
                [record[at], record[at + 1]],
                record[at + 4..at + len].to_vec(),
            ));
            at += len;
        }
        out
    }

    #[test]
    fn rock_ridge_keeps_names_and_runnable_files() {
        let image = build(
            "VMMC_TEST",
            &[
                ("install-linux.sh", b"#!/bin/sh\necho hi\n"),
                ("vmmc-agent", b"\x7fELF\x02\x01"),
                ("readme.txt", b"text"),
            ],
            None,
        );
        let primary = records(&image, 16);
        let dot = system_use(primary[0]);
        assert_eq!(&dot[0].0, b"SP");
        assert_eq!(dot[0].1, [0xbe, 0xef, 0]);
        let er = &dot.iter().find(|(sig, _)| sig == b"ER").expect("ER").1;
        assert_eq!(&er[4..4 + er[0] as usize], b"RRIP_1991A");
        let mut files: Vec<(String, u32)> = primary[2..]
            .iter()
            .map(|record| {
                let entries = system_use(record);
                let px = &entries.iter().find(|(sig, _)| sig == b"PX").unwrap().1;
                let nm = &entries.iter().find(|(sig, _)| sig == b"NM").unwrap().1;
                (String::from_utf8(nm[1..].to_vec()).unwrap(), u32_le(px))
            })
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                ("install-linux.sh".into(), 0o100555),
                ("readme.txt".into(), 0o100444),
                ("vmmc-agent".into(), 0o100555),
            ]
        );
        assert!(
            records(&image, 17)
                .iter()
                .all(|record| system_use(record).is_empty()),
            "no Rock Ridge in the Joliet directory"
        );
    }

    #[test]
    fn a_big_root_directory_takes_more_sectors() {
        let names: Vec<String> = (0..60)
            .map(|i| format!("a-long-file-name-{i:02}.txt"))
            .collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let image = build("VMMC_TEST", &files, None);
        for descriptor in [16, 17] {
            let pvd = &image[descriptor * SECTOR..];
            assert!(u32_le(&pvd[156 + 10..]) as usize > SECTOR);
            let listed = list(&image, descriptor);
            assert_eq!(listed.len(), 60);
            for (_, extent, len) in listed {
                assert_eq!(len, 1);
                assert_eq!(image[extent as usize * SECTOR], b'x');
            }
        }
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
        assert_eq!(iso_name("autounattend.xml", None), b"AUTOUNAT.XML;1");
        assert_eq!(iso_name("a-b.tx", None), b"A_B.TX;1");
        let names = iso_names(&[
            ("install-linux.sh", b""),
            ("install-windows.sh", b""),
            ("install-x.sh", b""),
        ]);
        assert_eq!(
            names,
            [&b"INSTALL_.SH;1"[..], b"INSTALL1.SH;1", b"INSTALL2.SH;1"]
        );
    }
}
