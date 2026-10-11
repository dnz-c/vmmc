//! Native dependencies built by the `native-deps` workflow: OVMF today, the TPM
//! simulator later. They are release assets of the (private) GitHub repo, fetched
//! through the logged-in `gh` CLI into `target/native/` and checked against the hashes
//! pinned here.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::guests::{read, verify};

const REPO: &str = "dnz-c/vmmc";
const RELEASE: &str = "native-deps-2";

/// `(file, SHA-256)` for each asset, from the release's SHA256SUMS.
const ASSETS: [(&str, &str); 7] = [
    // Release build, Secure Boot capable: what VMs run.
    (
        "OVMF_CODE.fd",
        "75143ba1242fee6567e447a10d0fac65fa076c08c712e1f8ef1835d39d57d606",
    ),
    // The same with debug logging to port 0x402.
    (
        "OVMF_CODE.debug.fd",
        "04e1064c997614f9738317f951a292f01311375f0b0c7dba2ca83821bb5d7e14",
    ),
    // Debug build without Secure Boot, so it has the UEFI shell tests drive.
    (
        "OVMF_CODE.test.fd",
        "4bb97031449c7d2f6ecb332889e925320d8033e53ff1e96d07899991ac3385c5",
    ),
    (
        "OVMF_VARS.fd",
        "5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e",
    ),
    (
        "OVMF_VARS.secboot.fd",
        "c5f7b33221c90fca199a155099e7ed40163561038a63c686ea347708f0c423e5",
    ),
    (
        "OVMF-LICENSE.txt",
        "50ce20c9cfdb0e19ee34fe0a51fc0afe961f743697b068359ab2f862b494df80",
    ),
    (
        "OVMF-BUILD-INFO.txt",
        "17c376f51dddadf5edc7deeb8586afae2d8cac1ef66292bcc336f2b52ed87789",
    ),
];

pub fn directory(workspace: &Path) -> PathBuf {
    workspace.join("target").join("native")
}

/// Makes sure every pinned asset is in `target/native/`, downloading what's missing or
/// stale. Returns the directory.
pub fn fetch(workspace: &Path) -> Result<PathBuf, String> {
    let dir = directory(workspace);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let missing: Vec<_> = ASSETS
        .iter()
        .filter(|(file, sha256)| {
            fs::read(dir.join(file)).map_or(true, |bytes| verify(file, &bytes, sha256).is_err())
        })
        .collect();
    if missing.is_empty() {
        return Ok(dir);
    }

    // Download into a scratch directory so a failed or tampered download never
    // replaces a good file.
    let partial = dir.join("partial");
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial).map_err(|e| format!("can't create {}: {e}", partial.display()))?;
    let mut command = Command::new("gh");
    command.args(["release", "download", RELEASE, "--repo", REPO, "--dir"]);
    command.arg(&partial);
    for (file, _) in &missing {
        command.args(["--pattern", file]);
    }
    eprintln!(
        "downloading {} native assets from {REPO} {RELEASE}",
        missing.len()
    );
    let status = command
        .status()
        .map_err(|e| format!("can't run gh (install the GitHub CLI and `gh auth login`): {e}"))?;
    if !status.success() {
        return Err(format!(
            "gh release download failed ({status}); is `gh auth login` done for an account \
             with access to {REPO}?"
        ));
    }
    for (file, sha256) in &missing {
        let downloaded = partial.join(file);
        verify(file, &read(&downloaded)?, sha256)?;
        fs::rename(&downloaded, dir.join(file))
            .map_err(|e| format!("can't move {}: {e}", downloaded.display()))?;
    }
    let _ = fs::remove_dir_all(&partial);
    Ok(dir)
}
