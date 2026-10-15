//! The worker's sandbox on Windows.
//!
//! - **Token:** the user's, restricted: administrator groups deny-only, every privilege
//!   but traversal dropped, and a second access check against restricting SIDs (the
//!   user, Everyone, Users, Authenticated Users and the logon session), at low
//!   integrity. So the worker can read what the user can, but write only to objects
//!   labeled low: its VM's folder, which [`allow_writes`] labels.
//! - **Job:** no child processes, and no access to the desktop, clipboard or other
//!   processes' windows.
//! - **Handles:** it inherits exactly its log (as stdout and stderr) and a null stdin.
//!
//! WHP works in this sandbox: the worker creates its partition at low integrity.

use std::fs::{File, OpenOptions};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, LocalFree, SetHandleInformation,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    GetSecurityDescriptorSacl, GetTokenInformation, LABEL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TOKEN_USER, TokenGroups, TokenIntegrityLevel, TokenUser,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Whether this process runs on the interactive window station (`WinSta0`), the
/// desktop's. Only there can a sandboxed process start: elsewhere (a service, an SSH
/// session, a CI runner) its restricted token gets no access to the window station,
/// and Windows fails its DLLs' start-up (STATUS_DLL_INIT_FAILED).
pub fn interactive() -> bool {
    use windows::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
    };
    let mut name = [0u16; 64];
    // SAFETY: plain calls; the station handle isn't ours to close, and the buffer's
    // size in bytes is passed.
    let named = unsafe {
        GetProcessWindowStation().and_then(|station| {
            GetUserObjectInformationW(
                HANDLE(station.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                std::mem::size_of_val(&name) as u32,
                None,
            )
        })
    };
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    named.is_ok() && String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("WinSta0")
}

/// How a sandboxed process ends when its desktop doesn't let it in: Windows can't
/// start its DLLs (user32's needs the window station and desktop). Seen on GitHub's
/// hosted runners, whose desktop doesn't admit a restricted, low-integrity token.
pub const DLL_INIT_FAILED: u32 = 0xc000_0142;

/// Token group flags (in SystemServices, which this crate doesn't otherwise need).
const SE_GROUP_INTEGRITY: u32 = 0x20;
const SE_GROUP_LOGON_ID: u32 = 0xc000_0000;

/// A started worker.
pub struct Process {
    handle: OwnedHandle,
}

impl Process {
    /// The exit code, once it has exited.
    pub fn exit_code(&mut self) -> Option<u32> {
        let handle = HANDLE(self.handle.as_raw_handle());
        // SAFETY: a valid process handle we own.
        unsafe {
            if WaitForSingleObject(handle, 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(handle, &mut code).ok()?;
            Some(code)
        }
    }

    pub fn id(&self) -> u32 {
        // SAFETY: a valid process handle we own.
        unsafe {
            windows::Win32::System::Threading::GetProcessId(HANDLE(self.handle.as_raw_handle()))
        }
    }
}

fn error(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

/// Labels `dir` (and what's in it, and what will be) low integrity, so the sandboxed
/// worker may write there. On file systems without labels (FAT), there's nothing to do.
pub fn allow_writes(dir: &Path) -> Result<(), String> {
    const ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;
    match set_low_label(dir) {
        // Setting a label takes WRITE_OWNER, which a folder on a data drive often
        // doesn't grant (Modify doesn't include it); the owner may grant it.
        Err(e) if e.code().0 == ACCESS_DENIED => {
            grant_label_rights(dir)?;
            set_low_label(dir)
        }
        other => other,
    }
    .or_else(|e| {
        // ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION: no security on this volume.
        if [0x8007_0032_u32 as i32, 0x8007_0001_u32 as i32].contains(&e.code().0) {
            Ok(())
        } else {
            Err(error(
                &format!("can't let the VM write to {}", dir.display()),
                e,
            ))
        }
    })
}

fn set_low_label(dir: &Path) -> windows::core::Result<()> {
    // Low integrity, no write up, inherited by files and folders.
    let sddl = HSTRING::from("S:(ML;OICI;NW;;;LW)");
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: plain FFI; the descriptor is freed below, the SACL points into it.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let mut present = windows::core::BOOL::default();
        let mut defaulted = windows::core::BOOL::default();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let got = GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted);
        let path = HSTRING::from(dir.as_os_str());
        let result = got.and_then(|()| {
            SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                None,
                None,
                None,
                Some(sacl),
            )
            .ok()
        });
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

/// Grants the user WRITE_OWNER on `dir` and everything in it, which labeling needs.
/// The folder's owner may always change its access list.
fn grant_label_rights(dir: &Path) -> Result<(), String> {
    const WRITE_OWNER: u32 = 0x0008_0000;
    // SAFETY: plain FFI; every buffer returned is freed here, and the user's SID
    // outlives the calls that point at it.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| error("can't open the user's token", e))?;
        let own = OwnedHandle::from_raw_handle(token.0);
        let (mut user, _) = token_sids(HANDLE(own.as_raw_handle()))?;
        let path = HSTRING::from(dir.as_os_str());
        let mut old: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old),
            None,
            &mut descriptor,
        )
        .ok()
        .map_err(|e| error(&format!("can't read {}'s access list", dir.display()), e))?;
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: WRITE_OWNER,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: PWSTR(user.as_mut_ptr().cast()),
                ..TRUSTEE_W::default()
            },
        };
        let mut new: *mut ACL = std::ptr::null_mut();
        let made = SetEntriesInAclW(Some(&[access]), Some(old), &mut new).ok();
        let result = made.and_then(|()| {
            SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new),
                None,
            )
            .ok()
        });
        if !new.is_null() {
            let _ = LocalFree(Some(HLOCAL(new.cast())));
        }
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result.map_err(|e| error(&format!("can't change {}'s access list", dir.display()), e))
    }
}

/// A token's SID of `class` (TokenUser) or the logon SID among its groups.
unsafe fn token_sids(token: HANDLE) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let query = |class| -> Result<Vec<u64>, String> {
        let mut len = 0u32;
        // SAFETY: a size query, then a call with a buffer of that size.
        unsafe {
            let _ = GetTokenInformation(token, class, None, 0, &mut len);
            let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
            GetTokenInformation(
                token,
                class,
                Some(buffer.as_mut_ptr().cast()),
                len,
                &mut len,
            )
            .map_err(|e| error("can't read the user's token", e))?;
            Ok(buffer)
        }
    };
    let copy_sid = |sid: PSID| -> Vec<u8> {
        // SAFETY: a valid SID inside a buffer we hold.
        unsafe {
            let len = windows::Win32::Security::GetLengthSid(sid) as usize;
            std::slice::from_raw_parts(sid.0.cast::<u8>(), len).to_vec()
        }
    };
    let user = query(TokenUser)?;
    // SAFETY: the buffer holds a TOKEN_USER.
    let user_sid = copy_sid(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid });
    let groups = query(TokenGroups)?;
    // SAFETY: the buffer holds a TOKEN_GROUPS with GroupCount entries.
    let logon = unsafe {
        let groups = &*groups.as_ptr().cast::<TOKEN_GROUPS>();
        std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
            .iter()
            .find(|g| g.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
            .map(|g| copy_sid(g.Sid))
    };
    Ok((user_sid, logon))
}

fn well_known(sddl: &str) -> Result<SidBuf, String> {
    let text = HSTRING::from(sddl);
    let mut sid = PSID::default();
    // SAFETY: plain FFI; the SID is freed by SidBuf.
    unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid) }
        .map_err(|e| error("can't make a SID", e))?;
    Ok(SidBuf::Local(sid))
}

enum SidBuf {
    Local(PSID),
    Owned(Vec<u8>),
}

impl SidBuf {
    fn psid(&mut self) -> PSID {
        match self {
            SidBuf::Local(sid) => *sid,
            SidBuf::Owned(bytes) => PSID(bytes.as_mut_ptr().cast()),
        }
    }
}

impl Drop for SidBuf {
    fn drop(&mut self) {
        if let SidBuf::Local(sid) = self {
            // SAFETY: allocated by ConvertStringSidToSidW.
            let _ = unsafe { LocalFree(Some(HLOCAL(sid.0))) };
        }
    }
}

/// The sandbox token: see the module's description.
fn sandbox_token() -> Result<OwnedHandle, String> {
    // SAFETY: plain FFI on this process's token; every handle is owned or closed.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token)
            .map_err(|e| error("can't open the user's token", e))?;
        let own = OwnedHandle::from_raw_handle(token.0);
        let (user, logon) = token_sids(token)?;
        let mut admins = well_known("S-1-5-32-544")?;
        let mut restricting = vec![
            SidBuf::Owned(user),
            well_known("S-1-1-0")?,
            well_known("S-1-5-32-545")?,
            well_known("S-1-5-11")?,
        ];
        if let Some(logon) = logon {
            restricting.push(SidBuf::Owned(logon));
        }
        let disable = [SID_AND_ATTRIBUTES {
            Sid: admins.psid(),
            Attributes: 0,
        }];
        let restrict: Vec<SID_AND_ATTRIBUTES> = restricting
            .iter_mut()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.psid(),
                Attributes: 0,
            })
            .collect();
        let mut restricted = HANDLE::default();
        CreateRestrictedToken(
            HANDLE(own.as_raw_handle()),
            DISABLE_MAX_PRIVILEGE,
            Some(&disable),
            None,
            Some(&restrict),
            &mut restricted,
        )
        .map_err(|e| error("can't make the sandbox token", e))?;
        let restricted = OwnedHandle::from_raw_handle(restricted.0);
        let mut low = well_known("S-1-16-4096")?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: low.psid(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        SetTokenInformation(
            HANDLE(restricted.as_raw_handle()),
            TokenIntegrityLevel,
            (&raw const label).cast(),
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        )
        .map_err(|e| error("can't lower the sandbox's integrity", e))?;
        Ok(restricted)
    }
}

/// A job that keeps its process from starting others or touching the desktop.
fn sandbox_job() -> Result<OwnedHandle, String> {
    // SAFETY: plain FFI; the job handle is owned.
    unsafe {
        let job = CreateJobObjectW(None, None).map_err(|e| error("can't make the job", e))?;
        let job = OwnedHandle::from_raw_handle(job.0);
        let handle = HANDLE(job.as_raw_handle());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
        .map_err(|e| error("can't limit the job", e))?;
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };
        SetInformationJobObject(
            handle,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast(),
            std::mem::size_of_val(&ui) as u32,
        )
        .map_err(|e| error("can't limit the job's UI", e))?;
        Ok(job)
    }
}

fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    // The usual rules: backslashes before a quote are doubled.
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn inheritable(file: &File) -> Result<HANDLE, String> {
    let handle = HANDLE(file.as_raw_handle());
    // SAFETY: a valid handle owned by `file`.
    unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .map_err(|e| error("can't pass the log to the VM", e))?;
    Ok(handle)
}

/// Starts `exe` with `args`, its output in `log` (replaced), detached from this
/// process, in the sandbox unless `sandboxed` is false.
pub fn spawn(exe: &Path, args: &[String], log: &Path, sandboxed: bool) -> Result<Process, String> {
    let log_file =
        File::create(log).map_err(|e| error(&format!("can't create {}", log.display()), e))?;
    let null = OpenOptions::new()
        .read(true)
        .open("NUL")
        .map_err(|e| error("can't open NUL", e))?;
    let handles = [inheritable(&log_file)?, inheritable(&null)?];
    let mut command: Vec<u16> = std::iter::once(quote(&exe.display().to_string()))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain([0])
        .collect();
    let exe_name = HSTRING::from(exe.as_os_str());
    let token = if sandboxed {
        Some(sandbox_token()?)
    } else {
        None
    };
    let job = if sandboxed {
        Some(sandbox_job()?)
    } else {
        None
    };
    // SAFETY: plain FFI; the attribute list is initialized, used and deleted here, and
    // outlives the CreateProcess call; every returned handle is owned or closed.
    unsafe {
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut list_buffer = vec![0u64; size.div_ceil(8)];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buffer.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)
            .map_err(|e| error("can't set up the VM's handles", e))?;
        let updated = UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            std::mem::size_of_val(&handles),
            None,
            None,
        );
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[1];
        startup.StartupInfo.hStdOutput = handles[0];
        startup.StartupInfo.hStdError = handles[0];
        startup.lpAttributeList = list;
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_NO_WINDOW
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_SUSPENDED;
        let mut info = PROCESS_INFORMATION::default();
        let created = updated.and_then(|()| match &token {
            Some(token) => CreateProcessAsUserW(
                Some(HANDLE(token.as_raw_handle())),
                PCWSTR(exe_name.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                None,
                None,
                &startup.StartupInfo,
                &mut info,
            ),
            None => CreateProcessW(
                PCWSTR(exe_name.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                None,
                None,
                &startup.StartupInfo,
                &mut info,
            ),
        });
        DeleteProcThreadAttributeList(list);
        created.map_err(|e| error(&format!("can't start {}", exe.display()), e))?;
        let process = OwnedHandle::from_raw_handle(info.hProcess.0);
        let thread = info.hThread;
        if let Some(job) = &job
            && let Err(e) = AssignProcessToJobObject(HANDLE(job.as_raw_handle()), info.hProcess)
        {
            let _ = windows::Win32::System::Threading::TerminateProcess(info.hProcess, 1);
            let _ = CloseHandle(thread);
            return Err(error("can't put the VM in its job", e));
        }
        ResumeThread(thread);
        let _ = CloseHandle(thread);
        Ok(Process { handle: process })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_arguments_as_windows_parses_them() {
        assert_eq!(quote("serve"), "serve");
        assert_eq!(quote(r"C:\VMs\My VM\x.vmmc"), r#""C:\VMs\My VM\x.vmmc""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(""), r#""""#);
    }

    #[test]
    fn a_sandboxed_process_runs_at_low_integrity_and_can_write_only_where_allowed() {
        if !interactive() {
            eprintln!("skipped: not on the interactive desktop (a CI runner?)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("vm");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::write(allowed.join("before.txt"), "x").unwrap();
        allow_writes(&allowed).unwrap();
        let log = allowed.join("log.txt");
        // Only cmd's built-ins: the job lets the process start no others. The temp
        // directory's paths have no spaces, so they need no quotes inside the script.
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        let script = format!(
            "echo inside> {} & echo outside> {} & echo again>> {}",
            allowed.join("new.txt").display(),
            dir.path().join("outside.txt").display(),
            allowed.join("before.txt").display(),
        );
        let mut process = spawn(cmd, &["/c".into(), script], &log, true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while process.exit_code().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if process.exit_code() == Some(DLL_INIT_FAILED) {
            eprintln!("skipped: this desktop doesn't admit sandboxed processes (a CI runner?)");
            return;
        }
        let output = format!(
            "exit {:?}: {}",
            process.exit_code(),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(allowed.join("new.txt").exists(), "writes inside: {output}");
        assert!(
            !dir.path().join("outside.txt").exists(),
            "no writes outside: {output}"
        );
        assert!(
            std::fs::read_to_string(allowed.join("before.txt"))
                .unwrap()
                .contains("again"),
            "files that were there are writable too"
        );
    }
}
