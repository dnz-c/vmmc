//! The console window (`--display window`): the guest's screen, keyboard and pointer,
//! and a toolbar, drawn by Slint's software renderer.
//!
//! - **Screen:** the display adapter's framebuffer, checked up to 60 times a second on
//!   a thread of its own, and handed to the window only when it changed. The window
//!   opens at the guest's resolution, one guest pixel per screen pixel, follows the
//!   guest's mode changes, and scales the picture down if the user makes it smaller.
//! - **Pointer:** the USB tablet, which is absolute, so the host pointer moves the
//!   guest's and nothing is captured.
//! - **Keyboard:** while a window of this process is in front, a low-level keyboard
//!   hook sends physical scan codes to the PS/2 keyboard, Windows and Alt+Tab included.
//!   Ctrl+Alt+Del never reaches a hook; the toolbar sends it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use machine::{Capture, MAX_PREFERRED, MIN_PREFERRED, PAUSE, TABLET_MAX};
use slint::{ComponentHandle, Image, Rgb8Pixel, SharedPixelBuffer};
use vm_core::StopReason;

use crate::console::Targets;
use crate::screenshot::Screenshots;

slint::slint! {
    import { Button, AboutSlint, HorizontalBox } from "std-widgets.slint";

    export component ConsoleWindow inherits Window {
        in property <image> screen;
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
                Image {
                    source: root.screen;
                    width: parent.width;
                    height: parent.height;
                    image-fit: contain;
                }
                TouchArea {
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
    fit_to_picture(&window, resolution);
    window.set_can_screenshot(screenshots.is_some());
    window.set_title_text(title.into());

    // The size of the picture shown, which the pointer maps against: width << 32 | height.
    let frame_size = Arc::new(AtomicU64::new(pack(resolution)));
    let capture = Capturer::start(targets.clone(), window.as_weak(), frame_size.clone());

    let wheel_targets = targets.clone();
    window.on_pointer(move |x, y, width, height, left, right, middle, wheel| {
        let Some(target) = wheel_targets.get() else {
            return;
        };
        let frame = unpack(frame_size.load(Ordering::Relaxed));
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

    let hook = keyboard::install(targets.clone());
    let result = window.run().map_err(|e| format!("the window failed: {e}"));
    drop(hook);
    drop(capture);
    // Closing the window ends the VM.
    targets.quit();
    result
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
/// below the toolbar, as far as the screen allows.
fn fit_to_picture(window: &ConsoleWindow, picture: (u32, u32)) {
    let scale = window.window().scale_factor();
    let toolbar = (TOOLBAR_HEIGHT * scale).ceil() as u32;
    let (space_w, space_h) = screen::space_for_picture(TOOLBAR_HEIGHT);
    let (width, height) = (picture.0.min(space_w), picture.1.min(space_h));
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
                    let (window, pending, frame_size) =
                        (window.clone(), pending.clone(), frame_size.clone());
                    let posted = slint::invoke_from_event_loop(move || {
                        pending.store(false, Ordering::Release);
                        let Some(window) = window.upgrade() else {
                            return;
                        };
                        // The guest changed its resolution: so does the window.
                        if frame_size.swap(pack(size), Ordering::Relaxed) != pack(size) {
                            fit_to_picture(&window, size);
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
mod keyboard {
    use std::cell::RefCell;
    use std::sync::mpsc::{self, Sender};
    use std::thread::{self, JoinHandle};

    use super::*;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, SetThreadInformation,
        SetThreadPriority, THREAD_POWER_THROTTLING_CURRENT_VERSION,
        THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
        THREAD_PRIORITY_TIME_CRITICAL, ThreadPowerThrottling,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_NUMLOCK, VK_PAUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
        GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG,
        PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WM_QUIT,
    };

    /// The hook's state, on its thread: where keys go, and the keys the guest holds.
    struct Sink {
        keys: Sender<(u16, bool)>,
        pressed: BTreeSet<u16>,
    }

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    }

    /// The hook's threads; dropping it unhooks and ends them.
    pub struct Hook {
        thread_id: u32,
        threads: Vec<JoinHandle<()>>,
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            // SAFETY: a plain call; the hook thread's loop ends on WM_QUIT, and the
            // delivery thread ends once the hook thread has dropped its sender.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    pub fn install(targets: Targets) -> Option<Hook> {
        let (keys, queued) = mpsc::channel::<(u16, bool)>();
        let delivery = thread::Builder::new()
            .name("keyboard-keys".into())
            .spawn(move || {
                for (scancode, pressed) in queued {
                    tracing::trace!(scancode, pressed, "window: key for the guest");
                    if let Some(target) = targets.get() {
                        target.ps2.key(scancode, pressed);
                    }
                }
            })
            .ok()?;
        let (ready, started) = mpsc::channel();
        let hook_thread = thread::Builder::new()
            .name("keyboard-hook".into())
            .spawn(move || {
                SINK.set(Some(Sink {
                    keys,
                    pressed: BTreeSet::new(),
                }));
                responsive();
                // SAFETY: `hook` matches HOOKPROC; the module handle is this
                // executable's. MSG is a valid out-pointer for the loop.
                unsafe {
                    let installed = GetModuleHandleW(None).and_then(|module| {
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(module.into()), 0)
                    });
                    match installed {
                        Ok(installed) => {
                            tracing::debug!("keyboard hook installed");
                            let _ = ready.send(Some(GetCurrentThreadId()));
                            let mut message = MSG::default();
                            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                                let _ = TranslateMessage(&message);
                                DispatchMessageW(&message);
                            }
                            let _ = UnhookWindowsHookEx(installed);
                        }
                        Err(e) => {
                            tracing::warn!(%e, "can't hook the keyboard; the guest won't get keys");
                            let _ = ready.send(None);
                        }
                    }
                }
                SINK.set(None);
            });
        let Ok(hook_thread) = hook_thread else {
            // The sender went with the closure, so the delivery thread ends.
            let _ = delivery.join();
            return None;
        };
        let threads = vec![hook_thread, delivery];
        match started.recv() {
            Ok(Some(thread_id)) => Some(Hook { thread_id, threads }),
            _ => {
                for thread in threads {
                    let _ = thread.join();
                }
                None
            }
        }
    }

    /// Keeps the calling thread quick to wake: the highest priority short of real time,
    /// and no power throttling. Windows 11 throttles threads it thinks are background
    /// work (EcoQoS, efficiency cores), and a hook that wakes too late is skipped: the
    /// key goes past it.
    fn responsive() {
        // SAFETY: plain calls on the current thread; `state` is a valid
        // THREAD_POWER_THROTTLING_STATE of the size passed.
        unsafe {
            let thread = GetCurrentThread();
            let _ = SetThreadPriority(thread, THREAD_PRIORITY_TIME_CRITICAL);
            let state = THREAD_POWER_THROTTLING_STATE {
                Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: 0,
            };
            let _ = SetThreadInformation(
                thread,
                ThreadPowerThrottling,
                (&raw const state).cast(),
                size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }

    /// Whether the window in front belongs to this process (the console window, if
    /// any, belongs to the terminal).
    fn ours_in_front() -> bool {
        let mut process = 0u32;
        // SAFETY: plain calls; `process` is a valid out-pointer.
        unsafe {
            let window = GetForegroundWindow();
            GetWindowThreadProcessId(window, Some(&mut process));
            process == GetCurrentProcessId()
        }
    }

    /// The set 1 scan code for a hook event, if the key has one.
    fn scancode(info: &KBDLLHOOKSTRUCT) -> Option<u16> {
        // Windows reports Num Lock as extended and Pause as plain, the other way round
        // from the keyboard.
        if info.vkCode == u32::from(VK_PAUSE.0) {
            return Some(PAUSE);
        }
        let (code, extended) = if info.scanCode != 0 {
            (
                info.scanCode as u16 & 0xff,
                info.flags.0 & LLKHF_EXTENDED.0 != 0,
            )
        } else {
            // Software that injects a virtual key alone gives no scan code.
            // SAFETY: a plain call.
            let mapped = unsafe { MapVirtualKeyW(info.vkCode, MAPVK_VK_TO_VSC_EX) };
            (mapped as u16 & 0xff, mapped & 0xff00 == 0xe000)
        };
        if code == 0 {
            None
        } else if extended && info.vkCode != u32::from(VK_NUMLOCK.0) {
            Some(0xe000 | code)
        } else {
            Some(code)
        }
    }

    impl Sink {
        /// Queues a key for the guest while `ours` (a window of this process is in
        /// front), or else lets go of the keys the guest still holds. Returns whether
        /// the key was the guest's.
        fn route(&mut self, info: &KBDLLHOOKSTRUCT, ours: bool) -> bool {
            if !ours {
                for scancode in std::mem::take(&mut self.pressed) {
                    let _ = self.keys.send((scancode, false));
                }
                return false;
            }
            let Some(scancode) = scancode(info) else {
                return false;
            };
            let pressed = info.flags.0 & LLKHF_UP.0 == 0;
            if pressed {
                self.pressed.insert(scancode);
            } else {
                self.pressed.remove(&scancode);
            }
            let _ = self.keys.send((scancode, pressed));
            true
        }
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for WH_KEYBOARD_LL, lparam points to a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let ours = ours_in_front();
            // The hook isn't re-entered while it runs; if it were, the key passes on.
            let guests = SINK.with(|sink| match sink.try_borrow_mut().as_deref_mut() {
                Ok(Some(sink)) => sink.route(info, ours),
                _ => false,
            });
            if guests {
                // Swallowed: the host doesn't act on keys meant for the guest.
                return LRESULT(1);
            }
        }
        // SAFETY: passing the event on, as every hook must.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
        use windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS;

        fn event(vk: u16, scan: u32, flags: u32) -> KBDLLHOOKSTRUCT {
            KBDLLHOOKSTRUCT {
                vkCode: u32::from(vk),
                scanCode: scan,
                flags: KBDLLHOOKSTRUCT_FLAGS(flags),
                ..Default::default()
            }
        }

        #[test]
        fn keys_go_to_the_guest_only_while_the_window_is_in_front() {
            let (keys, queued) = mpsc::channel();
            let mut sink = Sink {
                keys,
                pressed: BTreeSet::new(),
            };
            let enter = event(VK_RETURN.0, 0x1c, 0);
            let right_ctrl = event(0xa3, 0x1d, LLKHF_EXTENDED.0);
            assert!(sink.route(&enter, true));
            assert!(sink.route(&right_ctrl, true));
            assert!(sink.route(&event(VK_RETURN.0, 0x1c, LLKHF_UP.0), true));
            assert_eq!(
                queued.try_iter().collect::<Vec<_>>(),
                [(0x1c, true), (0xe01d, true), (0x1c, false)]
            );
            // Another window takes the keyboard: the held key is let go, once.
            assert!(!sink.route(&enter, false));
            assert!(!sink.route(&enter, false));
            assert_eq!(queued.try_iter().collect::<Vec<_>>(), [(0xe01d, false)]);
        }

        #[test]
        fn scan_codes_follow_the_keyboard() {
            assert_eq!(scancode(&event(VK_PAUSE.0, 0x45, 0)), Some(PAUSE));
            assert_eq!(
                scancode(&event(VK_NUMLOCK.0, 0x45, LLKHF_EXTENDED.0)),
                Some(0x45)
            );
            // A virtual key injected without a scan code gets the key's own.
            assert_eq!(scancode(&event(VK_RETURN.0, 0, 0)), Some(0x1c));
            assert_eq!(scancode(&event(0xff, 0, 0)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
