//! HDR metadata as the bitstream carries it: SEI walking and payload
//! extraction, shared by the dev harness and the FFI.
//!
//! In core rather than per client because two parsers is how two clients end
//! up disagreeing about the same stream. Platforms differ only in what they
//! do with the payloads: Android's decoder forwards them itself, Apple's
//! drops them and the client re-attaches (measured, not assumed).

/// The HDR static metadata a stream can carry, as distinct from its colour
/// tags.
///
/// Separate from the decoder's colour report on purpose: the description says how
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
    /// SMPTE ST 2094-40 (HDR10+) dynamic metadata rode this access unit —
    /// per-scene tone-mapping guidance, not a static description.
    pub hdr10_plus: bool,
    /// Mastering display peak, in 0.0001 cd/m² as the SEI codes it.
    pub max_mastering: Option<u32>,
    /// Mastering display black level, same 0.0001 cd/m² units.
    pub min_mastering: Option<u32>,
    /// Brightest coded pixel anywhere in the content (MaxCLL), in nits.
    pub max_cll: Option<u16>,
    /// Brightest frame-average light level (MaxFALL), in nits.
    pub max_fall: Option<u16>,
}

impl StaticMetadata {
    #[must_use]
    pub fn any(self) -> bool {
        self.mastering_display || self.content_light_level || self.hdr10_plus
    }

    /// Peak mastering luminance in nits, when the SEI carried one.
    #[must_use]
    pub fn max_mastering_nits(self) -> Option<u32> {
        self.max_mastering.map(|v| v / 10_000)
    }

    /// Read the facts out of the raw payloads.
    #[must_use]
    pub fn from_payloads(payloads: &HdrPayloads) -> Self {
        let mut found = Self {
            mastering_display: payloads.mastering_display.is_some(),
            content_light_level: payloads.content_light_level.is_some(),
            hdr10_plus: payloads.hdr10_plus.is_some(),
            ..Self::default()
        };
        // ST 2086: three primaries and a white point (16 bytes), then max
        // and min mastering luminance as big-endian u32s.
        if let Some(payload) = payloads.mastering_display.as_deref()
            && payload.len() >= 24
        {
            found.max_mastering = Some(u32::from_be_bytes([
                payload[16],
                payload[17],
                payload[18],
                payload[19],
            ]));
            found.min_mastering = Some(u32::from_be_bytes([
                payload[20],
                payload[21],
                payload[22],
                payload[23],
            ]));
        }
        if let Some(payload) = payloads.content_light_level.as_deref()
            && payload.len() >= 4
        {
            found.max_cll = Some(u16::from_be_bytes([payload[0], payload[1]]));
            found.max_fall = Some(u16::from_be_bytes([payload[2], payload[3]]));
        }
        found
    }
}

/// The HDR SEI payloads themselves, kept whole.
///
/// Needed beyond the parsed facts because VideoToolbox does not carry these
/// from the bitstream to its decoded buffers — a client that wants the
/// display to see them must re-attach the exact payload bytes itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HdrPayloads {
    /// ST 2086 mastering display colour volume, as coded.
    pub mastering_display: Option<Vec<u8>>,
    /// MaxCLL/MaxFALL content light level, as coded.
    pub content_light_level: Option<Vec<u8>>,
    /// ST 2094-40 (HDR10+) dynamic metadata: the whole ITU-T T.35 payload,
    /// country code first — the layout display pipelines take it in.
    pub hdr10_plus: Option<Vec<u8>>,
}

impl HdrPayloads {
    /// Whether the mastering payload carries real luminance values: a
    /// present-but-zeroed static description means "unknown", and handing a
    /// display "unknown" as if it were fact can only mislead its mapping.
    #[must_use]
    pub fn mastering_is_valued(&self) -> bool {
        self.mastering_display
            .as_deref()
            .is_some_and(|p| p.len() >= 24 && p[16..24].iter().any(|&b| b != 0))
    }

    /// Whether the light-level payload carries real values — judged on its
    /// own: MaxCLL/MaxFALL of zero is "unknown" per CTA-861.3.
    #[must_use]
    pub fn light_level_is_valued(&self) -> bool {
        self.content_light_level
            .as_deref()
            .is_some_and(|p| p.len() >= 4 && p[..4].iter().any(|&b| b != 0))
    }

    fn note_payload(&mut self, payload_type: u32, payload: &[u8]) {
        match payload_type {
            MASTERING_DISPLAY_COLOUR_VOLUME => {
                self.mastering_display = Some(payload.to_vec());
            }
            CONTENT_LIGHT_LEVEL_INFO => {
                self.content_light_level = Some(payload.to_vec());
            }
            // Registered ITU-T T.35 user data. Only the SMPTE-registered
            // ST 2094-40 stream counts as HDR10+; T.35 also carries closed
            // captions and other vendors' data.
            USER_DATA_REGISTERED_ITU_T_T35 => {
                if payload.len() >= 3
                    && payload[0] == T35_COUNTRY_US
                    && u16::from_be_bytes([payload[1], payload[2]]) == T35_PROVIDER_SMPTE
                {
                    self.hdr10_plus = Some(payload.to_vec());
                }
            }
            _ => {}
        }
    }
}

/// SEI payload types (H.265 Table D.1), shared with H.264.
const MASTERING_DISPLAY_COLOUR_VOLUME: u32 = 137;
const CONTENT_LIGHT_LEVEL_INFO: u32 = 144;
const USER_DATA_REGISTERED_ITU_T_T35: u32 = 4;
/// T.35 addressing for ST 2094-40: United States, SMPTE.
const T35_COUNTRY_US: u8 = 0xB5;
const T35_PROVIDER_SMPTE: u16 = 0x003C;

/// AV1 metadata OBU types (spec 6.7.1).
const AV1_METADATA_HDR_CLL: u64 = 1;
const AV1_METADATA_HDR_MDCV: u64 = 2;
const AV1_METADATA_ITUT_T35: u64 = 4;

/// Which HDR static-metadata messages an HEVC access unit carries.
///
/// `nals` are payloads with start codes already stripped. Prefix SEI is NAL
/// type 39 and suffix SEI is 40; both can carry these.
#[must_use]
pub fn hevc_static_metadata(nals: &[&[u8]]) -> StaticMetadata {
    StaticMetadata::from_payloads(&hevc_hdr_payloads(nals))
}

/// The HDR SEI payloads carried by an HEVC access unit still in Annex-B
/// form, as it crosses the FFI: the embedder decodes the same bytes, so the
/// extraction must see exactly what its decoder will.
#[must_use]
pub fn hevc_annex_b_hdr_payloads(au: &[u8]) -> HdrPayloads {
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut at = 0;
    let mut start: Option<usize> = None;
    while at + 2 < au.len() {
        if au[at] == 0 && au[at + 1] == 0 && au[at + 2] == 1 {
            if let Some(s) = start {
                // Trim the trailing zero of a 4-byte start code.
                let end = if at > s && au[at - 1] == 0 {
                    at - 1
                } else {
                    at
                };
                nals.push(&au[s..end]);
            }
            at += 3;
            start = Some(at);
        } else {
            at += 1;
        }
    }
    if let Some(s) = start {
        nals.push(&au[s..]);
    }
    hevc_hdr_payloads(&nals)
}

/// The HDR SEI payloads an HEVC access unit carries, whole.
#[must_use]
pub fn hevc_hdr_payloads(nals: &[&[u8]]) -> HdrPayloads {
    let mut found = HdrPayloads::default();
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
            // Only the type is visible here, so any T.35 reads as dynamic
            // metadata; on AV1 nothing else commonly rides it in a stream.
            AV1_METADATA_ITUT_T35 => found.hdr10_plus = true,
            _ => {}
        }
    }
    found
}

/// Walk an SEI message list, noting the payload types present.
///
/// Both the type and the size are coded as a run of 0xFF bytes plus a final
/// byte, so a message can be skipped without understanding it.
fn scan_sei_payloads(data: &[u8], found: &mut HdrPayloads) {
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
        let end = (at + payload_size as usize).min(data.len());
        found.note_payload(payload_type, &data[at..end]);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Annex-B walk must find the same messages the pre-split walk does,
    /// across both 3- and 4-byte start codes.
    #[test]
    fn annex_b_units_yield_the_same_payloads_as_split_nals() {
        let sei = [39 << 1, 0x01, 137u8, 2, 0x11, 0x22, 0x80];
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(&sei);
        au.extend_from_slice(&[0, 0, 1, 19 << 1, 0x01, 0xAA]);
        let found = hevc_annex_b_hdr_payloads(&au);
        assert!(found.mastering_display.is_some());
        assert_eq!(found.mastering_display.as_deref(), Some(&[0x11, 0x22][..]));
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

    /// AV1 puts the same facts in metadata OBUs rather than SEI: MDCV,
    /// CLL, and T.35 as the dynamic-metadata carrier.
    #[test]
    fn av1_metadata_types_map_to_the_same_facts() {
        assert!(av1_static_metadata(&[2]).mastering_display);
        assert!(av1_static_metadata(&[1]).content_light_level);
        assert!(av1_static_metadata(&[4]).hdr10_plus);
        assert!(!av1_static_metadata(&[3, 5]).any());
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
}
