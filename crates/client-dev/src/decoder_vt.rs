//! VideoToolbox hardware H.264 decoder (macOS). Parses our Annex-B access
//! units, builds a format description from in-band SPS/PPS, repacks to
//! AVCC, and decodes to BGRA pixel buffers.
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
    CFDictionary, CFNumber, CFNumberType, CFRetained, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSession};

use gsa_client_core::{DecodedFrame, VideoDecoder};
use gsa_core::{Error, Result};

/// BGRA FourCC for the decoder output ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const CF_NUMBER_SINT32: CFNumberType = CFNumberType(3);

pub struct VideoToolboxDecoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    /// Last seen SPS/PPS bytes; session is rebuilt when they change.
    param_sets: (Vec<u8>, Vec<u8>),
}

impl std::fmt::Debug for VideoToolboxDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolboxDecoder")
            .field("open", &self.session.is_some())
            .finish()
    }
}

impl VideoToolboxDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            session: None,
            format: None,
            param_sets: (Vec::new(), Vec::new()),
        }
    }

    fn ensure_session(&mut self, sps: &[u8], pps: &[u8]) -> Result<()> {
        if self.session.is_some() && self.param_sets.0 == sps && self.param_sets.1 == pps {
            return Ok(());
        }
        // (Re)build format description + session.
        let sps_ptr = NonNull::new(sps.as_ptr().cast_mut()).ok_or_else(err("sps empty"))?;
        let pps_ptr = NonNull::new(pps.as_ptr().cast_mut()).ok_or_else(err("pps empty"))?;
        let mut ptrs = [sps_ptr, pps_ptr];
        let mut sizes = [sps.len(), pps.len()];
        let mut fmt_raw: *const CMFormatDescription = ptr::null();
        // SAFETY: two valid parameter-set pointers + sizes; 4-byte NAL
        // headers (matches our AVCC repack below); valid out-pointer.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                2,
                NonNull::from(&mut ptrs).cast(),
                NonNull::from(&mut sizes).cast(),
                4,
                NonNull::from(&mut fmt_raw),
            )
        };
        if status != 0 || fmt_raw.is_null() {
            return Err(Error::Decode(format!(
                "format description failed: {status}"
            )));
        }
        // SAFETY: +1 retained out-param; take ownership.
        let format = unsafe { CFRetained::from_raw(NonNull::new_unchecked(fmt_raw.cast_mut())) };

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
        self.param_sets = (sps.to_vec(), pps.to_vec());
        tracing::info!("VideoToolbox decoder session (re)created");
        Ok(())
    }
}

impl Default for VideoToolboxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `VideoDecoder: Send` — the client owns the decoder on one thread
// at a time; VT session calls are made serially from that owner. The
// CFRetained refcounts are atomic.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>> {
        let nals = split_annex_b(access_unit);
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;
        let mut avcc = Vec::with_capacity(access_unit.len() + 16);
        for nal in &nals {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) => sps = Some(nal),
                Some(8) => pps = Some(nal),
                Some(_) => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
                None => {}
            }
        }
        if let (Some(sps), Some(pps)) = (sps, pps) {
            self.ensure_session(sps, pps)?;
        }
        let Some(session) = self.session.as_ref() else {
            return Ok(None); // waiting for the first IDR's parameter sets
        };
        if avcc.is_empty() {
            return Ok(None);
        }

        let sample = avcc_sample_buffer(&avcc, self.format.as_ref().expect("format with session"))?;

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
