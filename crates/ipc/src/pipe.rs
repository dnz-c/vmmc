//! The Windows plumbing: the control pipe, and the screen's memory section.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};
use windows::core::{HSTRING, PCWSTR};

use crate::screen::{SECTION_LEN, View};

/// A VM's control pipe.
pub fn pipe_path(uuid: &str) -> String {
    format!(r"\\.\pipe\vmmc-{uuid}")
}

/// The worker's end: hands out one connected pipe per client.
pub struct PipeServer {
    path: HSTRING,
    first: bool,
}

impl PipeServer {
    pub fn new(path: &str) -> Self {
        Self {
            path: HSTRING::from(path),
            first: true,
        }
    }

    /// Waits for the next client.
    pub fn accept(&mut self) -> io::Result<File> {
        self.instance()?.connect()
    }

    /// A new instance of the pipe, for the next client. The first is created with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a pipe someone else made first is refused.
    pub fn instance(&mut self) -> io::Result<Instance> {
        let mut mode = PIPE_ACCESS_DUPLEX;
        if self.first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: plain FFI with a valid name; the handle is checked below.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.path.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 << 10,
                64 << 10,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        self.first = false;
        // SAFETY: the handle is a new, owned pipe handle.
        let file = unsafe { File::from_raw_handle(handle.0 as RawHandle) };
        Ok(Instance { file })
    }
}

/// An instance of the pipe, waiting for a client.
pub struct Instance {
    file: File,
}

impl Instance {
    /// Waits for a client to connect.
    pub fn connect(self) -> io::Result<File> {
        let handle = HANDLE(self.file.as_raw_handle());
        // SAFETY: a valid pipe handle, owned by `self.file`; blocking connect.
        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) }
            && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
        {
            return Err(io::Error::from_raw_os_error(e.code().0 & 0xffff));
        }
        Ok(self.file)
    }
}

/// The client's end: connects to a worker's pipe, waiting up to `timeout` for it to
/// appear or have a free instance.
pub fn connect(path: &str, timeout: Duration) -> io::Result<File> {
    let deadline = Instant::now() + timeout;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => return Ok(file),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                let name = HSTRING::from(path);
                // SAFETY: plain FFI; waits at most a second.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1000) };
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A memory section mapped into this process.
pub struct Section {
    handle: HANDLE,
    base: MEMORY_MAPPED_VIEW_ADDRESS,
}

// SAFETY: the handle and mapping are process-wide; access goes through `View`.
unsafe impl Send for Section {}
// SAFETY: shared use only reads the handle value or makes views, whose access is
// atomic or guarded by the sequence numbers.
unsafe impl Sync for Section {}

impl Section {
    /// A new section for the screen, mapped for writing.
    pub fn create() -> io::Result<Self> {
        let len = SECTION_LEN as u64;
        // SAFETY: an anonymous section backed by the paging file.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (len >> 32) as u32,
                len as u32,
                None,
            )
        }
        .map_err(io::Error::other)?;
        Self::map(handle, FILE_MAP_WRITE)
    }

    /// Maps the section `handle` names in process `pid` for reading.
    pub fn open(pid: u32, handle: u64) -> io::Result<Self> {
        // SAFETY: plain FFI; every handle is checked and the process handle closed.
        unsafe {
            let process = OpenProcess(PROCESS_DUP_HANDLE, false, pid).map_err(io::Error::other)?;
            let mut ours = HANDLE::default();
            let duplicated = DuplicateHandle(
                process,
                HANDLE(handle as *mut _),
                GetCurrentProcess(),
                &mut ours,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            );
            let _ = CloseHandle(process);
            duplicated.map_err(io::Error::other)?;
            Self::map(ours, FILE_MAP_READ)
        }
    }

    fn map(handle: HANDLE, access: windows::Win32::System::Memory::FILE_MAP) -> io::Result<Self> {
        // SAFETY: maps the whole section; checked below.
        let base = unsafe { MapViewOfFile(handle, access, 0, 0, SECTION_LEN) };
        if base.Value.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: the handle is ours.
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle, base })
    }

    /// The handle's value in this process, for a client to duplicate.
    pub fn raw_handle(&self) -> u64 {
        self.handle.0 as u64
    }

    pub fn view(&self) -> View {
        // SAFETY: the mapping is SECTION_LEN bytes, page-aligned, and lives as long as
        // the section, which outlives every view the owner hands out.
        unsafe { View::new(self.base.Value.cast()) }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the mapping and handle are ours.
        unsafe {
            let _ = UnmapViewOfFile(self.base);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::thread;

    use super::*;
    use crate::{Request, Response, call, read_message, write_message};

    #[test]
    fn a_client_talks_to_the_server_over_the_pipe() {
        let path = pipe_path(&format!("test-{}", std::process::id()));
        let mut server = PipeServer::new(&path);
        let server_thread = thread::spawn(move || {
            let mut pipe = server.accept().unwrap();
            let request: Request = read_message(&mut pipe).unwrap();
            assert_eq!(request, Request::Status);
            write_message(&mut pipe, &Response::Done).unwrap();
            // A second instance of the same name isn't the first any more.
            assert!(!server.first);
        });
        let mut pipe = connect(&path, Duration::from_secs(5)).unwrap();
        assert_eq!(call(&mut pipe, &Request::Status).unwrap(), Response::Done);
        server_thread.join().unwrap();
        let mut rest = Vec::new();
        let _ = pipe.read_to_end(&mut rest);
        let _ = pipe.flush();
    }

    #[test]
    fn a_second_server_on_the_same_name_is_refused() {
        let path = pipe_path(&format!("squat-{}", std::process::id()));
        let mut first = PipeServer::new(&path);
        let waiting = thread::spawn(move || first.accept().map(|_| ()));
        thread::sleep(Duration::from_millis(200));
        let mut second = PipeServer::new(&path);
        assert!(second.accept().is_err());
        // Let the first one go.
        let _ = connect(&path, Duration::from_secs(5)).unwrap();
        waiting.join().unwrap().unwrap();
    }

    #[test]
    fn the_screen_section_maps_in_another_handle() {
        let section = Section::create().unwrap();
        section.view().init();
        section.view().publish_picture(2, 1, &[1, 2, 3, 4, 5, 6]);
        let reader = Section::open(std::process::id(), section.raw_handle()).unwrap();
        let view = reader.view();
        assert!(view.is_valid());
        let mut last = 0;
        assert_eq!(
            view.read_picture(&mut last).unwrap().rgb,
            [1, 2, 3, 4, 5, 6]
        );
    }
}
