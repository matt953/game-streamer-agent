//! GPU telemetry via NVML, dynamically loaded like the NVENC runtime.
//!
//! Answers the field question the encode timings alone cannot: when encode
//! slows down, is the ASIC busy (encoder util), the whole GPU saturated by
//! the game (gpu util), or the silicon capped (throttle reasons, SM clock,
//! temperature)? Absence of NVML is never an error — telemetry just goes dark.

use std::ffi::c_void;
use std::sync::OnceLock;

use libloading::Library;

#[cfg(windows)]
const NVML_LIBRARY: &str = "nvml.dll";
#[cfg(not(windows))]
const NVML_LIBRARY: &str = "libnvidia-ml.so.1";

const NVML_SUCCESS: u32 = 0;
const NVML_CLOCK_SM: u32 = 1;
const NVML_TEMPERATURE_GPU: u32 = 0;

#[repr(C)]
struct Utilization {
    gpu: u32,
    /// Present for ABI layout; we only report the GPU number.
    _memory: u32,
}

type Init = unsafe extern "C" fn() -> u32;
type GetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> u32;
type GetUtilization = unsafe extern "C" fn(*mut c_void, *mut Utilization) -> u32;
type GetEncoderUtilization = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> u32;
type GetClockInfo = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32;
type GetTemperature = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32;
type GetThrottleReasons = unsafe extern "C" fn(*mut c_void, *mut u64) -> u32;

/// One reading of everything we watch. Fields are `None` where the driver
/// declined that query (older GPUs drop individual counters, not the library).
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub gpu_util: Option<u32>,
    pub encoder_util: Option<u32>,
    pub sm_mhz: Option<u32>,
    pub temp_c: Option<u32>,
    pub throttle: Option<u64>,
}

impl Sample {
    /// The interesting throttle bits, decoded for the log line.
    #[must_use]
    pub fn throttle_names(&self) -> String {
        const BITS: &[(u64, &str)] = &[
            (0x2, "app_clocks"),
            (0x4, "sw_power"),
            (0x8, "hw_slowdown"),
            (0x20, "sw_thermal"),
            (0x40, "hw_thermal"),
            (0x80, "power_brake"),
        ];
        let Some(mask) = self.throttle else {
            return "unknown".into();
        };
        let names: Vec<&str> = BITS
            .iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, name)| *name)
            .collect();
        if names.is_empty() {
            "none".into()
        } else {
            names.join(",")
        }
    }
}

/// The loaded library plus the handle for GPU 0. NVML only enumerates NVIDIA
/// GPUs, so on a single-NVIDIA-GPU machine index 0 is the encoding GPU.
pub struct Nvml {
    _library: Library,
    device: *mut c_void,
    utilization: GetUtilization,
    encoder_utilization: GetEncoderUtilization,
    clock_info: GetClockInfo,
    temperature: GetTemperature,
    throttle_reasons: GetThrottleReasons,
}

// SAFETY: NVML documents all query functions as thread-safe, and the device
// handle is an opaque token valid process-wide.
unsafe impl Send for Nvml {}
// SAFETY: see above.
unsafe impl Sync for Nvml {}

impl Nvml {
    /// Load and initialize once per process; `None` if the driver, library,
    /// or GPU 0 handle is unavailable.
    pub fn get() -> Option<&'static Nvml> {
        static NVML: OnceLock<Option<Nvml>> = OnceLock::new();
        NVML.get_or_init(|| match Self::load() {
            Ok(nvml) => Some(nvml),
            Err(e) => {
                tracing::debug!(error = e, "NVML unavailable; GPU telemetry off");
                None
            }
        })
        .as_ref()
    }

    fn load() -> std::result::Result<Nvml, String> {
        // SAFETY: loading a system library by name; its initializers are the
        // NVIDIA driver's own.
        let library = unsafe { Library::new(NVML_LIBRARY) }
            .map_err(|e| format!("{NVML_LIBRARY} not loadable: {e}"))?;

        // The raw fn pointers below are copied out of their `Symbol`s so the
        // borrow of `library` ends before it moves into the struct.
        macro_rules! symbol {
            ($ty:ty, $name:literal) => {{
                // SAFETY: the symbol's signature is fixed by the NVML ABI.
                let sym: libloading::Symbol<$ty> = unsafe { library.get($name) }.map_err(|e| {
                    format!(
                        "{}: {e}",
                        String::from_utf8_lossy($name).trim_end_matches('\0')
                    )
                })?;
                *sym
            }};
        }

        let init = symbol!(Init, b"nvmlInit_v2\0");
        let get_handle = symbol!(GetHandle, b"nvmlDeviceGetHandleByIndex_v2\0");
        let utilization = symbol!(GetUtilization, b"nvmlDeviceGetUtilizationRates\0");
        let encoder_utilization =
            symbol!(GetEncoderUtilization, b"nvmlDeviceGetEncoderUtilization\0");
        let clock_info = symbol!(GetClockInfo, b"nvmlDeviceGetClockInfo\0");
        let temperature = symbol!(GetTemperature, b"nvmlDeviceGetTemperature\0");
        let throttle_reasons = symbol!(
            GetThrottleReasons,
            b"nvmlDeviceGetCurrentClocksThrottleReasons\0"
        );

        // SAFETY: no preconditions; repeat calls are reference-counted.
        let status = unsafe { init() };
        if status != NVML_SUCCESS {
            return Err(format!("nvmlInit_v2 failed: {status}"));
        }
        let mut device = std::ptr::null_mut();
        // SAFETY: `device` is a valid out-param.
        let status = unsafe { get_handle(0, &raw mut device) };
        if status != NVML_SUCCESS {
            return Err(format!("nvmlDeviceGetHandleByIndex_v2 failed: {status}"));
        }

        Ok(Nvml {
            _library: library,
            device,
            utilization,
            encoder_utilization,
            clock_info,
            temperature,
            throttle_reasons,
        })
    }

    /// Read everything; individual queries that fail come back `None`.
    #[must_use]
    pub fn sample(&self) -> Sample {
        let mut util = Utilization { gpu: 0, _memory: 0 };
        // SAFETY: `device` is the handle NVML gave us; out-params are valid.
        let gpu_util = (unsafe { (self.utilization)(self.device, &raw mut util) } == NVML_SUCCESS)
            .then_some(util.gpu);

        let (mut enc, mut period) = (0u32, 0u32);
        // SAFETY: as above.
        let encoder_util =
            (unsafe { (self.encoder_utilization)(self.device, &raw mut enc, &raw mut period) }
                == NVML_SUCCESS)
                .then_some(enc);

        let mut mhz = 0u32;
        // SAFETY: as above.
        let sm_mhz = (unsafe { (self.clock_info)(self.device, NVML_CLOCK_SM, &raw mut mhz) }
            == NVML_SUCCESS)
            .then_some(mhz);

        let mut temp = 0u32;
        // SAFETY: as above.
        let temp_c =
            (unsafe { (self.temperature)(self.device, NVML_TEMPERATURE_GPU, &raw mut temp) }
                == NVML_SUCCESS)
                .then_some(temp);

        let mut mask = 0u64;
        // SAFETY: as above.
        let throttle = (unsafe { (self.throttle_reasons)(self.device, &raw mut mask) }
            == NVML_SUCCESS)
            .then_some(mask);

        Sample {
            gpu_util,
            encoder_util,
            sm_mhz,
            temp_c,
            throttle,
        }
    }
}
