//! Writes cpio archives in the "newc" format the Linux kernel unpacks as an initramfs,
//! and reads them back (to take files out of a distribution's initrd).

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

/// Every regular file in `bytes`, which may hold several archives back to back (with
/// zero padding between them), as `(path, contents)`.
pub fn files(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut at = 0;
    let field = |header: &[u8], index: usize| -> Result<usize, String> {
        let text = std::str::from_utf8(&header[6 + 8 * index..14 + 8 * index])
            .map_err(|_| "bad cpio header")?;
        usize::from_str_radix(text, 16).map_err(|_| "bad cpio header".into())
    };
    loop {
        while at < bytes.len() && bytes[at] == 0 {
            at += 1;
        }
        if at >= bytes.len() {
            return Ok(files);
        }
        let header = bytes.get(at..at + 110).ok_or("truncated cpio header")?;
        if &header[..6] != MAGIC.as_bytes() {
            return Err(format!("not a newc cpio archive at byte {at}"));
        }
        let (mode, size, name_size) = (field(header, 1)?, field(header, 6)?, field(header, 11)?);
        let name_end = at + 110 + name_size;
        let name = bytes
            .get(at + 110..name_end.saturating_sub(1))
            .ok_or("truncated cpio name")?;
        let name = String::from_utf8_lossy(name).into_owned();
        let data_start = (name_end - at).next_multiple_of(4) + at;
        let data = bytes
            .get(data_start..data_start + size)
            .ok_or("truncated cpio file")?;
        at = (data_start + size - at).next_multiple_of(4) + at;
        if name != TRAILER && mode as u32 & 0o170000 == S_IFREG {
            files.push((name, data.to_vec()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_files_back_across_concatenated_archives() {
        let mut bytes = archive(&[Entry::dir("lib"), Entry::file("lib/a.ko", 0o644, b"first")]);
        bytes.extend_from_slice(&[0; 512]);
        bytes.extend(archive(&[Entry::file("b", 0o644, b"second!")]));
        let files = files(&bytes).unwrap();
        assert_eq!(
            files,
            [
                ("lib/a.ko".to_owned(), b"first".to_vec()),
                ("b".to_owned(), b"second!".to_vec())
            ]
        );
        assert!(super::files(&bytes[..200]).is_err(), "truncated");
    }

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
