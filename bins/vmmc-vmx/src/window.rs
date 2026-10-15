//! The console window (`--display window`): the guest's screen, keyboard and pointer,
//! and a toolbar, drawn by Slint's software renderer.
//!
//! - **Screen:** the display adapter's framebuffer, checked up to 60 times a second on
//!   a thread of its own, and handed to the window only when it changed. The window
//!   opens at the guest's resolution, one guest pixel per screen pixel, follows the
//!   guest's mode changes, and scales the picture down if the user makes it smaller.
//! - **Pointer:** the USB tablet, which is absolute, so the host pointer moves the
//!   guest's and nothing is captured.
//! - **Keyboard:** while the window (or its About popup) is in front, a low-level
//!   keyboard hook sends physical scan codes to the PS/2 keyboard, Windows and Alt+Tab
//!   included.
//!   Ctrl+Alt+Del never reaches a hook; the toolbar sends it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use machine::{Capture, Display, MAX_PREFERRED, MIN_PREFERRED, TABLET_MAX};
use slint::{ComponentHandle, Image, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};
use vm_core::StopReason;

use crate::console::Targets;
use crate::screenshot::Screenshots;

slint::slint! {
    import { Button, AboutSlint, HorizontalBox } from "std-widgets.slint";

    export component ConsoleWindow inherits Window {
        in property <image> screen;
        // The guest's picture size, which the cursor scales with.
        in property <int> frame-width: 1;
        in property <int> frame-height: 1;
        // The guest's hardware cursor: drawn here at the host's pointer, in place of
        // the host's own, once the guest has given an image.
        in property <image> cursor-image;
        in property <bool> cursor-hardware;
        in property <bool> cursor-shown;
        in property <int> cursor-hot-x;
        in property <int> cursor-hot-y;
        in property <string> title-text;
        title: root.title-text;
        in property <bool> can-screenshot;
        in-out property <bool> left;
        in-out property <bool> right;
        in-out property <bool> middle;
        callback pointer(length, length, length, length, bool, bool, bool, int);
        callback ctrl-alt-del();
        callback power-button();
        callback reset();
        callback screenshot();
        preferred-width: 1024px;
        preferred-height: 800px;
        background: #202020;

        VerticalLayout {
            HorizontalBox {
                height: 40px; // TOOLBAR_HEIGHT
                Button { text: "Ctrl+Alt+Del"; clicked => { root.ctrl-alt-del(); } }
                Button { text: "Power button"; clicked => { root.power-button(); } }
                Button { text: "Reset"; clicked => { root.reset(); } }
                Button {
                    text: "Screenshot";
                    enabled: root.can-screenshot;
                    clicked => { root.screenshot(); }
                }
                Rectangle { }
                Button { text: "About"; clicked => { about.show(); } }
            }
            area := Rectangle {
                background: black;
                // Screen pixels per guest pixel, as the picture is scaled to fit.
                property <float> scale: min(self.width / (root.frame-width * 1phx),
                    self.height / (root.frame-height * 1phx));
                Image {
                    source: root.screen;
                    width: parent.width;
                    height: parent.height;
                    image-fit: contain;
                }
                touch := TouchArea {
                    mouse-cursor: root.cursor-hardware ? MouseCursor.none : MouseCursor.default;
                    moved => {
                        root.pointer(self.mouse-x, self.mouse-y, area.width, area.height,
                            root.left, root.right, root.middle, 0);
                    }
                    pointer-event(event) => {
                        if (event.kind == PointerEventKind.down || event.kind == PointerEventKind.up) {
                            if (event.button == PointerEventButton.left) {
                                root.left = event.kind == PointerEventKind.down;
                            }
                            if (event.button == PointerEventButton.right) {
                                root.right = event.kind == PointerEventKind.down;
                            }
                            if (event.button == PointerEventButton.middle) {
                                root.middle = event.kind == PointerEventKind.down;
                            }
                        }
                        root.pointer(self.mouse-x, self.mouse-y, area.width, area.height,
                            root.left, root.right, root.middle, 0);
                    }
                    scroll-event(event) => {
                        root.pointer(self.mouse-x, self.mouse-y, area.width, area.height,
                            root.left, root.right, root.middle, event.delta-y > 0 ? 1 : -1);
                        accept
                    }
                }
                Image {
                    source: root.cursor-image;
                    x: touch.mouse-x - root.cursor-hot-x * area.scale * 1phx;
                    y: touch.mouse-y - root.cursor-hot-y * area.scale * 1phx;
                    width: root.cursor-image.width * area.scale * 1phx;
                    height: root.cursor-image.height * area.scale * 1phx;
                    visible: root.cursor-hardware && root.cursor-shown && touch.has-hover;
                }
            }
        }

        about := PopupWindow {
            x: 40px;
            y: 50px;
            width: 360px;
            Rectangle {
                background: #303030;
                border-radius: 6px;
                VerticalLayout {
                    padding: 12px;
                    spacing: 8px;
                    Text {
                        text: "vmmc: a desktop hypervisor, Apache-2.0.\nVMware is a trademark of Broadcom Inc.; vmmc isn't affiliated with Broadcom.";
                        wrap: word-wrap;
                        color: white;
                    }
                    AboutSlint { }
                }
            }
        }
    }
}

/// Scan codes the toolbar's Ctrl+Alt+Del presses: left Ctrl, left Alt, Delete.
const CTRL_ALT_DEL: [u16; 3] = [0x1d, 0x38, 0xe053];
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// How often the window checks the guest's cursor.
const CURSOR_INTERVAL: Duration = Duration::from_millis(30);
/// How often the window checks its size; a size that holds for two checks is passed on
/// to the guest, so dragging an edge doesn't ask for every size on the way.
const RESIZE_INTERVAL: Duration = Duration::from_millis(300);
/// The toolbar's height in logical pixels, as the Slint markup has it.
const TOOLBAR_HEIGHT: f32 = 40.0;
/// Resolutions the guest can be offered, largest first.
const RESOLUTIONS: [(u32, u32); 12] = [
    (3840, 2160),
    (2560, 1600),
    (2560, 1440),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1600, 900),
    (1440, 900),
    (1366, 768),
    (1280, 800),
    (1280, 720),
    (1024, 768),
];

/// The largest of [`RESOLUTIONS`] that fits in `space` (width, height), or the
/// smallest the firmware takes.
fn largest_fitting(space: (u32, u32)) -> (u32, u32) {
    RESOLUTIONS
        .into_iter()
        .filter(|&(w, h)| (w, h) <= MAX_PREFERRED && w >= MIN_PREFERRED.0 && h >= MIN_PREFERRED.1)
        .find(|&(w, h)| w <= space.0 && h <= space.1)
        .unwrap_or((1024, 768))
}

/// The resolution to give a guest shown in a window: the largest standard one that
/// fits on the primary screen at one guest pixel per screen pixel, with the window's
/// frame and toolbar.
pub fn fitting_resolution() -> (u32, u32) {
    largest_fitting(screen::space_for_picture(TOOLBAR_HEIGHT))
}

/// Where `(x, y)` in an `area`-sized view lands on the tablet, when a `frame`-sized
/// picture is scaled to fit the view and centred (Slint's `contain`).
fn tablet_position(x: f32, y: f32, area: (f32, f32), frame: (u32, u32)) -> (u16, u16) {
    let (frame_w, frame_h) = (frame.0.max(1) as f32, frame.1.max(1) as f32);
    let scale = (area.0 / frame_w)
        .min(area.1 / frame_h)
        .max(f32::MIN_POSITIVE);
    let (shown_w, shown_h) = (frame_w * scale, frame_h * scale);
    let (left, top) = ((area.0 - shown_w) / 2.0, (area.1 - shown_h) / 2.0);
    let fraction = |value: f32, start: f32, size: f32| ((value - start) / size).clamp(0.0, 1.0);
    let max = f32::from(TABLET_MAX);
    (
        (fraction(x, left, shown_w) * max).round() as u16,
        (fraction(y, top, shown_h) * max).round() as u16,
    )
}

/// Shows the window until it's closed or the VM ends (see [`close`]). Closing it asks
/// the VM to stop. `resolution` is the guest's, which the window opens at.
pub fn run(
    targets: Targets,
    screenshots: Option<Arc<Screenshots>>,
    title: &str,
    resolution: (u32, u32),
) -> Result<(), String> {
    let window = ConsoleWindow::new().map_err(|e| format!("can't open the window: {e}"))?;
    // The window's size below the toolbar that the guest's picture matches, as far as the
    // window knows: the size it took to show the picture, or the size it last asked the
    // guest for. Any other size is the user's doing, and goes to the guest.
    let matched = Arc::new(AtomicU64::new(0));
    fit_to_picture(&window, resolution, &matched);
    window.set_frame_width(resolution.0 as i32);
    window.set_frame_height(resolution.1 as i32);
    window.set_can_screenshot(screenshots.is_some());
    window.set_title_text(title.into());

    // The size of the picture shown, which the pointer maps against: width << 32 | height.
    let frame_size = Arc::new(AtomicU64::new(pack(resolution)));
    let capture = Capturer::start(
        targets.clone(),
        window.as_weak(),
        frame_size.clone(),
        matched.clone(),
    );

    let wheel_targets = targets.clone();
    let pointer_frame = frame_size.clone();
    window.on_pointer(move |x, y, width, height, left, right, middle, wheel| {
        let Some(target) = wheel_targets.get() else {
            return;
        };
        let frame = unpack(pointer_frame.load(Ordering::Relaxed));
        let (x, y) = tablet_position(x, y, (width, height), frame);
        let buttons = u8::from(left) | (u8::from(right) << 1) | (u8::from(middle) << 2);
        target.usb.pointer(x, y, buttons, wheel);
    });
    let key_targets = targets.clone();
    window.on_ctrl_alt_del(move || {
        if let Some(target) = key_targets.get() {
            for code in CTRL_ALT_DEL {
                target.ps2.key(code, true);
            }
            for code in CTRL_ALT_DEL.iter().rev() {
                target.ps2.key(*code, false);
            }
        }
    });
    let power_targets = targets.clone();
    window.on_power_button(move || {
        if let Some(target) = power_targets.get() {
            target.power.press();
        }
    });
    let reset_targets = targets.clone();
    window.on_reset(move || {
        if let Some(target) = reset_targets.get() {
            target.stop.request_stop(StopReason::Reset);
        }
    });
    let shot_targets = targets.clone();
    window.on_screenshot(move || {
        if let (Some(screenshots), Some(display)) = (&screenshots, shot_targets.display()) {
            match screenshots.save(&display, "screenshot", true) {
                Ok(Some(path)) => eprint!("\r\nvmmc-vmx: saved {}\r\n", path.display()),
                Ok(None) => eprint!("\r\nvmmc-vmx: no picture to save\r\n"),
                Err(e) => eprint!("\r\nvmmc-vmx: screenshot failed: {e}\r\n"),
            }
        }
    });

    keyhook::stop_raw_input();
    let _cursor = follow_cursor(&window, targets.clone());
    let _resize = fit_guest_to_window(&window, targets.clone(), frame_size.clone(), matched);
    let key_targets = targets.clone();
    let hook = keyhook::install(
        keyhook::current_thread(),
        Arc::new(AtomicBool::new(true)),
        move |scancode, pressed| {
            if let Some(target) = key_targets.get() {
                target.ps2.key(scancode, pressed);
            }
        },
    );
    let result = window.run().map_err(|e| format!("the window failed: {e}"));
    drop(hook);
    // After the guest's output, which can clear the terminal, so it stays readable.
    eprint!("\r\nvmmc-vmx: {}\r\n", keyhook::summary());
    drop(capture);
    // Closing the window ends the VM.
    targets.quit();
    result
}

/// Shows the guest's hardware cursor in place of the host's, as long as the timer lives.
fn follow_cursor(window: &ConsoleWindow, targets: Targets) -> slint::Timer {
    let weak = window.as_weak();
    let mut last_count = 0;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, CURSOR_INTERVAL, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(display) = targets.display() else {
            window.set_cursor_hardware(false);
            return;
        };
        let (count, shown, image) = display.cursor(last_count);
        if let Some(cursor) = image {
            let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                &cursor.rgba,
                cursor.width,
                cursor.height,
            );
            window.set_cursor_image(Image::from_rgba8(pixels));
            window.set_cursor_hot_x(cursor.hotspot.0 as i32);
            window.set_cursor_hot_y(cursor.hotspot.1 as i32);
        }
        // A guest that reset draws its cursor itself until its driver is back.
        last_count = count;
        window.set_cursor_hardware(count != 0);
        window.set_cursor_shown(shown);
    });
    timer
}

/// Asks the guest for the window's size whenever the user resizes it (and it then
/// holds still), so the guest's desktop fills the window. The window itself follows
/// the guest's mode changes (see [`Capturer`]), which don't count as resizing: neither
/// the sizes it takes then (`matched`), nor the ones it passes through while the guest
/// switches, which can go through another mode on the way to the size asked for.
fn fit_guest_to_window(
    window: &ConsoleWindow,
    targets: Targets,
    frame_size: Arc<AtomicU64>,
    matched: Arc<AtomicU64>,
) -> slint::Timer {
    let weak = window.as_weak();
    let mut seen = None;
    // The adapter last asked for a size.
    let mut asked: Option<Display> = None;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, RESIZE_INTERVAL, move || {
        let (Some(window), Some(display)) = (weak.upgrade(), targets.display()) else {
            return;
        };
        // A VM that reset has a new adapter, which knows nothing of the window's size.
        if asked
            .as_ref()
            .is_some_and(|asked| !asked.same_adapter(&display))
        {
            asked = None;
            matched.store(0, Ordering::Relaxed);
        }
        let size = window.window().size();
        let toolbar = (TOOLBAR_HEIGHT * window.window().scale_factor()).ceil() as u32;
        let area = (size.width, size.height.saturating_sub(toolbar));
        let picture = unpack(frame_size.load(Ordering::Relaxed));
        // Neither the window nor the picture changed since the last check: when the
        // picture changes, the window follows a moment later.
        let settled = seen == Some((area, picture));
        seen = Some((area, picture));
        if !settled
            || window.window().is_minimized()
            || area.0 < MIN_PREFERRED.0 / 2
            || area == picture
            || pack(area) == matched.load(Ordering::Relaxed)
        {
            return;
        }
        matched.store(pack(area), Ordering::Relaxed);
        let (width, height) = display.request_size(area.0, area.1);
        tracing::debug!(width, height, "window: asked the guest to fit the window");
        asked = Some(display);
    });
    timer
}

/// Closes the window from any thread, once the VM has ended.
pub fn close() {
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

fn pack((width, height): (u32, u32)) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// Sizes the window to show a `picture`-sized frame at one pixel per screen pixel,
/// below the toolbar, as far as the screen allows, and notes that size as `matched`
/// (see [`fit_guest_to_window`]). A maximized or full-screen window keeps its size,
/// and the picture is scaled into it: resizing it would un-maximize it (winit does
/// that for any new size), and the restored window would then ask the guest for its
/// old size.
fn fit_to_picture(window: &ConsoleWindow, picture: (u32, u32), matched: &AtomicU64) {
    if window.window().is_maximized() || window.window().is_fullscreen() {
        return;
    }
    let scale = window.window().scale_factor();
    let toolbar = (TOOLBAR_HEIGHT * scale).ceil() as u32;
    let (space_w, space_h) = screen::space_for_picture(TOOLBAR_HEIGHT);
    let (width, height) = (picture.0.min(space_w), picture.1.min(space_h));
    matched.store(pack((width, height)), Ordering::Relaxed);
    window
        .window()
        .set_size(slint::PhysicalSize::new(width, height + toolbar));
}

/// Watches the guest's screen on a thread of its own and hands changed pictures to the
/// window. Converting a frame takes a while (tens of milliseconds in a debug build);
/// done on the window's thread, it left no time for input.
struct Capturer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capturer {
    fn start(
        targets: Targets,
        window: slint::Weak<ConsoleWindow>,
        frame_size: Arc<AtomicU64>,
        matched: Arc<AtomicU64>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        // Set while a picture waits for the window to take it, so they don't queue up.
        let pending = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || {
                let mut last = Capture::default();
                while !stopping.load(Ordering::Relaxed) {
                    thread::sleep(FRAME_INTERVAL);
                    if pending.load(Ordering::Acquire) {
                        continue;
                    }
                    let Some(frame) = targets
                        .display()
                        .and_then(|display| display.capture(&mut last))
                    else {
                        continue;
                    };
                    let size = (frame.width, frame.height);
                    let pixels = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(
                        &frame.rgb, size.0, size.1,
                    );
                    pending.store(true, Ordering::Release);
                    let (window, pending, frame_size, matched) = (
                        window.clone(),
                        pending.clone(),
                        frame_size.clone(),
                        matched.clone(),
                    );
                    let posted = slint::invoke_from_event_loop(move || {
                        pending.store(false, Ordering::Release);
                        let Some(window) = window.upgrade() else {
                            return;
                        };
                        // The guest changed its resolution: so does the window.
                        if frame_size.swap(pack(size), Ordering::Relaxed) != pack(size) {
                            fit_to_picture(&window, size, &matched);
                            window.set_frame_width(size.0 as i32);
                            window.set_frame_height(size.1 as i32);
                        }
                        window.set_screen(Image::from_rgb8(pixels));
                    });
                    if posted.is_err() {
                        return;
                    }
                }
            })
            .ok();
        Self { stop, thread }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The host's screen.
mod screen {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetSystemMetricsForDpi,
        SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYCAPTION, SM_CYSIZEFRAME, SPI_GETWORKAREA,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };

    /// The room for the guest's picture, in screen pixels, in a window on the primary
    /// screen's work area: less the window's frame, its title bar and a toolbar
    /// `toolbar` logical pixels high.
    pub fn space_for_picture(toolbar: f32) -> (u32, u32) {
        // SAFETY: plain calls; `area` is a valid out-pointer of the size SPI_GETWORKAREA
        // writes.
        unsafe {
            // Real pixels from here on (the window library asks for the same); fails
            // harmlessly if it's set already.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let mut area = RECT::default();
            if SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&raw mut area).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_err()
            {
                return (1280, 800);
            }
            let dpi = GetDpiForSystem();
            let metric = |index| GetSystemMetricsForDpi(index, dpi).max(0) as u32;
            let frame_x = metric(SM_CXSIZEFRAME) + metric(SM_CXPADDEDBORDER);
            let frame_y = metric(SM_CYSIZEFRAME) + metric(SM_CXPADDEDBORDER);
            let toolbar = (toolbar * dpi as f32 / 96.0).ceil() as u32;
            let width = (area.right - area.left).max(0) as u32;
            let height = (area.bottom - area.top).max(0) as u32;
            (
                width.saturating_sub(2 * frame_x),
                height.saturating_sub(2 * frame_y + metric(SM_CYCAPTION) + toolbar),
            )
        }
    }
}

/// The low-level keyboard hook, on a thread of its own. Windows calls a hook on the
/// thread that installed it, from its message loop, and silently removes a hook that's
/// slow to answer. So the hook only queues keys, and another thread hands them to the
/// machine, whose locks it may have to wait for.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_and_the_keyboard_agree_on_pause() {
        assert_eq!(keyhook::PAUSE, machine::PAUSE);
    }

    #[test]
    fn the_largest_standard_resolution_that_fits_is_chosen() {
        // A 1920x1080 screen at 100%, less the taskbar, frame, title bar and toolbar.
        assert_eq!(largest_fitting((1904, 963)), (1600, 900));
        // 2560x1440.
        assert_eq!(largest_fitting((2544, 1323)), (1920, 1200));
        // 3840x2160 at 150%: the picture still gets screen pixels one to one.
        assert_eq!(largest_fitting((3824, 1994)), (2560, 1600));
        assert_eq!(
            largest_fitting((800, 600)),
            (1024, 768),
            "never below the floor"
        );
    }

    #[test]
    fn pointer_maps_across_the_picture_shown() {
        // A 1280x800 picture in a 1000x1000 view: 1000x625, with bands above and below.
        let frame = (1280, 800);
        let area = (1000.0, 1000.0);
        assert_eq!(tablet_position(0.0, 187.5, area, frame), (0, 0));
        assert_eq!(
            tablet_position(1000.0, 812.5, area, frame),
            (TABLET_MAX, TABLET_MAX)
        );
        let (x, y) = tablet_position(500.0, 500.0, area, frame);
        assert!(
            (16380..=16388).contains(&x) && (16380..=16388).contains(&y),
            "{x},{y}"
        );
        assert_eq!(
            tablet_position(-5.0, 0.0, area, frame),
            (0, 0),
            "outside: clamped"
        );
    }
}
