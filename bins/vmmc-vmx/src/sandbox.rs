//! How this process is confined, as its own token says: the worker reports it, so
//! tests (and users) can see the sandbox is on.

#[cfg(windows)]
pub fn describe() -> String {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsTokenRestricted,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: plain FFI on this process's own token, which is closed below; the
    // buffer is large enough for any integrity label and is read only after success.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return "unknown".into();
        }
        let mut buffer = [0u64; 16];
        let mut returned = 0u32;
        let level = if GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            std::mem::size_of_val(&buffer) as u32,
            &mut returned,
        )
        .is_ok()
        {
            let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let sid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            match *GetSidSubAuthority(sid, u32::from(count).saturating_sub(1)) {
                0 => "untrusted integrity",
                0x1000 => "low integrity",
                0x2000 => "medium integrity",
                0x3000 => "high integrity",
                _ => "system integrity",
            }
        } else {
            "unknown integrity"
        };
        // TRUE (Ok) for a restricted token; FALSE (an error) otherwise.
        let restricted = IsTokenRestricted(token).is_ok();
        let _ = CloseHandle(token);
        if restricted {
            format!("{level}, restricted token")
        } else {
            level.into()
        }
    }
}

#[cfg(not(windows))]
pub fn describe() -> String {
    "unconfined".into()
}
