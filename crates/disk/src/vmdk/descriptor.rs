//! The VMDK descriptor: the text that says what a virtual disk is made of. It's a file
//! of its own, or embedded in a monolithic sparse extent.
//!
//! ```text
//! # Disk DescriptorFile
//! version=1
//! encoding="UTF-8"
//! CID=7499d6e1
//! parentCID=ffffffff
//! createType="monolithicSparse"
//!
//! # Extent description
//! RW 134217728 SPARSE "disk.vmdk"
//!
//! # The Disk Data Base
//! #DDB
//!
//! ddb.adapterType = "lsilogic"
//! ```

use std::fmt::Write as _;

/// A CID that means "no parent".
pub const NO_PARENT: u32 = 0xffff_ffff;

/// How an extent may be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
    NoAccess,
}

/// Where an extent's data lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtentType {
    /// A hosted sparse extent file.
    Sparse,
    /// Plain data in a file, from a sector offset.
    Flat,
    /// Reads as zeroes, takes no space.
    Zero,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtentLine {
    pub access: Access,
    /// Size in 512-byte sectors.
    pub sectors: u64,
    pub kind: ExtentType,
    /// Relative to the descriptor's directory (empty for zero extents).
    pub file: String,
    /// Where the data starts in a flat extent's file, in sectors.
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// The header's `key=value` lines other than the ones below, in order.
    pub other: Vec<(String, String)>,
    pub cid: u32,
    pub parent_cid: u32,
    pub create_type: String,
    /// The parent's descriptor, relative to this one's directory, for a delta disk.
    pub parent_hint: Option<String>,
    pub extents: Vec<ExtentLine>,
    /// The disk database: `ddb.*` entries, in order.
    pub ddb: Vec<(String, String)>,
}

/// Descriptors are small; anything larger isn't one.
pub const MAX_LEN: usize = 1 << 20;

impl Descriptor {
    /// Parses a descriptor; `text` may end in padding NULs.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_LEN {
            return Err("descriptor too large".into());
        }
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let text = decode(&bytes[..end]);
        if !text.trim_start().starts_with("# Disk DescriptorFile") {
            return Err("not a VMDK descriptor".into());
        }
        let mut descriptor = Descriptor {
            other: Vec::new(),
            cid: NO_PARENT,
            parent_cid: NO_PARENT,
            create_type: String::new(),
            parent_hint: None,
            extents: Vec::new(),
            ddb: Vec::new(),
        };
        let mut cid = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(extent) = parse_extent(line)? {
                descriptor.extents.push(extent);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("descriptor line {line:?} isn't key=value"));
            };
            let key = key.trim();
            let value = unquote(value.trim());
            match key {
                "CID" => cid = Some(parse_cid(value)?),
                "parentCID" => descriptor.parent_cid = parse_cid(value)?,
                "createType" => descriptor.create_type = value.to_owned(),
                "parentFileNameHint" => descriptor.parent_hint = Some(value.to_owned()),
                _ if key.starts_with("ddb.") => {
                    descriptor.ddb.push((key.to_owned(), value.to_owned()));
                }
                // Written fresh on output.
                "encoding" => {}
                _ => descriptor.other.push((key.to_owned(), value.to_owned())),
            }
        }
        descriptor.cid = cid.ok_or("descriptor has no CID")?;
        if descriptor.create_type.is_empty() {
            return Err("descriptor has no createType".into());
        }
        if descriptor.extents.is_empty() {
            return Err("descriptor lists no extents".into());
        }
        Ok(descriptor)
    }

    /// The disk's size in sectors: its extents' sizes added up.
    pub fn sectors(&self) -> Result<u64, String> {
        self.extents
            .iter()
            .try_fold(0u64, |total, extent| total.checked_add(extent.sectors))
            .ok_or_else(|| "extents too large".into())
    }

    pub fn ddb(&self, key: &str) -> Option<&str> {
        self.ddb
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Sets a database entry, adding it at the end if it's new.
    pub fn set_ddb(&mut self, key: &str, value: String) {
        match self.ddb.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.ddb.push((key.to_owned(), value)),
        }
    }

    /// A new content ID, as a disk gets when its contents first change: the CID and
    /// the long content ID ending in it.
    pub fn new_content_id(&mut self, random: u128) {
        self.cid = random as u32;
        // 0xffffffff means "no parent" and 0xfffffffe "never written".
        if self.cid >= 0xffff_fffe {
            self.cid = 0x1000_0000;
        }
        let long = format!("{:024x}{:08x}", random >> 32 & ((1 << 96) - 1), self.cid);
        if self.ddb("ddb.longContentID").is_some() {
            self.set_ddb("ddb.longContentID", long);
        }
    }

    pub fn to_text(&self) -> String {
        let mut text = String::from("# Disk DescriptorFile\nversion=1\nencoding=\"UTF-8\"\n");
        for (key, value) in &self.other {
            if key != "version" {
                let _ = writeln!(text, "{key}={}", quote_if_needed(value));
            }
        }
        let _ = writeln!(text, "CID={:08x}", self.cid);
        let _ = writeln!(text, "parentCID={:08x}", self.parent_cid);
        if let Some(hint) = &self.parent_hint {
            let _ = writeln!(text, "parentFileNameHint=\"{hint}\"");
        }
        let _ = writeln!(text, "createType=\"{}\"", self.create_type);
        text.push_str("\n# Extent description\n");
        for extent in &self.extents {
            let access = match extent.access {
                Access::ReadWrite => "RW",
                Access::ReadOnly => "RDONLY",
                Access::NoAccess => "NOACCESS",
            };
            match extent.kind {
                ExtentType::Sparse => {
                    let _ = writeln!(
                        text,
                        "{access} {} SPARSE \"{}\"",
                        extent.sectors, extent.file
                    );
                }
                ExtentType::Flat => {
                    let _ = writeln!(
                        text,
                        "{access} {} FLAT \"{}\" {}",
                        extent.sectors, extent.file, extent.offset
                    );
                }
                ExtentType::Zero => {
                    let _ = writeln!(text, "{access} {} ZERO", extent.sectors);
                }
            }
        }
        text.push_str("\n# The Disk Data Base\n#DDB\n\n");
        for (key, value) in &self.ddb {
            let _ = writeln!(text, "{key} = \"{value}\"");
        }
        text
    }
}

fn parse_cid(text: &str) -> Result<u32, String> {
    u32::from_str_radix(text, 16).map_err(|_| format!("CID {text:?} isn't hexadecimal"))
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(text)
}

fn quote_if_needed(value: &str) -> String {
    if value.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        value.to_owned()
    } else {
        format!("\"{value}\"")
    }
}

/// `ACCESS SECTORS TYPE ["FILE" [OFFSET]]`, or None for a line that isn't an extent.
fn parse_extent(line: &str) -> Result<Option<ExtentLine>, String> {
    let (first, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let access = match first {
        "RW" => Access::ReadWrite,
        "RDONLY" => Access::ReadOnly,
        "NOACCESS" => Access::NoAccess,
        _ => return Ok(None),
    };
    let bad = || format!("bad extent line {line:?}");
    let rest = rest.trim_start();
    let (sectors, rest) = rest.split_once(char::is_whitespace).ok_or_else(bad)?;
    let sectors: u64 = sectors.parse().map_err(|_| bad())?;
    let rest = rest.trim_start();
    let (kind, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let kind = match kind {
        "SPARSE" => ExtentType::Sparse,
        "FLAT" => ExtentType::Flat,
        "ZERO" => ExtentType::Zero,
        other => return Err(format!("extents of type {other} aren't supported")),
    };
    let rest = rest.trim();
    let (file, rest) = if kind == ExtentType::Zero {
        (String::new(), rest)
    } else {
        let quoted = rest.strip_prefix('"').ok_or_else(bad)?;
        let end = quoted.find('"').ok_or_else(bad)?;
        (quoted[..end].to_owned(), quoted[end + 1..].trim())
    };
    let offset = if rest.is_empty() {
        0
    } else {
        rest.parse().map_err(|_| bad())?
    };
    Ok(Some(ExtentLine {
        access,
        sectors,
        kind,
        file,
        offset,
    }))
}

/// Descriptors are UTF-8 or Windows-1252: text that isn't valid UTF-8 is read as the
/// latter.
fn decode(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9f => HIGH[usize::from(b - 0x80)],
            _ => char::from(b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPLIT: &str = "# Disk DescriptorFile\nversion=1\nencoding=\"windows-1252\"\nCID=95f0e44f\nparentCID=ffffffff\ncreateType=\"twoGbMaxExtentSparse\"\n\n# Extent description\nRW 8323072 SPARSE \"Windows 10 x64-s001.vmdk\"\nRW 983040 SPARSE \"Windows 10 x64-s002.vmdk\"\n\n# The Disk Data Base \n#DDB\n\nddb.adapterType = \"lsilogic\"\nddb.longContentID = \"2771e2e4d63f0edb68d89fc895f0e44f\"\n";

    #[test]
    fn parses_what_vmware_writes() {
        let d = Descriptor::parse(SPLIT.as_bytes()).unwrap();
        assert_eq!(d.cid, 0x95f0_e44f);
        assert_eq!(d.parent_cid, NO_PARENT);
        assert_eq!(d.create_type, "twoGbMaxExtentSparse");
        assert_eq!(d.extents.len(), 2);
        assert_eq!(d.extents[0].file, "Windows 10 x64-s001.vmdk");
        assert_eq!(d.extents[1].sectors, 983_040);
        assert_eq!(d.sectors().unwrap(), 8_323_072 + 983_040);
        assert_eq!(d.ddb("ddb.adapterType"), Some("lsilogic"));
    }

    #[test]
    fn round_trips_through_text() {
        let mut d = Descriptor::parse(SPLIT.as_bytes()).unwrap();
        d.parent_hint = Some("..\\base.vmdk".into());
        d.extents.push(ExtentLine {
            access: Access::ReadOnly,
            sectors: 2048,
            kind: ExtentType::Flat,
            file: "x-flat.vmdk".into(),
            offset: 7,
        });
        d.extents.push(ExtentLine {
            access: Access::ReadWrite,
            sectors: 8,
            kind: ExtentType::Zero,
            file: String::new(),
            offset: 0,
        });
        let again = Descriptor::parse(d.to_text().as_bytes()).unwrap();
        assert_eq!(again, d);
    }

    #[test]
    fn a_new_content_id_ends_the_long_one() {
        let mut d = Descriptor::parse(SPLIT.as_bytes()).unwrap();
        d.new_content_id(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        assert_eq!(d.cid, 0x4455_6677);
        assert_eq!(
            d.ddb("ddb.longContentID"),
            Some("0123456789abcdef0011223344556677")
        );
    }

    #[test]
    fn reads_windows_1252_names() {
        let mut bytes = SPLIT
            .replace("Windows 10 x64-s001", "D\u{fc}sk")
            .into_bytes();
        // ü in Windows-1252, where UTF-8 would have two bytes.
        let at = bytes.windows(2).position(|w| w == "ü".as_bytes()).unwrap();
        bytes.splice(at..at + 2, [0xfc]);
        let d = Descriptor::parse(&bytes).unwrap();
        assert_eq!(d.extents[0].file, "Düsk.vmdk");
    }

    #[test]
    fn rejects_what_isnt_a_descriptor() {
        assert!(Descriptor::parse(b"hello").is_err());
        assert!(
            Descriptor::parse(b"# Disk DescriptorFile\nCID=1\n").is_err(),
            "no extents"
        );
        let bad = SPLIT.replace("SPARSE \"Windows 10 x64-s002.vmdk\"", "VMFS \"x\"");
        assert!(Descriptor::parse(bad.as_bytes()).is_err());
        let bad = SPLIT.replace("8323072", "lots");
        assert!(Descriptor::parse(bad.as_bytes()).is_err());
    }

    #[test]
    fn survives_arbitrary_bytes() {
        // A cheap fuzz: every prefix and some bit flips of a real descriptor.
        let bytes = SPLIT.as_bytes();
        for end in 0..bytes.len() {
            let _ = Descriptor::parse(&bytes[..end]);
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..2000 {
            let mut copy = bytes.to_vec();
            for _ in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let at = (seed as usize) % copy.len();
                copy[at] = (seed >> 32) as u8;
            }
            let _ = Descriptor::parse(&copy);
        }
    }
}
