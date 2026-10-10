//! Writes cpio archives in the "newc" format the Linux kernel unpacks as an initramfs.

const MAGIC: &str = "070701";
const TRAILER: &str = "TRAILER!!!";

pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFCHR: u32 = 0o020000;

/// One archive entry.
pub struct Entry<'a> {
    pub path: &'a str,
    pub mode: u32,
    pub data: &'a [u8],
    /// Device number for character devices.
    pub rdev: (u32, u32),
}

impl<'a> Entry<'a> {
    pub fn dir(path: &'a str) -> Self {
        Self {
            path,
            mode: S_IFDIR | 0o755,
            data: &[],
            rdev: (0, 0),
        }
    }

    pub fn file(path: &'a str, mode: u32, data: &'a [u8]) -> Self {
        Self {
            path,
            mode: S_IFREG | mode,
            data,
            rdev: (0, 0),
        }
    }

    pub fn char_device(path: &'a str, mode: u32, major: u32, minor: u32) -> Self {
        Self {
            path,
            mode: S_IFCHR | mode,
            data: &[],
            rdev: (major, minor),
        }
    }
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn header(out: &mut Vec<u8>, inode: u32, entry: &Entry<'_>) {
    let name_size = entry.path.len() as u32 + 1;
    let fields = [
        inode,
        entry.mode,
        0, // uid
        0, // gid
        if entry.mode & S_IFDIR != 0 { 2 } else { 1 },
        0, // mtime
        entry.data.len() as u32,
        0, // devmajor
        0, // devminor
        entry.rdev.0,
        entry.rdev.1,
        name_size,
        0, // check
    ];
    out.extend_from_slice(MAGIC.as_bytes());
    for field in fields {
        out.extend_from_slice(format!("{field:08x}").as_bytes());
    }
    out.extend_from_slice(entry.path.as_bytes());
    out.push(0);
    pad4(out);
    out.extend_from_slice(entry.data);
    pad4(out);
}

/// A complete archive of `entries`, with the trailer.
pub fn archive(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        header(&mut out, index as u32 + 1, entry);
    }
    header(
        &mut out,
        0,
        &Entry {
            path: TRAILER,
            mode: 0,
            data: &[],
            rdev: (0, 0),
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry read back from an archive.
    struct Parsed {
        path: String,
        mode: u32,
        data: Vec<u8>,
        rdev: (u32, u32),
    }

    fn parse(mut bytes: &[u8]) -> Vec<Parsed> {
        let mut entries = Vec::new();
        let start_len = bytes.len();
        loop {
            assert_eq!(&bytes[..6], MAGIC.as_bytes());
            let field = |i: usize| {
                u32::from_str_radix(
                    std::str::from_utf8(&bytes[6 + 8 * i..14 + 8 * i]).unwrap(),
                    16,
                )
                .unwrap()
            };
            let (mode, size, rdev, name_size) = (
                field(1),
                field(6) as usize,
                (field(9), field(10)),
                field(11) as usize,
            );
            let name_end = 110 + name_size;
            let name = std::str::from_utf8(&bytes[110..name_end - 1])
                .unwrap()
                .to_owned();
            let data_start = name_end.next_multiple_of(4);
            let data = bytes[data_start..data_start + size].to_vec();
            let next = (data_start + size).next_multiple_of(4);
            assert_eq!((start_len - bytes.len() + next) % 4, 0);
            bytes = &bytes[next..];
            if name == TRAILER {
                assert!(bytes.is_empty());
                return entries;
            }
            entries.push(Parsed {
                path: name,
                mode,
                data,
                rdev,
            });
        }
    }

    #[test]
    fn round_trips_files_dirs_and_devices() {
        let archive = archive(&[
            Entry::dir("bin"),
            Entry::file("bin/busybox", 0o755, b"\x7fELF..."),
            Entry::file("init", 0o755, b"#!/bin/sh\n"),
            Entry::char_device("dev/console", 0o600, 5, 1),
        ]);
        let entries = parse(&archive);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, "bin");
        assert_eq!(entries[0].mode, S_IFDIR | 0o755);
        assert_eq!(entries[1].data, b"\x7fELF...");
        assert_eq!(entries[2].path, "init");
        assert_eq!(entries[3].mode, S_IFCHR | 0o600);
        assert_eq!(entries[3].rdev, (5, 1));
    }
}
