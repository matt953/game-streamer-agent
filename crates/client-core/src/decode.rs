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
    /// Two planes of 10-bit samples in 16-bit words, BT.2020 primaries, PQ
    /// transfer: full-width luma followed by half-resolution interleaved
    /// Cb,Cr. Not converted, because there is nothing to convert *to* until
    /// the destination is known.
    ///
    /// An HDR surface wants exactly these values, so converting on the way
    /// out would mean undoing it again; and converting to SDR on the CPU
    /// costs more per frame than decoding one. Either way the choice belongs
    /// to the presenter, which is the only part that knows what the display
    /// can accept.
    P010Bt2020Pq {
        /// Whether the samples use the full 0-1023 range rather than studio
        /// 64-940. Carried rather than assumed: reading limited-range samples
        /// as full crushes blacks and clips whites, which looks like a bad
        /// stream rather than a mistake.
        full_range: bool,
    },
    /// Two planes of 8-bit samples: full-width luma, then half-resolution
    /// interleaved Cb,Cr — the decoder's native SDR layout. Same reasoning as
    /// the 10-bit planar case: the conversion belongs to the presenter, and
    /// requesting RGB from the decoder instead hides a per-frame conversion
    /// inside the decode call.
    Nv12 {
        full_range: bool,
        /// Whether the stream tagged itself BT.601 rather than BT.709. The
        /// two matrices differ in two coefficients — enough to shift every
        /// colour, invisibly, if the tag is ignored.
        bt601: bool,
    },
}

impl PixelOrder {
    /// Bytes one frame occupies at this size.
    ///
    /// Planar formats are not `width * height * 4`, and a presenter that
    /// assumes they are reads past the end of the buffer.
    #[must_use]
    pub fn frame_bytes(self, width: usize, height: usize) -> usize {
        match self {
            Self::Rgba | Self::Bgra => width * height * 4,
            // Luma, then half-resolution chroma pairs; two bytes a sample.
            Self::P010Bt2020Pq { .. } => {
                width * height * 2 + width.div_ceil(2) * height.div_ceil(2) * 4
            }
            // Luma bytes, then half-resolution Cb,Cr pairs.
            Self::Nv12 { .. } => width * height + width.div_ceil(2) * height.div_ceil(2) * 2,
        }
    }

    /// Whether these samples carry more than an SDR display can show.
    #[must_use]
    pub fn is_hdr(self) -> bool {
        matches!(self, Self::P010Bt2020Pq { .. })
    }
}

#[derive(Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed pixels in `order`. Decoders emit whatever order is free
    /// for them (BGRA from VideoToolbox, RGBA from openh264); presenters pick
    /// the matching texture format rather than swizzling on the CPU.
    ///
    /// Empty when `platform` carries the frame instead: copying a decoded
    /// picture to CPU memory and back costs more than decoding it, so a
    /// decoder that can hand its surface straight to the presenter does.
    pub pixels: Vec<u8>,
    pub order: PixelOrder,
    /// The decoder's own surface, still on the GPU, for presenters that can
    /// adopt it. Type-erased because this crate is platform-neutral: the
    /// decoder and presenter of one platform agree on what is inside, and
    /// nothing in between needs to know. `None` for CPU frames.
    pub platform: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("order", &self.order)
            .field("pixels", &self.pixels.len())
            .field("platform", &self.platform.is_some())
            .finish()
    }
}

/// What a decoder is really producing, for display alongside a stream.
///
/// Read from the decoder rather than from the request, because those are the
/// two things that can disagree: a session can ask for HDR, be answered in
/// HDR, and still be decoded into 8 bits with nothing reporting it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoFormat {
    /// Bits per component the decoder handed back.
    pub bit_depth: Option<u8>,
    /// The transfer function the stream is carried with, named — `PQ` and
    /// `HLG` are the HDR ones. A name rather than a verdict: what makes a
    /// picture HDR is the curve, and saying which one is checkable.
    pub transfer: Option<String>,
    /// Whether the stream's own signalling describes HDR: a wide gamut *and*
    /// an HDR curve. Says nothing about whether the content uses the range.
    pub hdr: bool,
}

impl VideoFormat {
    /// A short label for a stats overlay, e.g. `10-bit · HDR (PQ)`.
    #[must_use]
    pub fn label(&self) -> String {
        let depth = self
            .bit_depth
            .map_or_else(|| "—".to_string(), |bits| format!("{bits}-bit"));
        let range = match (self.hdr, self.transfer.as_deref()) {
            (true, Some(curve)) => format!("HDR ({curve})"),
            (true, None) => "HDR".to_string(),
            (false, _) => "SDR".to_string(),
        };
        format!("{depth} · {range}")
    }
}

/// An H.264 (M0) access-unit decoder.
pub trait VideoDecoder: Send {
    /// Feed one complete access unit. `Ok(None)` = decoder buffering
    /// (parameter sets, reordering) — not an error.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>>;

    /// What this decoder is producing, once it has configured itself from a
    /// keyframe. `None` before then, and for decoders that cannot say.
    fn video_format(&self) -> Option<VideoFormat> {
        None
    }
}

#[cfg(test)]
mod format_tests {
    use super::VideoFormat;

    /// The label names the curve rather than passing a verdict: a host can
    /// send a real PQ signal over content with no HDR range in it, and
    /// "HDR (PQ)" stays true where a bare "HDR" would read as a promise.
    #[test]
    fn the_label_names_the_curve_it_found() {
        assert_eq!(
            VideoFormat {
                bit_depth: Some(10),
                transfer: Some("PQ".into()),
                hdr: true,
            }
            .label(),
            "10-bit · HDR (PQ)"
        );
        assert_eq!(
            VideoFormat {
                bit_depth: Some(8),
                transfer: Some("BT.709".into()),
                hdr: false,
            }
            .label(),
            "8-bit · SDR"
        );
    }

    /// A decoder that cannot say must not be reported as 8-bit SDR: not
    /// knowing and knowing it is narrow are different answers.
    #[test]
    fn an_unknown_depth_is_shown_as_unknown() {
        assert_eq!(VideoFormat::default().label(), "— · SDR");
    }
}
