//! A TPM 2.0 for vmmc's VMs: Microsoft's reference implementation (ms-tpm-20-ref),
//! built with OpenSSL by the native-deps workflow and linked statically.
//!
//! - **One per process.** The reference implementation keeps its state in C globals, so
//!   [`Tpm::open`] succeeds once per process; vmmc runs one VM per process anyway.
//! - **State:** the TPM's NV memory is saved to a file whenever a command changes it. A
//!   file that doesn't exist yet means a newly manufactured TPM: new seeds, a new
//!   endorsement key, empty NV.
//! - **Resets:** [`Tpm::reset`] is the platform reset (`_TPM_Init`), which clears the
//!   PCRs. The firmware then sends TPM2_Startup.
//! - **Manufacture** includes what a TPM vendor adds: an endorsement key certificate
//!   in NV (see `provision`).
//!
//! Builds without the native libraries (see `build.rs`) have a stub whose
//! [`Tpm::open`] says how to get them.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(tpm_ref)]
mod provision;

/// The size of the reference implementation's NV memory (`NV_MEMORY_SIZE`), and so of
/// a TPM state file.
pub const NV_SIZE: usize = 16384;
/// The largest command or response the TPM handles (`MAX_COMMAND_SIZE`,
/// `MAX_RESPONSE_SIZE`).
pub const MAX_MESSAGE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("this vmmc was built without its TPM: run `cargo xtask fetch-native` and rebuild")]
    NotBuilt,
    #[error("a TPM is already open in this process")]
    InUse,
    #[error("TPM state {}: {source}", path.display())]
    State { path: PathBuf, source: io::Error },
    #[error("TPM state {} isn't {NV_SIZE} bytes", .0.display())]
    BadState(PathBuf),
    #[error("manufacturing the TPM failed ({0})")]
    Manufacture(i32),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The process's TPM.
pub struct Tpm {
    #[cfg(tpm_ref)]
    inner: imp::Inner,
    #[cfg(not(tpm_ref))]
    never: std::convert::Infallible,
}

impl Tpm {
    /// Powers on the TPM with its NV memory in `state`: loaded from the file, or, if
    /// there is none, a newly manufactured TPM saved there.
    pub fn open(state: &Path) -> Result<Self> {
        #[cfg(tpm_ref)]
        return imp::Inner::open(state).map(|inner| Self { inner });
        #[cfg(not(tpm_ref))]
        {
            let _ = state;
            Err(Error::NotBuilt)
        }
    }

    /// Whether this build has a TPM to open.
    pub const fn available() -> bool {
        cfg!(tpm_ref)
    }

    /// Runs one command sent from `locality` and returns the response.
    pub fn execute(&mut self, locality: u8, command: &[u8]) -> Vec<u8> {
        #[cfg(tpm_ref)]
        return self.inner.execute(locality, command);
        #[cfg(not(tpm_ref))]
        {
            let _ = (locality, command);
            match self.never {}
        }
    }

    /// The platform reset: `_TPM_Init`, which clears the PCRs and expects TPM2_Startup.
    pub fn reset(&mut self) {
        #[cfg(tpm_ref)]
        self.inner.reset();
        #[cfg(not(tpm_ref))]
        match self.never {}
    }

    /// All of NV memory: the TPM's persistent state, and after TPM2_Shutdown(STATE)
    /// its volatile state too.
    pub fn nv(&self) -> Vec<u8> {
        #[cfg(tpm_ref)]
        return self.inner.nv();
        #[cfg(not(tpm_ref))]
        match self.never {}
    }

    /// Replaces NV memory, and the state file, with `nv`: the TPM is powered off and
    /// on again with it, and a platform reset follows.
    pub fn set_nv(&mut self, nv: &[u8]) -> Result<()> {
        #[cfg(tpm_ref)]
        return self.inner.set_nv(nv);
        #[cfg(not(tpm_ref))]
        {
            let _ = nv;
            match self.never {}
        }
    }
}

#[cfg(tpm_ref)]
mod ffi {
    use std::ffi::{c_int, c_uchar, c_uint, c_void};

    pub const MANUF_FIRST_TIME: c_int = 1;
    pub const MANUF_OK: c_int = 0;

    // From ms-tpm-20-ref's platform_public_interface.h and Manufacture_fp.h.
    unsafe extern "C" {
        pub fn _plat__NVEnable(parameter: *mut c_void, size: usize) -> c_int;
        pub fn _plat__NVDisable(parameter: *mut c_void, size: usize);
        pub fn _plat__NvMemoryRead(start: c_uint, size: c_uint, data: *mut c_void) -> c_int;
        pub fn _plat__NvMemoryWrite(start: c_uint, size: c_uint, data: *mut c_void) -> c_int;
        pub fn _plat__SetNvAvail();
        pub fn _plat__Signal_PowerOn() -> c_int;
        pub fn _plat__Signal_Reset() -> c_int;
        pub fn _plat__Signal_PowerOff();
        pub fn _plat__LocalitySet(locality: c_uchar);
        pub fn _plat__RunCommand(
            request_size: u32,
            request: *mut c_uchar,
            response_size: *mut u32,
            response: *mut *mut c_uchar,
        );
        pub fn TPM_Manufacture(first_time: c_int) -> c_int;
    }
}

#[cfg(tpm_ref)]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Error, MAX_MESSAGE, NV_SIZE, Result, ffi};

    /// Whether the process's one TPM is open.
    static OPEN: AtomicBool = AtomicBool::new(false);

    pub struct Inner {
        path: PathBuf,
        /// The NV image as the state file has it.
        saved: Vec<u8>,
    }

    impl Inner {
        pub fn open(path: &Path) -> Result<Self> {
            if OPEN.swap(true, Ordering::AcqRel) {
                return Err(Error::InUse);
            }
            let state = |source| Error::State {
                path: path.to_owned(),
                source,
            };
            let loaded = match fs::read(path) {
                Ok(image) if image.len() == NV_SIZE => Some(image),
                Ok(_) => {
                    OPEN.store(false, Ordering::Release);
                    return Err(Error::BadState(path.to_owned()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    OPEN.store(false, Ordering::Release);
                    return Err(state(e));
                }
            };
            let mut inner = Self {
                path: path.to_owned(),
                saved: Vec::new(),
            };
            // SAFETY: OPEN makes this the only user of the library's globals. The NV
            // image is NV_SIZE bytes, all of NV memory.
            unsafe {
                ffi::_plat__NVEnable(ptr::null_mut(), 0);
                match &loaded {
                    Some(image) => {
                        ffi::_plat__NvMemoryWrite(
                            0,
                            NV_SIZE as u32,
                            image.as_ptr().cast_mut().cast(),
                        );
                    }
                    None => {
                        // A fresh NV image for a new TPM, in case one was open before.
                        let blank = vec![0u8; NV_SIZE];
                        ffi::_plat__NvMemoryWrite(
                            0,
                            NV_SIZE as u32,
                            blank.as_ptr().cast_mut().cast(),
                        );
                        let status = ffi::TPM_Manufacture(ffi::MANUF_FIRST_TIME);
                        if status != ffi::MANUF_OK {
                            OPEN.store(false, Ordering::Release);
                            return Err(Error::Manufacture(status));
                        }
                        tracing::info!(path = %path.display(), "manufactured a new TPM");
                    }
                }
                ffi::_plat__SetNvAvail();
                ffi::_plat__Signal_PowerOn();
                ffi::_plat__Signal_Reset();
            }
            match loaded {
                Some(image) => inner.saved = image,
                None => {
                    // Still powered on, before TPM2_Startup.
                    let provisioned = super::provision::endorsement_certificate(&mut |command| {
                        inner.execute(0, command)
                    });
                    if let Err(e) = provisioned {
                        tracing::warn!(%e, "the TPM has no endorsement key certificate");
                    }
                    // The guest's boot starts with a platform reset.
                    // SAFETY: OPEN makes this the only user of the library's globals.
                    unsafe {
                        ffi::_plat__Signal_Reset();
                    }
                    inner.save().map_err(state)?;
                }
            }
            Ok(inner)
        }

        pub fn execute(&mut self, locality: u8, command: &[u8]) -> Vec<u8> {
            let mut request = command[..command.len().min(MAX_MESSAGE)].to_vec();
            let mut buffer = vec![0u8; MAX_MESSAGE];
            let mut response = buffer.as_mut_ptr();
            let mut size = MAX_MESSAGE as u32;
            // SAFETY: `request` and `buffer` outlive the call and are the sizes passed.
            // The library answers in `buffer` or in a buffer of its own, which stays
            // valid until the next command; it's copied out before then.
            let reply = unsafe {
                ffi::_plat__LocalitySet(locality);
                ffi::_plat__RunCommand(
                    request.len() as u32,
                    request.as_mut_ptr(),
                    &mut size,
                    &mut response,
                );
                std::slice::from_raw_parts(response, (size as usize).min(MAX_MESSAGE)).to_vec()
            };
            if let Err(e) = self.save() {
                tracing::warn!(path = %self.path.display(), %e, "can't save the TPM state");
            }
            reply
        }

        pub fn reset(&mut self) {
            // SAFETY: OPEN makes this the only user of the library's globals.
            unsafe {
                ffi::_plat__Signal_Reset();
            }
        }

        pub fn nv(&self) -> Vec<u8> {
            let mut image = vec![0u8; NV_SIZE];
            // SAFETY: `image` is NV_SIZE bytes, all of NV memory.
            unsafe {
                ffi::_plat__NvMemoryRead(0, NV_SIZE as u32, image.as_mut_ptr().cast());
            }
            image
        }

        pub fn set_nv(&mut self, nv: &[u8]) -> Result<()> {
            if nv.len() != NV_SIZE {
                return Err(Error::BadState(self.path.clone()));
            }
            // SAFETY: OPEN makes this the only user of the library's globals. The image
            // is NV_SIZE bytes, all of NV memory.
            unsafe {
                ffi::_plat__Signal_PowerOff();
                ffi::_plat__NvMemoryWrite(0, NV_SIZE as u32, nv.as_ptr().cast_mut().cast());
                ffi::_plat__SetNvAvail();
                ffi::_plat__Signal_PowerOn();
                ffi::_plat__Signal_Reset();
            }
            self.save().map_err(|source| Error::State {
                path: self.path.clone(),
                source,
            })
        }

        /// Writes NV memory to the state file if it changed, through a temporary file
        /// so a crash never leaves half of it.
        fn save(&mut self) -> std::io::Result<()> {
            let mut image = vec![0u8; NV_SIZE];
            // SAFETY: `image` is NV_SIZE bytes, all of NV memory.
            unsafe {
                ffi::_plat__NvMemoryRead(0, NV_SIZE as u32, image.as_mut_ptr().cast());
            }
            if image == self.saved {
                return Ok(());
            }
            let temporary = self.path.with_extension("tmp");
            fs::write(&temporary, &image)?;
            fs::rename(&temporary, &self.path)?;
            self.saved = image;
            Ok(())
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            // SAFETY: OPEN makes this the only user of the library's globals.
            unsafe {
                ffi::_plat__Signal_PowerOff();
            }
            OPEN.store(false, Ordering::Release);
        }
    }
}

#[cfg(all(test, tpm_ref))]
mod tests {
    use super::*;

    const STARTUP_CLEAR: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x44, 0, 0];
    const GET_RANDOM_8: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x7b, 0, 8];

    fn response_code(response: &[u8]) -> u32 {
        u32::from_be_bytes(response[6..10].try_into().unwrap())
    }

    /// The EK certificate, read with owner authorization (TPM2_NV_Read): its DER
    /// header first, for the length.
    fn read_ek_certificate(tpm: &mut Tpm) -> Vec<u8> {
        const OWNER: u32 = 0x4000_0001;
        let mut read = |offset: usize, size: usize| {
            let command = provision::Command::new(provision::ST_SESSIONS, 0x14e)
                .u32(OWNER)
                .u32(provision::EK_CERTIFICATE_INDEX)
                .password()
                .u16(size as u16)
                .u16(offset as u16)
                .finish();
            let response = tpm.execute(0, &command);
            assert_eq!(response_code(&response), 0, "NV_Read");
            // Header, parameter size, then the data as a TPM2B.
            let len = usize::from(u16::from_be_bytes([response[14], response[15]]));
            response[16..16 + len].to_vec()
        };
        let mut certificate = read(0, 4);
        assert_eq!(&certificate[..2], &[0x30, 0x82], "a DER SEQUENCE");
        let total = 4 + usize::from(u16::from_be_bytes([certificate[2], certificate[3]]));
        while certificate.len() < total {
            let size = (total - certificate.len()).min(512);
            let chunk = read(certificate.len(), size);
            certificate.extend_from_slice(&chunk);
        }
        certificate
    }

    // One test: the library allows one open TPM per process.
    #[test]
    fn manufactures_runs_commands_and_keeps_its_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tpm-state.bin");

        let mut tpm = Tpm::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), NV_SIZE as u64);
        assert!(matches!(Tpm::open(&path), Err(Error::InUse)));
        assert_eq!(response_code(&tpm.execute(0, &STARTUP_CLEAR)), 0);
        let random = tpm.execute(0, &GET_RANDOM_8);
        assert_eq!(response_code(&random), 0);
        assert_eq!(&random[10..12], &[0, 8], "eight random bytes");
        // Started already: a second Startup is TPM_RC_INITIALIZE.
        assert_eq!(response_code(&tpm.execute(0, &STARTUP_CLEAR)), 0x100);
        tpm.reset();
        assert_eq!(response_code(&tpm.execute(0, &STARTUP_CLEAR)), 0);

        // Manufacture left an EK certificate, for the EK the guest makes.
        let certificate = read_ek_certificate(&mut tpm);
        assert_eq!(certificate[0], 0x30, "DER");
        let (_, ek) = provision::create_primary(
            &mut |command| tpm.execute(0, command),
            provision::RH_ENDORSEMENT,
            &provision::ek_template(),
        )
        .unwrap();
        assert!(
            certificate
                .windows(256)
                .any(|window| window == ek[0].as_slice()),
            "the certificate holds the EK's modulus"
        );

        // Saved as a suspend saves it, and taken back, as a snapshot does: it resumes.
        const SHUTDOWN_STATE: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x45, 0, 1];
        const STARTUP_STATE: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x44, 0, 1];
        assert_eq!(response_code(&tpm.execute(0, &SHUTDOWN_STATE)), 0);
        let suspended = tpm.nv();
        assert_eq!(suspended.len(), NV_SIZE);
        tpm.set_nv(&suspended).unwrap();
        assert_eq!(response_code(&tpm.execute(0, &STARTUP_STATE)), 0);
        assert_eq!(response_code(&tpm.execute(0, &GET_RANDOM_8)), 0);
        assert_eq!(std::fs::read(&path).unwrap().len(), NV_SIZE);
        drop(tpm);

        // The same TPM again, from its state file.
        let saved = std::fs::read(&path).unwrap();
        let mut tpm = Tpm::open(&path).unwrap();
        assert_eq!(response_code(&tpm.execute(0, &STARTUP_CLEAR)), 0);
        drop(tpm);
        assert_eq!(std::fs::read(&path).unwrap().len(), saved.len());

        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(Tpm::open(&path), Err(Error::BadState(_))));
    }
}

#[cfg(all(test, not(tpm_ref)))]
mod tests {
    use super::*;

    #[test]
    fn says_how_to_get_the_tpm() {
        assert!(!Tpm::available());
        let error = Tpm::open(Path::new("unused")).err().unwrap();
        assert!(error.to_string().contains("fetch-native"), "{error}");
    }
}
