//! Display adapters, and [`Display`]: what the host reads from whichever one a machine
//! has.

pub mod bochs;
pub mod edid;

use crate::virtio::gpu;

/// The largest preferred resolution an adapter offers: what fits bochs-display's
/// framebuffer.
pub const MAX_PREFERRED: (u32, u32) = (3840, 2160);
/// The smallest: what UEFI's graphics console needs.
pub const MIN_PREFERRED: (u32, u32) = (640, 480);

/// `size`, kept between [`MIN_PREFERRED`] and [`MAX_PREFERRED`].
pub fn preferred(size: (u32, u32)) -> (u32, u32) {
    (
        size.0.clamp(MIN_PREFERRED.0, MAX_PREFERRED.0),
        size.1.clamp(MIN_PREFERRED.1, MAX_PREFERRED.1),
    )
}

/// One captured frame, as packed 8-bit RGB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// The guest's hardware cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// How many images the guest has given; 0 while it draws the cursor itself.
    pub count: u32,
    pub shown: bool,
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    /// Straight-alpha RGBA, `width` x `height`.
    pub rgba: Vec<u8>,
}

/// What [`Display::capture`] saw last.
#[derive(Default)]
pub struct Capture {
    /// bochs-display: width, height, bytes per pixel, stride and start of the last
    /// picture.
    geometry: Option<[u64; 5]>,
    /// bochs-display: its pixels as the framebuffer held them.
    raw: Vec<u8>,
    /// A buffer to read the next picture into.
    pub(crate) spare: Vec<u8>,
    /// virtio-gpu: the device and its picture's generation.
    pub(crate) generation: Option<(usize, u64)>,
}

/// Reads what the machine's display adapter shows and asks it for sizes, from any
/// thread.
#[derive(Clone)]
pub enum Display {
    Bochs(bochs::Display),
    Gpu(gpu::Display),
}

impl Display {
    /// Whether `other` reads the same adapter. A VM that reset has a new one.
    pub fn same_adapter(&self, other: &Display) -> bool {
        match (self, other) {
            (Display::Bochs(a), Display::Bochs(b)) => a.same_adapter(b),
            (Display::Gpu(a), Display::Gpu(b)) => a.same_adapter(b),
            _ => false,
        }
    }

    /// Asks the guest for a `width` x `height` screen: what the host's window has room
    /// for. Returns the size asked for, after keeping it within what the adapter can
    /// show.
    pub fn request_size(&self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Display::Bochs(display) => display.request_size(width, height),
            Display::Gpu(display) => display.request_size(width, height),
        }
    }

    /// The cursor's state; its image only when it changed since `last_count`.
    pub fn cursor(&self, last_count: u32) -> (u32, bool, Option<Cursor>) {
        match self {
            Display::Bochs(display) => display.cursor(last_count),
            Display::Gpu(display) => display.cursor(last_count),
        }
    }

    /// The picture now, or `None` while nothing is shown.
    pub fn snapshot(&self) -> Option<Frame> {
        self.capture(&mut Capture::default())
    }

    /// The picture, if it changed since `last` saw it; `None` when it didn't, or
    /// while nothing is shown.
    pub fn capture(&self, last: &mut Capture) -> Option<Frame> {
        match self {
            Display::Bochs(display) => display.capture(last),
            Display::Gpu(display) => display.capture(last),
        }
    }
}
