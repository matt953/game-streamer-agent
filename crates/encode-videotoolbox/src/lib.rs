//! macOS VideoToolbox hardware encoder (spec 03).
//!
//! Only compiles to anything on macOS; empty elsewhere.

#[cfg(target_os = "macos")]
mod imp;

#[cfg(target_os = "macos")]
pub use imp::VideoToolboxEncoder;
