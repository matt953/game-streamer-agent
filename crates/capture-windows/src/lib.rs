//! Windows Graphics Capture desktop backend (spec 02).
//!
//! Only compiles to anything on Windows; elsewhere the crate is empty so the
//! workspace still builds on macOS/Linux CI.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod device;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod frame;

#[cfg(windows)]
pub use capture::{CaptureOutput, DesktopCapture};
#[cfg(windows)]
pub use device::{AdapterInfo, create_device_on, device_adapter_luid, list_adapters};
#[cfg(windows)]
pub use display::{DisplayInfo, list_displays};
#[cfg(windows)]
pub use frame::D3D11Frame;

/// Whether this Windows build supports Windows.Graphics.Capture at all
/// (it needs 1903+). Reported by `gsa doctor`.
#[cfg(windows)]
#[must_use]
pub fn capture_supported() -> bool {
    windows::Graphics::Capture::GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// The mix format `(sample_rate, channels)` of the endpoint system audio is
/// captured from, proven by opening a loopback stream. Reported by
/// `gsa doctor`.
#[cfg(windows)]
pub fn loopback_mix_format() -> gsa_core::Result<(u32, u16)> {
    audio::probe()
}
