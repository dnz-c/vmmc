//! The host's clipboard (its text), shared with the guest whose console is open.

use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Changes whenever anything sets the clipboard.
pub fn sequence() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetClipboardSequenceNumber() }
}

/// The clipboard, open until dropped.
struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this process opened it.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Opens the clipboard, waiting a little for whoever has it.
fn open() -> Option<Open> {
    for _ in 0..10 {
        // SAFETY: no window owns it; this thread does until `Open` drops.
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Some(Open);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// The clipboard's text, if it has some.
pub fn text() -> Option<String> {
    let format = u32::from(CF_UNICODETEXT.0);
    // SAFETY: plain query.
    unsafe { IsClipboardFormatAvailable(format) }.ok()?;
    let _open = open()?;
    // SAFETY: the clipboard is open; the handle is valid until it closes.
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    // SAFETY: CF_UNICODETEXT data is a global block of UTF-16 text.
    let pointer = unsafe { GlobalLock(global) }.cast::<u16>();
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the block is GlobalSize bytes, locked until GlobalUnlock.
    let units = unsafe { std::slice::from_raw_parts(pointer, GlobalSize(global) / 2) };
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    // SAFETY: locked above.
    let _ = unsafe { GlobalUnlock(global) };
    Some(text)
}

/// Puts `text` on the clipboard.
pub fn set_text(text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let Some(_open) = open() else {
        return false;
    };
    // SAFETY: the clipboard is open. The block is allocated, filled while locked, and
    // either handed to the clipboard (which then owns it) or freed.
    unsafe {
        if EmptyClipboard().is_err() {
            return false;
        }
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) else {
            return false;
        };
        let pointer = GlobalLock(global).cast::<u16>();
        if pointer.is_null() {
            let _ = GlobalFree(Some(global));
            return false;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), pointer, units.len());
        let _ = GlobalUnlock(global);
        if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0))).is_err() {
            let _ = GlobalFree(Some(global));
            return false;
        }
    }
    true
}
