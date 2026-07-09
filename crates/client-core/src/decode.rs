//! Decode seam: the embedding app supplies the platform decoder
//! (VideoToolbox / MediaCodec / openh264 in client-dev). Hot-path data
//! crosses this boundary as plain byte buffers — no platform types.

use gsa_core::Result;

/// One decoded frame in CPU memory.
///
/// M0 keeps this as BGRA + luma copies (software path). Zero-copy decode
/// surfaces (spec 01: platform textures stay on-GPU) arrive with the
/// platform decoders; this type then grows a handle variant, and `bgra`
/// becomes the debug path.
/// Byte order of a 4-byte pixel. Green sits at byte 1 in both, so
/// brightness-based readers (test-pattern marker) work on either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    Rgba,
    Bgra,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed 4-byte pixels (`width * height * 4` bytes) in `order`.
    /// Decoders emit whatever order is free for them (BGRA from
    /// VideoToolbox, RGBA from openh264); presenters pick the matching
    /// texture format rather than swizzling on the CPU.
    pub pixels: Vec<u8>,
    pub order: PixelOrder,
}

/// An H.264 (M0) access-unit decoder.
pub trait VideoDecoder: Send {
    /// Feed one complete access unit. `Ok(None)` = decoder buffering
    /// (parameter sets, reordering) — not an error.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>>;
}
