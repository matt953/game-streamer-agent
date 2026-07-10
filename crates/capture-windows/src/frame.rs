//! The zero-copy frame handle: a D3D11 texture carried by reference to a
//! hardware encoder, exactly as `IoSurfaceFrame` carries a CVPixelBuffer on
//! macOS. Pixels never leave the GPU.

use std::any::Any;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use gsa_capture_api::PlatformFrame;

/// A captured frame still resident in GPU memory.
///
/// It carries the device as well as the texture because an encoder can only
/// consume textures from the device that owns them, and the encoder is
/// constructed before it knows which device capture chose. Handing the device
/// through the frame lets the encoder open its session lazily on first submit
/// without the two ever coordinating.
///
/// Downcast target for [`gsa_capture_api::GpuHandle::Platform`].
pub struct D3D11Frame {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
}

impl D3D11Frame {
    pub(crate) fn new(texture: ID3D11Texture2D, device: ID3D11Device) -> Self {
        Self { texture, device }
    }

    /// The BGRA texture. Read-only: the capture ring owns it and will reuse
    /// it once every handle to this frame is dropped.
    #[must_use]
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// The device the texture belongs to.
    #[must_use]
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }
}

impl std::fmt::Debug for D3D11Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3D11Frame").finish_non_exhaustive()
    }
}

// SAFETY: a D3D11 device is free-threaded, and the texture is only ever read
// downstream — submitted to an encoder, never mutated. The COM refcounts are
// atomic. Writes to the texture happen only in the capture callback, and only
// once no frame handle to it survives (see the ring in `capture.rs`), so a
// reader never races the writer.
unsafe impl Send for D3D11Frame {}
// SAFETY: see above.
unsafe impl Sync for D3D11Frame {}

impl PlatformFrame for D3D11Frame {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
