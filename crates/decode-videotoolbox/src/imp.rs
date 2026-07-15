//! The macOS implementation. Mirrors the encode crate's objc2 idioms.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc;

use block2::RcBlock;
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType, kCFBooleanTrue,
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescription,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSession,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use gsa_core::media::Codec;
use gsa_core::{Error, Result};

const CF_NUMBER_SINT32: CFNumberType = CFNumberType::SInt32Type;

/// One decoded frame: an IOSurface-backed NV12 pixel buffer, GPU-resident.
/// Nothing maps it unless [`read_luma_region`](Self::read_luma_region) is
/// called (sampled verification only — never in the measured path).
pub struct DecodedSurface {
    buffer: CFRetained<CVPixelBuffer>,
}

impl std::fmt::Debug for DecodedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedSurface")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

// SAFETY: CVPixelBuffer is a thread-safe CoreFoundation object (retain/
// release and plane access are documented thread-safe).
unsafe impl Send for DecodedSurface {}

impl DecodedSurface {
    #[must_use]
    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.buffer) as u32
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.buffer) as u32
    }

    /// Read a rectangle of the luma plane (verification helper). This maps
    /// the buffer — call it only on sampled frames, after timing is recorded.
    pub fn read_luma_region(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let pb = &self.buffer;
        // SAFETY: valid buffer, read-only lock paired with unlock below.
        let status = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
        if status != 0 {
            return Err(Error::Decode(format!("CVPixelBufferLock failed: {status}")));
        }
        let (base, stride) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 0).cast::<u8>(),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
        );
        let mut out = Vec::with_capacity((w * h) as usize);
        if !base.is_null() {
            for row in y..y + h {
                // SAFETY: within the locked plane; caller passes in-bounds
                // coordinates (checked against width/height below).
                unsafe {
                    let src = base.add(row as usize * stride + x as usize);
                    out.extend_from_slice(std::slice::from_raw_parts(src, w as usize));
                }
            }
        }
        // SAFETY: paired with the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
        if base.is_null() {
            return Err(Error::Decode("luma plane has no base address".into()));
        }
        Ok(out)
    }
}

/// Hardware-only VideoToolbox decoder. Feeds Annex-B access units; the
/// session is (re)created from the parameter sets carried on IDR frames.
pub struct VtDecoder {
    codec: Codec,
    session: Option<CFRetained<VTDecompressionSession>>,
    /// Parameter sets the current session was built from (H.264: SPS,PPS;
    /// HEVC: VPS,SPS,PPS). A change re-creates the session.
    param_sets: Vec<Vec<u8>>,
    format: Option<CFRetained<CMVideoFormatDescription>>,
}

impl std::fmt::Debug for VtDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtDecoder")
            .field("codec", &self.codec)
            .field("open", &self.session.is_some())
            .finish()
    }
}

impl VtDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        if !matches!(codec, Codec::H264 | Codec::Hevc) {
            return Err(Error::Decode(format!(
                "VideoToolbox decode does H264 and HEVC, got {codec:?}"
            )));
        }
        Ok(Self {
            codec,
            session: None,
            param_sets: Vec::new(),
            format: None,
        })
    }

    /// Decode one Annex-B access unit. `Ok(None)` until the first IDR (no
    /// parameter sets yet) or when the decoder emits nothing for this AU.
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedSurface>> {
        let nals = split_annex_b(annex_b);
        if nals.is_empty() {
            return Ok(None);
        }

        // Collect parameter sets; rebuild the session when they change.
        let ps: Vec<Vec<u8>> = nals
            .iter()
            .filter(|n| self.is_param_set(n))
            .map(|n| n.to_vec())
            .collect();
        let expected = if self.codec == Codec::Hevc { 3 } else { 2 };
        if ps.len() >= expected && ps != self.param_sets {
            self.create_session(&ps)?;
            self.param_sets = ps;
        }
        let Some(session) = &self.session else {
            return Ok(None); // waiting for the first IDR's parameter sets
        };
        let Some(format) = &self.format else {
            return Ok(None);
        };

        // AVCC/HVCC payload: 4-byte BE length prefix per NAL; parameter sets
        // and AUDs stay out (they live in the format description).
        let mut payload = Vec::with_capacity(annex_b.len() + 16);
        for nal in &nals {
            if self.is_param_set(nal) || self.is_aud(nal) {
                continue;
            }
            payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            payload.extend_from_slice(nal);
        }
        if payload.is_empty() {
            return Ok(None);
        }

        let sample = make_sample(&payload, format)?;

        // Synchronous decode: the handler runs before the call returns, so a
        // plain channel hands the buffer back without shared state.
        let (tx, rx) = mpsc::channel::<(i32, Option<CFRetained<CVPixelBuffer>>)>();
        let handler = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _duration: CMTime| {
                let buffer = NonNull::new(image).map(|p| {
                    // SAFETY: VideoToolbox hands a +0 image buffer valid for
                    // the callback; retain it to carry it out.
                    unsafe { CFRetained::retain(p.cast::<CVPixelBuffer>()) }
                });
                let _ = tx.send((status, buffer));
            },
        );
        // SAFETY: valid session + sample; flags empty = synchronous decode;
        // the handler block is copied by VideoToolbox for the call.
        let status = unsafe {
            session.decode_frame_with_output_handler(
                &sample,
                VTDecodeFrameFlags::empty(),
                ptr::null_mut(),
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            return Err(Error::Decode(format!(
                "VTDecompressionSessionDecodeFrame: {status}"
            )));
        }
        match rx.try_recv() {
            Ok((0, Some(buffer))) => Ok(Some(DecodedSurface { buffer })),
            Ok((s, _)) if s != 0 => Err(Error::Decode(format!("decode callback status {s}"))),
            _ => Ok(None),
        }
    }

    fn is_param_set(&self, nal: &[u8]) -> bool {
        let Some(&b) = nal.first() else { return false };
        match self.codec {
            Codec::Hevc => matches!((b >> 1) & 0x3f, 32..=34),
            _ => matches!(b & 0x1f, 7 | 8),
        }
    }

    fn is_aud(&self, nal: &[u8]) -> bool {
        let Some(&b) = nal.first() else { return false };
        match self.codec {
            Codec::Hevc => (b >> 1) & 0x3f == 35,
            _ => b & 0x1f == 9,
        }
    }

    fn create_session(&mut self, ps: &[Vec<u8>]) -> Result<()> {
        self.session = None;
        self.format = None;

        let mut ptrs: Vec<NonNull<u8>> = ps
            .iter()
            .map(|p| NonNull::new(p.as_ptr().cast_mut()).unwrap())
            .collect();
        let mut sizes: Vec<usize> = ps.iter().map(Vec::len).collect();
        let mut fmt_raw: *const CMVideoFormatDescription = ptr::null();
        // SAFETY: pointer/size arrays are live locals matching in length;
        // 4-byte NAL length headers match the payload we build.
        let status = unsafe {
            match self.codec {
                Codec::Hevc => {
                    objc2_core_media::CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        None,
                        ps.len(),
                        NonNull::new(ptrs.as_mut_ptr()).unwrap(),
                        NonNull::new(sizes.as_mut_ptr()).unwrap(),
                        4,
                        None,
                        NonNull::from(&mut fmt_raw),
                    )
                }
                _ => objc2_core_media::CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    ps.len(),
                    NonNull::new(ptrs.as_mut_ptr()).unwrap(),
                    NonNull::new(sizes.as_mut_ptr()).unwrap(),
                    4,
                    NonNull::from(&mut fmt_raw),
                ),
            }
        };
        let format = NonNull::new(fmt_raw.cast_mut())
            .filter(|_| status == 0)
            // SAFETY: create returned +1; take ownership.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| {
                Error::Decode(format!("CMVideoFormatDescriptionCreate failed: {status}"))
            })?;

        // Destination: NV12, IOSurface-backed (empty IOSurfaceProperties dict
        // opts in). No CPU-mappable requirement — GPU-resident is the point.
        let dest = nv12_iosurface_attrs()?;
        // Hardware required: creation FAILS on machines without it — the
        // harness gate must never silently measure a software decoder.
        // SAFETY: `&'static` CFString key + constant CFBoolean.
        let spec = unsafe {
            single_value_dict(
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                kCFBooleanTrue.map(|b| b as &CFType).unwrap(),
            )
        }?;

        let mut raw: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: valid format + attribute dictionaries; no C callback record
        // (the per-frame output handler variant is used instead).
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(&spec),
                Some(&dest),
                ptr::null(),
                NonNull::from(&mut raw),
            )
        };
        let session = NonNull::new(raw)
            .filter(|_| status == 0)
            // SAFETY: create returned +1; take ownership.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| {
                Error::Decode(format!(
                    "VTDecompressionSessionCreate failed (hardware decoder required): {status}"
                ))
            })?;

        tracing::info!(codec = ?self.codec, "VideoToolbox hardware decode session created");
        self.format = Some(format);
        self.session = Some(session);
        Ok(())
    }
}

/// Split an Annex-B stream into NAL payloads (start codes stripped).
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= data.len() {
        let (sc, len) = if data[i..].starts_with(&[0, 0, 0, 1]) {
            (true, 4)
        } else if data[i..].starts_with(&[0, 0, 1]) {
            (true, 3)
        } else {
            (false, 0)
        };
        if sc {
            if let Some(s) = start
                && s < i
            {
                nals.push(&data[s..i]);
            }
            i += len;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start
        && s < data.len()
    {
        nals.push(&data[s..]);
    }
    nals
}

/// Wrap an AVCC/HVCC payload in a CMSampleBuffer ready for decode.
fn make_sample(
    payload: &[u8],
    format: &CMVideoFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block_raw: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: NULL memory block + assure-allocated: CMBlockBuffer allocates
    // and owns `len` bytes; out-param is a live local.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            payload.len(),
            None,
            ptr::null(),
            0,
            payload.len(),
            objc2_core_media::kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block_raw),
        )
    };
    let block = NonNull::new(block_raw)
        .filter(|_| status == 0)
        // SAFETY: create returned +1; take ownership.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| Error::Decode(format!("CMBlockBufferCreate failed: {status}")))?;
    // SAFETY: the block owns len bytes; copy the payload in.
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(
            NonNull::new(payload.as_ptr().cast_mut().cast::<c_void>()).unwrap(),
            &block,
            0,
            payload.len(),
        )
    };
    if status != 0 {
        return Err(Error::Decode(format!(
            "CMBlockBufferReplaceDataBytes failed: {status}"
        )));
    }

    let timing = CMSampleTimingInfo {
        duration: CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        },
        presentationTimeStamp: CMTime {
            value: 0,
            timescale: 1_000_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        },
        decodeTimeStamp: CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        },
    };
    let sizes = [payload.len()];
    let mut sample_raw: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: valid block/format; timing + size arrays are live locals.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            sizes.as_ptr(),
            NonNull::from(&mut sample_raw),
        )
    };
    NonNull::new(sample_raw)
        .filter(|_| status == 0)
        // SAFETY: create returned +1; take ownership.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| Error::Decode(format!("CMSampleBufferCreateReady failed: {status}")))
}

/// Destination attributes: NV12 + IOSurface-backed.
fn nv12_iosurface_attrs() -> Result<CFRetained<CFDictionary>> {
    // SAFETY: constant pixel-format value; CFNumber wraps a live i32.
    let pixfmt = unsafe {
        let v = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32;
        CFNumber::new(None, CF_NUMBER_SINT32, (&v as *const i32).cast::<c_void>())
    }
    .ok_or_else(|| Error::Decode("CFNumberCreate failed".into()))?;
    // SAFETY: empty dictionary with standard callbacks.
    let empty = unsafe {
        CFDictionary::new(
            None,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(|| Error::Decode("CFDictionaryCreate failed".into()))?;

    // SAFETY: `&'static` CFString attribute keys.
    let (k_pixfmt, k_iosurface) = unsafe {
        (
            objc2_core_video::kCVPixelBufferPixelFormatTypeKey,
            objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey,
        )
    };
    let mut keys: [*const c_void; 2] = [
        (k_pixfmt as *const CFString).cast(),
        (k_iosurface as *const CFString).cast(),
    ];
    let mut values: [*const c_void; 2] = [
        (&*pixfmt as *const CFNumber).cast(),
        (&*empty as *const CFDictionary).cast(),
    ];
    // SAFETY: key/value arrays are live locals of matching length.
    unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(|| Error::Decode("CFDictionaryCreate failed".into()))
}

/// One-entry CFDictionary.
///
/// # Safety
/// `key` and `value` must be valid CF objects.
unsafe fn single_value_dict(key: &CFString, value: &CFType) -> Result<CFRetained<CFDictionary>> {
    let mut keys: [*const c_void; 1] = [(key as *const CFString).cast()];
    let mut values: [*const c_void; 1] = [(value as *const CFType).cast()];
    // SAFETY: caller contract — live CF objects; arrays are live locals.
    unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(|| Error::Decode("CFDictionaryCreate failed".into()))
}
