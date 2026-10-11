//! Ctrl-A m: saves guest RAM and every vCPU's state, for working out offline why a
//! guest hangs (by walking its page tables and reading kernel globals, say).
//!
//! The guest keeps running meanwhile, so the snapshot is only consistent for a guest
//! that has stopped doing anything, which is the case it's for.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use vm_core::{
    Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion, StopHandle,
};

const CHUNK: usize = 64 << 20;

/// Writes each RAM region to `ram-<guest-physical address>.bin` in a new directory
/// under the system temp directory, and has every vCPU append its state to
/// `vcpus.txt` there. Returns the directory.
pub fn save(memory: &GuestMemoryMmap, stop: &StopHandle) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let dir = std::env::temp_dir().join(format!("vmmc-memory-{stamp}"));
    fs::create_dir_all(&dir)?;

    let vcpus = Arc::new(Mutex::new(File::create(dir.join("vcpus.txt"))?));
    stop.request_state_dump(move |state| {
        let mut file = vcpus.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{state}\n");
    });

    let mut buffer = vec![0u8; CHUNK];
    for region in memory.iter() {
        let start = region.start_addr().0;
        let size = region.len() as usize;
        let mut file = File::create(dir.join(format!("ram-{start:#x}.bin")))?;
        let mut done = 0;
        while done < size {
            let len = CHUNK.min(size - done);
            memory
                .read_slice(&mut buffer[..len], GuestAddress(start + done as u64))
                .map_err(io::Error::other)?;
            file.write_all(&buffer[..len])?;
            done += len;
        }
    }
    Ok(dir)
}
