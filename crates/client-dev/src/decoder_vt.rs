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
use std::sync::{Arc, mpsc};

use block2::RcBlock;
use objc2_core_foundation::{
    CFData, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString,
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMVideoCodecType_AV1,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
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
use gsa_client_core::{DecodedFrame, VideoDecoder};
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
    conversion: Option<Arc<Conversion>>,
    /// Whether the output format has been reported once.
    reported_output: bool,
    /// Whether a frame carrying actual variation has been measured.
    reported_luma: bool,
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
            conversion: None,
            reported_output: false,
            reported_luma: false,
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
        // SAFETY: as above — a static framework key and a live dictionary.
        let extensions = unsafe {
            let key: *const CFString =
                kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms;
            let mut keys: [*const c_void; 1] = [key.cast()];
            let mut values: [*const c_void; 1] = [CFRetained::as_ptr(&atoms).as_ptr().cast()];
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
            "stream colour description"
        );
        self.output_format =
            crate::hdr_probe::wanted_output_format(colour.stream_bit_depth, colour.full_range);
        self.conversion = Some(Arc::new(Conversion::build(
            &colour,
            self.output_format == crate::hdr_probe::PIXEL_FORMAT_420_10_FULL,
            self.mapping,
        )));
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
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>> {
        // AV1 is not Annex-B: no start codes, no parameter-set NALs, and the
        // sample is the temporal unit exactly as it arrived.
        if self.codec == Codec::Av1 {
            self.ensure_av1_session(access_unit)?;
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
    fn report_luma(&mut self, luma: crate::hdr_probe::LumaStats, clipped: u64) {
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
        let conversion = self.conversion.clone();
        let handler = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _dur: CMTime| {
                let frame = if status == 0 && !image.is_null() {
                    // SAFETY: non-null decoded image buffer from VideoToolbox,
                    // valid for the duration of this callback.
                    unsafe { copy_frame(&*image, conversion.as_deref()) }
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
                self.report_luma(decoded.luma, decoded.clipped);
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
unsafe fn copy_frame(image: &CVImageBuffer, conversion: Option<&Conversion>) -> Option<Decoded> {
    let pb: &CVPixelBuffer = image;
    let format = CVPixelBufferGetPixelFormatType(pb);
    // SAFETY: valid pixel buffer, locked for the whole read.
    if format == crate::hdr_probe::PIXEL_FORMAT_420_10_VIDEO
        || format == crate::hdr_probe::PIXEL_FORMAT_420_10_FULL
    {
        // The tables are built with the session; a 10-bit buffer without them
        // cannot be read at all, so refusing beats guessing at a conversion.
        // SAFETY: caller contract — a valid decoded buffer, and the format
        // check above says it is the biplanar 10-bit layout.
        return unsafe { copy_biplanar_10bit(pb, conversion?) };
    }
    // SAFETY: as above.
    unsafe { copy_bgra(pb) }.map(|frame| Decoded {
        frame,
        luma: crate::hdr_probe::LumaStats::default(),
        clipped: 0,
    })
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
        })
    };
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// Ten-bit samples sit in the top of each 16-bit word, six zero bits below.
///
/// Measured, not assumed: reading them as if they were in the low bits gives
/// values around 48,000 where 1,023 is the ceiling, and every pixel is wrong.
const TEN_BIT_SHIFT: u32 = 6;
const TEN_BIT_MAX: usize = 1023;

/// The YCbCr→RGB matrix the stream asked to be read with.
///
/// Honoured rather than assumed: a host may tag an HD stream BT.601, and
/// decoding it as BT.709 shifts every colour slightly with nothing to show
/// for it. The two coefficients are all that differ between the standards.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColourMatrix {
    kr: f32,
    kb: f32,
}

impl ColourMatrix {
    const BT601: Self = Self {
        kr: 0.299,
        kb: 0.114,
    };
    const BT709: Self = Self {
        kr: 0.2126,
        kb: 0.0722,
    };
    const BT2020: Self = Self {
        kr: 0.2627,
        kb: 0.0593,
    };

    /// BT.709 is the fallback: it is what an untagged HD stream means, and
    /// every stream reaching this decoder is HD.
    fn from_report(colour: &crate::hdr_probe::ColourReport) -> Self {
        match colour.matrix.as_deref() {
            Some(m) if m.contains("2020") => Self::BT2020,
            Some(m) if m.contains("601") => Self::BT601,
            _ => Self::BT709,
        }
    }
}

/// How to turn what the decoder produced into what the window can show.
///
/// An SDR stream needs only the matrix. A PQ stream needs its curve undone
/// and its gamut narrowed as well, and skipping that does not fail — it
/// produces a washed-out, oversaturated picture that reads as a bad stream.
enum Conversion {
    Sdr(Box<ConversionTables>),
    Pq(Box<PqTables>),
}

impl Conversion {
    fn build(
        colour: &crate::hdr_probe::ColourReport,
        full_range: bool,
        mapping: DisplayMapping,
    ) -> Self {
        let matrix = ColourMatrix::from_report(colour);
        match colour.transfer.as_deref() {
            Some(t) if t.contains("2084") => {
                Self::Pq(Box::new(PqTables::build(matrix, full_range, mapping)))
            }
            // HLG is the other HDR curve. No host in play sends it, and
            // guessing at it would be worse than saying so.
            Some(t) if t.contains("HLG") => {
                tracing::warn!(
                    transfer = t,
                    "HLG transfer is not converted; the picture will be shown as if SDR"
                );
                Self::Sdr(Box::new(ConversionTables::build(matrix, full_range)))
            }
            _ => Self::Sdr(Box::new(ConversionTables::build(matrix, full_range))),
        }
    }

    /// One 10-bit sample triple to 8-bit RGB.
    fn rgb(&self, y: usize, cb: usize, cr: usize) -> [u8; 3] {
        match self {
            Self::Sdr(t) => {
                let luma = t.luma[y];
                [
                    clamp_u8(luma + t.r_from_cr[cr]),
                    clamp_u8(luma + t.g_from_cb[cb] + t.g_from_cr[cr]),
                    clamp_u8(luma + t.b_from_cb[cb]),
                ]
            }
            Self::Pq(t) => t.rgb(y, cb, cr),
        }
    }
}

/// PQ decode, gamut narrowing and SDR re-encode, all as lookups.
///
/// The curve is per-component and the gamut is a matrix in linear light, so
/// the order matters: decode PQ first, convert primaries second, encode last.
struct PqTables {
    /// Luma and chroma contributions, in PQ-coded RGB.
    luma: [f32; 1024],
    r_from_cr: [f32; 1024],
    g_from_cb: [f32; 1024],
    g_from_cr: [f32; 1024],
    b_from_cb: [f32; 1024],
    /// PQ code value to linear light, scaled so reference white is 1.0.
    eotf: [f32; 1024],
    /// Linear light back to an 8-bit SDR display value.
    encode: [u8; 1024],
}

impl PqTables {
    fn build(matrix: ColourMatrix, full_range: bool, mapping: DisplayMapping) -> Self {
        let (kr, kb) = (matrix.kr, matrix.kb);
        let kg = 1.0 - kr - kb;
        let (luma_offset, luma_span, chroma_span) = range_constants(full_range);
        let mut t = Self {
            luma: [0.0; 1024],
            r_from_cr: [0.0; 1024],
            g_from_cb: [0.0; 1024],
            g_from_cr: [0.0; 1024],
            b_from_cb: [0.0; 1024],
            eotf: [0.0; 1024],
            encode: [0; 1024],
        };
        for sample in 0..1024usize {
            t.luma[sample] = (sample as f32 - luma_offset) / luma_span;
            let c = (sample as f32 - 512.0) / chroma_span;
            t.r_from_cr[sample] = 2.0 * (1.0 - kr) * c;
            t.b_from_cb[sample] = 2.0 * (1.0 - kb) * c;
            t.g_from_cb[sample] = -2.0 * kb * (1.0 - kb) / kg * c;
            t.g_from_cr[sample] = -2.0 * kr * (1.0 - kr) / kg * c;
            let code = sample as f32 / TEN_BIT_MAX as f32;
            t.eotf[sample] = pq_eotf_nits(code) / mapping.sdr_white_nits;
            t.encode[sample] = (srgb_encode(code) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        t
    }

    fn rgb(&self, y: usize, cb: usize, cr: usize) -> [u8; 3] {
        let luma = self.luma[y];
        let coded = [
            luma + self.r_from_cr[cr],
            luma + self.g_from_cb[cb] + self.g_from_cr[cr],
            luma + self.b_from_cb[cb],
        ];
        let linear = coded.map(|v| {
            let index = (v * TEN_BIT_MAX as f32).clamp(0.0, TEN_BIT_MAX as f32) as usize;
            self.eotf[index]
        });
        // BT.2020 to BT.709 primaries, in linear light. Out-of-gamut results
        // are normal for saturated colour and clip on the way out.
        let (r, g, b) = (linear[0], linear[1], linear[2]);
        let narrowed = [
            1.6605 * r - 0.5876 * g - 0.0728 * b,
            -0.1246 * r + 1.1329 * g - 0.0083 * b,
            -0.0182 * r - 0.1006 * g + 1.1187 * b,
        ];
        narrowed.map(|v| {
            let index = (v * TEN_BIT_MAX as f32).clamp(0.0, TEN_BIT_MAX as f32) as usize;
            self.encode[index]
        })
    }
}

/// The PQ EOTF (SMPTE ST 2084), code value in [0,1] to absolute nits.
fn pq_eotf_nits(code: f32) -> f32 {
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

/// Linear light to an sRGB display value, both in [0,1].
fn srgb_encode(linear: f32) -> f32 {
    let v = linear.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
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
unsafe fn copy_biplanar_10bit(pb: &CVPixelBuffer, conversion: &Conversion) -> Option<Decoded> {
    // SAFETY: valid pixel buffer; lock for CPU read access.
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
        let (chroma_base, chroma_stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 1),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
        );
        let chroma_height = CVPixelBufferGetHeightOfPlane(pb, 1);
        let chroma_width = CVPixelBufferGetWidthOfPlane(pb, 1);
        if luma_base.is_null() || chroma_base.is_null() || width == 0 || height == 0 {
            return None;
        }

        let mut pixels = vec![0u8; width * height * 4];
        let mut stats = crate::hdr_probe::LumaStats::default();
        let mut clipped = 0u64;
        // Every sample OR'd together: if any low bit is ever set, the samples
        // are not where this code reads them from.
        let mut low_bits: u16 = 0;

        for row in 0..height {
            // SAFETY: row < plane height, so the row start and `width`
            // 16-bit samples from it lie in the locked plane.
            let luma = unsafe {
                std::slice::from_raw_parts(
                    luma_base.cast::<u8>().add(row * luma_stride).cast::<u16>(),
                    width,
                )
            };
            let chroma_row = (row / 2).min(chroma_height.saturating_sub(1));
            // SAFETY: as above; the chroma plane is interleaved Cb,Cr pairs.
            let chroma = unsafe {
                std::slice::from_raw_parts(
                    chroma_base
                        .cast::<u8>()
                        .add(chroma_row * chroma_stride)
                        .cast::<u16>(),
                    chroma_width * 2,
                )
            };
            let out = &mut pixels[row * width * 4..][..width * 4];
            for col in 0..width {
                let raw = luma[col];
                low_bits |= raw;
                let y = (raw >> TEN_BIT_SHIFT) as usize;
                stats.observe(y as u16);
                let chroma_col = (col / 2).min(chroma_width.saturating_sub(1));
                let cb = (chroma[chroma_col * 2] >> TEN_BIT_SHIFT) as usize;
                let cr = (chroma[chroma_col * 2 + 1] >> TEN_BIT_SHIFT) as usize;
                let rgb = conversion.rgb(y.min(TEN_BIT_MAX), cb, cr);
                if rgb == [255, 255, 255] {
                    clipped += 1;
                }
                let px = &mut out[col * 4..][..4];
                px[..3].copy_from_slice(&rgb);
                px[3] = 255;
            }
        }

        // The one reading that would make everything above wrong: the samples
        // are ten bits at the top of a 16-bit word, and the whole picture is
        // misread if they are anywhere else.
        if low_bits & ((1 << TEN_BIT_SHIFT) - 1) != 0 {
            tracing::error!(
                low_bits = format!("{:#06x}", low_bits),
                "10-bit samples are not left-justified in their word; \
                 the conversion is reading them wrongly"
            );
        }

        Some(Decoded {
            frame: DecodedFrame {
                width: width as u32,
                height: height as u32,
                pixels,
                order: gsa_client_core::PixelOrder::Rgba,
            },
            luma: stats,
            clipped,
        })
    })();
    // SAFETY: paired with the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    decoded
}

/// Per-component contributions for every possible 10-bit sample.
///
/// A table rather than arithmetic per pixel: this runs on two million pixels
/// a frame, and the tables are a thousand entries built once.
struct ConversionTables {
    luma: [i32; 1024],
    r_from_cr: [i32; 1024],
    g_from_cb: [i32; 1024],
    g_from_cr: [i32; 1024],
    b_from_cb: [i32; 1024],
}

impl ConversionTables {
    fn build(matrix: ColourMatrix, full_range: bool) -> Self {
        let (kr, kb) = (matrix.kr, matrix.kb);
        let kg = 1.0 - kr - kb;
        let (luma_offset, luma_span, chroma_span) = range_constants(full_range);
        let mut tables = Self {
            luma: [0; 1024],
            r_from_cr: [0; 1024],
            g_from_cb: [0; 1024],
            g_from_cr: [0; 1024],
            b_from_cb: [0; 1024],
        };
        for sample in 0..1024usize {
            let y = (sample as f32 - luma_offset) / luma_span;
            tables.luma[sample] = (y * 255.0 * 256.0) as i32;
            let c = (sample as f32 - 512.0) / chroma_span;
            tables.r_from_cr[sample] = (2.0 * (1.0 - kr) * c * 255.0 * 256.0) as i32;
            tables.b_from_cb[sample] = (2.0 * (1.0 - kb) * c * 255.0 * 256.0) as i32;
            tables.g_from_cb[sample] = (-2.0 * kb * (1.0 - kb) / kg * c * 255.0 * 256.0) as i32;
            tables.g_from_cr[sample] = (-2.0 * kr * (1.0 - kr) / kg * c * 255.0 * 256.0) as i32;
        }
        tables
    }
}

/// Round a Q8 fixed-point value to a byte, clipping out-of-gamut results.
fn clamp_u8(value: i32) -> u8 {
    ((value + 128) >> 8).clamp(0, 255) as u8
}

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
    use crate::decoder::DisplayMapping;
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

    /// Convert one sample the way the pixel loop does.
    fn to_rgb(tables: &ConversionTables, y: usize, cb: usize, cr: usize) -> [u8; 3] {
        let luma = tables.luma[y];
        [
            clamp_u8(luma + tables.r_from_cr[cr]),
            clamp_u8(luma + tables.g_from_cb[cb] + tables.g_from_cr[cr]),
            clamp_u8(luma + tables.b_from_cb[cb]),
        ]
    }

    /// Video range does not start at zero. Reading its floor and ceiling as
    /// full range washes out black and clips white, which looks like a bad
    /// stream rather than a bad conversion.
    #[test]
    fn video_range_endpoints_land_on_black_and_white() {
        let tables = ConversionTables::build(ColourMatrix::BT709, false);
        assert_eq!(to_rgb(&tables, 64, 512, 512), [0, 0, 0]);
        assert_eq!(to_rgb(&tables, 940, 512, 512), [255, 255, 255]);
        // Below the floor is legal in the bitstream and clips to black.
        assert_eq!(to_rgb(&tables, 0, 512, 512), [0, 0, 0]);

        let full = ConversionTables::build(ColourMatrix::BT709, true);
        assert_eq!(to_rgb(&full, 0, 512, 512), [0, 0, 0]);
        assert_eq!(to_rgb(&full, 1023, 512, 512), [255, 255, 255]);
    }

    /// The matrices differ only in two coefficients, and the difference is
    /// small enough to look like nothing until a saturated colour is checked.
    #[test]
    fn the_matrices_disagree_where_they_should() {
        let (bt601, bt709) = (
            ConversionTables::build(ColourMatrix::BT601, false),
            ConversionTables::build(ColourMatrix::BT709, false),
        );
        // Mid luma with a full red chroma excursion.
        let (y, cb, cr) = (502, 512, 960);
        assert_ne!(to_rgb(&bt601, y, cb, cr), to_rgb(&bt709, y, cb, cr));
        // Grey has no chroma excursion, so every matrix agrees on it.
        assert_eq!(to_rgb(&bt601, y, 512, 512), to_rgb(&bt709, y, 512, 512));
    }

    /// The tag is followed, not guessed: this host labels 1080p BT.601, and
    /// overriding that with the "obvious" HD matrix would shift every colour.
    #[test]
    fn the_streams_own_matrix_tag_is_honoured() {
        let tagged = |m: &str| {
            ColourMatrix::from_report(&crate::hdr_probe::ColourReport {
                matrix: Some(m.into()),
                ..Default::default()
            })
        };
        assert_eq!(tagged("ITU_R_601_4"), ColourMatrix::BT601);
        assert_eq!(tagged("ITU_R_709_2"), ColourMatrix::BT709);
        assert_eq!(tagged("ITU_R_2020"), ColourMatrix::BT2020);
        // Untagged HD means BT.709.
        assert_eq!(
            ColourMatrix::from_report(&crate::hdr_probe::ColourReport::default()),
            ColourMatrix::BT709
        );
    }

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

    /// A PQ stream shown on an SDR window hangs on where reference white sits:
    /// too low and the desktop glares, too high and it is a grey wash. Neither
    /// fails — they just look wrong, so the anchor is pinned here.
    #[test]
    fn reference_white_reaches_white_and_black_stays_black() {
        let mapping = DisplayMapping::default();
        let tables = PqTables::build(ColourMatrix::BT2020, false, mapping);
        let luma_code = |code: f32| (64.0 + code * 876.0).round() as usize;

        let white = tables.rgb(luma_code(pq_code_for(mapping.sdr_white_nits)), 512, 512);
        for channel in white {
            assert!(channel >= 250, "reference white came out at {white:?}");
        }

        assert_eq!(tables.rgb(luma_code(0.0), 512, 512), [0, 0, 0]);

        // Above reference white there is nothing left to give: highlights clip
        // rather than wrapping around.
        let highlight = tables.rgb(luma_code(pq_code_for(1000.0)), 512, 512);
        assert_eq!(highlight, [255, 255, 255]);

        // And the midpoint must actually sit between the two.
        let mid = tables.rgb(luma_code(pq_code_for(50.0)), 512, 512);
        assert!(
            (1..255).contains(&mid[0]),
            "50 nits collapsed to {mid:?} instead of a mid grey"
        );
    }

    /// Grey is grey in any gamut: the BT.2020 to BT.709 matrix must leave the
    /// neutral axis alone, which is the one error it can make invisibly.
    #[test]
    fn narrowing_the_gamut_leaves_neutrals_neutral() {
        let tables = PqTables::build(ColourMatrix::BT2020, false, DisplayMapping::default());
        for nits in [5.0f32, 50.0, 150.0] {
            let code = (64.0 + pq_code_for(nits) * 876.0).round() as usize;
            let [r, g, b] = tables.rgb(code, 512, 512);
            assert!(
                r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1,
                "{nits} nits of grey came out as {:?}",
                [r, g, b]
            );
        }
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
