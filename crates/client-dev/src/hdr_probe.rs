//! What a backend actually sent, when HDR was asked for.
//!
//! "HDR works" is not one fact, and each part can be true while the next is
//! false. Reported separately so a failure names itself rather than being
//! guessed at from a picture that looks a bit flat:
//!
//! 1. **What was asked for** — the mode in the launch request.
//! 2. **What the bitstream says** — bit depth, and the colour description:
//!    primaries, transfer function, matrix. Ten bits are necessary but not
//!    sufficient; a 10-bit stream tagged BT.709 is still SDR.
//! 3. **What the decoder produced** — the output pixel format's real bit
//!    depth. This is where a correctly signalled HDR stream quietly becomes
//!    SDR, because the client asked for an 8-bit output format.
//! 4. **What the pixels contain** — highlights above SDR white. Correct flags
//!    over dull content look identical to broken HDR until you measure.
//!
//! Read from the format description rather than a hand-written parser: the
//! platform already extracted this from the bitstream, and it answers the same
//! way for H.264, HEVC and AV1.

use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, kCMFormatDescriptionExtension_ColorPrimaries,
    kCMFormatDescriptionExtension_FullRangeVideo, kCMFormatDescriptionExtension_TransferFunction,
    kCMFormatDescriptionExtension_YCbCrMatrix,
};

/// What the stream and the decoder say about colour.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColourReport {
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub matrix: Option<String>,
    pub full_range: Option<bool>,
    /// FourCC of the decoder's output format, e.g. `BGRA` or `x420`.
    pub pixel_format: Option<String>,
    /// Bits per component the output format carries.
    pub output_bit_depth: Option<u8>,
}

impl ColourReport {
    /// Whether the *signalling* describes an HDR stream.
    ///
    /// Both parts are required: BT.2020 primaries alone is a wide gamut, and
    /// a PQ transfer alone is meaningless without the primaries to read it in.
    #[must_use]
    pub fn signals_hdr(&self) -> bool {
        let wide = self
            .primaries
            .as_deref()
            .is_some_and(|p| p.contains("2020"));
        let curve = self.transfer.as_deref().is_some_and(|t| {
            // PQ (SMPTE ST 2084) and HLG are the two HDR transfers in use.
            t.contains("2084") || t.contains("HLG") || t.contains("2100")
        });
        wide && curve
    }

    /// Whether the decoder is handing us more than 8 bits to show.
    #[must_use]
    pub fn output_is_wide(&self) -> bool {
        self.output_bit_depth.is_some_and(|bits| bits > 8)
    }
}

/// Read the colour description the platform parsed out of the bitstream.
///
/// # Safety
/// `format` must be a live format description.
pub unsafe fn read_format(format: &CMFormatDescription) -> ColourReport {
    // SAFETY: static framework keys, and a live format description.
    unsafe {
        ColourReport {
            primaries: extension_string(format, kCMFormatDescriptionExtension_ColorPrimaries),
            transfer: extension_string(format, kCMFormatDescriptionExtension_TransferFunction),
            matrix: extension_string(format, kCMFormatDescriptionExtension_YCbCrMatrix),
            full_range: extension_bool(format, kCMFormatDescriptionExtension_FullRangeVideo),
            ..Default::default()
        }
    }
}

/// A pixel format FourCC and how many bits per component it carries.
#[must_use]
pub fn describe_pixel_format(fourcc: u32) -> (String, Option<u8>) {
    let name: String = fourcc
        .to_be_bytes()
        .iter()
        .map(|b| char::from(*b))
        .collect();
    // The 10- and 16-bit formats are the ones that can carry HDR; everything
    // else has already thrown the range away by the time we see it.
    let bits = match name.as_str() {
        "x420" | "x422" | "x444" | "xf20" | "xf22" | "xf44" => Some(10),
        "b64a" | "RGhA" => Some(16),
        "BGRA" | "420v" | "420f" | "24RG" | "32BGRA" => Some(8),
        _ => None,
    };
    (name, bits)
}

/// # Safety
/// Live format description; `key` a static framework string.
unsafe fn extension_string(format: &CMFormatDescription, key: &CFString) -> Option<String> {
    // SAFETY: caller contract.
    let value = unsafe { format.extension(key) }?;
    let text = value.downcast_ref::<CFString>()?;
    Some(text.to_string())
}

/// # Safety
/// Live format description; `key` a static framework string.
unsafe fn extension_bool(format: &CMFormatDescription, key: &CFString) -> Option<bool> {
    // SAFETY: caller contract.
    let value: CFRetained<CFType> = unsafe { format.extension(key) }?;
    let number = value.downcast_ref::<objc2_core_foundation::CFBoolean>()?;
    Some(number.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten bits is necessary but not sufficient: the transfer function is what
    /// separates an HDR stream from a wide-gamut SDR one.
    #[test]
    fn signalling_needs_both_the_gamut_and_the_curve() {
        let hdr = ColourReport {
            primaries: Some("ITU_R_2020".into()),
            transfer: Some("SMPTE_ST_2084_PQ".into()),
            ..Default::default()
        };
        assert!(hdr.signals_hdr());

        let wide_sdr = ColourReport {
            primaries: Some("ITU_R_2020".into()),
            transfer: Some("ITU_R_709_2".into()),
            ..Default::default()
        };
        assert!(!wide_sdr.signals_hdr(), "BT.2020 with a 709 curve is SDR");

        let curve_only = ColourReport {
            primaries: Some("ITU_R_709_2".into()),
            transfer: Some("SMPTE_ST_2084_PQ".into()),
            ..Default::default()
        };
        assert!(!curve_only.signals_hdr(), "PQ in a 709 gamut is malformed");
    }

    /// An 8-bit output format is where correct HDR signalling is thrown away,
    /// so the check is on the decoder's output, not on what it was fed.
    #[test]
    fn an_eight_bit_output_is_not_wide_however_the_stream_was_tagged() {
        let (name, bits) = describe_pixel_format(u32::from_be_bytes(*b"BGRA"));
        assert_eq!(name, "BGRA");
        assert_eq!(bits, Some(8));
        let truncated = ColourReport {
            primaries: Some("ITU_R_2020".into()),
            transfer: Some("SMPTE_ST_2084_PQ".into()),
            pixel_format: Some(name),
            output_bit_depth: bits,
            ..Default::default()
        };
        assert!(truncated.signals_hdr(), "the stream was HDR");
        assert!(!truncated.output_is_wide(), "but we asked for 8 bits");
    }

    #[test]
    fn ten_bit_formats_are_recognised() {
        let (name, bits) = describe_pixel_format(u32::from_be_bytes(*b"x420"));
        assert_eq!(name, "x420");
        assert_eq!(bits, Some(10));
    }
}
