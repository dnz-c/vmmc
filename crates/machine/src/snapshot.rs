//! A running machine's whole state, saved, and a machine started from it: snapshots
//! with memory, and suspend.
//!
//! - **Saving** ([`Snapshotter::save`], from any thread while [`Machine::run`] runs):
//!   every vCPU leaves the guest and parks, the guest's time stops, and the devices
//!   finish what they have in flight. The vCPUs, the partition, every device and RAM
//!   are written out, then the guest runs on as if nothing happened.
//! - **Restoring** ([`Machine::restore`], before `run`): a machine built from the same
//!   configuration takes the state back, and `run` carries on from it rather than
//!   booting.
//!
//! The state file is a magic and a format version; a header (postcard, LZ4: the
//! vCPUs, the partition, the devices, the device clock); then RAM, range by range, in
//! 1 MiB chunks: a bitmap of the chunk's pages that aren't all zeroes, then those
//! pages, LZ4-compressed. The format version goes up whenever any of it changes,
//! device states included: a state file belongs to the vmmc that wrote it.
//!
//! [`Machine::run`]: crate::Machine::run
//! [`Machine::restore`]: crate::Machine::restore

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use devices::clock::VmClock;
use devices::snapshot::Snapshot;
use hv::{Partition, PartitionState, VcpuState};
use serde::{Deserialize, Serialize};
use vm_core::{StopHandle, StopReason, TimeControl, VmVcpu};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"VMMCSTAT";
/// The state file's format. Goes up with any change to it, device states included.
pub const STATE_FORMAT: u32 = 4;
const PAGE: usize = 4096;
const CHUNK: usize = 1 << 20;
const BITMAP: usize = CHUNK / PAGE / 8;
static ZERO_PAGE: [u8; PAGE] = [0; PAGE];
/// The largest header a state file may have, decompressed.
const MAX_HEADER: u64 = 1 << 30;
/// How long a save waits for the vCPUs to leave the guest.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A device the machine saves, by the name its state is filed under.
pub(crate) type SavedDevice = (&'static str, Arc<Mutex<dyn Snapshot>>);

#[derive(Serialize, Deserialize)]
struct Header {
    cpus: u8,
    /// RAM ranges, as (guest-physical address, length).
    ram: Vec<(u64, u64)>,
    /// The devices' clock.
    clock: Duration,
    partition: PartitionState,
    vcpus: Vec<VcpuState>,
    devices: Vec<(String, Vec<u8>)>,
}

/// How the vCPU threads and a save meet.
#[derive(Default)]
pub(crate) struct Pause {
    state: Mutex<PauseState>,
    changed: Condvar,
}

#[derive(Default)]
struct PauseState {
    /// vCPU threads in the machine's run.
    running: u32,
    /// Of those, the ones parked for a pause.
    parked: u32,
    /// Bumped to ask the parked vCPUs for their state.
    generation: u64,
    /// Each vCPU's answer, with the generation it answers.
    saved: Vec<Option<(u64, std::result::Result<VcpuState, String>)>>,
}

impl Pause {
    fn lock(&self) -> std::sync::MutexGuard<'_, PauseState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A vCPU thread starts running the guest.
    pub(crate) fn enter(&self) {
        self.lock().running += 1;
        self.changed.notify_all();
    }

    /// It's done.
    pub(crate) fn leave(&self) {
        self.lock().running -= 1;
        self.changed.notify_all();
    }

    /// Holds a vCPU that a pause took out of the guest until the pause ends, saving its
    /// state when asked.
    pub(crate) fn park<P: Partition>(&self, vcpu: &VmVcpu<'_, P>, stop: &StopHandle) {
        let index = vcpu.index() as usize;
        let mut state = self.lock();
        state.parked += 1;
        self.changed.notify_all();
        let mut answered = 0;
        while stop.is_paused() && stop.reason().is_none() {
            if state.generation != answered {
                answered = state.generation;
                let saved = vcpu.save_state().map_err(|e| e.to_string());
                if state.saved.len() <= index {
                    state.saved.resize_with(index + 1, || None);
                }
                state.saved[index] = Some((answered, saved));
                self.changed.notify_all();
                continue;
            }
            // A stop doesn't notify: look again now and then.
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.parked -= 1;
        self.changed.notify_all();
    }
}

/// Saves a running machine's state, from any thread (see [`crate::Machine::snapshotter`]).
#[derive(Clone)]
pub struct Snapshotter {
    pub(crate) pause: Arc<Pause>,
    pub(crate) stop: StopHandle,
    pub(crate) time: TimeControl,
    pub(crate) clock: Arc<VmClock>,
    pub(crate) memory: GuestMemoryMmap,
    pub(crate) ram: Vec<(u64, u64)>,
    pub(crate) cpus: u8,
    pub(crate) devices: Arc<Vec<SavedDevice>>,
    /// One save at a time.
    pub(crate) saving: Arc<Mutex<()>>,
}

/// Undoes a pause, however far it got.
struct Paused<'a> {
    snapshotter: &'a Snapshotter,
    time_suspended: bool,
    quiesced: bool,
    /// Stop the machine rather than let the guest run on.
    stop: Option<StopReason>,
}

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        let s = self.snapshotter;
        // Before the vCPUs are let go, so the guest runs no further.
        if let Some(reason) = self.stop {
            s.stop.request_stop(reason);
        }
        if self.quiesced {
            for (_, device) in s.devices.iter() {
                lock(device).resume();
            }
        }
        if self.time_suspended {
            s.clock.resume();
            if let Err(e) = s.time.resume_time() {
                tracing::error!(%e, "can't resume the guest's time after a snapshot");
            }
        }
        s.stop.unpause();
        s.pause.changed.notify_all();
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn state_error(message: impl Into<String>) -> Error {
    Error::State(message.into())
}

impl Snapshotter {
    /// Pauses the guest, writes its whole state to `out`, and lets it run on, or with
    /// `then_stop` (a suspend), stops the machine for that reason without it running any
    /// further. A save that fails always lets the guest run on. Returns how long the
    /// guest was paused.
    pub fn save(&self, out: &mut dyn Write, then_stop: Option<StopReason>) -> Result<Duration> {
        self.paused(then_stop, |state| state.write(out))
    }

    /// [`Snapshotter::save`] to a file at `path`, which is whole and in place (written
    /// beside it, synced, then renamed) before the guest runs on or stops. Then, still
    /// paused, `while_paused` runs: what has to change with the guest stopped, such as
    /// its disks switching to new deltas. If that fails, the file is removed.
    pub fn save_file(
        &self,
        path: &Path,
        then_stop: Option<StopReason>,
        while_paused: impl FnOnce() -> std::result::Result<(), String>,
    ) -> Result<Duration> {
        let partial = path.with_extension("partial");
        let io = |e: io::Error| state_error(format!("can't write {}: {e}", path.display()));
        let result = self.paused(then_stop, |state| {
            let file = fs::File::create(&partial).map_err(io)?;
            let mut out = BufWriter::with_capacity(1 << 20, file);
            state.write(&mut out)?;
            let file = out.into_inner().map_err(|e| io(e.into_error()))?;
            file.sync_all().map_err(io)?;
            drop(file);
            fs::rename(&partial, path).map_err(io)?;
            while_paused().map_err(|e| {
                let _ = fs::remove_file(path);
                state_error(e)
            })
        });
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }

    /// Pauses the guest, hands its state to `work`, and lets it run on (or stops it).
    fn paused(
        &self,
        then_stop: Option<StopReason>,
        work: impl FnOnce(&Saved<'_>) -> Result<()>,
    ) -> Result<Duration> {
        let _one = lock(&self.saving);
        let started = Instant::now();
        let mut paused = Paused {
            snapshotter: self,
            time_suspended: false,
            quiesced: false,
            stop: None,
        };
        self.park_vcpus()?;
        self.time.suspend_time()?;
        self.clock.pause();
        paused.time_suspended = true;
        for (_, device) in self.devices.iter() {
            lock(device).quiesce();
        }
        paused.quiesced = true;

        let vcpus = self.vcpu_states()?;
        let header = Header {
            cpus: self.cpus,
            ram: self.ram.clone(),
            clock: devices::clock::Clock::now(&*self.clock),
            partition: self.time.save_state()?,
            vcpus,
            devices: self
                .devices
                .iter()
                .map(|(name, device)| ((*name).to_owned(), lock(device).save()))
                .collect(),
        };
        work(&Saved {
            header,
            memory: &self.memory,
        })?;
        paused.stop = then_stop;
        drop(paused);
        Ok(started.elapsed())
    }

    /// Takes every vCPU out of the guest and waits until each is parked.
    fn park_vcpus(&self) -> Result<()> {
        self.stop.pause();
        let deadline = Instant::now() + PAUSE_TIMEOUT;
        let mut state = self.pause.lock();
        loop {
            if self.stop.reason().is_some() {
                return Err(state_error("the VM stopped"));
            }
            if state.running == u32::from(self.cpus) && state.parked == state.running {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(state_error(format!(
                    "{} of {} vCPUs didn't leave the guest",
                    u32::from(self.cpus) - state.parked,
                    self.cpus
                )));
            }
            // A kick can land just before a vCPU enters the guest: kick again.
            self.stop.kick_all();
            state = self
                .pause
                .changed
                .wait_timeout(state, Duration::from_millis(5))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Asks the parked vCPUs for their state.
    fn vcpu_states(&self) -> Result<Vec<VcpuState>> {
        let mut state = self.pause.lock();
        state.generation += 1;
        let generation = state.generation;
        self.pause.changed.notify_all();
        let deadline = Instant::now() + PAUSE_TIMEOUT;
        loop {
            let answers: Option<Vec<_>> = (0..usize::from(self.cpus))
                .map(|index| match state.saved.get(index) {
                    Some(Some((answered, saved))) if *answered == generation => Some(saved.clone()),
                    _ => None,
                })
                .collect();
            if let Some(answers) = answers {
                return answers
                    .into_iter()
                    .enumerate()
                    .map(|(index, saved)| {
                        saved.map_err(|e| state_error(format!("vCPU {index}: {e}")))
                    })
                    .collect();
            }
            if Instant::now() > deadline {
                return Err(state_error("the vCPUs didn't save their state"));
            }
            state = self
                .pause
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A paused machine's state, ready to write.
struct Saved<'a> {
    header: Header,
    memory: &'a GuestMemoryMmap,
}

impl Saved<'_> {
    fn write(&self, out: &mut dyn Write) -> Result<()> {
        let io = |e: io::Error| state_error(format!("can't write the state: {e}"));
        out.write_all(MAGIC).map_err(io)?;
        out.write_all(&STATE_FORMAT.to_le_bytes()).map_err(io)?;
        let header = postcard::to_allocvec(&self.header)
            .map_err(|e| state_error(format!("can't encode the state: {e}")))?;
        out.write_all(&(header.len() as u64).to_le_bytes())
            .map_err(io)?;
        let compressed = lz4_flex::block::compress(&header);
        out.write_all(&(compressed.len() as u64).to_le_bytes())
            .map_err(io)?;
        out.write_all(&compressed).map_err(io)?;
        write_ram(self.memory, &self.header.ram, out).map_err(io)?;
        out.flush().map_err(io)
    }
}

/// A state file's contents, up to its RAM.
pub(crate) struct Restored {
    pub vcpus: Vec<VcpuState>,
}

/// Reads the state from `input` into `machine`, built from the same configuration as
/// the one saved: RAM, the devices, the clock and the partition (whose time it
/// suspends; the machine's run resumes it). Returns the vCPUs' states, for the run to
/// start them with.
pub(crate) fn restore(
    input: &mut dyn Read,
    machine: &Snapshotter,
    restore_partition: impl FnOnce(&PartitionState) -> Result<()>,
) -> Result<Restored> {
    let Snapshotter {
        time: vm_time,
        memory,
        ram,
        cpus,
        clock,
        devices,
        ..
    } = machine;
    let cpus = *cpus;
    let io = |e: io::Error| state_error(format!("can't read the state: {e}"));
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic).map_err(io)?;
    if &magic != MAGIC {
        return Err(state_error("this isn't a vmmc state file"));
    }
    let mut word = [0u8; 4];
    input.read_exact(&mut word).map_err(io)?;
    let format = u32::from_le_bytes(word);
    if format != STATE_FORMAT {
        return Err(state_error(format!(
            "it's in state format {format}, and this vmmc reads format {STATE_FORMAT}: \
             it was saved by another version of vmmc"
        )));
    }
    let mut long = [0u8; 8];
    input.read_exact(&mut long).map_err(io)?;
    let header_len = u64::from_le_bytes(long);
    input.read_exact(&mut long).map_err(io)?;
    let compressed_len = u64::from_le_bytes(long);
    if header_len > MAX_HEADER || compressed_len > MAX_HEADER {
        return Err(state_error("its header is too big"));
    }
    let mut compressed = vec![0u8; compressed_len as usize];
    input.read_exact(&mut compressed).map_err(io)?;
    let header = lz4_flex::block::decompress(&compressed, header_len as usize)
        .map_err(|e| state_error(format!("its header is damaged: {e}")))?;
    let header: Header = postcard::from_bytes(&header)
        .map_err(|e| state_error(format!("its header is damaged: {e}")))?;
    if header.cpus != cpus || header.vcpus.len() != usize::from(cpus) {
        return Err(state_error(format!(
            "it has {} vCPUs, and this VM {cpus}",
            header.cpus
        )));
    }
    if header.ram != *ram {
        return Err(state_error(
            "its RAM is laid out differently: was the VM's memory size changed?",
        ));
    }
    let saved: Vec<&str> = header
        .devices
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let here: Vec<&str> = devices.iter().map(|(name, _)| *name).collect();
    if saved != here {
        return Err(state_error(format!(
            "it has other devices ({saved:?}, not {here:?}): were the VM's settings changed?"
        )));
    }

    vm_time.suspend_time()?;
    restore_partition(&header.partition)?;
    read_ram(memory, ram, input)?;
    for ((name, state), (_, device)) in header.devices.iter().zip(devices.iter()) {
        lock(device)
            .restore(state)
            .map_err(|e| state_error(format!("{name}: {e}")))?;
    }
    clock.set(header.clock);
    Ok(Restored {
        vcpus: header.vcpus,
    })
}

/// Writes RAM, compressing chunks on several threads.
fn write_ram(memory: &GuestMemoryMmap, ram: &[(u64, u64)], out: &mut dyn Write) -> io::Result<()> {
    let threads = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let batch = threads * 8;
    for &(start, len) in ram {
        let chunks = (len as usize).div_ceil(CHUNK);
        let mut first = 0;
        while first < chunks {
            let last = (first + batch).min(chunks);
            let encoded: Vec<io::Result<Vec<u8>>> = thread::scope(|scope| {
                let workers: Vec<_> = (0..threads)
                    .map(|worker| {
                        scope.spawn(move || {
                            (first + worker..last)
                                .step_by(threads)
                                .map(|index| {
                                    let offset = (index * CHUNK) as u64;
                                    let size = (len - offset).min(CHUNK as u64) as usize;
                                    (index, encode_chunk(memory, start + offset, size))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                let mut all: Vec<_> = workers
                    .into_iter()
                    .flat_map(|worker| worker.join().expect("a compression thread panicked"))
                    .collect();
                all.sort_by_key(|(index, _)| *index);
                all.into_iter().map(|(_, encoded)| encoded).collect()
            });
            for chunk in encoded {
                out.write_all(&chunk?)?;
            }
            first = last;
        }
    }
    Ok(())
}

/// One chunk: the bitmap of its pages that aren't zero, then, if there are any, their
/// compressed length and them, compressed.
fn encode_chunk(memory: &GuestMemoryMmap, address: u64, size: usize) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; size];
    memory
        .read_slice(&mut data, GuestAddress(address))
        .map_err(io::Error::other)?;
    let mut bitmap = [0u8; BITMAP];
    let mut pages = Vec::new();
    for (index, page) in data.chunks(PAGE).enumerate() {
        if page != &ZERO_PAGE[..page.len()] {
            bitmap[index / 8] |= 1 << (index % 8);
            pages.extend_from_slice(page);
        }
    }
    let mut encoded = bitmap.to_vec();
    if !pages.is_empty() {
        let compressed = lz4_flex::block::compress(&pages);
        encoded.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&compressed);
    }
    Ok(encoded)
}

fn read_ram(memory: &GuestMemoryMmap, ram: &[(u64, u64)], input: &mut dyn Read) -> Result<()> {
    let io = |e: io::Error| state_error(format!("can't read the saved RAM: {e}"));
    for &(start, len) in ram {
        let mut offset = 0u64;
        while offset < len {
            let size = (len - offset).min(CHUNK as u64) as usize;
            let mut bitmap = [0u8; BITMAP];
            input.read_exact(&mut bitmap).map_err(io)?;
            let present: Vec<usize> = (0..size / PAGE)
                .filter(|index| bitmap[index / 8] & (1 << (index % 8)) != 0)
                .collect();
            if !present.is_empty() {
                let mut word = [0u8; 4];
                input.read_exact(&mut word).map_err(io)?;
                let compressed_len = u32::from_le_bytes(word) as usize;
                if compressed_len > 2 * CHUNK {
                    return Err(state_error("the saved RAM is damaged"));
                }
                let mut compressed = vec![0u8; compressed_len];
                input.read_exact(&mut compressed).map_err(io)?;
                let pages = lz4_flex::block::decompress(&compressed, present.len() * PAGE)
                    .map_err(|e| state_error(format!("the saved RAM is damaged: {e}")))?;
                if pages.len() != present.len() * PAGE {
                    return Err(state_error("the saved RAM is damaged"));
                }
                for (page, index) in pages.chunks(PAGE).zip(present) {
                    let address = start + offset + (index * PAGE) as u64;
                    memory.write_slice(page, GuestAddress(address))?;
                }
            }
            offset += size as u64;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_survives_the_round_trip_and_zero_pages_take_no_room() {
        let ram = [
            (0u64, 3 * CHUNK as u64 + 8 * PAGE as u64),
            (0x1_0000_0000, CHUNK as u64),
        ];
        let ranges: Vec<_> = ram
            .iter()
            .map(|&(start, len)| (GuestAddress(start), len as usize))
            .collect();
        let memory = GuestMemoryMmap::from_ranges(&ranges).unwrap();
        // A few pages with something in them; everything else stays zero.
        for (address, byte) in [
            (0x10u64, 1u8),
            (CHUNK as u64 + 5 * PAGE as u64, 2),
            (3 * CHUNK as u64 + 7 * PAGE as u64 + 4095, 3),
            (0x1_0000_0000 + 17, 4),
        ] {
            memory.write_slice(&[byte], GuestAddress(address)).unwrap();
        }
        let mut out = Vec::new();
        write_ram(&memory, &ram, &mut out).unwrap();
        // Bitmaps for every chunk, plus four compressed pages.
        assert!(out.len() < 6 * BITMAP + 4 * 200, "{} bytes", out.len());

        let restored = GuestMemoryMmap::from_ranges(&ranges).unwrap();
        read_ram(&restored, &ram, &mut out.as_slice()).unwrap();
        for &(start, len) in &ram {
            let mut a = vec![0u8; len as usize];
            let mut b = vec![0u8; len as usize];
            memory.read_slice(&mut a, GuestAddress(start)).unwrap();
            restored.read_slice(&mut b, GuestAddress(start)).unwrap();
            assert!(a == b, "RAM at {start:#x} differs");
        }
    }

    #[test]
    fn damaged_ram_is_refused() {
        let ram = [(0u64, CHUNK as u64)];
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), CHUNK)]).unwrap();
        memory.write_slice(&[7], GuestAddress(0)).unwrap();
        let mut out = Vec::new();
        write_ram(&memory, &ram, &mut out).unwrap();
        out.truncate(out.len() - 3);
        assert!(read_ram(&memory, &ram, &mut out.as_slice()).is_err());
    }
}
