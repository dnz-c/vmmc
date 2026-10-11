//! A VM's state directory: what has to survive restarts of the VM.
//!
//! - `nvram.fd`: the UEFI variable store, copied from a template on first use.
//! - `uuid`: the SMBIOS system UUID, generated on first use.
//! - `mac`: the first network adapter's MAC address, derived from the UUID on first
//!   use (QEMU's 52:54:00 prefix, locally administered).

use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::path::{Path, PathBuf};

pub struct VmDir {
    pub nvram: PathBuf,
    pub uuid: [u8; 16],
    pub mac: [u8; 6],
}

/// Opens (creating it if needed) the state directory at `dir`. A missing variable store
/// is copied from `vars_template`.
pub fn open(dir: &Path, vars_template: &Path) -> Result<VmDir, String> {
    let context = |what: &str, path: &Path, e: io::Error| format!("{what} {}: {e}", path.display());
    fs::create_dir_all(dir).map_err(|e| context("can't create", dir, e))?;

    let nvram = dir.join("nvram.fd");
    if !nvram.exists() {
        // Copy then rename, so an interrupted copy never leaves a short variable store.
        let partial = dir.join("nvram.fd.partial");
        fs::copy(vars_template, &partial).map_err(|e| context("can't copy", vars_template, e))?;
        fs::rename(&partial, &nvram).map_err(|e| context("can't create", &nvram, e))?;
    }

    let uuid_path = dir.join("uuid");
    let uuid = match fs::read_to_string(&uuid_path) {
        Ok(text) => parse_uuid(text.trim())
            .ok_or_else(|| format!("{} doesn't hold a UUID", uuid_path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let uuid = random_uuid();
            fs::write(&uuid_path, format_uuid(&uuid) + "\n")
                .map_err(|e| context("can't write", &uuid_path, e))?;
            uuid
        }
        Err(e) => return Err(context("can't read", &uuid_path, e)),
    };
    let mac_path = dir.join("mac");
    let mac = match fs::read_to_string(&mac_path) {
        Ok(text) => crate::parse_mac(text.trim())
            .ok_or_else(|| format!("{} doesn't hold a MAC address", mac_path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mac = [0x52, 0x54, 0x00, uuid[13], uuid[14], uuid[15]];
            let text: Vec<String> = mac.iter().map(|b| format!("{b:02x}")).collect();
            fs::write(&mac_path, text.join(":") + "\n")
                .map_err(|e| context("can't write", &mac_path, e))?;
            mac
        }
        Err(e) => return Err(context("can't read", &mac_path, e)),
    };
    Ok(VmDir { nvram, uuid, mac })
}

/// A version 4 (random) UUID. The randomness comes from the standard library's
/// per-process hash keys, which is plenty for telling VMs apart.
fn random_uuid() -> [u8; 16] {
    let state = RandomState::new();
    let mut uuid = [0u8; 16];
    uuid[..8].copy_from_slice(&state.hash_one(0u8).to_le_bytes());
    uuid[8..].copy_from_slice(&state.hash_one(1u8).to_le_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    uuid
}

/// The usual `8-4-4-4-12` lowercase hex form.
fn format_uuid(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || text.len() != 36 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_round_trip_and_are_version_4() {
        let uuid = random_uuid();
        assert_eq!(uuid[6] >> 4, 4);
        assert_eq!(uuid[8] >> 6, 0b10);
        let text = format_uuid(&uuid);
        assert_eq!(text.len(), 36);
        assert_eq!(parse_uuid(&text), Some(uuid));
        assert_ne!(random_uuid(), uuid);
        assert_eq!(parse_uuid("not-a-uuid"), None);
    }

    #[test]
    fn first_open_creates_state_and_later_opens_keep_it() {
        let scratch = tempfile::tempdir().unwrap();
        let template = scratch.path().join("OVMF_VARS.fd");
        fs::write(&template, [0xab; 64]).unwrap();
        let dir = scratch.path().join("vm");

        let first = open(&dir, &template).unwrap();
        assert_eq!(fs::read(&first.nvram).unwrap(), [0xab; 64]);
        // The guest's variable writes land in the copy, never in the template.
        fs::write(&first.nvram, [0xcd; 64]).unwrap();

        let second = open(&dir, &template).unwrap();
        assert_eq!(second.uuid, first.uuid);
        assert_eq!(second.mac, first.mac);
        assert_eq!(&first.mac[..3], &[0x52, 0x54, 0x00]);
        assert_eq!(fs::read(&second.nvram).unwrap(), [0xcd; 64]);
        assert_eq!(fs::read(&template).unwrap(), [0xab; 64]);
    }
}
