//! Dynamic loading of the NVENC runtime.
//!
//! The driver ships the library; nothing is linked at build time and no SDK is
//! required to compile. A machine with no NVIDIA driver simply fails to load
//! it, which is a `Result`, never a panic — the agent falls back to software.

mod ffi;

pub use ffi::*;

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;

use libloading::Library;

use gsa_core::{Error, Result};

/// The driver's NVENC runtime. Linux ships the same API as
/// `libnvidia-encode.so.1`; only this name and the device/resource type
/// constants differ.
#[cfg(windows)]
const NVENC_LIBRARY: &str = "nvEncodeAPI64.dll";
#[cfg(not(windows))]
const NVENC_LIBRARY: &str = "libnvidia-encode.so.1";

type CreateInstance = unsafe extern "system" fn(*mut NvEncodeApiFunctionList) -> NvencStatus;
type GetMaxVersion = unsafe extern "system" fn(*mut u32) -> NvencStatus;

/// The loaded runtime: the dispatch table plus the library that owns it.
pub struct Nvenc {
    /// Kept alive: the function pointers below point into it.
    _library: Library,
    functions: NvEncodeApiFunctionList,
}

// SAFETY: the dispatch table is immutable once loaded, and NVENC's own
// documentation permits calls from any thread. The `Library` handle is only
// read.
unsafe impl Send for Nvenc {}
// SAFETY: see above.
unsafe impl Sync for Nvenc {}

impl std::fmt::Debug for Nvenc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nvenc").finish_non_exhaustive()
    }
}

impl Nvenc {
    /// Load the runtime once per process.
    ///
    /// `Err` on any machine without a suitable NVIDIA driver — no GPU, no
    /// driver, or a driver older than the API version we speak. Callers treat
    /// that as "no hardware encoder", not as a failure.
    pub fn get() -> Result<&'static Nvenc> {
        static NVENC: OnceLock<std::result::Result<Nvenc, String>> = OnceLock::new();
        NVENC
            .get_or_init(|| Self::load().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| Error::Encode(e.clone()))
    }

    fn load() -> Result<Nvenc> {
        // SAFETY: loading a system library by name. Its initializers are the
        // NVIDIA driver's own.
        let library = unsafe { Library::new(NVENC_LIBRARY) }
            .map_err(|e| Error::Encode(format!("{NVENC_LIBRARY} not loadable: {e}")))?;

        // The symbols borrow `library`, so they live in inner scopes: the
        // borrow must end before `library` moves into the returned struct.
        let driver = {
            // SAFETY: the symbol's signature is fixed by the NVENC ABI.
            let max_version: libloading::Symbol<GetMaxVersion> = unsafe {
                library.get(b"NvEncodeAPIGetMaxSupportedVersion\0")
            }
            .map_err(|e| Error::Encode(format!("NvEncodeAPIGetMaxSupportedVersion: {e}")))?;
            let mut driver = 0u32;
            // SAFETY: `driver` is a valid out-param.
            let status = unsafe { max_version(&raw mut driver) };
            check(status, "NvEncodeAPIGetMaxSupportedVersion")?;
            driver
        };

        // The driver reports `(major << 4) | minor`. NVENC is backward
        // compatible, so an *older* driver than we speak is the only failure.
        let (driver_major, driver_minor) = (driver >> 4, driver & 0xf);
        if (driver_major, driver_minor) < (NVENC_MAJOR_VERSION, NVENC_MINOR_VERSION) {
            return Err(Error::Encode(format!(
                "NVIDIA driver supports NVENC API {driver_major}.{driver_minor}, \
                 this build needs {NVENC_MAJOR_VERSION}.{NVENC_MINOR_VERSION}"
            )));
        }

        let functions = {
            // SAFETY: the symbol's signature is fixed by the NVENC ABI.
            let create: libloading::Symbol<CreateInstance> =
                unsafe { library.get(b"NvEncodeAPICreateInstance\0") }
                    .map_err(|e| Error::Encode(format!("NvEncodeAPICreateInstance: {e}")))?;
            // SAFETY: all-zero is the documented initial state; only `version`
            // is read by the driver, and it fills the rest.
            let mut functions: NvEncodeApiFunctionList = unsafe { std::mem::zeroed() };
            functions.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            // SAFETY: `functions` is a correctly versioned, correctly sized table.
            let status = unsafe { create(&raw mut functions) };
            check(status, "NvEncodeAPICreateInstance")?;
            functions
        };

        tracing::debug!(
            driver_api = format!("{driver_major}.{driver_minor}"),
            "NVENC runtime loaded"
        );
        Ok(Nvenc {
            _library: library,
            functions,
        })
    }

    pub(crate) fn functions(&self) -> &NvEncodeApiFunctionList {
        &self.functions
    }
}

/// Turn a status into a `Result`, naming the call that produced it.
pub(crate) fn check(status: NvencStatus, call: &str) -> Result<()> {
    if status == NV_ENC_SUCCESS {
        return Ok(());
    }
    Err(Error::Encode(format!("{call}: NVENC status {status}")))
}

/// The driver's own description of the last failure on this encoder, if any.
pub(crate) fn last_error(nvenc: &Nvenc, encoder: *mut c_void) -> String {
    let Some(get) = nvenc.functions().nvEncGetLastErrorString else {
        return String::new();
    };
    // SAFETY: `encoder` is a live session handle; the driver returns a
    // NUL-terminated string it owns, valid until the next call.
    let ptr = unsafe { get(encoder) };
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null, NUL-terminated, driver-owned.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
