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
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSession, VTIsHardwareDecodeSupported,
};

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
    pub fn new(codec: Codec) -> Result<Self> {
        // SAFETY: a pure capability query on a codec constant.
        if !unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) } {
            return Err(Error::Decode(format!(
                "no hardware decode for {codec:?} on this machine"
            )));
        }
        Ok(Self {
            codec,
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
    fn open_session(&mut self, format: CFRetained<CMFormatDescription>) -> Result<()> {
        let attrs = bgra_output_attrs()?;
        let mut raw: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: valid format + attrs; null callback record (we use the
        // per-frame output handler API); valid out-pointer.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(&attrs),
                ptr::null(),
                NonNull::from(&mut raw),
            )
        };
        let session = NonNull::new(raw)
            .filter(|_| status == 0)
            // SAFETY: create returned +1; take ownership.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| Error::Decode(format!("VTDecompressionSessionCreate: {status}")))?;

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

        let (tx, rx) = mpsc::sync_channel::<Option<DecodedFrame>>(1);
        let handler = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _dur: CMTime| {
                let frame = if status == 0 && !image.is_null() {
                    // SAFETY: non-null decoded image buffer from VideoToolbox,
                    // valid for the duration of this callback.
                    unsafe { copy_bgra(&*image) }
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
        match rx.try_recv() {
            Ok(frame) => Ok(frame),
            Err(_) => Ok(None),
        }
    }
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

/// Copy a locked BGRA pixel buffer into a tightly-packed RGBA frame.
///
/// # Safety
/// `image` must be a valid, decoded CVPixelBuffer.
unsafe fn copy_bgra(image: &CVImageBuffer) -> Option<DecodedFrame> {
    let pb: &CVPixelBuffer = image;
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

/// `{ PixelFormatType: BGRA }` destination attributes.
fn bgra_output_attrs() -> Result<CFRetained<CFDictionary>> {
    let format = PIXEL_FORMAT_BGRA as i32;
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
