//! virtio-gpu, 2D: the guest draws into resources backed by its own memory and copies
//! them to the host's copies (TRANSFER_TO_HOST_2D); the host shows the resource set as
//! the scanout. Linux drives it (virtio_gpu), and so does OVMF (VirtioGpuDxe) for the
//! firmware's screen.
//!
//! A size the host asks for (its window's) becomes the scanout's preferred mode, in the
//! display info and the EDID, with a display event: Linux's driver marks its connector
//! for desktops to follow such a change (`hotplug_mode_update`), and GNOME does, so the
//! guest's screen follows the window.
//!
//! Commands run as the driver queues them, on the notifying vCPU. Every rectangle, size
//! and offset comes from the guest and is checked before any copy; resources' host
//! copies are capped in all ([`MAX_HOST_BYTES`]).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use virtio_queue::{DescriptorChain, Queue, QueueState, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::{
    QueueInterrupt, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_RING_INDIRECT_DESC, VIRTIO_F_VERSION_1,
    VirtioDevice,
};
use crate::display::edid::{EDID_SIZE, edid};
use crate::display::{Capture, Cursor, Frame, preferred};
use crate::snapshot;

const QUEUE_SIZES: [u16; 2] = [256, 16];
const CONTROL_QUEUE: usize = 0;
const CURSOR_QUEUE: usize = 1;
const VIRTIO_GPU_F_EDID: u64 = 1 << 1;
const EVENT_DISPLAY: u32 = 1;
const FLAG_FENCE: u32 = 1;
/// How many scanouts the display info lists (only the first is used).
const SCANOUTS: usize = 16;
/// What resources' host copies may take in all.
const MAX_HOST_BYTES: u64 = 512 << 20;
/// The largest resource, each way.
const MAX_SIDE: u32 = 16384;
/// Memory pieces one resource's backing may have.
const MAX_BACKING_ENTRIES: u32 = 1 << 16;
/// The cursor's side, in pixels, as virtio-gpu has it.
const CURSOR_SIDE: u32 = 64;

// Commands.
const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;
const GET_EDID: u32 = 0x010a;
const UPDATE_CURSOR: u32 = 0x0300;
const MOVE_CURSOR: u32 = 0x0301;
// Responses.
const OK_NODATA: u32 = 0x1100;
const OK_DISPLAY_INFO: u32 = 0x1101;
const OK_EDID: u32 = 0x1104;
const ERR_UNSPEC: u32 = 0x1200;
const ERR_OUT_OF_MEMORY: u32 = 0x1201;
const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const ERR_INVALID_PARAMETER: u32 = 0x1205;

/// Where red, green and blue are in a pixel of each 32-bit format the device takes.
fn channels(format: u32) -> Option<[usize; 3]> {
    Some(match format {
        // B8G8R8A8, B8G8R8X8: blue first in memory.
        1 | 2 => [2, 1, 0],
        // A8R8G8B8, X8R8G8B8.
        3 | 4 => [1, 2, 3],
        // R8G8B8A8, R8G8B8X8.
        67 | 134 => [0, 1, 2],
        // X8B8G8R8, A8B8G8R8.
        68 | 121 => [3, 2, 1],
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    /// The rectangle a request starts with.
    fn new(words: &[u32]) -> Self {
        Self {
            x: words[0],
            y: words[1],
            width: words[2],
            height: words[3],
        }
    }

    /// Whether it lies inside a `width` x `height` resource.
    fn inside(&self, width: u32, height: u32) -> bool {
        self.x
            .checked_add(self.width)
            .is_some_and(|right| right <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|bottom| bottom <= height)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Resource {
    format: u32,
    width: u32,
    height: u32,
    /// The guest memory backing it: (address, length) pieces, in order.
    backing: Vec<(u64, u32)>,
    /// The host's copy, 4 bytes a pixel; read again from the backing after a restore.
    #[serde(skip)]
    pixels: Vec<u8>,
}

impl Resource {
    fn bytes(width: u32, height: u32) -> u64 {
        u64::from(width) * u64::from(height) * 4
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct CursorState {
    resource: u32,
    hotspot: (u32, u32),
    /// Images given so far.
    count: u32,
}

/// What the device saves (resources without their pixels).
#[derive(Serialize, Deserialize)]
struct Saved<'a> {
    resources: Cow<'a, BTreeMap<u32, Resource>>,
    scanout: Option<(u32, Rect)>,
    cursor: CursorState,
    preferred: (u32, u32),
    events: u32,
}

struct State {
    resources: BTreeMap<u32, Resource>,
    host_bytes: u64,
    /// The scanout's resource and the part of it shown.
    scanout: Option<(u32, Rect)>,
    cursor: CursorState,
    /// The mode the display info and EDID prefer: the host's size.
    preferred: (u32, u32),
    /// Pending events (`events_read`).
    events: u32,
    /// Goes up whenever the picture may have changed.
    generation: u64,
    interrupt: Option<Arc<dyn QueueInterrupt>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads `buf.len()` bytes at `offset` within a resource's backing.
fn read_backing(
    memory: &GuestMemoryMmap,
    backing: &[(u64, u32)],
    mut offset: u64,
    buf: &mut [u8],
) -> bool {
    let mut done = 0;
    for &(address, length) in backing {
        let length = u64::from(length);
        if offset >= length {
            offset -= length;
            continue;
        }
        let now = ((length - offset) as usize).min(buf.len() - done);
        let Some(at) = address.checked_add(offset) else {
            return false;
        };
        if memory
            .read_slice(&mut buf[done..done + now], GuestAddress(at))
            .is_err()
        {
            return false;
        }
        done += now;
        offset = 0;
        if done == buf.len() {
            return true;
        }
    }
    done == buf.len()
}

/// Reads what the scanout shows and asks for sizes, from any thread.
#[derive(Clone)]
pub struct Display {
    state: Arc<Mutex<State>>,
}

impl Display {
    pub fn same_adapter(&self, other: &Display) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Asks the guest for a `width` x `height` screen: it becomes the preferred mode,
    /// with a display event. Returns the size asked for, within what's offered.
    pub fn request_size(&self, width: u32, height: u32) -> (u32, u32) {
        let size = preferred((width, height));
        let mut state = lock(&self.state);
        if state.preferred != size {
            state.preferred = size;
            state.events |= EVENT_DISPLAY;
            if let Some(interrupt) = &state.interrupt {
                interrupt.signal_config();
            }
        }
        size
    }

    /// The cursor's state; its image only when it changed since `last_count`.
    pub fn cursor(&self, last_count: u32) -> (u32, bool, Option<Cursor>) {
        let state = lock(&self.state);
        let cursor = state.cursor;
        let resource = state.resources.get(&cursor.resource);
        let shown = resource.is_some();
        if cursor.count == last_count || cursor.count == 0 {
            return (cursor.count, shown, None);
        }
        let image = resource.and_then(|resource| {
            let order = channels(resource.format)?;
            // The alpha is the byte that isn't a colour.
            let alpha = (0..4).find(|i| !order.contains(i)).unwrap_or(3);
            let (width, height) = (
                resource.width.min(CURSOR_SIDE),
                resource.height.min(CURSOR_SIDE),
            );
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for y in 0..height {
                let start = (y * resource.width * 4) as usize;
                let row = &resource.pixels[start..start + (width * 4) as usize];
                for pixel in row.as_chunks::<4>().0 {
                    // DRM's planes blend premultiplied colours; the window wants
                    // straight ones.
                    let a = pixel[alpha];
                    let straight = |c: u8| {
                        (u32::from(c) * 255)
                            .checked_div(u32::from(a))
                            .unwrap_or(0)
                            .min(255) as u8
                    };
                    rgba.extend_from_slice(&[
                        straight(pixel[order[0]]),
                        straight(pixel[order[1]]),
                        straight(pixel[order[2]]),
                        a,
                    ]);
                }
            }
            Some(Cursor {
                count: cursor.count,
                shown,
                width,
                height,
                hotspot: cursor.hotspot,
                rgba,
            })
        });
        (cursor.count, shown, image)
    }

    /// The picture now, or `None` while nothing is shown.
    pub fn snapshot(&self) -> Option<Frame> {
        self.capture(&mut Capture::default())
    }

    /// The picture, if the guest flushed or switched it since `last` saw it; `None`
    /// when it didn't, or while nothing is shown.
    pub fn capture(&self, last: &mut Capture) -> Option<Frame> {
        // The pixels are copied with the device locked and converted after, so the
        // guest's commands wait only for the copy.
        let mut raw = std::mem::take(&mut last.spare);
        let (rect, order, seen) = {
            let state = lock(&self.state);
            let (id, rect) = state.scanout?;
            let seen = (Arc::as_ptr(&self.state) as usize, state.generation);
            if last.generation == Some(seen) {
                last.spare = raw;
                return None;
            }
            let resource = state.resources.get(&id)?;
            let order = channels(resource.format)?;
            if rect.width == 0 || rect.height == 0 || !rect.inside(resource.width, resource.height)
            {
                return None;
            }
            let row = (rect.width * 4) as usize;
            raw.clear();
            for y in rect.y..rect.y + rect.height {
                let start = ((y * resource.width + rect.x) * 4) as usize;
                raw.extend_from_slice(&resource.pixels[start..start + row]);
            }
            (rect, order, seen)
        };
        let mut rgb = vec![0u8; (rect.width * rect.height * 3) as usize];
        for (out, pixel) in rgb
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(raw.as_chunks::<4>().0)
        {
            *out = [pixel[order[0]], pixel[order[1]], pixel[order[2]]];
        }
        last.generation = Some(seen);
        last.spare = raw;
        Some(Frame {
            width: rect.width,
            height: rect.height,
            rgb,
        })
    }
}

struct Running {
    memory: GuestMemoryMmap,
    queues: Vec<Queue>,
}

pub struct VirtioGpu {
    state: Arc<Mutex<State>>,
    running: Option<Running>,
}

impl VirtioGpu {
    /// A device whose display prefers `resolution`.
    pub fn new(resolution: (u32, u32)) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                resources: BTreeMap::new(),
                host_bytes: 0,
                scanout: None,
                cursor: CursorState::default(),
                preferred: preferred(resolution),
                events: 0,
                generation: 0,
                interrupt: None,
            })),
            running: None,
        }
    }

    pub fn display(&self) -> Display {
        Display {
            state: self.state.clone(),
        }
    }

    /// Serves what the driver queued on `index`.
    fn process(&mut self, index: usize) {
        let Some(running) = &mut self.running else {
            return;
        };
        let memory = &running.memory;
        // OVMF enables only the control queue.
        let Some(queue) = running.queues.get_mut(index).filter(|queue| queue.ready()) else {
            return;
        };
        let served = (|| -> Result<bool, virtio_queue::Error> {
            loop {
                queue.disable_notification(memory)?;
                while let Some(chain) = queue.pop_descriptor_chain(memory) {
                    let head = chain.head_index();
                    let used = handle(&self.state, memory, chain);
                    queue.add_used(memory, head, used)?;
                }
                if !queue.enable_notification(memory)? {
                    return queue.needs_notification(memory);
                }
            }
        })();
        match served {
            Ok(false) => {}
            Ok(true) => {
                if let Some(interrupt) = &lock(&self.state).interrupt {
                    interrupt.signal_queue(index as u16);
                }
            }
            Err(e) => tracing::warn!(%e, "virtio-gpu: a broken queue"),
        }
    }
}

/// Runs one request; returns the bytes written into the driver's buffers.
fn handle(
    state: &Mutex<State>,
    memory: &GuestMemoryMmap,
    chain: DescriptorChain<&GuestMemoryMmap>,
) -> u32 {
    let Ok(mut reader) = chain.clone().reader(memory) else {
        return 0;
    };
    let Ok(mut writer) = chain.writer(memory) else {
        return 0;
    };
    let mut header = [0u8; 24];
    if reader.read_exact(&mut header).is_err() {
        return 0;
    }
    let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let flags = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let (response, body) = execute(&mut lock(state), memory, kind, &mut reader);
    if writer.available_bytes() == 0 {
        // The cursor queue's requests take no answer.
        return 0;
    }
    // The answer's header: its type, and the request's fence echoed.
    let mut answer = header;
    answer[0..4].copy_from_slice(&response.to_le_bytes());
    answer[4..8].copy_from_slice(&(flags & FLAG_FENCE).to_le_bytes());
    if flags & FLAG_FENCE == 0 {
        answer[8..16].fill(0);
    }
    let mut out = answer.to_vec();
    out.extend_from_slice(&body);
    let len = out.len().min(writer.available_bytes());
    if writer.write_all(&out[..len]).is_err() {
        return 0;
    }
    len as u32
}

/// Reads `N` little-endian u32s of a request's body.
fn words<const N: usize>(reader: &mut impl Read) -> Option<[u32; N]> {
    let mut bytes = vec![0u8; N * 4];
    reader.read_exact(&mut bytes).ok()?;
    let mut out = [0u32; N];
    for (word, chunk) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    Some(out)
}

/// Runs command `kind`: its response type, and the body after the header.
fn execute(
    state: &mut State,
    memory: &GuestMemoryMmap,
    kind: u32,
    reader: &mut impl Read,
) -> (u32, Vec<u8>) {
    let ok = (OK_NODATA, Vec::new());
    let error = |code| (code, Vec::new());
    match kind {
        GET_DISPLAY_INFO => {
            let mut body = vec![0u8; SCANOUTS * 24];
            let (width, height) = state.preferred;
            body[8..12].copy_from_slice(&width.to_le_bytes());
            body[12..16].copy_from_slice(&height.to_le_bytes());
            body[16..20].copy_from_slice(&1u32.to_le_bytes());
            (OK_DISPLAY_INFO, body)
        }
        GET_EDID => {
            let Some([scanout, _]) = words::<2>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            if scanout != 0 {
                return error(ERR_INVALID_SCANOUT_ID);
            }
            let mut body = vec![0u8; 8 + 1024];
            body[0..4].copy_from_slice(&(EDID_SIZE as u32).to_le_bytes());
            let (width, height) = state.preferred;
            body[8..8 + EDID_SIZE].copy_from_slice(&edid(width, height));
            (OK_EDID, body)
        }
        RESOURCE_CREATE_2D => {
            let Some([id, format, width, height]) = words::<4>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            if id == 0 || state.resources.contains_key(&id) {
                return error(ERR_INVALID_RESOURCE_ID);
            }
            if channels(format).is_none()
                || width == 0
                || height == 0
                || width > MAX_SIDE
                || height > MAX_SIDE
            {
                return error(ERR_INVALID_PARAMETER);
            }
            let bytes = Resource::bytes(width, height);
            if state.host_bytes + bytes > MAX_HOST_BYTES {
                return error(ERR_OUT_OF_MEMORY);
            }
            state.host_bytes += bytes;
            state.resources.insert(
                id,
                Resource {
                    format,
                    width,
                    height,
                    backing: Vec::new(),
                    pixels: vec![0; bytes as usize],
                },
            );
            ok
        }
        RESOURCE_UNREF => {
            let Some([id, _]) = words::<2>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            let Some(resource) = state.resources.remove(&id) else {
                return error(ERR_INVALID_RESOURCE_ID);
            };
            state.host_bytes -= Resource::bytes(resource.width, resource.height);
            if state.scanout.is_some_and(|(scanout, _)| scanout == id) {
                state.scanout = None;
                state.generation += 1;
            }
            ok
        }
        SET_SCANOUT => {
            let Some(words) = words::<6>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            let rect = Rect::new(&words);
            let (scanout, id) = (words[4], words[5]);
            if scanout != 0 {
                return error(ERR_INVALID_SCANOUT_ID);
            }
            if id == 0 {
                state.scanout = None;
            } else {
                let Some(resource) = state.resources.get(&id) else {
                    return error(ERR_INVALID_RESOURCE_ID);
                };
                if !rect.inside(resource.width, resource.height) {
                    return error(ERR_INVALID_PARAMETER);
                }
                state.scanout = Some((id, rect));
            }
            state.generation += 1;
            ok
        }
        RESOURCE_FLUSH => {
            let Some(words) = words::<6>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            let id = words[4];
            if !state.resources.contains_key(&id) {
                return error(ERR_INVALID_RESOURCE_ID);
            }
            if state.scanout.is_some_and(|(scanout, _)| scanout == id) {
                state.generation += 1;
            }
            ok
        }
        TRANSFER_TO_HOST_2D => {
            let Some(words) = words::<8>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            let rect = Rect::new(&words);
            let offset = u64::from(words[4]) | u64::from(words[5]) << 32;
            let id = words[6];
            let Some(resource) = state.resources.get_mut(&id) else {
                return error(ERR_INVALID_RESOURCE_ID);
            };
            if !rect.inside(resource.width, resource.height) || resource.backing.is_empty() {
                return error(ERR_INVALID_PARAMETER);
            }
            let stride = u64::from(resource.width) * 4;
            let row = (rect.width * 4) as usize;
            for y in 0..rect.height {
                let at = ((u64::from(rect.y + y) * u64::from(resource.width) + u64::from(rect.x))
                    * 4) as usize;
                let Some(source) = offset.checked_add(u64::from(y) * stride) else {
                    return error(ERR_INVALID_PARAMETER);
                };
                if !read_backing(
                    memory,
                    &resource.backing,
                    source,
                    &mut resource.pixels[at..at + row],
                ) {
                    return error(ERR_INVALID_PARAMETER);
                }
            }
            ok
        }
        RESOURCE_ATTACH_BACKING => {
            let Some([id, entries]) = words::<2>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            if entries > MAX_BACKING_ENTRIES {
                return error(ERR_INVALID_PARAMETER);
            }
            let mut raw = vec![0u8; entries as usize * 16];
            if reader.read_exact(&mut raw).is_err() {
                return error(ERR_INVALID_PARAMETER);
            }
            let backing: Vec<(u64, u32)> = raw
                .as_chunks::<16>()
                .0
                .iter()
                .map(|entry| {
                    (
                        u64::from_le_bytes(entry[0..8].try_into().unwrap()),
                        u32::from_le_bytes(entry[8..12].try_into().unwrap()),
                    )
                })
                .collect();
            let Some(resource) = state.resources.get_mut(&id) else {
                return error(ERR_INVALID_RESOURCE_ID);
            };
            resource.backing = backing;
            ok
        }
        RESOURCE_DETACH_BACKING => {
            let Some([id, _]) = words::<2>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            let Some(resource) = state.resources.get_mut(&id) else {
                return error(ERR_INVALID_RESOURCE_ID);
            };
            resource.backing.clear();
            ok
        }
        UPDATE_CURSOR | MOVE_CURSOR => {
            // The position (scanout, x, y, padding), then the resource and hot spot.
            let Some(words) = words::<8>(reader) else {
                return error(ERR_INVALID_PARAMETER);
            };
            if kind == UPDATE_CURSOR {
                state.cursor.resource = words[4];
                state.cursor.hotspot = (words[5], words[6]);
                state.cursor.count = state.cursor.count.wrapping_add(1).max(1);
            }
            ok
        }
        other => {
            tracing::debug!(other, "virtio-gpu: an unsupported command");
            error(ERR_UNSPEC)
        }
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_type(&self) -> u16 {
        virtio_bindings::virtio_ids::VIRTIO_ID_GPU as u16
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
            | VIRTIO_F_RING_INDIRECT_DESC
            | VIRTIO_F_RING_EVENT_IDX
            | VIRTIO_GPU_F_EDID
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // events_read, events_clear, num_scanouts, num_capsets.
        let mut config = [0u8; 16];
        config[0..4].copy_from_slice(&lock(&self.state).events.to_le_bytes());
        config[8..12].copy_from_slice(&1u32.to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if offset == 4 && data.len() == 4 {
            let clear = u32::from_le_bytes(data.try_into().unwrap());
            lock(&self.state).events &= !clear;
        }
    }

    fn activate(
        &mut self,
        memory: GuestMemoryMmap,
        _features: u64,
        queues: Vec<Queue>,
        interrupt: Arc<dyn QueueInterrupt>,
    ) -> Result<(), String> {
        if queues.len() != QUEUE_SIZES.len() {
            return Err("virtio-gpu needs its control and cursor queues".into());
        }
        {
            let mut state = lock(&self.state);
            state.interrupt = Some(interrupt);
            // After a restore: the host's copies, from the backing, which holds what
            // the guest drew last (a fresh start has no resources).
            for resource in state.resources.values_mut() {
                read_backing(&memory, &resource.backing, 0, &mut resource.pixels);
            }
            state.generation += 1;
        }
        self.running = Some(Running { memory, queues });
        // Requests made before a snapshot that saved these queues are served now.
        self.process(CONTROL_QUEUE);
        self.process(CURSOR_QUEUE);
        Ok(())
    }

    fn queue_notify(&mut self, queue: u16) {
        self.process(usize::from(queue));
    }

    fn reset(&mut self) {
        self.running = None;
        let mut state = lock(&self.state);
        state.resources.clear();
        state.host_bytes = 0;
        state.scanout = None;
        state.cursor = CursorState::default();
        state.events = 0;
        state.generation += 1;
        state.interrupt = None;
    }

    fn queue_states(&self) -> Vec<QueueState> {
        self.running
            .iter()
            .flat_map(|running| running.queues.iter().map(Queue::state))
            .collect()
    }

    fn save_device(&self) -> Vec<u8> {
        let state = lock(&self.state);
        snapshot::encode(&Saved {
            resources: Cow::Borrowed(&state.resources),
            scanout: state.scanout,
            cursor: state.cursor,
            preferred: state.preferred,
            events: state.events,
        })
    }

    fn restore_device(&mut self, saved: &[u8]) -> Result<(), String> {
        let saved: Saved = snapshot::decode(saved).map_err(|e| e.0)?;
        let mut resources = saved.resources.into_owned();
        let mut host_bytes = 0;
        for resource in resources.values_mut() {
            if channels(resource.format).is_none()
                || !(1..=MAX_SIDE).contains(&resource.width)
                || !(1..=MAX_SIDE).contains(&resource.height)
            {
                return Err("virtio-gpu: a saved resource doesn't fit".into());
            }
            host_bytes += Resource::bytes(resource.width, resource.height);
            if host_bytes > MAX_HOST_BYTES {
                return Err("virtio-gpu: the saved resources are too big".into());
            }
            // Its pixels come from the backing when the device starts.
            resource.pixels = vec![0; Resource::bytes(resource.width, resource.height) as usize];
        }
        if let Some((id, rect)) = saved.scanout
            && !resources
                .get(&id)
                .is_some_and(|resource| rect.inside(resource.width, resource.height))
        {
            return Err("virtio-gpu: the saved scanout doesn't fit".into());
        }
        let mut state = lock(&self.state);
        state.resources = resources;
        state.host_bytes = host_bytes;
        state.scanout = saved.scanout;
        state.cursor = saved.cursor;
        state.preferred = preferred(saved.preferred);
        state.events = saved.events;
        state.generation += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use virtio_bindings::virtio_ring::{VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;

    use super::*;
    use crate::display::edid;

    const REQUEST: u64 = 0x10_0000;
    const RESPONSE: u64 = 0x20_0000;
    /// Where the guest draws: two pieces, apart.
    const BACKING: [u64; 2] = [0x30_0000, 0x38_0000];

    fn desc(addr: u64, len: u32, flags: u32, next: u16) -> RawDescriptor {
        RawDescriptor::from(Descriptor::new(addr, len, flags as u16, next))
    }

    #[derive(Default)]
    struct Interrupts {
        config: AtomicU32,
    }

    impl QueueInterrupt for Interrupts {
        fn signal_queue(&self, _queue: u16) {}

        fn signal_config(&self) {
            self.config.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Guest {
        memory: GuestMemoryMmap,
        gpu: VirtioGpu,
        interrupts: Arc<Interrupts>,
    }

    impl Guest {
        fn new() -> Self {
            let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap();
            let mut gpu = VirtioGpu::new((1280, 800));
            let interrupts = Arc::new(Interrupts::default());
            let queues = (0..2)
                .map(|i| {
                    MockSplitQueue::create(&memory, GuestAddress(0x1000 * i), 16)
                        .create_queue()
                        .unwrap()
                })
                .collect();
            gpu.activate(memory.clone(), 0, queues, interrupts.clone())
                .unwrap();
            Self {
                memory,
                gpu,
                interrupts,
            }
        }

        /// Sends `kind` with `body` (and `extra` after it, in its own descriptor) and a
        /// fence; returns the response's type and body, after checking its header.
        fn send(&self, kind: u32, body: &[u32], extra: &[u8]) -> (u32, Vec<u8>) {
            let mut request = Vec::new();
            for word in [kind, FLAG_FENCE, 77, 0, 0, 0].iter().chain(body) {
                request.extend_from_slice(&word.to_le_bytes());
            }
            self.memory
                .write_slice(&request, GuestAddress(REQUEST))
                .unwrap();
            self.memory
                .write_slice(extra, GuestAddress(REQUEST + 0x1000))
                .unwrap();
            let mut pieces = vec![(REQUEST, request.len() as u32, 0)];
            if !extra.is_empty() {
                pieces.push((REQUEST + 0x1000, extra.len() as u32, 0));
            }
            pieces.push((RESPONSE, 2048, VRING_DESC_F_WRITE));
            let last = pieces.len() - 1;
            let descs: Vec<RawDescriptor> = pieces
                .into_iter()
                .enumerate()
                .map(|(i, (address, len, flags))| match i < last {
                    true => desc(address, len, flags | VRING_DESC_F_NEXT, i as u16 + 1),
                    false => desc(address, len, flags, 0),
                })
                .collect();
            let mock = MockSplitQueue::create(&self.memory, GuestAddress(0x8000), 16);
            mock.add_desc_chains(&descs, 0).unwrap();
            let mut queue: Queue = mock.create_queue().unwrap();
            let chain = queue.pop_descriptor_chain(&self.memory).unwrap();
            let used = handle(&self.gpu.state, &self.memory, chain) as usize;
            let mut response = vec![0u8; used];
            self.memory
                .read_slice(&mut response, GuestAddress(RESPONSE))
                .unwrap();
            let word =
                |i: usize| u32::from_le_bytes(response[i * 4..i * 4 + 4].try_into().unwrap());
            assert_eq!((word(1), word(2)), (FLAG_FENCE, 77), "the fence, echoed");
            (word(0), response[24..].to_vec())
        }

        fn ok(&self, kind: u32, body: &[u32]) {
            assert_eq!(self.send(kind, body, &[]).0, OK_NODATA, "command {kind:#x}");
        }

        /// A `width` x `height` resource `id` backed by the two pieces of [`BACKING`].
        fn resource(&self, id: u32, format: u32, width: u32, height: u32) {
            self.ok(RESOURCE_CREATE_2D, &[id, format, width, height]);
            let half = width * height * 2;
            let mut entries = Vec::new();
            for address in BACKING {
                entries.extend_from_slice(&address.to_le_bytes());
                entries.extend_from_slice(&half.to_le_bytes());
                entries.extend_from_slice(&0u32.to_le_bytes());
            }
            let (kind, _) = self.send(RESOURCE_ATTACH_BACKING, &[id, 2], &entries);
            assert_eq!(kind, OK_NODATA);
        }

        /// Writes pixel `index` of what the guest drew into a resource's backing.
        fn draw(&self, index: u32, half: u32, pixel: [u8; 4]) {
            let (piece, at) = (index * 4 / half, index * 4 % half);
            self.memory
                .write_slice(
                    &pixel,
                    GuestAddress(BACKING[piece as usize] + u64::from(at)),
                )
                .unwrap();
        }
    }

    #[test]
    fn the_hosts_size_becomes_the_preferred_mode_with_an_event() {
        let mut guest = Guest::new();
        let (kind, info) = guest.send(GET_DISPLAY_INFO, &[], &[]);
        assert_eq!(kind, OK_DISPLAY_INFO);
        let word = |bytes: &[u8], i: usize| {
            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        };
        assert_eq!(
            (word(&info, 2), word(&info, 3), word(&info, 4)),
            (1280, 800, 1)
        );
        assert_eq!(word(&info, 6 + 4), 0, "one scanout enabled");

        let display = guest.gpu.display();
        assert_eq!(display.request_size(1500, 844), (1500, 844));
        assert_eq!(guest.interrupts.config.load(Ordering::SeqCst), 1);
        let mut events = [0u8; 4];
        guest.gpu.read_config(0, &mut events);
        assert_eq!(u32::from_le_bytes(events), EVENT_DISPLAY);
        let (_, info) = guest.send(GET_DISPLAY_INFO, &[], &[]);
        assert_eq!((word(&info, 2), word(&info, 3)), (1500, 844));
        let (kind, answer) = guest.send(GET_EDID, &[0, 0], &[]);
        assert_eq!(kind, OK_EDID);
        assert_eq!(word(&answer, 0), EDID_SIZE as u32);
        assert_eq!(&answer[8..8 + EDID_SIZE], &edid::edid(1500, 844)[..]);
        assert_eq!(guest.send(GET_EDID, &[1, 0], &[]).0, ERR_INVALID_SCANOUT_ID);

        guest.gpu.write_config(4, &EVENT_DISPLAY.to_le_bytes());
        guest.gpu.read_config(0, &mut events);
        assert_eq!(u32::from_le_bytes(events), 0, "the driver cleared it");
        // The same size again isn't news; sizes stay within what's offered.
        display.request_size(1500, 844);
        assert_eq!(guest.interrupts.config.load(Ordering::SeqCst), 1);
        assert_eq!(
            display.request_size(100, 100),
            crate::display::MIN_PREFERRED
        );
    }

    #[test]
    fn a_flushed_resource_is_the_picture() {
        let guest = Guest::new();
        let display = guest.gpu.display();
        let mut last = Capture::default();
        assert!(display.capture(&mut last).is_none(), "nothing shown yet");
        // 4x2, B8G8R8X8, in two pieces of 16 bytes: pixel 5 (x 1, y 1) is in the second.
        guest.resource(1, 2, 4, 2);
        guest.draw(5, 16, [0x33, 0x22, 0x11, 0]);
        guest.ok(SET_SCANOUT, &[0, 0, 4, 2, 0, 1]);
        guest.ok(TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]);
        guest.ok(RESOURCE_FLUSH, &[0, 0, 4, 2, 1, 0]);
        let frame = display.capture(&mut last).expect("the first picture");
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(&frame.rgb[5 * 3..6 * 3], &[0x11, 0x22, 0x33]);
        assert!(display.capture(&mut last).is_none(), "unchanged");

        // A transfer of part of it: the rectangle's rows, from its offset on.
        guest.draw(6, 16, [0xff, 0, 0, 0]);
        guest.ok(TRANSFER_TO_HOST_2D, &[2, 1, 1, 1, 6 * 4, 0, 1, 0]);
        assert!(display.capture(&mut last).is_none(), "not flushed yet");
        guest.ok(RESOURCE_FLUSH, &[2, 1, 1, 1, 1, 0]);
        let frame = display.capture(&mut last).expect("flushed");
        assert_eq!(&frame.rgb[6 * 3..7 * 3], &[0, 0, 0xff]);

        // Part of a resource as the scanout.
        guest.ok(SET_SCANOUT, &[1, 1, 2, 1, 0, 1]);
        let frame = display.capture(&mut last).expect("a new scanout");
        assert_eq!(frame.rgb, [0x11, 0x22, 0x33, 0, 0, 0xff]);
        // Gone with its resource.
        guest.ok(RESOURCE_UNREF, &[1, 0]);
        assert_eq!(display.snapshot(), None);
    }

    #[test]
    fn requests_out_of_bounds_fail() {
        let guest = Guest::new();
        guest.resource(1, 2, 4, 2);
        let error = |kind, body: &[u32]| guest.send(kind, body, &[]).0;
        assert_eq!(
            error(RESOURCE_CREATE_2D, &[1, 2, 4, 2]),
            ERR_INVALID_RESOURCE_ID,
            "taken"
        );
        assert_eq!(
            error(RESOURCE_CREATE_2D, &[0, 2, 4, 2]),
            ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            error(RESOURCE_CREATE_2D, &[2, 99, 4, 2]),
            ERR_INVALID_PARAMETER,
            "format"
        );
        assert_eq!(
            error(RESOURCE_CREATE_2D, &[2, 2, MAX_SIDE + 1, 1]),
            ERR_INVALID_PARAMETER
        );
        assert_eq!(
            error(RESOURCE_CREATE_2D, &[2, 2, MAX_SIDE, MAX_SIDE]),
            ERR_OUT_OF_MEMORY,
            "1 GiB is more than the host keeps"
        );
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[3, 0, 2, 1, 0, 0, 1, 0]),
            ERR_INVALID_PARAMETER
        );
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[u32::MAX, 0, 2, 1, 0, 0, 1, 0]),
            ERR_INVALID_PARAMETER,
            "wraps around"
        );
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[0, 0, 1, 1, 0, 0, 7, 0]),
            ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[0, 1, 4, 1, 32, 0, 1, 0]),
            ERR_INVALID_PARAMETER,
            "past the end of the backing"
        );
        assert_eq!(
            error(SET_SCANOUT, &[0, 0, 5, 2, 0, 1]),
            ERR_INVALID_PARAMETER
        );
        assert_eq!(
            error(SET_SCANOUT, &[0, 0, 4, 2, 1, 1]),
            ERR_INVALID_SCANOUT_ID
        );
        assert_eq!(error(0x0999, &[]), ERR_UNSPEC);

        // Backing outside guest memory.
        let mut entry = Vec::new();
        entry.extend_from_slice(&(1u64 << 40).to_le_bytes());
        entry.extend_from_slice(&32u32.to_le_bytes());
        entry.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            guest.send(RESOURCE_ATTACH_BACKING, &[1, 1], &entry).0,
            OK_NODATA
        );
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]),
            ERR_INVALID_PARAMETER
        );
        guest.ok(RESOURCE_DETACH_BACKING, &[1, 0]);
        assert_eq!(
            error(TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]),
            ERR_INVALID_PARAMETER
        );
        assert_eq!(lock(&guest.gpu.state).host_bytes, 32);
        guest.ok(RESOURCE_UNREF, &[1, 0]);
        assert_eq!(lock(&guest.gpu.state).host_bytes, 0);
    }

    #[test]
    fn the_cursor_comes_out_straight() {
        let guest = Guest::new();
        let display = guest.gpu.display();
        // 64x64 B8G8R8A8; pixel 0 half-transparent premultiplied red, pixel 1 opaque
        // blue.
        guest.resource(5, 1, 64, 64);
        guest.draw(0, 64 * 64 * 2, [0, 0, 0x40, 0x80]);
        guest.draw(1, 64 * 64 * 2, [0xff, 0, 0, 0xff]);
        guest.ok(TRANSFER_TO_HOST_2D, &[0, 0, 64, 64, 0, 0, 5, 0]);
        assert_eq!(display.cursor(0), (0, false, None));
        guest.ok(UPDATE_CURSOR, &[0, 10, 10, 0, 5, 3, 4, 0]);
        let (count, shown, cursor) = display.cursor(0);
        assert_eq!((count, shown), (1, true));
        let cursor = cursor.expect("a new image");
        assert_eq!(
            (cursor.width, cursor.height, cursor.hotspot),
            (64, 64, (3, 4))
        );
        assert_eq!(&cursor.rgba[..8], &[0x7f, 0, 0, 0x80, 0, 0, 0xff, 0xff]);
        assert_eq!(display.cursor(1).2, None, "unchanged since");
        guest.ok(MOVE_CURSOR, &[0, 20, 20, 0, 0, 0, 0, 0]);
        assert_eq!(
            display.cursor(1),
            (1, true, None),
            "moving isn't a new image"
        );
        guest.ok(UPDATE_CURSOR, &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(!display.cursor(2).1, "hidden");
    }

    #[test]
    fn a_restored_device_shows_what_the_guest_drew() {
        let guest = Guest::new();
        guest.resource(1, 2, 4, 2);
        guest.draw(3, 16, [0x33, 0x22, 0x11, 0]);
        guest.ok(SET_SCANOUT, &[0, 0, 4, 2, 0, 1]);
        guest.ok(TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]);
        guest.gpu.display().request_size(1500, 844);
        let saved = guest.gpu.save_device();
        let before = guest.gpu.display().snapshot().unwrap();

        let mut restored = VirtioGpu::new((640, 480));
        restored.restore_device(&saved).unwrap();
        let queues = (0..2)
            .map(|i| {
                MockSplitQueue::create(&guest.memory, GuestAddress(0x1000 * i), 16)
                    .create_queue()
                    .unwrap()
            })
            .collect();
        restored
            .activate(
                guest.memory.clone(),
                0,
                queues,
                Arc::new(Interrupts::default()),
            )
            .unwrap();
        assert_eq!(restored.display().snapshot(), Some(before));
        assert_eq!(lock(&restored.state).preferred, (1500, 844));
        assert_eq!(restored.save_device(), saved);
        // A state that doesn't fit is refused.
        assert!(restored.restore_device(&saved[..saved.len() - 3]).is_err());

        // A reset forgets the resources, but not the host's size.
        restored.reset();
        assert_eq!(restored.display().snapshot(), None);
        assert_eq!(lock(&restored.state).preferred, (1500, 844));
    }
}
