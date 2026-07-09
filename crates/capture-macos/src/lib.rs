//! macOS ScreenCaptureKit capture backend (spec 02).
//!
//! Only compiles to anything on macOS; elsewhere the crate is empty so the
//! workspace still builds on Linux/Windows CI.

#[cfg(target_os = "macos")]
mod imp;

#[cfg(target_os = "macos")]
pub use imp::{
    DesktopCapture, DisplayInfo, IoSurfaceFrame, list_displays, screen_recording_authorized,
};
