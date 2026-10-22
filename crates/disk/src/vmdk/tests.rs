use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use super::descriptor::{Access, Descriptor, ExtentLine, ExtentType};
use super::sparse::{self, Header};
use super::*;

const MIB: u64 = 1 << 20;
const GRAIN: u64 = 64 << 10;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

fn read(disk: &dyn BlockBackend, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    disk.read_at(&mut buf, offset).unwrap();
    buf
}

fn header(path: &Path) -> Header {
    Header::parse(&fs::read(path).unwrap()[..512]).unwrap()
}

#[test]
fn a_new_sparse_disk_reads_zeroes_and_keeps_what_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, 100 * MIB).unwrap();
    assert!(create_sparse(&path, MIB).is_err(), "never replaced");
    let empty_len = fs::metadata(&path).unwrap().len();
    {
        let disk = Vmdk::open(&path, false).unwrap();
        assert_eq!(disk.len(), 100 * MIB);
        assert_eq!(read(&disk, 0, 4096), vec![0; 4096]);
        assert!(header(&path).unclean_shutdown, "marked while open");
        // Unaligned, across a grain boundary, and in the last grain.
        disk.write_at(&pattern(5000, 1), GRAIN - 1000).unwrap();
        disk.write_at(&pattern(512, 2), 100 * MIB - 512).unwrap();
        assert_eq!(read(&disk, GRAIN - 1000, 5000), pattern(5000, 1));
        // Around the write, the new grains still read as zeroes.
        assert_eq!(read(&disk, GRAIN - 2000, 1000), vec![0; 1000]);
        assert_eq!(read(&disk, GRAIN + 4000, 1000), vec![0; 1000]);
    }
    assert!(!header(&path).unclean_shutdown, "cleared on close");
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        empty_len + 3 * GRAIN,
        "three grains allocated"
    );
    let disk = Vmdk::open(&path, true).unwrap();
    assert_eq!(read(&disk, GRAIN - 1000, 5000), pattern(5000, 1));
    assert_eq!(read(&disk, 100 * MIB - 512, 512), pattern(512, 2));
    assert!(disk.write_at(b"x", 0).is_err(), "read-only");
    disk.flush().unwrap();
}

#[test]
fn the_first_write_changes_the_content_id_and_both_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, 10 * MIB).unwrap();
    let before = Vmdk::open(&path, true).unwrap().descriptor().cid;
    {
        let disk = Vmdk::open(&path, false).unwrap();
        assert_eq!(
            disk.descriptor().cid,
            before,
            "opening alone changes nothing"
        );
        disk.write_at(&[7; 512], 3 * GRAIN).unwrap();
        disk.write_at(&[8; 512], 4 * GRAIN).unwrap();
    }
    let disk = Vmdk::open(&path, true).unwrap();
    let after = disk.descriptor();
    assert_ne!(after.cid, before);
    assert!(
        after
            .ddb("ddb.longContentID")
            .unwrap()
            .ends_with(&format!("{:08x}", after.cid))
    );
    // The redundant grain table points at the same grains as the primary.
    let bytes = fs::read(&path).unwrap();
    let h = header(&path);
    let entry = |dir: u64, grain: usize| {
        let gt = u32::from_le_bytes(bytes[dir as usize * 512..][..4].try_into().unwrap());
        let at = gt as usize * 512 + grain * 4;
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    };
    for grain in [3, 4] {
        assert_ne!(entry(h.gd_offset, grain), 0);
        assert_eq!(entry(h.gd_offset, grain), entry(h.rgd_offset, grain));
    }
}

#[test]
fn a_missing_grain_table_is_allocated_in_both_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, 100 * MIB).unwrap();
    // As other tools leave them: directory entry 1 without a table.
    let h = header(&path);
    let mut bytes = fs::read(&path).unwrap();
    for directory in [h.gd_offset, h.rgd_offset] {
        let at = directory as usize * 512 + 4;
        bytes[at..at + 4].fill(0);
    }
    fs::write(&path, &bytes).unwrap();
    let offset = 512 * GRAIN + 100; // in table 1
    {
        let disk = Vmdk::open(&path, false).unwrap();
        assert_eq!(read(&disk, offset, 16), vec![0; 16]);
        disk.write_at(b"in a new table", offset).unwrap();
    }
    let disk = Vmdk::open(&path, true).unwrap();
    assert_eq!(read(&disk, offset, 14), b"in a new table");
    let bytes = fs::read(&path).unwrap();
    let gde = |directory: u64| {
        let at = directory as usize * 512 + 4;
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    };
    assert_ne!(gde(h.gd_offset), 0);
    assert_ne!(gde(h.rgd_offset), 0);
    assert_ne!(gde(h.gd_offset), gde(h.rgd_offset));
}

#[test]
fn a_delta_reads_through_to_its_parent_and_keeps_it_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.vmdk");
    let delta = dir.path().join("snapshots").join("delta.vmdk");
    fs::create_dir(dir.path().join("snapshots")).unwrap();
    create_sparse(&base, 10 * MIB).unwrap();
    {
        let disk = Vmdk::open(&base, false).unwrap();
        disk.write_at(&pattern(3 * GRAIN as usize, 1), 0).unwrap();
    }
    let base_bytes = fs::read(&base).unwrap();
    create_delta(&delta, &base).unwrap();
    let descriptor = Vmdk::open(&delta, true).unwrap().descriptor();
    assert_eq!(
        descriptor.parent_hint.as_deref(),
        Some(r"..\base.vmdk").map(|hint| { if cfg!(windows) { hint } else { "../base.vmdk" } })
    );
    {
        let disk = Vmdk::open(&delta, false).unwrap();
        assert_eq!(disk.len(), 10 * MIB);
        assert_eq!(
            read(&disk, 0, 3 * GRAIN as usize),
            pattern(3 * GRAIN as usize, 1)
        );
        // Half a grain: the other half still comes from the parent.
        disk.write_at(&pattern(GRAIN as usize / 2, 9), GRAIN)
            .unwrap();
        disk.write_zeroes(2 * GRAIN, 100).unwrap();
        let mut expected = pattern(3 * GRAIN as usize, 1);
        expected[GRAIN as usize..][..GRAIN as usize / 2]
            .copy_from_slice(&pattern(GRAIN as usize / 2, 9));
        expected[2 * GRAIN as usize..][..100].fill(0);
        assert_eq!(read(&disk, 0, 3 * GRAIN as usize), expected);
        assert!(Vmdk::open(&base, false).is_err(), "the parent is in use");
    }
    assert_eq!(fs::read(&base).unwrap(), base_bytes, "parent untouched");
}

#[test]
fn a_delta_merges_into_its_parent_and_the_one_above_moves_onto_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.vmdk");
    let middle = dir.path().join("base-000001.vmdk");
    let top = dir.path().join("base-000002.vmdk");
    create_sparse(&base, 10 * MIB).unwrap();
    Vmdk::open(&base, false)
        .unwrap()
        .write_at(&pattern(4 * GRAIN as usize, 1), 0)
        .unwrap();
    create_delta(&middle, &base).unwrap();
    {
        let disk = Vmdk::open(&middle, false).unwrap();
        disk.write_at(&pattern(GRAIN as usize / 2, 2), GRAIN)
            .unwrap();
        disk.write_zeroes(2 * GRAIN, 100).unwrap();
        // The last grain too.
        disk.write_at(&pattern(512, 3), 10 * MIB - 512).unwrap();
    }
    create_delta(&top, &middle).unwrap();
    Vmdk::open(&top, false)
        .unwrap()
        .write_at(&pattern(1000, 4), 3 * GRAIN)
        .unwrap();
    let whole = |path: &Path| read(&Vmdk::open(path, true).unwrap(), 0, 10 * MIB as usize);
    let (middle_view, top_view) = (whole(&middle), whole(&top));
    assert_eq!(
        parent_of(&top).unwrap(),
        Some(dir.path().join("base-000001.vmdk"))
    );
    assert_eq!(parent_of(&base).unwrap(), None);

    assert_eq!(
        merge_into_parent(&middle).unwrap(),
        dir.path().join("base.vmdk")
    );
    assert_eq!(whole(&base), middle_view, "the base reads as the delta did");
    assert!(
        Vmdk::open(&top, true).is_err(),
        "its parent's parent changed"
    );
    reparent(&top, &base).unwrap();
    fs::remove_file(&middle).unwrap();
    assert_eq!(whole(&top), top_view, "the top reads as before");
    assert_eq!(parent_of(&top).unwrap(), Some(dir.path().join("base.vmdk")));
    assert!(merge_into_parent(&base).is_err(), "not a delta");
}

#[test]
fn a_delta_on_a_changed_parent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.vmdk");
    let delta = dir.path().join("delta.vmdk");
    create_sparse(&base, MIB).unwrap();
    create_delta(&delta, &base).unwrap();
    Vmdk::open(&base, false).unwrap().write_at(b"x", 0).unwrap();
    let error = Vmdk::open(&delta, true).err().unwrap();
    assert!(error.to_string().contains("changed after"), "{error}");
}

#[test]
fn zeroes_on_a_disk_without_parent_allocate_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, 10 * MIB).unwrap();
    let len = fs::metadata(&path).unwrap().len();
    Vmdk::open(&path, false)
        .unwrap()
        .write_zeroes(0, 10 * MIB)
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
}

#[test]
fn split_and_flat_extents_make_one_disk() {
    let dir = tempfile::tempdir().unwrap();
    // A flat extent, a zero one, and a sparse one (as a split disk's).
    fs::write(dir.path().join("a-flat.vmdk"), vec![0u8; 3 * 512]).unwrap();
    let (_, sparse_bytes) = sparse::layout(2048, 0);
    fs::write(dir.path().join("b-s001.vmdk"), &sparse_bytes).unwrap();
    let descriptor = Descriptor {
        other: Vec::new(),
        cid: 0x1234_5678,
        parent_cid: NO_PARENT,
        create_type: "custom".into(),
        parent_hint: None,
        extents: vec![
            ExtentLine {
                access: Access::ReadWrite,
                sectors: 2,
                kind: ExtentType::Flat,
                file: "a-flat.vmdk".into(),
                offset: 1,
            },
            ExtentLine {
                access: Access::ReadOnly,
                sectors: 2,
                kind: ExtentType::Zero,
                file: String::new(),
                offset: 0,
            },
            ExtentLine {
                access: Access::ReadWrite,
                sectors: 2048,
                kind: ExtentType::Sparse,
                file: "b-s001.vmdk".into(),
                offset: 0,
            },
        ],
        ddb: Vec::new(),
    };
    let path = dir.path().join("disk.vmdk");
    fs::write(&path, descriptor.to_text()).unwrap();
    {
        let disk = Vmdk::open(&path, false).unwrap();
        assert_eq!(disk.len(), (2 + 2 + 2048) * 512);
        disk.write_at(&[1; 1024], 0).unwrap();
        assert!(disk.write_at(&[2; 512], 1024).is_err(), "zero extent");
        disk.write_at(&[3; 1024], 2048).unwrap();
        assert_eq!(
            read(&disk, 0, 3072),
            [[1; 1024], [0; 1024], [3; 1024]].concat()
        );
    }
    // The flat data starts at its offset; the descriptor got a new CID.
    assert_eq!(
        fs::read(dir.path().join("a-flat.vmdk")).unwrap()[512..],
        [1; 1024]
    );
    let again = Descriptor::parse(&fs::read(&path).unwrap()).unwrap();
    assert_ne!(again.cid, 0x1234_5678);
    let disk = Vmdk::open(&path, true).unwrap();
    assert_eq!(read(&disk, 2048, 1024), [3; 1024]);
}

#[test]
fn reads_compressed_stream_grains() {
    // A stream-optimized extent: header, grain directory, one grain table, one
    // compressed grain (a marker: LBA, size, zlib data).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.vmdk");
    let grain = pattern(GRAIN as usize, 5);
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&grain, 6);
    let mut h = sparse::layout(256, 0).0;
    h.version = 3;
    h.flags = sparse::FLAG_NEWLINE_TEST | sparse::FLAG_COMPRESSED | sparse::FLAG_MARKERS;
    h.compression = 1;
    h.rgd_offset = 0;
    h.gd_offset = 1;
    h.overhead = 128;
    let mut bytes = vec![0u8; 128 * 512];
    bytes[..512].copy_from_slice(&h.to_bytes());
    bytes[512..516].copy_from_slice(&2u32.to_le_bytes()); // GD → table at 2
    bytes[1024 + 4..1024 + 8].copy_from_slice(&128u32.to_le_bytes()); // grain 1 at 128
    bytes.extend_from_slice(&128u64.to_le_bytes()); // LBA
    bytes.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&compressed);
    bytes.resize(bytes.len().next_multiple_of(512), 0);
    fs::write(&path, &bytes).unwrap();
    let file = fs::File::open(&path).unwrap();
    let extent = SparseExtent::open(file, true).unwrap();
    assert_eq!(extent.lookup(0).unwrap(), Grain::Unallocated);
    assert_eq!(extent.lookup(1).unwrap(), Grain::At(128));
    let mut back = vec![0u8; 1000];
    extent.read_grain(128, 500, &mut back).unwrap();
    assert_eq!(back, grain[500..1500]);
    assert!(
        SparseExtent::open(fs::File::open(&path).unwrap(), false).is_err(),
        "read-only"
    );
}

#[test]
fn grain_tables_pointing_into_metadata_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, MIB).unwrap();
    let h = header(&path);
    let mut bytes = fs::read(&path).unwrap();
    let gt = u32::from_le_bytes(bytes[h.gd_offset as usize * 512..][..4].try_into().unwrap());
    bytes[gt as usize * 512..][..4].copy_from_slice(&1u32.to_le_bytes()); // the descriptor
    fs::write(&path, &bytes).unwrap();
    let disk = Vmdk::open(&path, false).unwrap();
    let mut buf = [0u8; 512];
    assert!(disk.read_at(&mut buf, 0).is_err());
    assert!(disk.write_at(&buf, 0).is_err());
}

#[test]
fn concurrent_writers_each_keep_their_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    create_sparse(&path, 64 * MIB).unwrap();
    let disk = Arc::new(Vmdk::open(&path, false).unwrap());
    let threads: Vec<_> = (0..8u8)
        .map(|t| {
            let disk = disk.clone();
            thread::spawn(move || {
                for i in 0..32u64 {
                    // Interleaved grains, some shared by two threads (different halves).
                    let offset = (i * 8 + u64::from(t / 2)) * GRAIN + u64::from(t % 2) * GRAIN / 2;
                    disk.write_at(&pattern(GRAIN as usize / 2, t), offset)
                        .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    for t in 0..8u8 {
        for i in 0..32u64 {
            let offset = (i * 8 + u64::from(t / 2)) * GRAIN + u64::from(t % 2) * GRAIN / 2;
            assert_eq!(
                read(&*disk, offset, GRAIN as usize / 2),
                pattern(GRAIN as usize / 2, t)
            );
        }
    }
}

#[test]
fn opens_by_name_never_by_contents() {
    let dir = tempfile::tempdir().unwrap();
    // A raw image whose guest wrote a VMDK header into it stays raw.
    let raw = dir.path().join("disk.img");
    let (_, bytes) = sparse::layout(2048, 20);
    fs::write(&raw, &bytes).unwrap();
    let disk = crate::open(&raw, true).unwrap();
    assert_eq!(disk.len(), bytes.len() as u64);
    let vmdk = dir.path().join("disk.vmdk");
    create_sparse(&vmdk, MIB).unwrap();
    assert_eq!(crate::open(&vmdk, true).unwrap().len(), MIB);
    fs::write(dir.path().join("raw.vmdk"), [0u8; 4096]).unwrap();
    assert!(crate::open(&dir.path().join("raw.vmdk"), true).is_err());
}

#[test]
fn relative_paths_climb_out_of_the_delta_s_directory() {
    let base = std::env::temp_dir();
    let sep = std::path::MAIN_SEPARATOR_STR;
    assert_eq!(
        relative_path(
            &base.join("vm").join("vmmc"),
            &base.join("vm").join("disk.vmdk")
        ),
        format!("..{sep}disk.vmdk")
    );
    assert_eq!(relative_path(&base, &base.join("disk.vmdk")), "disk.vmdk");
}
