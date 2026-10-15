//! The console's keyboard on Windows: a low-level keyboard hook that takes every key
//! (the Windows key and Alt+Tab included) while the console window is in front, and
//! hands it on as a PS/2 set 1 scan code. Keys typed into other windows pass on as
//! usual, and keys the guest holds when the console loses the keyboard are let go.

#![cfg(windows)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_NUMLOCK, VK_PAUSE,
};
use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_QUIT,
};

/// The scan code for Pause, which has no set 1 make code of its own (E1 1D 45).
pub const PAUSE: u16 = 0xe11d;

/// The hook's state, on its thread: where keys go, the keys the guest holds, the
/// thread whose windows are the console window's, and whether to take keys now.
struct Sink {
    keys: Sender<(u16, bool)>,
    pressed: BTreeSet<u16>,
    ui_thread: u32,
    capture: Arc<AtomicBool>,
}

/// Takes the keyboard back from raw input, which the window library asks for when
/// its event loop starts (for device events, which nothing here uses). While a
/// process reads the keyboard as raw input, Windows doesn't call the process's
/// low-level keyboard hooks for keys typed into its own windows: the hook saw keys
/// typed anywhere but into the console window, and the guest got none. Call it
/// once the window exists.
pub fn stop_raw_input() {
    // HID usage page 1 (generic desktop), usage 6: keyboards.
    let keyboards = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x06,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    // SAFETY: one valid RAWINPUTDEVICE, of the size passed.
    let stopped =
        unsafe { RegisterRawInputDevices(&[keyboards], size_of::<RAWINPUTDEVICE>() as u32) };
    if let Err(e) = stopped {
        tracing::warn!(%e, "can't stop raw keyboard input; the guest may get no keys");
    }
}

/// The calling thread's ID, for [`install`]'s `ui_thread`.
pub fn current_thread() -> u32 {
    // SAFETY: a plain call.
    unsafe { GetCurrentThreadId() }
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

// What the hook saw, for the summary printed when the window closes.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static IN_FRONT: AtomicU64 = AtomicU64::new(0);
static DELIVERED: AtomicU64 = AtomicU64::new(0);
/// The window in front, and its process, the last time it wasn't ours.
static OTHER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static OTHER_PROCESS: AtomicU32 = AtomicU32::new(0);

/// What the keyboard hook saw and passed on, for finding out why keys don't
/// reach the guest.
pub fn summary() -> String {
    let mut text = format!(
        "keyboard: {} key events seen, {} with this window in front, {} delivered to the guest",
        EVENTS.load(Ordering::Relaxed),
        IN_FRONT.load(Ordering::Relaxed),
        DELIVERED.load(Ordering::Relaxed)
    );
    let process = OTHER_PROCESS.load(Ordering::Relaxed);
    if process != 0 {
        let window = HWND(OTHER_WINDOW.load(Ordering::Relaxed) as *mut _);
        text += &format!(
            "; otherwise in front: {:?} of process {process} ({}), this process is {}",
            class_name(window),
            process_name(process),
            // SAFETY: a plain call.
            unsafe { GetCurrentProcessId() }
        );
    }
    text
}

fn class_name(window: HWND) -> String {
    let mut buffer = [0u16; 128];
    // SAFETY: `buffer` is a valid out-buffer; a stale handle just fails.
    let len = unsafe { GetClassNameW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn process_name(process: u32) -> String {
    let mut buffer = [0u16; 512];
    let mut len = buffer.len() as u32;
    // SAFETY: plain calls; `buffer` and `len` describe a valid out-buffer, and the
    // handle is closed after use.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process) else {
            return "?".into();
        };
        let named = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if named.is_err() {
            return "?".into();
        }
    }
    String::from_utf16_lossy(&buffer[..len as usize])
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

/// Hooks the keyboard for the window that thread `ui_thread` runs: while `capture` is
/// set and that window is in front, every key (the Windows key and Alt+Tab included)
/// goes to `deliver` as a set 1 scan code (`0xE0nn` for extended keys, [`PAUSE`])
/// instead of to Windows.
pub fn install(
    ui_thread: u32,
    capture: Arc<AtomicBool>,
    mut deliver: impl FnMut(u16, bool) + Send + 'static,
) -> Option<Hook> {
    let (keys, queued) = mpsc::channel::<(u16, bool)>();
    let delivery = thread::Builder::new()
        .name("keyboard-keys".into())
        .spawn(move || {
            for (scancode, pressed) in queued {
                tracing::trace!(scancode, pressed, "key for the guest");
                deliver(scancode, pressed);
                DELIVERED.fetch_add(1, Ordering::Relaxed);
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
                ui_thread,
                capture,
            }));
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

/// Whether the window in front is one `ui_thread` runs: the console window or its
/// popup. (Not "this process": Windows can report a classic console window as its
/// program's, and keys typed into the terminal aren't the guest's.)
fn ours_in_front(ui_thread: u32) -> bool {
    let mut process = 0u32;
    // SAFETY: plain calls; `process` is a valid out-pointer.
    unsafe {
        let window = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(window, Some(&mut process));
        let ours = thread == ui_thread && process == GetCurrentProcessId();
        if !ours {
            OTHER_WINDOW.store(window.0 as isize, Ordering::Relaxed);
            OTHER_PROCESS.store(process, Ordering::Relaxed);
        }
        ours
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
        let ui_thread = SINK.with(|sink| {
            sink.try_borrow().ok().and_then(|sink| {
                sink.as_ref()
                    .filter(|sink| sink.capture.load(Ordering::Relaxed))
                    .map(|sink| sink.ui_thread)
            })
        });
        let ours = ui_thread.is_some_and(ours_in_front);
        EVENTS.fetch_add(1, Ordering::Relaxed);
        if ours {
            IN_FRONT.fetch_add(1, Ordering::Relaxed);
        }
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
            ui_thread: 0,
            capture: Arc::new(AtomicBool::new(true)),
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
