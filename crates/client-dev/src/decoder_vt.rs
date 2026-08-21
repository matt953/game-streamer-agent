//! VideoToolbox hardware decoder (macOS) for H.264 and HEVC. Parses Annex-B
//! access units, builds a format description from the in-band parameter sets,
//! repacks to length-prefixed samples, and decodes to BGRA pixel buffers.
//!
//! The two codecs differ in exactly two places, and both are easy to get
//! subtly wrong:
//!
//! - **NAL type.** H.264 keeps it in the low 5 bits of a 1-byte header; HEVC
//!   uses bits 1-6 of a 2-byte header. Reading an HEVC stream with H.264's
//!   mask finds parameter sets that are not there.
//! - **Parameter sets.** H.264 needs SPS and PPS; HEVC needs VPS as well, and
//!   the format description will not build without all three.
//!
//! One CPU copy remains (decoded CVPixelBuffer → `DecodedFrame.rgba`) —
//! true zero-copy IOSurface→wgpu texture interop is a later optimization;
//! hardware decode already cuts decode time to ~1-2 ms.
//!
//! This module is the FFI boundary of this binary: unsafe is allowed here,
//! every block documented.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc;

use block2::RcBlock;
use objc2_core_foundation::{
    CFData, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, kCFBooleanFalse,
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    kCMFormatDescriptionColorPrimaries_ITU_R_709_2, kCMFormatDescriptionColorPrimaries_ITU_R_2020,
    kCMFormatDescriptionColorPrimaries_SMPTE_C, kCMFormatDescriptionExtension_ColorPrimaries,
    kCMFormatDescriptionExtension_FullRangeVideo,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMFormatDescriptionExtension_TransferFunction, kCMFormatDescriptionExtension_YCbCrMatrix,
    kCMFormatDescriptionTransferFunction_ITU_R_709_2,
    kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG,
    kCMFormatDescriptionTransferFunction_SMPTE_240M_1995,
    kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4, kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_2020, kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995,
    kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSession, VTIsHardwareDecodeSupported,
};

use crate::decoder::DisplayMapping;
use gsa_client_core::{DecodedFrame, VideoDecoder, VideoFormat};
use gsa_core::media::Codec;
use gsa_core::{Error, Result};

/// BGRA FourCC for the decoder output ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const CF_NUMBER_SINT32: CFNumberType = CFNumberType(3);

/// Codecs this machine decodes in hardware, richest first.
///
/// H.264 is always included: every Mac VideoToolbox runs on decodes it, and a
/// list without a floor would leave a session with nothing to negotiate.
#[must_use]
pub fn hardware_codecs() -> Vec<Codec> {
    let supported = |codec: Codec| {
        // SAFETY: a pure capability query on a codec constant.
        unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) }
    };
    let codecs: Vec<Codec> = [Codec::Av1, Codec::Hevc]
        .into_iter()
        .filter(|&c| supported(c))
        .chain(std::iter::once(Codec::H264))
        .collect();
    tracing::info!(?codecs, "hardware decode support");
    codecs
}

/// The VideoToolbox four-character code for a codec.
fn codec_type(codec: Codec) -> CMVideoCodecType {
    match codec {
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Av1 => kCMVideoCodecType_AV1,
        _ => kCMVideoCodecType_H264,
    }
}

pub struct VideoToolboxDecoder {
    codec: Codec,
    /// What the stream said about colour, and what we ended up decoding to.
    colour: crate::hdr_probe::ColourReport,
    /// The pixel format asked of the session, which the stream's own bit depth
    /// decides — see `hdr_probe::wanted_output_format`.
    output_format: u32,
    /// How HDR content is mapped onto this SDR window.
    mapping: DisplayMapping,
    /// How decoded samples become displayable pixels, built once from the
    /// stream's own colour description rather than per frame.
    stream_colour: Option<StreamColour>,
    /// Whether the output format has been reported once.
    reported_output: bool,
    /// Whether a frame carrying actual variation has been measured.
    reported_luma: bool,
    /// Whether the wire has been checked for HDR static metadata.
    reported_metadata: Option<crate::hdr_probe::StaticMetadata>,
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    /// Last seen parameter sets, in the order the format description wants
    /// them; the session is rebuilt when they change.
    param_sets: Vec<Vec<u8>>,
}

impl std::fmt::Debug for VideoToolboxDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolboxDecoder")
            .field("open", &self.session.is_some())
            .finish()
    }
}

impl VideoToolboxDecoder {
    /// A decoder for one codec. Fails rather than falling back: the caller
    /// negotiated this codec with the host, so silently decoding something
    /// else would produce a session that streams and shows nothing.
    pub fn new(codec: Codec, mapping: DisplayMapping) -> Result<Self> {
        // SAFETY: a pure capability query on a codec constant.
        if !unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) } {
            return Err(Error::Decode(format!(
                "no hardware decode for {codec:?} on this machine"
            )));
        }
        Ok(Self {
            codec,
            colour: crate::hdr_probe::ColourReport::default(),
            output_format: PIXEL_FORMAT_BGRA,
            mapping,
            stream_colour: None,
            reported_output: false,
            reported_luma: false,
            reported_metadata: None,
            session: None,
            format: None,
            param_sets: Vec::new(),
        })
    }

    /// Build the session for an AV1 stream from its `av1C` record.
    ///
    /// AV1 has no parameter-set NALs to hand VideoToolbox; the configuration
    /// is an `av1C` record carried as a sample-description atom, and the frame
    /// size comes from the sequence header rather than from the description.
    fn ensure_av1_session(&mut self, access_unit: &[u8]) -> Result<()> {
        let Some(header) = crate::av1::sequence_header(access_unit) else {
            // Normal: hosts send the sequence header with keyframes only.
            return Ok(());
        };
        let obu = crate::av1::sequence_header_obu(access_unit)
            .ok_or_else(err("sequence header without its OBU"))?;
        let record = crate::av1::av1c(&header, &obu);
        if self.session.is_some() && self.param_sets.first().is_some_and(|s| *s == record) {
            return Ok(());
        }
        // The bytes as well as the reading: ports of this parser on other
        // platforms are checked against a real host's header, not a synthetic
        // one, and this is where a real one can be captured.
        tracing::info!(
            ?header,
            obu = obu.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "AV1 sequence header"
        );

        // `av1C` is passed the way the container formats carry it: a
        // sample-description atom keyed by its four-character name.
        let data = CFData::from_bytes(&record);
        let key = CFString::from_str("av1C");
        // SAFETY: single-entry CFType dictionaries; keys and values stay alive
        // for the call and are retained by the dictionary callbacks.
        let atoms = unsafe {
            let mut keys: [*const c_void; 1] = [CFRetained::as_ptr(&key).as_ptr().cast()];
            let mut values: [*const c_void; 1] = [CFRetained::as_ptr(&data).as_ptr().cast()];
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(err("CFDictionaryCreate failed"))?;
        // The colour description has to be attached by hand. `av1C` does not
        // restate it and VideoToolbox does not read the sequence header, so a
        // description built from the atom alone reports no colour at all — and
        // a PQ stream then decodes as BT.709 with lifted blacks, silently.
        // SAFETY: static framework keys, and values that outlive the call.
        let extensions = unsafe {
            let mut keys: Vec<*const c_void> = vec![
                (kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms as *const CFString)
                    .cast::<c_void>(),
            ];
            let mut values: Vec<*const c_void> = vec![CFRetained::as_ptr(&atoms).as_ptr().cast()];

            let colour: [(*const CFString, Option<&'static CFString>); 3] = [
                (
                    kCMFormatDescriptionExtension_ColorPrimaries,
                    colour_primaries(header.color_primaries),
                ),
                (
                    kCMFormatDescriptionExtension_TransferFunction,
                    transfer_function(header.transfer_characteristics),
                ),
                (
                    kCMFormatDescriptionExtension_YCbCrMatrix,
                    ycbcr_matrix(header.matrix_coefficients),
                ),
            ];
            for (key, value) in colour {
                // An unspecified or unrecognised code is left off entirely:
                // saying nothing is honest, saying BT.709 would be a guess
                // that later code could not tell from a real reading.
                if let Some(value) = value {
                    keys.push(key.cast());
                    values.push((value as *const CFString).cast());
                }
            }
            let range = if header.full_range {
                kCFBooleanTrue
            } else {
                kCFBooleanFalse
            };
            if let Some(range) = range {
                keys.push((kCMFormatDescriptionExtension_FullRangeVideo as *const CFString).cast());
                values.push((range as *const objc2_core_foundation::CFBoolean).cast());
            }

            let count = keys.len() as isize;
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                count,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(err("CFDictionaryCreate failed"))?;

        let mut fmt_raw: *const CMFormatDescription = ptr::null();
        // SAFETY: valid extensions dictionary and out-pointer; dimensions come
        // from the stream's own sequence header.
        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                kCMVideoCodecType_AV1,
                header.width as i32,
                header.height as i32,
                Some(&extensions),
                NonNull::from(&mut fmt_raw),
            )
        };
        if status != 0 || fmt_raw.is_null() {
            return Err(Error::Decode(format!("AV1 format description: {status}")));
        }
        // SAFETY: +1 retained out-param; take ownership.
        let format = unsafe { CFRetained::from_raw(NonNull::new_unchecked(fmt_raw.cast_mut())) };
        self.open_session(format)?;
        self.param_sets = vec![record];
        Ok(())
    }

    fn ensure_session(&mut self, sets: &[&[u8]]) -> Result<()> {
        if self.session.is_some() && self.param_sets.len() == sets.len() {
            let unchanged = self.param_sets.iter().zip(sets).all(|(a, b)| a == b);
            if unchanged {
                return Ok(());
            }
        }
        let mut ptrs: Vec<NonNull<u8>> = Vec::with_capacity(sets.len());
        for set in sets {
            ptrs.push(
                NonNull::new(set.as_ptr().cast_mut()).ok_or_else(err("empty parameter set"))?,
            );
        }
        let mut sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut fmt_raw: *const CMFormatDescription = ptr::null();
        // SAFETY: `sets.len()` valid parameter-set pointers and matching sizes,
        // both alive for the call; 4-byte NAL length headers, matching the
        // repack below; valid out-pointer.
        let status = unsafe {
            let count = sets.len();
            let ptrs = NonNull::from(ptrs.as_mut_slice()).cast();
            let sizes = NonNull::from(sizes.as_mut_slice()).cast();
            let out = NonNull::from(&mut fmt_raw);
            match self.codec {
                Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None, count, ptrs, sizes, 4, None, out,
                ),
                _ => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None, count, ptrs, sizes, 4, out,
                ),
            }
        };
        if status != 0 || fmt_raw.is_null() {
            return Err(Error::Decode(format!(
                "format description failed: {status}"
            )));
        }
        // SAFETY: +1 retained out-param; take ownership.
        let format = unsafe { CFRetained::from_raw(NonNull::new_unchecked(fmt_raw.cast_mut())) };

        self.open_session(format)?;
        self.param_sets = sets.iter().map(|s| s.to_vec()).collect();
        Ok(())
    }

    /// Open a decompression session against `format`, whichever codec built it.
    ///
    /// The output format is chosen here rather than fixed, because it is the
    /// one decision that can silently discard what the host sent: a 10-bit
    /// stream decoded into an 8-bit buffer loses its precision with no error
    /// anywhere. The stream's own configuration record decides.
    fn open_session(&mut self, format: CFRetained<CMFormatDescription>) -> Result<()> {
        // What the platform read out of the bitstream, before anything of
        // ours has had a chance to throw it away.
        // SAFETY: the format description was just built and is live.
        let colour = unsafe { crate::hdr_probe::read_format(&format) };
        tracing::info!(
            codec = ?self.codec,
            primaries = ?colour.primaries,
            transfer = ?colour.transfer,
            matrix = ?colour.matrix,
            full_range = ?colour.full_range,
            stream_bit_depth = ?colour.stream_bit_depth,
            signals_hdr = colour.signals_hdr(),
            // Present only when the host is really driving an HDR display, so
            // this says what brightness statistics cannot: whether the source
            // has HDR at all, independently of what is on screen.
            mastering_display = colour.mastering_display,
            content_light_level = colour.content_light_level,
            "stream colour description"
        );
        self.output_format =
            crate::hdr_probe::wanted_output_format(colour.stream_bit_depth, colour.full_range);
        self.stream_colour = StreamColour::build(
            &colour,
            self.output_format == crate::hdr_probe::PIXEL_FORMAT_420_10_FULL
                || self.output_format == crate::hdr_probe::PIXEL_FORMAT_NV12_FULL,
        );
        self.colour = colour;

        let session = match create_session(&format, self.output_format) {
            Ok(session) => session,
            // A machine that cannot deliver the wider format still has to
            // stream, but it does so having lost the extra bits — said out
            // loud, because that is the failure this whole path exists to
            // stop happening quietly.
            Err(e) if self.output_format != PIXEL_FORMAT_BGRA => {
                tracing::warn!(
                    error = %e,
                    "no 10-bit output from this decoder; falling back to 8-bit, \
                     which discards the stream's extra precision"
                );
                self.output_format = PIXEL_FORMAT_BGRA;
                create_session(&format, PIXEL_FORMAT_BGRA)?
            }
            Err(e) => return Err(e),
        };

        self.session = Some(session);
        self.format = Some(format);
        tracing::info!(codec = ?self.codec, "VideoToolbox decoder session (re)created");
        Ok(())
    }
}

// SAFETY: `VideoDecoder: Send` — the client owns the decoder on one thread
// at a time; VT session calls are made serially from that owner. The
// CFRetained refcounts are atomic.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoDecoder for VideoToolboxDecoder {
    /// What came out, not what was asked for: the output format is the one
    /// this decoder actually opened its session with, and the transfer is the
    /// one read from the stream's own description.
    fn video_format(&self) -> Option<VideoFormat> {
        self.session.as_ref()?;
        Some(VideoFormat {
            bit_depth: crate::hdr_probe::describe_pixel_format(self.output_format).1,
            transfer: self.colour.transfer.as_deref().map(short_transfer_name),
            hdr: self.colour.signals_hdr(),
        })
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>> {
        // AV1 is not Annex-B: no start codes, no parameter-set NALs, and the
        // sample is the temporal unit exactly as it arrived.
        if self.codec == Codec::Av1 {
            self.ensure_av1_session(access_unit)?;
            let found =
                crate::hdr_probe::av1_static_metadata(&crate::av1::metadata_types(access_unit));
            self.report_static_metadata(found);
            return self.decode_sample(access_unit);
        }

        let nals = split_annex_b(access_unit);
        // Parameter sets are held in the order the format description expects:
        // VPS, SPS, PPS for HEVC; SPS, PPS for H.264.
        let mut sets: [Option<&[u8]>; 3] = [None; 3];
        let mut avcc = Vec::with_capacity(access_unit.len() + 16);
        for nal in &nals {
            match parameter_set_slot(self.codec, nal) {
                Some(slot) => sets[slot] = Some(nal),
                None if nal.is_empty() => {}
                None => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }
        let found = crate::hdr_probe::hevc_static_metadata(&nals);
        self.report_static_metadata(found);
        let wanted = if self.codec == Codec::Hevc { 3 } else { 2 };
        let found: Vec<&[u8]> = sets.iter().skip(3 - wanted).flatten().copied().collect();
        if found.len() == wanted {
            self.ensure_session(&found)?;
        }
        if self.session.is_none() {
            return Ok(None); // waiting for the first keyframe's parameter sets
        }
        if avcc.is_empty() {
            return Ok(None);
        }
        self.decode_sample(&avcc)
    }
}

impl VideoToolboxDecoder {
    /// Say whether the wire carries HDR static metadata.
    ///
    /// Read from the access units, not from the format description: these are
    /// SEI messages (or, for AV1, metadata OBUs), and a description built from
    /// parameter sets alone cannot see them. Reported once a keyframe has been
    /// seen, since that is where a host puts them.
    fn report_static_metadata(&mut self, found: crate::hdr_probe::StaticMetadata) {
        // Keep looking until a keyframe actually carries some, or until the
        // session ends having carried none — a P-frame saying nothing is not
        // an answer. Re-reported whenever the metadata itself changes:
        // scene-adaptive hosts start emitting (or revalue) mid-session, and a
        // first-keyframe snapshot would call that "none" forever.
        if !found.any() && self.session.is_none() {
            return;
        }
        // Accumulated across the session: static SEI rides keyframes and the
        // dynamic SEI rides the frames between, so no single access unit
        // shows the whole story.
        let mut union = self.reported_metadata.unwrap_or_default();
        union.mastering_display |= found.mastering_display;
        union.content_light_level |= found.content_light_level;
        union.hdr10_plus |= found.hdr10_plus;
        union.max_mastering = found.max_mastering.or(union.max_mastering);
        union.min_mastering = found.min_mastering.or(union.min_mastering);
        union.max_cll = found.max_cll.or(union.max_cll);
        union.max_fall = found.max_fall.or(union.max_fall);
        if self.reported_metadata == Some(union) {
            return;
        }
        self.reported_metadata = Some(union);
        let found = union;
        tracing::info!(
            mastering_display = found.mastering_display,
            content_light_level = found.content_light_level,
            hdr10_plus = found.hdr10_plus,
            mastering_peak_nits = found.max_mastering_nits(),
            max_cll_nits = found.max_cll,
            max_fall_nits = found.max_fall,
            "HDR static metadata on the wire"
        );
    }

    /// Say what the decoder is actually producing.
    ///
    /// The last place HDR can be lost, and the quietest: a stream tagged
    /// BT.2020 PQ decoded into an 8-bit buffer has already thrown its range
    /// away, and nothing upstream reports an error.
    fn report_output(&mut self) {
        let (name, bits) = crate::hdr_probe::describe_pixel_format(self.output_format);
        self.colour.pixel_format = Some(name.clone());
        self.colour.output_bit_depth = bits;
        tracing::info!(
            pixel_format = name,
            stream_bit_depth = ?self.colour.stream_bit_depth,
            output_bit_depth = ?bits,
            signals_hdr = self.colour.signals_hdr(),
            output_is_wide = self.colour.output_is_wide(),
            "decoder output format"
        );
        if self.colour.truncates_the_stream() {
            tracing::warn!(
                "the stream carries more bits than this decoder was asked for, \
                 so its precision is being discarded before anything can show it"
            );
        }
    }

    /// Say what the pixels of a frame actually contained.
    ///
    /// Measured on the first frame that carries any variation, not the first
    /// frame at all: a session opens on a black frame often enough that
    /// reporting from it would say "no precision" about a picture that has
    /// not arrived yet.
    fn report_luma(&mut self, luma: &crate::hdr_probe::LumaStats, clipped: u64) {
        tracing::info!(
            min = luma.min,
            max = luma.max,
            off_grid = luma.off_grid,
            samples = luma.samples,
            finer_than_eight_bit = luma.finer_than_eight_bit(),
            clipped_to_white = clipped,
            sdr_white_nits = self.mapping.sdr_white_nits,
            "decoded luma range"
        );
        self.report_brightness(luma);
    }

    /// Say how bright the picture actually is, in absolute nits.
    ///
    /// This is the last layer, and the only one that separates real HDR from
    /// an SDR desktop wrapped in an HDR signal: PQ codes are absolute, so a
    /// picture that never exceeds diffuse white has no HDR range in it no
    /// matter how correctly it is tagged.
    fn report_brightness(&self, luma: &crate::hdr_probe::LumaStats) {
        let Some(transfer) = self.colour.transfer.as_deref() else {
            return;
        };
        if !transfer.contains("2084") || luma.samples == 0 {
            // Only PQ carries absolute levels. A relative curve has no nits to
            // report, and inventing some would be the guess this exists to
            // avoid.
            return;
        }
        let full_range = self.output_format == crate::hdr_probe::PIXEL_FORMAT_420_10_FULL;
        let (offset, span, _) = range_constants(full_range);
        let nits = |code: u16| pq_eotf_nits(((f32::from(code) - offset) / span).clamp(0.0, 1.0));

        // The code at which diffuse white sits, so "above it" means a
        // highlight rather than ordinary picture content.
        let reference_code = reference_white_code(self.mapping.sdr_white_nits, offset, span);
        #[allow(clippy::cast_precision_loss)]
        let above_reference = luma.count_above(reference_code) as f64 * 100.0 / luma.samples as f64;

        tracing::info!(
            peak_nits = format!("{:.0}", nits(luma.max)),
            p99_nits = format!("{:.0}", nits(luma.percentile(0.99))),
            median_nits = format!("{:.1}", nits(luma.percentile(0.50))),
            above_reference_white_pct = format!("{above_reference:.2}"),
            reference_white_nits = self.mapping.sdr_white_nits,
            "decoded brightness"
        );
    }

    /// Hand one prepared sample to the session and wait for its frame.
    fn decode_sample(&mut self, sample_data: &[u8]) -> Result<Option<DecodedFrame>> {
        let Some(session) = self.session.as_ref() else {
            return Ok(None); // waiting for the first keyframe's configuration
        };
        if sample_data.is_empty() {
            return Ok(None);
        }
        let sample = avcc_sample_buffer(
            sample_data,
            self.format.as_ref().expect("format with session"),
        )?;

        let (tx, rx) = mpsc::sync_channel::<Option<Decoded>>(1);
        let stream_colour = self.stream_colour;
        let handler = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _dur: CMTime| {
                let frame = if status == 0 && !image.is_null() {
                    // SAFETY: non-null decoded image buffer from VideoToolbox,
                    // valid for the duration of this callback.
                    unsafe { copy_frame(&*image, stream_colour) }
                } else {
                    None
                };
                let _ = tx.send(frame);
            },
        );
        let mut info = VTDecodeInfoFlags(0);
        // SAFETY: valid session + sample buffer + escaping handler block.
        let status = unsafe {
            session.decode_frame_with_output_handler(
                &sample,
                VTDecodeFrameFlags(0),
                &mut info,
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            return Err(Error::Decode(format!("decode_frame: {status}")));
        }
        // Synchronous decode (no async flag requested): handler already ran.
        let decoded = rx.try_recv().unwrap_or(None);
        if let Some(decoded) = decoded {
            if !self.reported_output {
                self.reported_output = true;
                self.report_output();
            }
            if !self.reported_luma && decoded.luma.max > decoded.luma.min {
                self.reported_luma = true;
                self.report_luma(&decoded.luma, decoded.clipped);
            }
            return Ok(Some(decoded.frame));
        }
        Ok(None)
    }
}

/// A decoded frame together with what its luma plane contained.
struct Decoded {
    frame: DecodedFrame,
    luma: crate::hdr_probe::LumaStats,
    /// Pixels that reached display white. A large count means the host encoded
    /// its content above where `DisplayMapping` puts white, and detail is
    /// being lost at the top rather than shown.
    clipped: u64,
}

/// Which parameter-set slot a NAL belongs in, or `None` for picture data.
///
/// Slots are ordered as the format description wants them — VPS, SPS, PPS —
/// with H.264 using the last two. The NAL type lives in different bits per
/// codec: the low 5 of a 1-byte header for H.264, bits 1-6 of a 2-byte header
/// for HEVC.
fn parameter_set_slot(codec: Codec, nal: &[u8]) -> Option<usize> {
    let first = *nal.first()?;
    match codec {
        Codec::Hevc => match (first >> 1) & 0x3f {
            32 => Some(0), // VPS
            33 => Some(1), // SPS
            34 => Some(2), // PPS
            _ => None,
        },
        _ => match first & 0x1f {
            7 => Some(1), // SPS
            8 => Some(2), // PPS
            _ => None,
        },
    }
}

/// Copy whatever the decoder produced into a frame the presenter can show.
///
/// # Safety
/// `image` must be a valid, decoded CVPixelBuffer.
/// The decoder's surface, handed to the presenter still on the GPU.
///
/// Copying a decoded 1080p picture to CPU memory and uploading it again cost
/// more than decoding it (about a millisecond each way, and gigabytes a
/// second of memory traffic at 120 fps). The presenter wraps this buffer's
/// planes as Metal textures instead; the pixels never leave the GPU. Reads —
/// screenshots, the brightness probe — lock the buffer and copy on demand,
/// which happens a handful of times a session rather than per frame.
pub struct VtSurface {
    buffer: CFRetained<CVPixelBuffer>,
    /// Two 10-bit planes (P010) when true; packed BGRA when false.
    pub planar: bool,
}

// SAFETY: CoreVideo buffers are reference-counted CF objects, safe to retain,
// release and read from any thread; all mutation happened before the decoder
// output callback handed the buffer over.
unsafe impl Send for VtSurface {}
// SAFETY: as above — reads only, on an immutable decoded buffer.
unsafe impl Sync for VtSurface {}

impl VtSurface {
    #[must_use]
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }
}

unsafe fn copy_frame(image: &CVImageBuffer, colour: Option<StreamColour>) -> Option<Decoded> {
    let pb: &CVPixelBuffer = image;
    let format = CVPixelBufferGetPixelFormatType(pb);
    if format == crate::hdr_probe::PIXEL_FORMAT_420_10_VIDEO
        || format == crate::hdr_probe::PIXEL_FORMAT_420_10_FULL
    {
        // A 10-bit buffer without a colour description cannot be interpreted;
        // refusing beats showing a wrong picture.
        // SAFETY: caller contract — a valid decoded buffer of the checked
        // biplanar layout.
        return unsafe { wrap_planar(pb, colour?) };
    }
    if format == crate::hdr_probe::PIXEL_FORMAT_NV12_VIDEO
        || format == crate::hdr_probe::PIXEL_FORMAT_NV12_FULL
    {
        let colour = colour?;
        let width = CVPixelBufferGetWidthOfPlane(pb, 0);
        let height = CVPixelBufferGetHeightOfPlane(pb, 0);
        if width == 0 || height == 0 {
            return None;
        }
        // SAFETY: `pb` is a live CF object; retaining is thread-safe.
        let retained = unsafe { CFRetained::retain(std::ptr::NonNull::from(pb)) };
        return Some(Decoded {
            frame: DecodedFrame {
                #[allow(clippy::cast_possible_truncation)]
                width: width as u32,
                #[allow(clippy::cast_possible_truncation)]
                height: height as u32,
                pixels: Vec::new(),
                order: gsa_client_core::PixelOrder::Nv12 {
                    full_range: colour.full_range,
                    bt601: colour.bt601,
                },
                platform: Some(std::sync::Arc::new(VtSurface {
                    buffer: retained,
                    planar: true,
                })),
            },
            luma: crate::hdr_probe::LumaStats::default(),
            clipped: 0,
        });
    }
    let width = CVPixelBufferGetWidth(pb);
    let height = CVPixelBufferGetHeight(pb);
    if width == 0 || height == 0 {
        return None;
    }
    // SAFETY: `pb` is a live CF object; retaining is thread-safe.
    let retained = unsafe { CFRetained::retain(std::ptr::NonNull::from(pb)) };
    Some(Decoded {
        frame: DecodedFrame {
            #[allow(clippy::cast_possible_truncation)]
            width: width as u32,
            #[allow(clippy::cast_possible_truncation)]
            height: height as u32,
            pixels: Vec::new(),
            order: gsa_client_core::PixelOrder::Bgra,
            platform: Some(std::sync::Arc::new(VtSurface {
                buffer: retained,
                planar: false,
            })),
        },
        luma: crate::hdr_probe::LumaStats::default(),
        clipped: 0,
    })
}

/// Wrap a 10-bit biplanar buffer and probe its brightness, copying nothing.
///
/// # Safety
/// `pb` must be a valid, decoded 10-bit biplanar CVPixelBuffer.
unsafe fn wrap_planar(pb: &CVPixelBuffer, colour: StreamColour) -> Option<Decoded> {
    // SAFETY: valid pixel buffer; lock for the probe's CPU read.
    let lock = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if lock != 0 {
        return None;
    }
    let decoded = (|| {
        let width = CVPixelBufferGetWidthOfPlane(pb, 0);
        let height = CVPixelBufferGetHeightOfPlane(pb, 0);
        let (luma_base, luma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
        );
        if luma_base.is_null() || width == 0 || height == 0 {
            return None;
        }

        // Brightness is measured on a sample of the picture: reading two
        // million pixels a frame to report one number cost more than
        // decoding the frame did, and this stride still covers thousands of
        // points spread across the image.
        const PROBE_STRIDE: usize = 16;
        let mut stats = crate::hdr_probe::LumaStats::default();
        let mut clipped = 0u64;
        let mut low_bits: u16 = 0;
        for row in (0..height).step_by(PROBE_STRIDE) {
            // SAFETY: row < plane height, so `width` 16-bit samples from the
            // row start lie in the locked plane.
            let luma = unsafe {
                std::slice::from_raw_parts(
                    luma_base.cast::<u8>().add(row * luma_stride).cast::<u16>(),
                    width,
                )
            };
            for col in (0..width).step_by(PROBE_STRIDE) {
                let raw = luma[col];
                low_bits |= raw;
                let y = raw >> TEN_BIT_SHIFT;
                stats.observe(y);
                if usize::from(y) >= TEN_BIT_MAX {
                    clipped += 1;
                }
            }
        }
        // The one reading that would make everything else wrong: the samples
        // are ten bits at the top of a 16-bit word, and the whole picture is
        // misread if they are anywhere else.
        if low_bits & ((1 << TEN_BIT_SHIFT) - 1) != 0 {
            tracing::error!(
                low_bits = format!("{:#06x}", low_bits),
                "10-bit samples are not left-justified in their word; \
                 the conversion is reading them wrongly"
            );
        }

        // SAFETY: `pb` is a live CF object; retaining is thread-safe.
        let retained = unsafe { CFRetained::retain(std::ptr::NonNull::from(pb)) };
        Some(Decoded {
            frame: DecodedFrame {
                #[allow(clippy::cast_possible_truncation)]
                width: width as u32,
                #[allow(clippy::cast_possible_truncation)]
                height: height as u32,
                pixels: Vec::new(),
                order: gsa_client_core::PixelOrder::P010Bt2020Pq {
                    full_range: colour.full_range,
                },
                platform: Some(std::sync::Arc::new(VtSurface {
                    buffer: retained,
                    planar: true,
                })),
            },
            luma: stats,
            clipped,
        })
    })();
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    decoded
}

/// Copy a wrapped frame's pixels out of its surface, for the rare paths that
/// need bytes — screenshots and frame dumps. Returns the same frame with
/// `pixels` filled and the handle dropped.
#[must_use]
pub fn materialise(frame: &DecodedFrame) -> Option<DecodedFrame> {
    let surface = frame.platform.as_ref()?.downcast_ref::<VtSurface>()?;
    let pb = surface.pixel_buffer();
    if matches!(frame.order, gsa_client_core::PixelOrder::Nv12 { .. }) {
        // SAFETY: a live decoded buffer of the biplanar 8-bit layout.
        return unsafe { copy_nv12_pixels(pb) }.map(|pixels| DecodedFrame {
            pixels,
            platform: None,
            ..frame.clone()
        });
    }
    if surface.planar {
        // SAFETY: a live decoded buffer of the planar layout it was wrapped as.
        unsafe { copy_planar_pixels(pb) }.map(|pixels| DecodedFrame {
            pixels,
            platform: None,
            ..frame.clone()
        })
    } else {
        // SAFETY: as above, packed BGRA.
        unsafe { copy_bgra(pb) }.map(|copied| DecodedFrame {
            pixels: copied.pixels,
            platform: None,
            ..frame.clone()
        })
    }
}

/// The 8-bit biplanar copy-out, used only by [`materialise`].
///
/// # Safety
/// `pb` must be a valid, decoded NV12 CVPixelBuffer.
unsafe fn copy_nv12_pixels(pb: &CVPixelBuffer) -> Option<Vec<u8>> {
    // SAFETY: valid pixel buffer; lock for CPU read access.
    let lock = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if lock != 0 {
        return None;
    }
    let pixels = (|| {
        let width = CVPixelBufferGetWidthOfPlane(pb, 0);
        let height = CVPixelBufferGetHeightOfPlane(pb, 0);
        let (luma_base, luma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
        );
        let (chroma_base, chroma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 1),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
        );
        let chroma_height = CVPixelBufferGetHeightOfPlane(pb, 1);
        let chroma_width = CVPixelBufferGetWidthOfPlane(pb, 1);
        if luma_base.is_null() || chroma_base.is_null() || width == 0 || height == 0 {
            return None;
        }
        let mut pixels = vec![0u8; width * height + chroma_width * chroma_height * 2];
        let (luma_out, chroma_out) = pixels.split_at_mut(width * height);
        for row in 0..height {
            // SAFETY: row < plane height; `width` bytes lie in the locked row.
            let src = unsafe {
                std::slice::from_raw_parts(luma_base.cast::<u8>().add(row * luma_stride), width)
            };
            luma_out[row * width..][..width].copy_from_slice(src);
        }
        for row in 0..chroma_height {
            // SAFETY: as above; interleaved Cb,Cr pairs.
            let src = unsafe {
                std::slice::from_raw_parts(
                    chroma_base.cast::<u8>().add(row * chroma_stride),
                    chroma_width * 2,
                )
            };
            chroma_out[row * chroma_width * 2..][..chroma_width * 2].copy_from_slice(src);
        }
        Some(pixels)
    })();
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    pixels
}

/// The planar copy-out, used only by [`materialise`].
///
/// # Safety
/// `pb` must be a valid, decoded 10-bit biplanar CVPixelBuffer.
unsafe fn copy_planar_pixels(pb: &CVPixelBuffer) -> Option<Vec<u8>> {
    // SAFETY: valid pixel buffer; lock for CPU read access.
    let lock = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if lock != 0 {
        return None;
    }
    let pixels = (|| {
        let width = CVPixelBufferGetWidthOfPlane(pb, 0);
        let height = CVPixelBufferGetHeightOfPlane(pb, 0);
        let (luma_base, luma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
        );
        let (chroma_base, chroma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 1),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
        );
        let chroma_height = CVPixelBufferGetHeightOfPlane(pb, 1);
        let chroma_width = CVPixelBufferGetWidthOfPlane(pb, 1);
        if luma_base.is_null() || chroma_base.is_null() || width == 0 || height == 0 {
            return None;
        }
        let mut pixels = vec![0u8; width * height * 2 + chroma_width * chroma_height * 4];
        let (luma_out, chroma_out) = pixels.split_at_mut(width * height * 2);
        for row in 0..height {
            // SAFETY: row < plane height, so `width` 16-bit samples from the
            // row start lie in the locked plane.
            let src = unsafe {
                std::slice::from_raw_parts(luma_base.cast::<u8>().add(row * luma_stride), width * 2)
            };
            luma_out[row * width * 2..][..width * 2].copy_from_slice(src);
        }
        for row in 0..chroma_height {
            // SAFETY: as above; interleaved Cb,Cr pairs, so two samples wide.
            let src = unsafe {
                std::slice::from_raw_parts(
                    chroma_base.cast::<u8>().add(row * chroma_stride),
                    chroma_width * 4,
                )
            };
            chroma_out[row * chroma_width * 4..][..chroma_width * 4].copy_from_slice(src);
        }
        Some(pixels)
    })();
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    pixels
}

/// Copy a locked BGRA pixel buffer into a tightly-packed RGBA frame.
///
/// # Safety
/// `image` must be a valid, decoded CVPixelBuffer.
unsafe fn copy_bgra(pb: &CVPixelBuffer) -> Option<DecodedFrame> {
    // SAFETY: valid pixel buffer; lock for CPU read access.
    let lock = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if lock != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(pb);
    let height = CVPixelBufferGetHeight(pb);
    let (base, stride) = (
        CVPixelBufferGetBaseAddress(pb),
        CVPixelBufferGetBytesPerRow(pb),
    );
    let frame = if base.is_null() || width == 0 || height == 0 {
        None
    } else {
        // Straight row memcpys, keeping BGRA order — the presenter samples
        // a BGRA texture, so no CPU swizzle is ever needed.
        let mut pixels = vec![0u8; width * height * 4];
        for row in 0..height {
            // SAFETY: row < height, so base+row*stride..+width*4 is in the
            // locked buffer.
            let src = unsafe {
                std::slice::from_raw_parts(base.cast::<u8>().add(row * stride), width * 4)
            };
            pixels[row * width * 4..][..width * 4].copy_from_slice(src);
        }
        Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            pixels,
            order: gsa_client_core::PixelOrder::Bgra,
            platform: None,
        })
    };
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// A transfer function's short name, for somewhere with no room for
/// `SMPTE_ST_2084_PQ`.
fn short_transfer_name(name: &str) -> String {
    if name.contains("2084") {
        "PQ".to_string()
    } else if name.contains("HLG") || name.contains("2100") {
        "HLG".to_string()
    } else if name.contains("709") {
        "BT.709".to_string()
    } else if name.contains("2020") {
        "BT.2020".to_string()
    } else {
        name.to_string()
    }
}

/// AV1's colour codes are the ones ISO/IEC 23091-2 assigns, shared with HEVC
/// and H.264 — so these mappings are the same ones VideoToolbox applies when
/// it reads an HEVC bitstream itself. `None` means unspecified or unhandled,
/// which is reported as unknown rather than guessed at.
fn colour_primaries(code: u8) -> Option<&'static CFString> {
    // SAFETY: statics from the framework, valid for the process.
    unsafe {
        match code {
            1 => Some(kCMFormatDescriptionColorPrimaries_ITU_R_709_2),
            // 170M and 240M share BT.601's primaries, which CoreMedia names
            // after SMPTE C.
            6 | 7 => Some(kCMFormatDescriptionColorPrimaries_SMPTE_C),
            9 => Some(kCMFormatDescriptionColorPrimaries_ITU_R_2020),
            _ => None,
        }
    }
}

fn transfer_function(code: u8) -> Option<&'static CFString> {
    // SAFETY: as above.
    unsafe {
        match code {
            // 170M carries the same curve as BT.709.
            1 | 6 => Some(kCMFormatDescriptionTransferFunction_ITU_R_709_2),
            7 => Some(kCMFormatDescriptionTransferFunction_SMPTE_240M_1995),
            16 => Some(kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ),
            18 => Some(kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG),
            _ => None,
        }
    }
}

fn ycbcr_matrix(code: u8) -> Option<&'static CFString> {
    // SAFETY: as above.
    unsafe {
        match code {
            1 => Some(kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2),
            6 => Some(kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4),
            7 => Some(kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995),
            9 => Some(kCMFormatDescriptionYCbCrMatrix_ITU_R_2020),
            _ => None,
        }
    }
}

/// Ten-bit samples sit in the top of each 16-bit word, six zero bits below.
///
/// Measured, not assumed: reading them as if they were in the low bits gives
/// values around 48,000 where 1,023 is the ceiling, and every pixel is wrong.
const TEN_BIT_SHIFT: u32 = 6;
const TEN_BIT_MAX: usize = 1023;

/// The YCbCr→RGB matrix the stream asked to be read with.
/// What the presenter cannot infer from the samples themselves.
///
/// The shader does the conversion now, but it cannot guess how the values
/// are encoded, and reading them under the wrong assumption gives a washed-out
/// or crushed picture that reads as a bad stream rather than as a bug. So the
/// stream's own colour description is still required before any 10-bit frame
/// is interpreted — it is carried to the GPU instead of baked into per-pixel
/// tables.
#[derive(Debug, Clone, Copy)]
struct StreamColour {
    full_range: bool,
    /// BT.601 rather than BT.709, from the stream's own matrix tag. Two
    /// coefficients apart — enough to shift every colour if ignored, and this
    /// host really does tag SDR as 601.
    bt601: bool,
}

impl StreamColour {
    /// `None` when a 10-bit stream is not PQ, which refuses the frame rather
    /// than showing it wrongly. 8-bit streams always build: their matrix tag
    /// decides the coefficients, untagged HD meaning BT.709.
    fn build(colour: &crate::hdr_probe::ColourReport, full_range: bool) -> Option<Self> {
        let bt601 = colour.matrix.as_deref().is_some_and(|m| m.contains("601"));
        let ten_bit = colour.stream_bit_depth.is_some_and(|bits| bits > 8);
        let pq = colour
            .transfer
            .as_deref()
            .is_some_and(|t| t.contains("2084"));
        if ten_bit && !pq {
            // HLG is the other HDR curve, and no host in play sends it.
            // Guessing at it would be worse than saying so.
            tracing::warn!(
                transfer = colour.transfer.as_deref().unwrap_or("none"),
                "10-bit stream is not PQ; it will not be presented"
            );
            return None;
        }
        Some(Self { full_range, bt601 })
    }
}

/// The luma code that `nits` of diffuse white sits at, for this range.
fn reference_white_code(nits: f32, offset: f32, span: f32) -> u16 {
    // Invert the EOTF by search rather than by a second formula: one curve in
    // the file means the two cannot drift apart.
    let target = (0..=TEN_BIT_MAX)
        .find(|code| {
            #[allow(clippy::cast_precision_loss)]
            let normalised = ((*code as f32) - offset) / span;
            pq_eotf_nits(normalised.clamp(0.0, 1.0)) >= nits
        })
        .unwrap_or(TEN_BIT_MAX);
    #[allow(clippy::cast_possible_truncation)]
    let code = target as u16;
    code
}

/// The PQ EOTF (SMPTE ST 2084), code value in [0,1] to absolute nits.
pub fn pq_eotf_nits(code: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let powed = code.max(0.0).powf(1.0 / M2);
    let numerator = (powed - C1).max(0.0);
    let denominator = C2 - C3 * powed;
    if denominator <= 0.0 {
        return 10_000.0;
    }
    (numerator / denominator).powf(1.0 / M1) * 10_000.0
}

/// Where black and white sit, and how far chroma swings, for each range.
///
/// Video range puts luma in [64,940] and chroma in [64,960]; full range uses
/// the whole word. Reading one as the other crushes or clips.
fn range_constants(full_range: bool) -> (f32, f32, f32) {
    if full_range {
        (0.0, 1023.0, 1023.0)
    } else {
        (64.0, 876.0, 896.0)
    }
}

/// Copy a 10-bit biplanar YCbCr buffer into an 8-bit RGBA frame.
///
/// The window presents 8 bits, so the extra precision is measured here and
/// then folded down for display — receiving it is what stops the decoder
/// discarding it, and what the range statistics are read from. Showing all
/// ten bits additionally needs a wide-colour surface, which is not this.
///
/// # Safety
/// `pb` must be a valid, decoded 10-bit biplanar CVPixelBuffer.
/// Split an Annex-B stream into NAL unit payloads (no start codes).
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i < data.len() {
        let is_start3 = i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1];
        let is_start4 = i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1];
        if is_start4 || is_start3 {
            if let Some(s) = start {
                nals.push(&data[s..i]);
            }
            i += if is_start4 { 4 } else { 3 };
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        nals.push(&data[s..]);
    }
    nals
}

/// Wrap AVCC bytes in a CMSampleBuffer for the decoder.
fn avcc_sample_buffer(
    avcc: &[u8],
    format: &CMFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block_raw: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: NULL memory block + block_allocator => CoreMedia allocates
    // `block_length` bytes internally; we then copy our data in. Valid
    // out-pointer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            avcc.len(),
            None,
            ptr::null(),
            0,
            avcc.len(),
            2, // kCMBlockBufferAssureMemoryNowFlag
            NonNull::from(&mut block_raw),
        )
    };
    let block = NonNull::new(block_raw)
        .filter(|_| status == 0)
        // SAFETY: +1 from create.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| Error::Decode(format!("CMBlockBufferCreate: {status}")))?;
    // SAFETY: block has `avcc.len()` bytes assured; copy our payload in.
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(
            NonNull::new(avcc.as_ptr().cast_mut().cast::<c_void>()).expect("non-empty"),
            &block,
            0,
            avcc.len(),
        )
    };
    if status != 0 {
        return Err(Error::Decode(format!(
            "CMBlockBufferReplaceDataBytes: {status}"
        )));
    }

    let mut sample_raw: *mut CMSampleBuffer = ptr::null_mut();
    let sample_size = avcc.len();
    // SAFETY: ready data buffer + format description; no timing needed for
    // immediate display (we present newest-wins); valid out-pointer.
    let status = unsafe {
        CMSampleBuffer::create(
            None,
            Some(&block),
            true,
            None,
            ptr::null_mut(),
            Some(format),
            1,
            0,
            ptr::null(),
            1,
            &sample_size,
            NonNull::from(&mut sample_raw),
        )
    };
    NonNull::new(sample_raw)
        .filter(|_| status == 0)
        // SAFETY: +1 from create.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| Error::Decode(format!("CMSampleBufferCreate: {status}")))
}

/// Open a decompression session producing `pixel_format`.
fn create_session(
    format: &CMFormatDescription,
    pixel_format: u32,
) -> Result<CFRetained<VTDecompressionSession>> {
    let attrs = output_attrs(pixel_format)?;
    let mut raw: *mut VTDecompressionSession = ptr::null_mut();
    // SAFETY: valid format + attrs; null callback record (we use the
    // per-frame output handler API); valid out-pointer.
    let status = unsafe {
        VTDecompressionSession::create(
            None,
            format,
            None,
            Some(&attrs),
            ptr::null(),
            NonNull::from(&mut raw),
        )
    };
    NonNull::new(raw)
        .filter(|_| status == 0)
        // SAFETY: create returned +1; take ownership.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| Error::Decode(format!("VTDecompressionSessionCreate: {status}")))
}

/// `{ PixelFormatType: <fourcc> }` destination attributes.
fn output_attrs(pixel_format: u32) -> Result<CFRetained<CFDictionary>> {
    let format = pixel_format as i32;
    // SAFETY: value_ptr points at a live i32.
    let number = unsafe { CFNumber::new(None, CF_NUMBER_SINT32, (&format as *const i32).cast()) }
        .ok_or_else(err("CFNumberCreate failed"))?;
    // SAFETY: single-entry CFType dictionary; key is a static CFString,
    // value retained by the dictionary callbacks.
    let dict = unsafe {
        let key: *const objc2_core_foundation::CFString = kCVPixelBufferPixelFormatTypeKey;
        let value = CFRetained::as_ptr(&number).as_ptr();
        let mut keys: [*const c_void; 1] = [key.cast()];
        let mut values: [*const c_void; 1] = [value.cast()];
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(err("CFDictionaryCreate failed"))?;
    Ok(dict)
}

fn err(msg: &'static str) -> impl Fn() -> Error {
    move || Error::Decode(msg.into())
}

#[cfg(test)]
mod tests {
    use super::parameter_set_slot;
    use gsa_core::media::Codec;

    /// The two codecs keep the NAL type in different bits, so each one's
    /// parameter sets are invisible to the other's reading — silently, as
    /// picture data. A stream then decodes nothing while looking well-formed.
    #[test]
    fn parameter_sets_are_found_by_the_right_codecs_rules() {
        // H.264: type in the low 5 bits. SPS = 7, PPS = 8, IDR = 5.
        assert_eq!(parameter_set_slot(Codec::H264, &[0x67]), Some(1));
        assert_eq!(parameter_set_slot(Codec::H264, &[0x68]), Some(2));
        assert_eq!(parameter_set_slot(Codec::H264, &[0x65]), None);

        // HEVC: type in bits 1-6. VPS = 32, SPS = 33, PPS = 34, IDR = 19.
        assert_eq!(parameter_set_slot(Codec::Hevc, &[32 << 1, 0x01]), Some(0));
        assert_eq!(parameter_set_slot(Codec::Hevc, &[33 << 1, 0x01]), Some(1));
        assert_eq!(parameter_set_slot(Codec::Hevc, &[34 << 1, 0x01]), Some(2));
        assert_eq!(parameter_set_slot(Codec::Hevc, &[19 << 1, 0x01]), None);

        // Each codec's sets read as picture data under the other's rules,
        // which is why the decoder must be built for the negotiated codec.
        assert_eq!(parameter_set_slot(Codec::H264, &[33 << 1, 0x01]), None);
        assert_eq!(parameter_set_slot(Codec::Hevc, &[0x67]), None);
    }

    /// An empty NAL is not a parameter set and must not index a slot.
    #[test]
    fn an_empty_nal_is_not_a_parameter_set() {
        assert_eq!(parameter_set_slot(Codec::H264, &[]), None);
        assert_eq!(parameter_set_slot(Codec::Hevc, &[]), None);
    }

    use super::*;

    /// Inverse PQ: absolute nits to a code value in [0,1].
    fn pq_code_for(nits: f32) -> f32 {
        const M1: f32 = 2610.0 / 16384.0;
        const M2: f32 = 2523.0 / 4096.0 * 128.0;
        const C1: f32 = 3424.0 / 4096.0;
        const C2: f32 = 2413.0 / 4096.0 * 32.0;
        const C3: f32 = 2392.0 / 4096.0 * 32.0;
        let l = (nits / 10_000.0).powf(M1);
        ((C1 + C2 * l) / (1.0 + C3 * l)).powf(M2)
    }

    /// The curve has to round-trip, or every brightness below is off.
    #[test]
    fn the_pq_curve_inverts_itself() {
        for nits in [0.1f32, 1.0, 100.0, 203.0, 1000.0, 10_000.0] {
            let round_tripped = pq_eotf_nits(pq_code_for(nits));
            assert!(
                (round_tripped - nits).abs() < nits * 0.01 + 0.01,
                "{nits} nits came back as {round_tripped}"
            );
        }
    }

    /// The mapping is checked against what VideoToolbox itself reported for
    /// the *same host's HEVC stream of the same desktop*, where the platform
    /// read the bitstream rather than us: SDR came back SMPTE_C / 709 / 601_4
    /// and HDR came back 2020 / PQ / 2020. AV1 carries the same ISO colour
    /// codes, so the two codecs must describe that picture identically — if
    /// they disagree, one of them is being decoded wrongly.
    #[test]
    fn av1_colour_codes_map_to_what_the_platform_reads_from_hevc() {
        let name = |s: Option<&'static CFString>| s.map(std::string::ToString::to_string);

        // Apollo's SDR AV1 header: 6 / 6 / 6.
        assert_eq!(name(colour_primaries(6)).as_deref(), Some("SMPTE_C"));
        assert_eq!(name(transfer_function(6)).as_deref(), Some("ITU_R_709_2"));
        assert_eq!(name(ycbcr_matrix(6)).as_deref(), Some("ITU_R_601_4"));

        // Apollo's HDR AV1 header: 9 / 16 / 9.
        assert_eq!(name(colour_primaries(9)).as_deref(), Some("ITU_R_2020"));
        assert_eq!(
            name(transfer_function(16)).as_deref(),
            Some("SMPTE_ST_2084_PQ")
        );
        assert_eq!(name(ycbcr_matrix(9)).as_deref(), Some("ITU_R_2020"));

        // Unspecified (2) must stay unknown. Filling in BT.709 here would be a
        // guess that nothing downstream could tell from a real reading.
        assert_eq!(colour_primaries(2), None);
        assert_eq!(transfer_function(2), None);
        assert_eq!(ycbcr_matrix(2), None);
    }

    /// The whole point of carrying the codes: only a stream that declares PQ
    /// may be presented down the planar path. Guessing at anything else shows
    /// a wrong picture that reads as a bad stream.
    #[test]
    fn only_a_pq_description_is_presentable() {
        let pq = crate::hdr_probe::ColourReport {
            primaries: Some("ITU_R_2020".into()),
            transfer: Some("SMPTE_ST_2084_PQ".into()),
            matrix: Some("ITU_R_2020".into()),
            ..Default::default()
        };
        let colour = StreamColour::build(&pq, false).expect("PQ is presentable");
        assert!(!colour.full_range);
        assert!(
            StreamColour::build(&pq, true)
                .expect("full range carried")
                .full_range
        );

        // A *10-bit* stream that does not declare PQ must be refused, not
        // shown wrongly.
        let ten_bit_unknown = crate::hdr_probe::ColourReport {
            stream_bit_depth: Some(10),
            ..Default::default()
        };
        assert!(StreamColour::build(&ten_bit_unknown, false).is_none());

        // An 8-bit stream always presents; its matrix tag picks the
        // coefficients, untagged meaning BT.709.
        let sdr = StreamColour::build(&crate::hdr_probe::ColourReport::default(), false)
            .expect("SDR is always presentable");
        assert!(!sdr.bt601);
        let tagged_601 = crate::hdr_probe::ColourReport {
            matrix: Some("ITU_R_601_4".into()),
            ..Default::default()
        };
        assert!(
            StreamColour::build(&tagged_601, false)
                .expect("presentable")
                .bt601,
            "the stream's own 601 tag must reach the shader"
        );
    }

    /// PQ codes are absolute, which is the whole reason brightness can be
    /// reported at all — so the code diffuse white sits at must land where the
    /// curve says, or every "above reference white" figure is meaningless.
    #[test]
    fn reference_white_lands_where_the_curve_puts_it() {
        let (offset, span, _) = range_constants(false);
        let code = reference_white_code(203.0, offset, span);
        let nits = pq_eotf_nits(((f32::from(code) - offset) / span).clamp(0.0, 1.0));
        assert!(
            (nits - 203.0).abs() < 5.0,
            "203 nits resolved to code {code}, which is {nits} nits"
        );
        // Brighter white must sit higher up the curve, never lower.
        assert!(reference_white_code(400.0, offset, span) > code);
        assert!(reference_white_code(100.0, offset, span) < code);
    }

    /// An SDR desktop wrapped in a PQ signal and a real HDR picture are told
    /// apart by the distribution, not the tag — so the percentile has to be
    /// read from the whole histogram rather than from the peak.
    #[test]
    fn percentiles_come_from_the_distribution_not_the_peak() {
        let mut stats = crate::hdr_probe::LumaStats::default();
        for _ in 0..990 {
            stats.observe(500);
        }
        // A handful of specular highlights, and one stuck pixel at the top.
        for _ in 0..9 {
            stats.observe(800);
        }
        stats.observe(1023);

        assert_eq!(stats.max, 1023, "the peak sees the outlier");
        assert_eq!(stats.percentile(0.50), 500, "the bulk does not");
        assert!(stats.percentile(0.99) <= 800, "nor does the 99th");
        assert_eq!(stats.count_above(500), 10);
        assert_eq!(stats.count_above(1023), 0);
    }

    #[test]
    fn annex_b_split_handles_3_and_4_byte_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let nals = split_annex_b(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
        assert_eq!(nals[2], &[0x65, 9, 9]);
    }
}
