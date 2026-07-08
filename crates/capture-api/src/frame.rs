//! Cross-platform frame handles (spec 01). At M0 only the CPU variant is
//! populated; platform texture variants are added with their capture crates
//! and never require touching this enum's consumers (zero-copy handles flow
//! through untouched).

use std::sync::Arc;

use gsa_core::media::PixelFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// CPU-memory frame (debug/test sources, software paths).
#[derive(Debug, Clone)]
pub struct CpuFrame {
    /// Pixel data; refcounted so sinks can drop/clone without copies.
    pub data: Arc<Vec<u8>>,
    /// Row stride in bytes.
    pub stride: usize,
}

/// Platform frame handle. Pixels never leave the GPU on real backends;
/// `Cpu` exists for tests and software encode (spec 01).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GpuHandle {
    Cpu(CpuFrame),
    // M1+: IoSurface(..) [macos], D3d11(..) [windows], DmaBuf(..) [linux]
}

/// One captured frame moving from a `RenderSource` toward an encoder.
#[derive(Debug, Clone)]
pub struct GpuFrame {
    pub handle: GpuHandle,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Agent media-clock timestamp (µs) stamped in the capture callback —
    /// the origin of every glass-to-glass measurement.
    pub capture_ts_us: u64,
    /// Damage hints when the platform provides them.
    pub dirty_rects: Option<Vec<Rect>>,
}
