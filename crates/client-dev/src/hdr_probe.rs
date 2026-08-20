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

use objc2_core_foundation::{CFData, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, kCMFormatDescriptionExtension_ColorPrimaries,
    kCMFormatDescriptionExtension_ContentLightLevelInfo,
    kCMFormatDescriptionExtension_FullRangeVideo,
    kCMFormatDescriptionExtension_MasteringDisplayColorVolume,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMFormatDescriptionExtension_TransferFunction, kCMFormatDescriptionExtension_YCbCrMatrix,
};

/// What the stream and the decoder say about colour.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColourReport {
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub matrix: Option<String>,
    pub full_range: Option<bool>,
    /// Bits per component the *bitstream* carries, from its configuration
    /// record. What arrived, before we choose what to ask the decoder for.
    pub stream_bit_depth: Option<u8>,
    /// Whether the stream carries HDR static metadata: the mastering display's
    /// colour volume, and the content light levels (MaxCLL/MaxFALL).
    ///
    /// The one reading here that does not depend on what is on screen. A host
    /// emits these because it is genuinely driving an HDR display, so their
    /// presence separates real HDR from a correctly-tagged SDR desktop in a
    /// way that measuring brightness cannot — a dark HDR picture and a dark
    /// SDR one look the same on every other measure.
    pub mastering_display: bool,
    pub content_light_level: bool,
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

    /// Whether the decoder is being asked for fewer bits than arrived.
    ///
    /// Independent of HDR: a 10-bit SDR stream decoded into an 8-bit buffer
    /// has still lost precision, and this is the only place it is visible.
    #[must_use]
    pub fn truncates_the_stream(&self) -> bool {
        match (self.stream_bit_depth, self.output_bit_depth) {
            (Some(stream), Some(output)) => output < stream,
            _ => false,
        }
    }
}

/// What the luma plane actually contained, measured rather than assumed.
///
/// A decoder can hand back a 10-bit buffer whose samples all came from 8 bits,
/// and it looks identical to a real one until the values are counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LumaStats {
    pub min: u16,
    pub max: u16,
    /// Samples that are not multiples of four — values an 8-bit pipeline
    /// shifted into 10 bits could not produce.
    pub off_grid: u64,
    pub samples: u64,
    /// How many samples landed on each of the 1024 codes.
    ///
    /// Kept because the peak alone cannot tell a picture with real highlights
    /// from one with a single stuck pixel, and because brightness only means
    /// something as a distribution.
    histogram: Box<[u32; 1024]>,
}

impl Default for LumaStats {
    fn default() -> Self {
        Self {
            min: 0,
            max: 0,
            off_grid: 0,
            samples: 0,
            histogram: Box::new([0; 1024]),
        }
    }
}

impl LumaStats {
    /// Fold one sample in.
    pub fn observe(&mut self, luma: u16) {
        if self.samples == 0 {
            self.min = luma;
            self.max = luma;
        } else {
            self.min = self.min.min(luma);
            self.max = self.max.max(luma);
        }
        if !luma.is_multiple_of(4) {
            self.off_grid += 1;
        }
        self.histogram[usize::from(luma).min(1023)] += 1;
        self.samples += 1;
    }

    /// The code at or below which `fraction` of samples fall.
    #[must_use]
    pub fn percentile(&self, fraction: f64) -> u16 {
        if self.samples == 0 {
            return 0;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let target = (self.samples as f64 * fraction) as u64;
        let mut seen = 0u64;
        for (code, count) in self.histogram.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= target {
                #[allow(clippy::cast_possible_truncation)]
                return code as u16;
            }
        }
        self.max
    }

    /// How many samples sit above `code`.
    #[must_use]
    pub fn count_above(&self, code: u16) -> u64 {
        self.histogram
            .iter()
            .skip(usize::from(code) + 1)
            .map(|c| u64::from(*c))
            .sum()
    }

    /// Whether the plane carries precision finer than 8 bits.
    ///
    /// Proves the *path* preserves ten bits — not that the source had ten bits
    /// of real detail. A host encoding an 8-bit desktop into a 10-bit stream
    /// still lands off the grid, because its colour conversion is done in the
    /// wider space. What this rules out is our own truncation.
    #[must_use]
    pub fn finer_than_eight_bit(&self) -> bool {
        self.off_grid > 0
    }
}

/// The HDR static metadata a stream can carry, as distinct from its colour
/// tags.
///
/// Separate from [`ColourReport`] on purpose: the colour description says how
/// to *interpret* the samples and lives in the parameter sets, while these say
/// how bright the mastering display was and how bright the content gets. They
/// travel as SEI messages inside the access units, so a format description
/// built from parameter sets alone cannot see them — reading their absence
/// there and calling it "the host sent none" is the mistake this exists to
/// stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StaticMetadata {
    /// SMPTE ST 2086 mastering display colour volume.
    pub mastering_display: bool,
    /// MaxCLL / MaxFALL content light level.
    pub content_light_level: bool,
}

impl StaticMetadata {
    #[must_use]
    pub fn any(self) -> bool {
        self.mastering_display || self.content_light_level
    }

    fn note_payload_type(&mut self, payload_type: u32) {
        match payload_type {
            MASTERING_DISPLAY_COLOUR_VOLUME => self.mastering_display = true,
            CONTENT_LIGHT_LEVEL_INFO => self.content_light_level = true,
            _ => {}
        }
    }
}

/// SEI payload types (H.265 Table D.1), shared with H.264.
const MASTERING_DISPLAY_COLOUR_VOLUME: u32 = 137;
const CONTENT_LIGHT_LEVEL_INFO: u32 = 144;

/// AV1 metadata OBU types (spec 6.7.1).
const AV1_METADATA_HDR_CLL: u64 = 1;
const AV1_METADATA_HDR_MDCV: u64 = 2;

/// Which HDR static-metadata messages an HEVC access unit carries.
///
/// `nals` are payloads with start codes already stripped. Prefix SEI is NAL
/// type 39 and suffix SEI is 40; both can carry these.
#[must_use]
pub fn hevc_static_metadata(nals: &[&[u8]]) -> StaticMetadata {
    let mut found = StaticMetadata::default();
    for nal in nals {
        let Some(first) = nal.first() else { continue };
        if !matches!((first >> 1) & 0x3f, 39 | 40) {
            continue;
        }
        // Two-byte NAL header, then the SEI message list.
        if nal.len() > 2 {
            scan_sei_payloads(&strip_emulation_prevention(&nal[2..]), &mut found);
        }
    }
    found
}

/// Which HDR static-metadata messages an AV1 temporal unit carries.
#[must_use]
pub fn av1_static_metadata(metadata_types: &[u64]) -> StaticMetadata {
    let mut found = StaticMetadata::default();
    for kind in metadata_types {
        match *kind {
            AV1_METADATA_HDR_MDCV => found.mastering_display = true,
            AV1_METADATA_HDR_CLL => found.content_light_level = true,
            _ => {}
        }
    }
    found
}

/// Walk an SEI message list, noting the payload types present.
///
/// Both the type and the size are coded as a run of 0xFF bytes plus a final
/// byte, so a message can be skipped without understanding it.
fn scan_sei_payloads(data: &[u8], found: &mut StaticMetadata) {
    let mut at = 0;
    let read_extended = |at: &mut usize| -> Option<u32> {
        let mut value: u32 = 0;
        loop {
            let byte = *data.get(*at)?;
            *at += 1;
            value = value.checked_add(u32::from(byte))?;
            if byte != 0xFF {
                return Some(value);
            }
        }
    };
    loop {
        let Some(payload_type) = read_extended(&mut at) else {
            return;
        };
        let Some(payload_size) = read_extended(&mut at) else {
            return;
        };
        found.note_payload_type(payload_type);
        at += payload_size as usize;
        // A trailing 0x80 marks the end of the list, and anything past the
        // buffer means the unit was cut short.
        if at >= data.len() {
            return;
        }
    }
}

/// Undo emulation prevention (`00 00 03` → `00 00`) before reading a payload.
fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(byte);
    }
    out
}

/// Bits per component a codec configuration record describes.
///
/// The record is what the decoder itself was configured from, so it cannot
/// disagree with the stream the way a separate parser can drift.
#[must_use]
pub fn record_bit_depth(kind: &str, record: &[u8]) -> Option<u8> {
    match kind {
        // `hvcC`: bit_depth_luma_minus8 in the low 3 bits of byte 17.
        "hvcC" => record.get(17).map(|b| 8 + (b & 0x07)),
        // `av1C`: high_bitdepth and twelve_bit in byte 2. Twelve is only
        // meaningful in profile 2, and profile 2 is not in play here.
        "av1C" => record.get(2).map(|b| match (b & 0x40 != 0, b & 0x20 != 0) {
            (false, _) => 8,
            (true, false) => 10,
            (true, true) => 12,
        }),
        // `avcC` carries bit depth only in an optional trailing section, and
        // 10-bit H.264 is not a mode any of these hosts offer.
        _ => None,
    }
}

/// Read the bit depth out of the configuration record the format description
/// was built with.
///
/// # Safety
/// `format` must be a live format description.
pub unsafe fn stream_bit_depth(format: &CMFormatDescription) -> Option<u8> {
    // SAFETY: static framework key, live format description.
    let atoms =
        unsafe { format.extension(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) }?;
    let atoms = atoms.downcast_ref::<CFDictionary>()?;
    for kind in ["hvcC", "av1C", "avcC"] {
        let key = CFString::from_str(kind);
        // SAFETY: live dictionary; the key outlives the lookup, and the
        // returned value is borrowed from the dictionary.
        let value = unsafe { atoms.value(CFRetained::as_ptr(&key).as_ptr().cast()) };
        if value.is_null() {
            continue;
        }
        // SAFETY: non-null value borrowed from a live dictionary.
        let value: &CFType = unsafe { &*value.cast::<CFType>() };
        let Some(data) = value.downcast_ref::<CFData>() else {
            continue;
        };
        let len = data.length().max(0) as usize;
        let ptr = data.byte_ptr();
        if ptr.is_null() {
            continue;
        }
        // SAFETY: `len` bytes at `ptr`, owned by the live dictionary.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        if let Some(bits) = record_bit_depth(kind, bytes) {
            return Some(bits);
        }
    }
    None
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
            stream_bit_depth: stream_bit_depth(format),
            mastering_display: has_extension(
                format,
                kCMFormatDescriptionExtension_MasteringDisplayColorVolume,
            ),
            content_light_level: has_extension(
                format,
                kCMFormatDescriptionExtension_ContentLightLevelInfo,
            ),
            ..Default::default()
        }
    }
}

/// The 10-bit biplanar output formats, video and full range.
pub const PIXEL_FORMAT_420_10_VIDEO: u32 = u32::from_be_bytes(*b"x420");
pub const PIXEL_FORMAT_420_10_FULL: u32 = u32::from_be_bytes(*b"xf20");
/// 8-bit biplanar YCbCr — the decoder's native output for an 8-bit stream.
pub const PIXEL_FORMAT_NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
pub const PIXEL_FORMAT_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");

/// The output format to ask the decoder for, given what the stream carries.
///
/// Asking for more than the stream has gains nothing and costs a conversion;
/// asking for less throws the extra bits away where nothing reports it.
#[must_use]
pub fn wanted_output_format(stream_bits: Option<u8>, full_range: Option<bool>) -> u32 {
    if stream_bits.is_some_and(|bits| bits > 8) {
        if full_range == Some(true) {
            PIXEL_FORMAT_420_10_FULL
        } else {
            PIXEL_FORMAT_420_10_VIDEO
        }
    } else if full_range == Some(true) {
        // The decoder's own layout. Asking for BGRA instead makes the decode
        // call pay a hidden YUV→RGB conversion — measured at about a
        // millisecond a frame — for a conversion the presenter's shader does
        // as part of sampling anyway.
        PIXEL_FORMAT_NV12_FULL
    } else {
        PIXEL_FORMAT_NV12_VIDEO
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

/// Whether the format description carries `key` at all.
///
/// # Safety
/// Live format description; `key` a static framework string.
unsafe fn has_extension(format: &CMFormatDescription, key: &CFString) -> bool {
    // SAFETY: caller contract.
    unsafe { format.extension(key) }.is_some()
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

    /// Byte 17 of `hvcC` is `bit_depth_luma_minus8` in its low three bits;
    /// reading the wrong byte yields a plausible number rather than an error,
    /// so the offset is pinned against a record laid out by hand.
    #[test]
    fn hevc_bit_depth_comes_from_its_own_byte() {
        let mut record = [0u8; 23];
        record[0] = 1; // configurationVersion
        record[12] = 120; // general_level_idc
        record[16] = 0xFC | 1; // chromaFormat 4:2:0, reserved bits set
        record[17] = 0xF8 | 2; // bit_depth_luma_minus8 = 2
        record[18] = 0xF8 | 2;
        assert_eq!(record_bit_depth("hvcC", &record), Some(10));

        record[17] = 0xF8; // 8-bit
        assert_eq!(record_bit_depth("hvcC", &record), Some(8));

        // A record cut short must not be read past.
        assert_eq!(record_bit_depth("hvcC", &record[..10]), None);
    }

    /// The `av1C` byte we write ourselves, read back the same way.
    #[test]
    fn av1_bit_depth_comes_from_the_record_we_build() {
        // tier 1, 8-bit, colour, 4:2:0, chroma sample position 1 — the real
        // Apollo AV1 record's third byte.
        assert_eq!(
            record_bit_depth("av1C", &[0x81, 0x09, 0b1000_1101]),
            Some(8)
        );
        // high_bitdepth set, twelve_bit clear.
        assert_eq!(
            record_bit_depth("av1C", &[0x81, 0x09, 0b1100_1101]),
            Some(10)
        );
        assert_eq!(
            record_bit_depth("av1C", &[0x81, 0x09, 0b1110_1101]),
            Some(12)
        );
    }

    /// Truncation is about the two depths disagreeing, not about HDR: a 10-bit
    /// SDR stream decoded to 8 bits has lost precision just the same.
    #[test]
    fn truncation_is_reported_without_reference_to_hdr() {
        let sdr_ten_bit = ColourReport {
            primaries: Some("ITU_R_709_2".into()),
            transfer: Some("ITU_R_709_2".into()),
            stream_bit_depth: Some(10),
            output_bit_depth: Some(8),
            ..Default::default()
        };
        assert!(!sdr_ten_bit.signals_hdr());
        assert!(sdr_ten_bit.truncates_the_stream());

        let matched = ColourReport {
            stream_bit_depth: Some(10),
            output_bit_depth: Some(10),
            ..Default::default()
        };
        assert!(!matched.truncates_the_stream());

        // Nothing known is not the same as nothing lost.
        assert!(!ColourReport::default().truncates_the_stream());
    }

    /// Asking for ten bits when the stream has eight buys a conversion and no
    /// precision, so the request follows the stream.
    #[test]
    fn the_output_format_follows_what_the_stream_carries() {
        assert_eq!(
            wanted_output_format(Some(8), Some(false)),
            PIXEL_FORMAT_NV12_VIDEO,
            "native biplanar, not BGRA: BGRA hides a per-frame conversion"
        );
        assert_eq!(
            wanted_output_format(Some(8), Some(true)),
            PIXEL_FORMAT_NV12_FULL
        );
        assert_eq!(wanted_output_format(None, None), PIXEL_FORMAT_NV12_VIDEO);
        assert_eq!(
            wanted_output_format(Some(10), Some(false)),
            PIXEL_FORMAT_420_10_VIDEO
        );
        assert_eq!(
            wanted_output_format(Some(10), Some(true)),
            PIXEL_FORMAT_420_10_FULL
        );
    }

    /// A "no metadata found" reading is only worth what the walk is worth:
    /// if the payload list were mis-stepped, every stream would look bare.
    #[test]
    fn the_sei_walk_finds_the_hdr_messages_among_others() {
        // Prefix SEI (NAL type 39), two-byte header, then three messages:
        // a 4-byte type 1 (pic timing), mastering display (137), and
        // content light level (144).
        let nal = [
            39 << 1,
            0x01,
            1,
            4,
            0xAA,
            0xBB,
            0xCC,
            0xDD, // an unrelated message
            137,
            2,
            0x11,
            0x22, // mastering display
            144,
            4,
            0x00,
            0x10,
            0x00,
            0x20, // content light level
            0x80, // trailing marker
        ];
        let found = hevc_static_metadata(&[&nal[..]]);
        assert!(found.mastering_display);
        assert!(found.content_light_level);
        assert!(found.any());
    }

    /// The types are extended by runs of 0xFF, so a large type must not be
    /// read as a small one — which would silently match the wrong message.
    #[test]
    fn extended_payload_types_are_accumulated() {
        // Type 255 + 144 = 399, size 1. Not one of ours, and must not be
        // mistaken for 144.
        let nal = [39 << 1, 0x01, 0xFF, 144, 1, 0x00, 0x80];
        assert!(!hevc_static_metadata(&[&nal[..]]).any());
    }

    /// Only SEI NALs carry these; a slice that happens to contain the same
    /// bytes must not be read as metadata.
    #[test]
    fn a_picture_nal_is_not_scanned_for_sei() {
        // NAL type 19 (IDR) whose payload bytes look like a 137 message.
        let nal = [19 << 1, 0x01, 137, 2, 0x11, 0x22, 0x80];
        assert!(!hevc_static_metadata(&[&nal[..]]).any());
    }

    /// AV1 puts the same two facts in metadata OBUs rather than SEI.
    #[test]
    fn av1_metadata_types_map_to_the_same_two_facts() {
        assert!(av1_static_metadata(&[2]).mastering_display);
        assert!(av1_static_metadata(&[1]).content_light_level);
        assert!(!av1_static_metadata(&[3, 4]).any());
        assert!(!av1_static_metadata(&[]).any());
    }

    /// Emulation prevention must be undone first, or a payload containing
    /// `00 00 03` shifts every subsequent message.
    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(
            strip_emulation_prevention(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x02]),
            vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x02]
        );
        // A 0x03 that is not preceded by two zeros is real data.
        assert_eq!(
            strip_emulation_prevention(&[0x01, 0x03, 0x00, 0x03]),
            vec![0x01, 0x03, 0x00, 0x03]
        );
    }

    /// Values an 8-bit pipeline cannot produce are the evidence; every sample
    /// landing on a multiple of four means the extra bits carry nothing.
    #[test]
    fn eight_bit_content_shifted_into_ten_bits_lands_on_the_grid() {
        let mut shifted = LumaStats::default();
        for eight_bit in [16u16, 32, 128, 235] {
            shifted.observe(eight_bit << 2);
        }
        assert!(!shifted.finer_than_eight_bit());
        assert_eq!(shifted.samples, 4);

        let mut real = LumaStats::default();
        for value in [65u16, 130, 511, 939] {
            real.observe(value);
        }
        assert!(real.finer_than_eight_bit());
        assert_eq!(real.min, 65);
        assert_eq!(real.max, 939);
    }
}
