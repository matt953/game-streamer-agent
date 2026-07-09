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
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed RGBA8 (`width * height * 4` bytes) for presentation.
    /// (Test-pattern marker readback samples this directly via
    /// `pattern::read_marker_rgba` — no separate luma plane.)
    pub rgba: Vec<u8>,
}

/// An H.264 (M0) access-unit decoder.
pub trait VideoDecoder: Send {
    /// Feed one complete access unit. `Ok(None)` = decoder buffering
    /// (parameter sets, reordering) — not an error.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>>;
}
