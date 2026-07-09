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

/// A backend-owned GPU frame (macOS IOSurface, D3D11 texture, DMA-BUF).
///
/// Type-erased so `capture-api` stays platform-free and `unsafe`-free: the
/// capture crate boxes a concrete wrapper (which asserts the platform
/// handle's thread-safety in its own FFI crate) and the encoder downcasts
/// via [`PlatformFrame::as_any`]. Pixels never leave the GPU.
pub trait PlatformFrame: std::fmt::Debug + Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Platform frame handle. Pixels never leave the GPU on real backends;
/// `Cpu` exists for tests and software encode (spec 01).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GpuHandle {
    Cpu(CpuFrame),
    /// Backend GPU surface; downcast through [`PlatformFrame::as_any`].
    Platform(Arc<dyn PlatformFrame>),
}

impl GpuHandle {
    /// Downcast a platform handle to a concrete backend frame type.
    #[must_use]
    pub fn downcast_platform<T: 'static>(&self) -> Option<&T> {
        match self {
            GpuHandle::Platform(p) => p.as_any().downcast_ref::<T>(),
            GpuHandle::Cpu(_) => None,
        }
    }
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
