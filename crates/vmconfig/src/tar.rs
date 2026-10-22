//! The plain tar archives OVA packages are (POSIX ustar): a 512-byte header before each
//! file, its data padded to 512 bytes, and two blocks of zeroes at the end.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const BLOCK: u64 = 512;

/// A file in an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Where its data starts in the archive, and how long it is.
    pub offset: u64,
    pub size: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// A header field: text up to its first NUL.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// An octal number field.
fn octal(field: &[u8]) -> io::Result<u64> {
    let digits = text(field);
    let digits = digits.trim_matches(|c: char| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(digits, 8).map_err(|_| invalid("a bad number in a tar header"))
}

/// The regular files in the archive at `path`, in order.
pub fn entries(path: &Path) -> io::Result<Vec<Entry>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut entries = Vec::new();
    let mut at = 0u64;
    let mut header = [0u8; BLOCK as usize];
    while at + BLOCK <= len {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut header)?;
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
            .sum();
        if octal(&header[148..156])? != sum {
            return Err(invalid(
                "a tar header's checksum is wrong (not a tar archive?)",
            ));
        }
        let size = octal(&header[124..136])?;
        let mut name = text(&header[0..100]);
        if &header[257..262] == b"ustar" {
            let prefix = text(&header[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let data = at + BLOCK;
        if data.checked_add(size).is_none_or(|end| end > len) {
            return Err(invalid("a file in the tar archive runs past its end"));
        }
        // Regular files; extended headers and directories are skipped.
        if matches!(header[156], b'0' | 0) {
            entries.push(Entry {
                name,
                offset: data,
                size,
            });
        }
        at = data + size.next_multiple_of(BLOCK);
    }
    Ok(entries)
}

/// Copies `entry`'s data out of the archive at `archive` into a new file at `to`.
pub fn extract(archive: &Path, entry: &Entry, to: &Path) -> io::Result<()> {
    let mut file = File::open(archive)?;
    file.seek(SeekFrom::Start(entry.offset))?;
    let out = OpenOptions::new().write(true).create_new(true).open(to)?;
    let mut out = BufWriter::with_capacity(1 << 20, out);
    let copied = io::copy(&mut (&mut file).take(entry.size), &mut out)?;
    if copied != entry.size {
        return Err(invalid("the tar archive ended inside a file"));
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()
}

/// A ustar header for a file of `size` bytes called `name`.
fn header(name: &str, size: u64) -> io::Result<[u8; BLOCK as usize]> {
    if name.len() > 100 {
        return Err(invalid(&format!(
            "{name}: too long a name for a tar archive"
        )));
    }
    let mut header = [0u8; BLOCK as usize];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let mut field = |at: usize, len: usize, value: u64| {
        let digits = format!("{value:0width$o}", width = len - 1);
        header[at..at + len - 1].copy_from_slice(digits.as_bytes());
    };
    field(100, 8, 0o644);
    field(108, 8, 0);
    field(116, 8, 0);
    field(124, 12, size);
    field(136, 12, 0);
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].fill(b' ');
    let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
    let digits = format!("{sum:06o}\0 ");
    header[148..156].copy_from_slice(digits.as_bytes());
    Ok(header)
}

/// Writes a new archive at `path` of `files` (each its name in the archive and where
/// it is), in order.
pub fn write(path: &Path, files: &[(&str, &Path)]) -> io::Result<()> {
    let out = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut out = BufWriter::with_capacity(1 << 20, out);
    for (name, from) in files {
        let mut file = File::open(from)?;
        let size = file.metadata()?.len();
        out.write_all(&header(name, size)?)?;
        let copied = io::copy(&mut file, &mut out)?;
        if copied != size {
            return Err(invalid(&format!(
                "{} changed while it was archived",
                from.display()
            )));
        }
        let padding = size.next_multiple_of(BLOCK) - size;
        out.write_all(&vec![0u8; padding as usize])?;
    }
    out.write_all(&[0u8; 2 * BLOCK as usize])?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_in_and_come_out() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.ovf");
        let two = dir.path().join("two.vmdk");
        std::fs::write(&one, b"<Envelope/>").unwrap();
        std::fs::write(&two, vec![7u8; 1000]).unwrap();
        let archive = dir.path().join("vm.ova");
        write(&archive, &[("vm.ovf", &one), ("vm-disk1.vmdk", &two)]).unwrap();
        assert_eq!(std::fs::metadata(&archive).unwrap().len(), 512 * 7);
        let entries = entries(&archive).unwrap();
        assert_eq!(
            entries,
            [
                Entry {
                    name: "vm.ovf".into(),
                    offset: 512,
                    size: 11
                },
                Entry {
                    name: "vm-disk1.vmdk".into(),
                    offset: 1536,
                    size: 1000
                },
            ]
        );
        let out = dir.path().join("out.vmdk");
        extract(&archive, &entries[1], &out).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), vec![7u8; 1000]);
        // A changed byte in a header.
        let mut bytes = std::fs::read(&archive).unwrap();
        bytes[0] = b'V';
        std::fs::write(&archive, bytes).unwrap();
        assert!(super::entries(&archive).is_err());
    }
}
