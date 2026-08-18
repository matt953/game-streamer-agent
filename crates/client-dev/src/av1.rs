//! Enough AV1 bitstream reading to configure a hardware decoder.
//!
//! AV1 is not shaped like H.264 or HEVC. There are no Annex-B start codes and
//! no parameter-set NALs: a frame is a sequence of OBUs, each with its own
//! header and length, and the decoder is configured from an `av1C` record
//! rather than from the parameter sets directly. VideoToolbox will not build a
//! format description for AV1 without one, and there is no
//! `CreateFromAV1ParameterSets` to do it for us.
//!
//! `av1C` restates fields that live inside the sequence header — profile,
//! level, tier, bit depth, monochrome, chroma subsampling — so the header has
//! to be parsed to fill it in. Getting a field wrong here does not fail
//! loudly: the decoder accepts the description and produces wrong pixels.

/// OBU types this module cares about.
const OBU_SEQUENCE_HEADER: u8 = 1;

/// Reads big-endian bit fields, as the AV1 syntax is written.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    /// `f(n)` in the spec: `n` bits, most significant first.
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *self.bytes.get(self.bit / 8)?;
            let bit = (byte >> (7 - (self.bit % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.bit += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.f(1)? == 1)
    }

    /// `uvlc()`: leading zeros, then that many bits.
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.f(zeros)?;
        Some(rest + (1 << zeros) - 1)
    }
}

/// The sequence header fields `av1C` and the format description need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub width: u32,
    pub height: u32,
    /// The colour description, as the AV1 spec's numeric codes.
    ///
    /// Carried because nothing else will: `av1C` does not restate it, and
    /// VideoToolbox does not read the sequence header, so a format description
    /// built from `av1C` alone reports no colour at all. A PQ stream then
    /// decodes as if it were BT.709 — no error, just lifted blacks.
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
}

/// Split a temporal unit into its OBUs, returning `(type, payload_with_header)`.
///
/// The payload keeps its header and size field: `av1C` embeds the sequence
/// header OBU whole, not just its body.
fn obus(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let start = at;
        let header = data[at];
        let obu_type = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        at += 1;
        if extension {
            at += 1;
        }
        let size = if has_size {
            let (value, used) = match leb128(&data[at.min(data.len())..]) {
                Some(v) => v,
                None => return out,
            };
            at += used;
            value as usize
        } else {
            // Without a size field the OBU runs to the end of the unit.
            data.len().saturating_sub(at)
        };
        let end = at.saturating_add(size).min(data.len());
        out.push((obu_type, &data[start..end]));
        at = end;
        if size == 0 && !has_size {
            break;
        }
    }
    out
}

/// `leb128()`: up to 8 bytes, 7 bits each, low group first.
fn leb128(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for i in 0..8 {
        let byte = *bytes.get(i)?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Parse the sequence header OBU out of a temporal unit.
///
/// `None` when the unit carries no sequence header, which is normal: hosts
/// send it with keyframes, not with every frame.
pub fn sequence_header(access_unit: &[u8]) -> Option<SequenceHeader> {
    let (_, obu) = obus(access_unit)
        .into_iter()
        .find(|(kind, _)| *kind == OBU_SEQUENCE_HEADER)?;
    // Step over the OBU header and size field to reach the payload.
    let extension = obu.first()? & 0x04 != 0;
    let has_size = obu.first()? & 0x02 != 0;
    let mut at = 1 + usize::from(extension);
    if has_size {
        at += leb128(obu.get(at..)?)?.1;
    }
    parse_sequence_header(obu.get(at..)?)
}

#[allow(clippy::too_many_lines)]
fn parse_sequence_header(payload: &[u8]) -> Option<SequenceHeader> {
    let r = &mut BitReader::new(payload);
    let profile = r.f(3)? as u8;
    let _still_picture = r.flag()?;
    let reduced = r.flag()?;

    let (level, tier);
    if reduced {
        level = r.f(5)? as u8;
        tier = 0;
    } else {
        let mut buffer_delay_length = 0;
        let decoder_model = if r.flag()? {
            // timing_info()
            let _num_units_in_display_tick = r.f(32)?;
            let _time_scale = r.f(32)?;
            if r.flag()? {
                let _num_ticks_per_picture_minus_1 = r.uvlc()?;
            }
            let present = r.flag()?;
            if present {
                // decoder_model_info()
                buffer_delay_length = r.f(5)? + 1;
                let _num_units_in_decoding_tick = r.f(32)?;
                let _buffer_removal_time_length_minus_1 = r.f(5)?;
                let _frame_presentation_time_length_minus_1 = r.f(5)?;
            }
            present
        } else {
            false
        };
        let initial_display_delay = r.flag()?;
        let operating_points = r.f(5)? + 1;

        // `av1C` describes the first operating point; the rest are read only
        // to reach the fields that follow them.
        let mut first = (0u8, 0u8);
        for i in 0..operating_points {
            let _idc = r.f(12)?;
            let seq_level_idx = r.f(5)? as u8;
            let seq_tier = if seq_level_idx > 7 { r.f(1)? as u8 } else { 0 };
            if decoder_model && r.flag()? {
                // operating_parameters_info()
                let _decoder_buffer_delay = r.f(buffer_delay_length)?;
                let _encoder_buffer_delay = r.f(buffer_delay_length)?;
                let _low_delay_mode_flag = r.flag()?;
            }
            if initial_display_delay && r.flag()? {
                let _initial_display_delay_minus_1 = r.f(4)?;
            }
            if i == 0 {
                first = (seq_level_idx, seq_tier);
            }
        }
        (level, tier) = first;
    }

    let width_bits = r.f(4)? + 1;
    let height_bits = r.f(4)? + 1;
    let width = r.f(width_bits)? + 1;
    let height = r.f(height_bits)? + 1;

    if !reduced && r.flag()? {
        let _delta_frame_id_length_minus_2 = r.f(4)?;
        let _additional_frame_id_length_minus_1 = r.f(3)?;
    }
    let _use_128x128_superblock = r.flag()?;
    let _enable_filter_intra = r.flag()?;
    let _enable_intra_edge_filter = r.flag()?;
    if !reduced {
        let _enable_interintra_compound = r.flag()?;
        let _enable_masked_compound = r.flag()?;
        let _enable_warped_motion = r.flag()?;
        let _enable_dual_filter = r.flag()?;
        let enable_order_hint = r.flag()?;
        if enable_order_hint {
            let _enable_jnt_comp = r.flag()?;
            let _enable_ref_frame_mvs = r.flag()?;
        }
        let force_screen_content = if r.flag()? { 2 } else { r.f(1)? };
        if force_screen_content > 0 && !r.flag()? {
            let _seq_force_integer_mv = r.f(1)?;
        }
        if enable_order_hint {
            let _order_hint_bits_minus_1 = r.f(3)?;
        }
    }
    let _enable_superres = r.flag()?;
    let _enable_cdef = r.flag()?;
    let _enable_restoration = r.flag()?;

    // color_config()
    let high_bitdepth = r.flag()?;
    let twelve_bit = if profile == 2 && high_bitdepth {
        r.flag()?
    } else {
        false
    };
    let monochrome = if profile == 1 { false } else { r.flag()? };
    let (primaries, transfer, matrix) = if r.flag()? {
        (r.f(8)?, r.f(8)?, r.f(8)?)
    } else {
        (2, 2, 2) // unspecified
    };

    let (subsampling_x, subsampling_y, chroma_sample_position);
    let full_range;
    if monochrome {
        full_range = r.flag()?;
        (subsampling_x, subsampling_y, chroma_sample_position) = (true, true, 0);
    } else if primaries == 1 && transfer == 13 && matrix == 0 {
        // sRGB, which is 4:4:4 and full range by definition.
        full_range = true;
        (subsampling_x, subsampling_y, chroma_sample_position) = (false, false, 0);
    } else {
        full_range = r.flag()?;
        let (sx, sy) = match profile {
            0 => (true, true),
            1 => (false, false),
            _ if twelve_bit => {
                let sx = r.flag()?;
                let sy = if sx { r.flag()? } else { false };
                (sx, sy)
            }
            _ => (true, false),
        };
        let csp = if sx && sy { r.f(2)? as u8 } else { 0 };
        (subsampling_x, subsampling_y, chroma_sample_position) = (sx, sy, csp);
    }

    Some(SequenceHeader {
        profile,
        level,
        tier,
        high_bitdepth,
        twelve_bit,
        monochrome,
        subsampling_x,
        subsampling_y,
        chroma_sample_position,
        width,
        height,
        color_primaries: primaries as u8,
        transfer_characteristics: transfer as u8,
        matrix_coefficients: matrix as u8,
        full_range,
    })
}

/// Build the `av1C` configuration record VideoToolbox needs, with the sequence
/// header OBU appended as the configuration OBU.
#[must_use]
pub fn av1c(header: &SequenceHeader, sequence_header_obu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + sequence_header_obu.len());
    // marker = 1, version = 1.
    out.push(0x81);
    out.push((header.profile << 5) | (header.level & 0x1f));
    out.push(
        (header.tier << 7)
            | (u8::from(header.high_bitdepth) << 6)
            | (u8::from(header.twelve_bit) << 5)
            | (u8::from(header.monochrome) << 4)
            | (u8::from(header.subsampling_x) << 3)
            | (u8::from(header.subsampling_y) << 2)
            | (header.chroma_sample_position & 0x03),
    );
    // No initial presentation delay.
    out.push(0);
    out.extend_from_slice(sequence_header_obu);
    out
}

/// The `metadata_type` of every OBU_METADATA in a temporal unit.
///
/// AV1 carries HDR static metadata in its own OBU rather than in an SEI, so
/// this is the AV1 half of the same question.
#[must_use]
pub fn metadata_types(access_unit: &[u8]) -> Vec<u64> {
    const OBU_METADATA: u8 = 5;
    obus(access_unit)
        .into_iter()
        .filter(|(kind, _)| *kind == OBU_METADATA)
        .filter_map(|(_, payload)| {
            // Skip the OBU header and size field to reach metadata_type.
            let first = *payload.first()?;
            let mut at = 1 + usize::from(first & 0x04 != 0);
            if first & 0x02 != 0 {
                at += leb128(payload.get(at..)?)?.1;
            }
            Some(leb128(payload.get(at..)?)?.0)
        })
        .collect()
}

/// The sequence header OBU exactly as it appeared, for `av1C`.
#[must_use]
pub fn sequence_header_obu(access_unit: &[u8]) -> Option<Vec<u8>> {
    obus(access_unit)
        .into_iter()
        .find(|(kind, _)| *kind == OBU_SEQUENCE_HEADER)
        .map(|(_, bytes)| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the bit fields the parser reads, so a header can be built to a
    /// known shape and read back.
    struct BitWriter {
        bytes: Vec<u8>,
        bit: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
            }
        }
        fn f(&mut self, n: u32, value: u32) {
            for i in (0..n).rev() {
                if self.bit.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                let last = self.bytes.len() - 1;
                self.bytes[last] |= bit << (7 - (self.bit % 8));
                self.bit += 1;
            }
        }
    }

    /// A reduced-still-picture header: the short path through the syntax, and
    /// enough to pin the fields `av1C` restates.
    fn reduced_header(profile: u32, high_bitdepth: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.f(3, profile); // seq_profile
        w.f(1, 1); // still_picture
        w.f(1, 1); // reduced_still_picture_header
        w.f(5, 8); // seq_level_idx[0]
        w.f(4, 10); // frame_width_bits_minus_1
        w.f(4, 10); // frame_height_bits_minus_1
        w.f(11, 1919); // max_frame_width_minus_1
        w.f(11, 1079); // max_frame_height_minus_1
        w.f(1, 0); // use_128x128_superblock
        w.f(1, 0); // enable_filter_intra
        w.f(1, 0); // enable_intra_edge_filter
        w.f(1, 0); // enable_superres
        w.f(1, 0); // enable_cdef
        w.f(1, 0); // enable_restoration
        w.f(1, u32::from(high_bitdepth)); // high_bitdepth
        w.f(1, 0); // mono_chrome
        w.f(1, 0); // color_description_present_flag
        w.f(1, 0); // color_range
        w.f(2, 0); // chroma_sample_position (profile 0 is 4:2:0)
        w.f(1, 0); // separate_uv_delta_q
        w.bytes
    }

    #[test]
    fn a_sequence_header_yields_the_fields_av1c_restates() {
        let parsed = parse_sequence_header(&reduced_header(0, false)).unwrap();
        assert_eq!(parsed.profile, 0);
        assert_eq!(parsed.level, 8);
        assert_eq!(parsed.width, 1920);
        assert_eq!(parsed.height, 1080);
        assert!(!parsed.high_bitdepth);
        assert!(!parsed.monochrome);
        // Profile 0 is 4:2:0.
        assert!(parsed.subsampling_x);
        assert!(parsed.subsampling_y);
    }

    /// Bit depth is what separates an SDR stream from an HDR one, and it is a
    /// single bit deep inside the header — worth pinning on its own.
    #[test]
    fn ten_bit_is_read_from_the_header_not_assumed() {
        let parsed = parse_sequence_header(&reduced_header(0, true)).unwrap();
        assert!(parsed.high_bitdepth);
        assert!(!parsed.twelve_bit);
    }

    /// The record's packed byte is what the decoder actually reads; a shifted
    /// field there produces wrong pixels rather than an error.
    #[test]
    fn the_record_packs_the_fields_where_the_decoder_looks_for_them() {
        let header = SequenceHeader {
            profile: 0,
            level: 8,
            tier: 0,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            subsampling_x: true,
            subsampling_y: true,
            chroma_sample_position: 0,
            width: 1920,
            height: 1080,
            color_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            full_range: false,
        };
        let record = av1c(&header, &[0xAA, 0xBB]);
        assert_eq!(record[0], 0x81, "marker and version");
        assert_eq!(record[1], 8, "profile 0, level 8");
        assert_eq!(record[2], 0b0000_1100, "4:2:0, 8-bit, colour");
        assert_eq!(record[3], 0);
        assert_eq!(&record[4..], &[0xAA, 0xBB], "sequence header follows");
    }

    /// The colour description lives only in the sequence header: `av1C` does
    /// not restate it and VideoToolbox does not read it, so if this parse is
    /// wrong nothing else will contradict it — a PQ stream simply decodes as
    /// BT.709 with lifted blacks.
    ///
    /// Both vectors captured from Apollo on 2026-08-18: the same host, the
    /// same picture, asked for SDR and then for HDR.
    #[test]
    fn a_real_hosts_colour_description_is_read_from_the_header() {
        let sdr = hex("0a0e0000004eabbfc370086641818189");
        let parsed = sequence_header(&sdr).unwrap();
        assert!(!parsed.high_bitdepth);
        assert_eq!(parsed.color_primaries, 6, "SMPTE 170M");
        assert_eq!(parsed.transfer_characteristics, 6, "SMPTE 170M");
        assert_eq!(parsed.matrix_coefficients, 6, "BT.601");
        assert!(!parsed.full_range);

        let hdr = hex("0a0e0000004eabbfc370086742440249");
        let parsed = sequence_header(&hdr).unwrap();
        assert!(parsed.high_bitdepth, "10-bit");
        assert!(!parsed.twelve_bit);
        assert_eq!(parsed.color_primaries, 9, "BT.2020");
        assert_eq!(parsed.transfer_characteristics, 16, "PQ (SMPTE ST 2084)");
        assert_eq!(
            parsed.matrix_coefficients, 9,
            "BT.2020 non-constant luminance"
        );
        assert!(!parsed.full_range);
        // Same picture either way; only the range changed.
        assert_eq!((parsed.width, parsed.height), (1920, 1080));
        assert_eq!(parsed.profile, 0);
        assert!(parsed.subsampling_x && parsed.subsampling_y);
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn obus_are_split_by_their_own_lengths() {
        // Two OBUs, each with a size field: a sequence header then a frame.
        let unit = [
            (OBU_SEQUENCE_HEADER << 3) | 0x02,
            2,
            0xAA,
            0xBB,
            (6 << 3) | 0x02,
            1,
            0xCC,
        ];
        let split = obus(&unit);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].0, OBU_SEQUENCE_HEADER);
        assert_eq!(
            split[0].1,
            &[(OBU_SEQUENCE_HEADER << 3) | 0x02, 2, 0xAA, 0xBB]
        );
        assert_eq!(split[1].0, 6);
    }

    /// A truncated unit must stop rather than read past its end: frames arrive
    /// from the network and a loss can cut one anywhere.
    #[test]
    fn a_truncated_unit_is_not_read_past() {
        let unit = [(OBU_SEQUENCE_HEADER << 3) | 0x02, 40, 0xAA];
        let split = obus(&unit);
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].1.len(), 3);
        assert!(sequence_header(&unit).is_none());
    }

    #[test]
    fn leb128_reads_multi_byte_lengths() {
        assert_eq!(leb128(&[0x00]), Some((0, 1)));
        assert_eq!(leb128(&[0x7f]), Some((127, 1)));
        assert_eq!(leb128(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(leb128(&[0x80]), None);
    }
}
