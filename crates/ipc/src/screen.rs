//! The screen a worker shares: one memory section with a header, the guest's picture
//! (RGB, 3 bytes a pixel, rows packed) and its hardware cursor (RGBA, 128x128 at most).
//!
//! Each part has a sequence number that's odd while the worker writes it and even
//! once it's done (a seqlock): a reader copies a part when its number is even and
//! new, and keeps the copy only if the number didn't change meanwhile.

use std::sync::atomic::{AtomicU32, Ordering, fence};

pub const MAGIC: u32 = u32::from_le_bytes(*b"VMSC");
pub const LAYOUT_VERSION: u32 = 1;
pub const HEADER_LEN: usize = 4096;
/// Room for the largest picture: 32 MiB of RGB, more than 4096x2160.
pub const MAX_PICTURE: usize = 32 << 20;
pub const CURSOR_MAX: u32 = 128;
pub const CURSOR_LEN: usize = (CURSOR_MAX * CURSOR_MAX * 4) as usize;
pub const PICTURE_OFFSET: usize = HEADER_LEN;
pub const CURSOR_OFFSET: usize = PICTURE_OFFSET + MAX_PICTURE;
pub const SECTION_LEN: usize = CURSOR_OFFSET + CURSOR_LEN;

/// The start of the section. Every field is atomic: two processes share it.
#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub frame_seq: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub cursor_seq: AtomicU32,
    /// The guest driver's cursor count: 0 means the guest draws its cursor itself.
    pub cursor_count: AtomicU32,
    pub cursor_shown: AtomicU32,
    pub cursor_width: AtomicU32,
    pub cursor_height: AtomicU32,
    pub hot_x: AtomicU32,
    pub hot_y: AtomicU32,
}

/// A picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// The guest's hardware cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// 0: the guest has none (it draws its pointer into the picture).
    pub count: u32,
    pub shown: bool,
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    pub rgba: Vec<u8>,
}

/// A cursor image to publish.
#[derive(Clone, Copy, Debug)]
pub struct CursorImage<'a> {
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    pub rgba: &'a [u8],
}

/// A view of a section, from either side. `base` points at [`SECTION_LEN`] bytes that
/// stay mapped for the view's life.
pub struct View {
    base: *mut u8,
}

// SAFETY: the view only touches the section through atomics and raw copies guarded by
// the sequence numbers; the mapping stays valid until its owner unmaps it.
unsafe impl Send for View {}

impl View {
    /// # Safety
    /// `base` must point at a mapping of at least [`SECTION_LEN`] bytes, aligned to 4,
    /// that outlives the view.
    pub unsafe fn new(base: *mut u8) -> Self {
        Self { base }
    }

    pub fn header(&self) -> &Header {
        // SAFETY: the mapping starts with a Header, and is aligned (a page).
        unsafe { &*self.base.cast::<Header>() }
    }

    /// Lays out a fresh section.
    pub fn init(&self) {
        let header = self.header();
        header.version.store(LAYOUT_VERSION, Ordering::Relaxed);
        header.magic.store(MAGIC, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        let header = self.header();
        header.magic.load(Ordering::Acquire) == MAGIC
            && header.version.load(Ordering::Relaxed) == LAYOUT_VERSION
    }

    /// Writes a picture (the writer's side). Too large a picture is dropped.
    pub fn publish_picture(&self, width: u32, height: u32, rgb: &[u8]) {
        if rgb.len() > MAX_PICTURE || rgb.len() != width as usize * height as usize * 3 {
            return;
        }
        let header = self.header();
        let seq = header.frame_seq.load(Ordering::Relaxed);
        header.frame_seq.store(seq | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        header.width.store(width, Ordering::Relaxed);
        header.height.store(height, Ordering::Relaxed);
        // SAFETY: in bounds (checked above); readers detect a torn copy by the seq.
        unsafe {
            std::ptr::copy_nonoverlapping(rgb.as_ptr(), self.base.add(PICTURE_OFFSET), rgb.len());
        }
        header
            .frame_seq
            .store((seq | 1).wrapping_add(1), Ordering::Release);
    }

    /// Writes the cursor's state, and its image if it changed (the writer's side).
    pub fn publish_cursor(&self, count: u32, shown: bool, image: Option<CursorImage>) {
        let header = self.header();
        let seq = header.cursor_seq.load(Ordering::Relaxed);
        header.cursor_seq.store(seq | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        header.cursor_count.store(count, Ordering::Relaxed);
        header
            .cursor_shown
            .store(u32::from(shown), Ordering::Relaxed);
        if let Some(CursorImage {
            width,
            height,
            hotspot: (hot_x, hot_y),
            rgba,
        }) = image
            && width <= CURSOR_MAX
            && height <= CURSOR_MAX
            && rgba.len() == (width * height * 4) as usize
        {
            header.cursor_width.store(width, Ordering::Relaxed);
            header.cursor_height.store(height, Ordering::Relaxed);
            header.hot_x.store(hot_x, Ordering::Relaxed);
            header.hot_y.store(hot_y, Ordering::Relaxed);
            // SAFETY: at most CURSOR_LEN bytes, inside the section.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr(),
                    self.base.add(CURSOR_OFFSET),
                    rgba.len(),
                );
            }
        }
        header
            .cursor_seq
            .store((seq | 1).wrapping_add(1), Ordering::Release);
    }

    /// The picture, if it changed since `last` (a frame sequence number, updated).
    pub fn read_picture(&self, last: &mut u32) -> Option<Picture> {
        let header = self.header();
        for _ in 0..8 {
            let seq = header.frame_seq.load(Ordering::Acquire);
            if seq & 1 == 1 {
                std::thread::yield_now();
                continue;
            }
            if seq == *last {
                return None;
            }
            let width = header.width.load(Ordering::Relaxed);
            let height = header.height.load(Ordering::Relaxed);
            let len = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(3))
                .filter(|&n| n <= MAX_PICTURE)?;
            let mut rgb = vec![0u8; len];
            // SAFETY: in bounds; a torn copy is thrown away below.
            unsafe {
                std::ptr::copy_nonoverlapping(self.base.add(PICTURE_OFFSET), rgb.as_mut_ptr(), len);
            }
            fence(Ordering::Acquire);
            if header.frame_seq.load(Ordering::Relaxed) == seq {
                *last = seq;
                return Some(Picture { width, height, rgb });
            }
        }
        None
    }

    /// The cursor, if it changed since `last` (a cursor sequence number, updated).
    pub fn read_cursor(&self, last: &mut u32) -> Option<Cursor> {
        let header = self.header();
        for _ in 0..8 {
            let seq = header.cursor_seq.load(Ordering::Acquire);
            if seq & 1 == 1 {
                std::thread::yield_now();
                continue;
            }
            if seq == *last {
                return None;
            }
            let width = header.cursor_width.load(Ordering::Relaxed).min(CURSOR_MAX);
            let height = header.cursor_height.load(Ordering::Relaxed).min(CURSOR_MAX);
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            // SAFETY: at most CURSOR_LEN bytes; a torn copy is thrown away below.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.base.add(CURSOR_OFFSET),
                    rgba.as_mut_ptr(),
                    rgba.len(),
                );
            }
            let cursor = Cursor {
                count: header.cursor_count.load(Ordering::Relaxed),
                shown: header.cursor_shown.load(Ordering::Relaxed) != 0,
                width,
                height,
                hotspot: (
                    header.hot_x.load(Ordering::Relaxed),
                    header.hot_y.load(Ordering::Relaxed),
                ),
                rgba,
            };
            fence(Ordering::Acquire);
            if header.cursor_seq.load(Ordering::Relaxed) == seq {
                *last = seq;
                return Some(cursor);
            }
        }
        None
    }
}

/// A section in this process's memory, for tests.
pub struct LocalSection {
    memory: Vec<u32>,
}

impl Default for LocalSection {
    fn default() -> Self {
        Self {
            memory: vec![0; SECTION_LEN / 4],
        }
    }
}

impl LocalSection {
    pub fn view(&mut self) -> View {
        // SAFETY: SECTION_LEN bytes, aligned to 4, living as long as `self`.
        unsafe { View::new(self.memory.as_mut_ptr().cast()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_and_cursors_pass_through() {
        let mut section = LocalSection::default();
        let writer = section.view();
        writer.init();
        let reader = section.view();
        assert!(reader.is_valid());
        let mut last = 0;
        assert_eq!(reader.read_picture(&mut last), None, "nothing yet");
        let rgb: Vec<u8> = (0..4 * 2 * 3).map(|i| i as u8).collect();
        writer.publish_picture(4, 2, &rgb);
        let picture = reader.read_picture(&mut last).unwrap();
        assert_eq!((picture.width, picture.height, &picture.rgb), (4, 2, &rgb));
        assert_eq!(reader.read_picture(&mut last), None, "not new");
        writer.publish_picture(1, 1, &[1, 2]);
        assert_eq!(
            reader.read_picture(&mut last),
            None,
            "a bad picture is dropped"
        );

        let mut cursor_seq = 0;
        writer.publish_cursor(
            3,
            true,
            Some(CursorImage {
                width: 2,
                height: 1,
                hotspot: (1, 0),
                rgba: &[9; 8],
            }),
        );
        let cursor = reader.read_cursor(&mut cursor_seq).unwrap();
        assert_eq!(
            (cursor.count, cursor.shown, cursor.width, cursor.height),
            (3, true, 2, 1)
        );
        assert_eq!((cursor.hotspot, cursor.rgba), ((1, 0), vec![9; 8]));
        writer.publish_cursor(3, false, None);
        let cursor = reader.read_cursor(&mut cursor_seq).unwrap();
        assert!(!cursor.shown);
        assert_eq!(cursor.rgba, vec![9; 8], "the image stays");
    }
}
