//! Windows Graphics Capture desktop backend (spec 02).
//!
//! Only compiles to anything on Windows; elsewhere the crate is empty so the
//! workspace still builds on macOS/Linux CI.

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::{DesktopCapture, DisplayInfo, capture_supported, list_displays};
