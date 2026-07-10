//! Windows Graphics Capture desktop backend (spec 02).
//!
//! Only compiles to anything on Windows; elsewhere the crate is empty so the
//! workspace still builds on macOS/Linux CI.

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
pub use device::{AdapterInfo, list_adapters};
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
