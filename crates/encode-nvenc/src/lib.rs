//! NVENC hardware H.264 encoder (spec 03).
//!
//! Consumes the D3D11 texture that `capture-windows` produces, with no
//! readback and no CPU colour conversion — NVENC does the BGRA→NV12 convert
//! on the GPU. Where the software encoder costs ~83 ms per 2560x1600 frame,
//! this costs single-digit milliseconds.
//!
//! Only compiles to anything on Windows; elsewhere the crate is empty so the
//! workspace still builds on macOS/Linux CI. The `sys` layer is already
//! platform-neutral: Linux needs a different library name and a CUDA device
//! type, and nothing else.

#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use encoder::NvencEncoder;

/// Whether this machine can hardware-encode, and on which GPU.
///
/// `None` means no NVIDIA driver, no NVENC-capable GPU, or a driver older than
/// the API version this build speaks. The agent uses the returned LUID to pin
/// capture to the same adapter, so the texture and the encoder share a device.
#[cfg(windows)]
#[must_use]
pub fn probe() -> Option<Support> {
    encoder::probe()
}

/// What [`probe`] found.
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub struct Support {
    /// DXGI adapter the encoder lives on. Capture must produce textures here.
    pub adapter_luid: i64,
}
