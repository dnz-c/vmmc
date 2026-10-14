//! VMware's `.vmx` files: `key = "value"` lines, keys case-insensitive, with `|XX`
//! hex escapes in values (`|22` is a quote, `|7C` a bar). Read only; vmmc never writes
//! one.

use std::collections::HashMap;

/// A parsed `.vmx`.
#[derive(Clone, Debug, Default)]
pub struct Vmx {
    /// Keys lowercased.
    entries: HashMap<String, String>,
}

/// `.vmx` files are small; anything far larger isn't one.
pub const MAX_LEN: usize = 4 << 20;

impl Vmx {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_LEN {
            return Err("too large for a .vmx".into());
        }
        let mut entries = HashMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: not key = \"value\"", number + 1));
            };
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            entries.insert(key.trim().to_ascii_lowercase(), unescape(value));
        }
        if entries.is_empty() {
            return Err("empty .vmx".into());
        }
        Ok(Self { entries })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// `TRUE` / `FALSE` (any case), as VMware writes them.
    pub fn flag(&self, key: &str) -> Option<bool> {
        match self.get(key)?.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        }
    }

    pub fn number(&self, key: &str) -> Option<u64> {
        self.get(key)?.trim().parse().ok()
    }

    /// Every key starting with `prefix` (lowercased), for looking through devices.
    pub fn keys_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> {
        let prefix = prefix.to_ascii_lowercase();
        self.entries
            .keys()
            .filter(move |key| key.starts_with(&prefix))
            .map(String::as_str)
    }
}

/// `|XX` is the character with hex code XX.
fn unescape(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'|'
            && let Some(hex) = value.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// VMware's `uuid.bios`: 16 hex bytes, spaces and a dash between them, in the order
/// SMBIOS stores them.
pub fn parse_bios_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_vmware_writes() {
        let vmx = Vmx::parse(
            ".encoding = \"UTF-8\"\nconfig.version = \"8\"\ndisplayName = \"Windows 10 x64\"\nnumvcpus = \"4\"\nmemsize = \"4096\"\nnvme0:0.fileName = \"Windows 10 x64.vmdk\"\nsata0:1.fileName = \"D:\\OSs\\Windows.iso\"\nfloppy0.present = \"FALSE\"\nannotation = \"a |22quoted|22 note|0Aline two\"\n",
        )
        .unwrap();
        assert_eq!(vmx.get("displayname"), Some("Windows 10 x64"));
        assert_eq!(vmx.get("DisplayName"), Some("Windows 10 x64"));
        assert_eq!(vmx.number("numvcpus"), Some(4));
        assert_eq!(vmx.flag("floppy0.present"), Some(false));
        assert_eq!(vmx.get("sata0:1.fileName"), Some(r"D:\OSs\Windows.iso"));
        assert_eq!(vmx.get("annotation"), Some("a \"quoted\" note\nline two"));
        let mut nvme: Vec<_> = vmx.keys_with_prefix("nvme0:").collect();
        nvme.sort();
        assert_eq!(nvme, ["nvme0:0.filename"]);
    }

    #[test]
    fn reads_the_bios_uuid() {
        assert_eq!(
            parse_bios_uuid("56 4d b7 47 33 14 81 8d-e5 c4 23 db 1c df 26 c1"),
            Some([
                0x56, 0x4d, 0xb7, 0x47, 0x33, 0x14, 0x81, 0x8d, 0xe5, 0xc4, 0x23, 0xdb, 0x1c, 0xdf,
                0x26, 0xc1
            ])
        );
        assert_eq!(parse_bios_uuid("56 4d"), None);
    }

    #[test]
    fn survives_junk() {
        assert!(Vmx::parse("").is_err());
        assert!(Vmx::parse("no equals sign").is_err());
        let _ = Vmx::parse("a = \"|\"\nb = \"|4\"\nc = \"|zz\"\nd = \"\u{e9}|C3|A9\"");
        assert_eq!(
            Vmx::parse("d = \"|C3|A9\"").unwrap().get("d"),
            Some("\u{e9}"),
            "escaped UTF-8"
        );
    }
}
