//! VideoToolbox hardware H.264 encoder (spec 03). Consumes the
//! IOSurface-backed CVPixelBuffer captured upstream with no color
//! conversion, and emits Annex-B bitstream (SPS/PPS inlined on IDR) ready
//! for the datagram path.

use std::ffi::{c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::mpsc;

use block2::RcBlock;
use bytes::Bytes;
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType, kCFBooleanFalse,
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags, CMVideoCodecType};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use gsa_capture_api::GpuFrame;
use gsa_capture_macos::IoSurfaceFrame;
use gsa_core::id::FrameId;
use gsa_core::media::{Codec, FrameKind, PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::{EncodeConfig, EncodedChunk, Encoder, EncoderCaps, FrameDirectives};

/// H.264 codec FourCC ('avc1').
const CODEC_H264: CMVideoCodecType = u32::from_be_bytes(*b"avc1");
/// CFNumber type for a 32-bit signed int (kCFNumberSInt32Type).
const CF_NUMBER_SINT32: CFNumberType = CFNumberType(3);

pub struct VideoToolboxEncoder {
    clock: MediaClock,
    session: Option<CFRetained<VTCompressionSession>>,
    mode: VideoMode,
    next_frame_id: u64,
    force_idr: bool,
    tx: mpsc::Sender<EncodedChunk>,
    rx: mpsc::Receiver<EncodedChunk>,
}

impl std::fmt::Debug for VideoToolboxEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolboxEncoder").field("open", &self.session.is_some()).finish()
    }
}

// SAFETY: `Encoder: Send`. The session is owned by one thread at a time
// (session methods are documented safe to call serially from any thread);
// output-handler blocks run on VideoToolbox's own thread and only touch the
// Send channel + clock.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    #[must_use]
    pub fn new(clock: MediaClock) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            clock,
            session: None,
            mode: VideoMode { width: 0, height: 0, fps: 0 },
            next_frame_id: 0,
            force_idr: true,
            tx,
            rx,
        }
    }
}

impl Encoder for VideoToolboxEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            name: "videotoolbox-h264",
            codecs: vec![Codec::H264],
            input_formats: vec![PixelFormat::Nv12],
            max_width: 7680,
            max_height: 4320,
            supports_slices: false,
            supports_intra_refresh: false,
            supports_ref_invalidation: false,
        }
    }

    fn open(&mut self, cfg: EncodeConfig) -> Result<()> {
        if cfg.codec != Codec::H264 {
            return Err(Error::Encode(format!("VideoToolbox M1 does H264 only, got {:?}", cfg.codec)));
        }
        // Dedicated low-latency rate control (macOS 11.3+): trims the
        // encoder's pipeline latency vs plain real-time mode (spec 03).
        let spec = single_bool_dict(
            // SAFETY: `&'static` CFString encoder-specification key.
            unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
            true,
        )?;

        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: valid dimensions + out-pointer; no C output callback (we
        // use the per-frame output handler variant below).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                cfg.mode.width as i32,
                cfg.mode.height as i32,
                CODEC_H264,
                Some(&spec),
                None,
                None,
                None,
                ptr::null_mut(),
                NonNull::from(&mut raw),
            )
        };
        let session = NonNull::new(raw)
            .filter(|_| status == 0)
            // SAFETY: create returned +1; take ownership.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| Error::Encode(format!("VTCompressionSessionCreate failed: {status}")))?;

        // SAFETY: the property keys and profile-level value are `&'static`
        // CFString constants exported by VideoToolbox.
        let (real_time, no_reorder, avg_bitrate, max_kf_interval, profile_key, profile_value) = unsafe {
            (
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Main_AutoLevel,
            )
        };
        set_bool(&session, real_time, true)?;
        set_bool(&session, no_reorder, false)?;
        set_i32(&session, avg_bitrate, cfg.bitrate_bps as i32)?;
        // No periodic IDR — keyframes on demand only (spec 03).
        set_i32(&session, max_kf_interval, i32::MAX)?;
        // SAFETY: valid session + key + CFString value.
        unsafe { set_property(&session, profile_key, Some(profile_value as &CFType))? };

        self.session = Some(session);
        self.mode = cfg.mode;
        self.next_frame_id = 0;
        self.force_idr = true;
        Ok(())
    }

    fn submit(&mut self, frame: &GpuFrame, directives: FrameDirectives) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| Error::Encode("encoder not open".into()))?;
        let io = frame
            .handle
            .downcast_platform::<IoSurfaceFrame>()
            .ok_or_else(|| Error::Encode("VideoToolbox needs an IOSurface frame".into()))?;
        let image_buffer: &CVImageBuffer = io.pixel_buffer();

        let is_idr = directives.idr || self.force_idr;
        self.force_idr = false;
        let frame_id = FrameId(self.next_frame_id);
        self.next_frame_id += 1;
        let capture_ts_us = frame.capture_ts_us;

        let pts = CMTime {
            value: capture_ts_us as i64,
            timescale: 1_000_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let duration = CMTime {
            value: 1,
            timescale: self.mode.fps.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };

        let frame_props = if is_idr { Some(force_keyframe_dict()?) } else { None };

        let tx = self.tx.clone();
        let clock = self.clock.clone();
        let handler = RcBlock::new(
            move |status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                if status != 0 || sample.is_null() {
                    return;
                }
                // SAFETY: non-null sample buffer from VideoToolbox.
                let sample = unsafe { &*sample };
                if let Some(data) = sample_to_annex_b(sample, is_idr) {
                    let _ = tx.send(EncodedChunk {
                        frame_id,
                        kind: if is_idr { FrameKind::Idr } else { FrameKind::P },
                        data: Bytes::from(data),
                        capture_ts_us,
                        encode_done_ts_us: clock.now_us(),
                    });
                }
            },
        );

        let frame_props_ref = frame_props.as_deref();
        // SAFETY: valid session + image buffer; handler block outlives the
        // call (real-time mode invokes it synchronously or copies it).
        let status = unsafe {
            session.encode_frame_with_output_handler(
                image_buffer,
                pts,
                duration,
                frame_props_ref,
                ptr::null_mut(),
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            return Err(Error::Encode(format!("VTCompressionSessionEncodeFrame: {status}")));
        }
        Ok(())
    }

    fn poll_bitstream(&mut self) -> Result<Option<EncodedChunk>> {
        Ok(self.rx.try_recv().ok())
    }

    fn next_chunk(&mut self, timeout: std::time::Duration) -> Result<Option<EncodedChunk>> {
        // VideoToolbox delivers asynchronously via the output handler; block
        // for it rather than returning empty and stalling a frame.
        Ok(self.rx.recv_timeout(timeout).ok())
    }

    fn update_rate(&mut self, bitrate_bps: u32) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| Error::Encode("encoder not open".into()))?;
        // SAFETY: `&'static` CFString constant.
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        set_i32(session, key, bitrate_bps as i32)
    }

    fn force_idr(&mut self) {
        self.force_idr = true;
    }

    fn invalidate_refs(&mut self, _older_than: FrameId) -> bool {
        false // unsupported; session layer falls back to force_idr (spec 04)
    }

    fn close(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: deterministic teardown of a valid session.
            unsafe { session.invalidate() };
        }
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        self.close();
    }
}

fn set_bool(session: &VTCompressionSession, key: &CFString, value: bool) -> Result<()> {
    // SAFETY: constant CFBoolean values. Must pass an explicit false — a NULL
    // value is rejected by VTSessionSetProperty (kVTPropertyNotSupportedErr).
    let cf = unsafe {
        if value { kCFBooleanTrue } else { kCFBooleanFalse }
    }
    .map(|b| b as &CFType);
    // SAFETY: valid session + key + CFBoolean value.
    unsafe { set_property(session, key, cf) }
}

fn set_i32(session: &VTCompressionSession, key: &CFString, value: i32) -> Result<()> {
    // SAFETY: value_ptr points at a live i32 for the duration of the call.
    let number = unsafe {
        CFNumber::new(None, CF_NUMBER_SINT32, (&value as *const i32).cast::<c_void>())
    }
    .ok_or_else(|| Error::Encode("CFNumberCreate failed".into()))?;
    // SAFETY: valid session + key + CFNumber value.
    unsafe { set_property(session, key, Some(&number)) }
}

/// # Safety
/// `key` must be a valid VideoToolbox property key; `value` a compatible type.
unsafe fn set_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: Option<&CFType>,
) -> Result<()> {
    // SAFETY: forwarded to the caller's contract.
    let status = unsafe { objc2_video_toolbox::VTSessionSetProperty(session, key, value) };
    if status != 0 {
        return Err(Error::Encode(format!(
            "VTSessionSetProperty({key}) failed: {status}"
        )));
    }
    Ok(())
}

/// `{ ForceKeyFrame: true }` frame-properties dictionary.
fn force_keyframe_dict() -> Result<CFRetained<CFDictionary>> {
    // SAFETY: `&'static` CFString frame-option key.
    single_bool_dict(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, true)
}

/// A single-entry `{ key: bool }` CFDictionary (CFType key + CFBoolean value).
fn single_bool_dict(key: &CFString, value: bool) -> Result<CFRetained<CFDictionary>> {
    // SAFETY: constant CFBoolean values.
    let boolean = unsafe {
        if value { kCFBooleanTrue } else { kCFBooleanFalse }
    }
    .ok_or_else(|| Error::Encode("CFBoolean constant null".into()))?;

    // SAFETY: single-entry CFType dictionary; key/value outlive the call and
    // are retained by the copy per kCFTypeDictionary*CallBacks.
    let dict = unsafe {
        let mut keys: [*const c_void; 1] = [(key as *const CFString).cast()];
        let mut values: [*const c_void; 1] =
            [(boolean as *const objc2_core_foundation::CFBoolean).cast()];
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(|| Error::Encode("CFDictionaryCreate failed".into()))?;
    Ok(dict)
}

/// Convert a compressed CMSampleBuffer (AVCC length-prefixed) to Annex-B,
/// inlining SPS/PPS from the format description on IDR frames.
fn sample_to_annex_b(sample: &CMSampleBuffer, is_idr: bool) -> Option<Vec<u8>> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::new();

    if is_idr {
        // SAFETY: compressed samples carry a format description.
        let fmt = unsafe { sample.format_description() }?;
        for idx in 0..2usize {
            let mut ps_ptr: *const u8 = ptr::null();
            let mut ps_size: usize = 0;
            // SAFETY: valid format description; out-params are live locals.
            let status = unsafe {
                objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fmt,
                    idx,
                    &mut ps_ptr,
                    &mut ps_size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != 0 || ps_ptr.is_null() || ps_size == 0 {
                continue;
            }
            // SAFETY: VideoToolbox guarantees ps_ptr..ps_size is valid while
            // fmt is retained (it is, for this scope).
            let ps = unsafe { std::slice::from_raw_parts(ps_ptr, ps_size) };
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(ps);
        }
    }

    // SAFETY: compressed samples carry a data buffer.
    let block = unsafe { sample.data_buffer() }?;
    let mut total_len: usize = 0;
    let mut len_at: usize = 0;
    let mut data_ptr: *mut c_char = ptr::null_mut();
    // SAFETY: valid block buffer; out-params live locals.
    let status =
        unsafe { block.data_pointer(0, &mut len_at, &mut total_len, &mut data_ptr) };
    if status != 0 || data_ptr.is_null() {
        return None;
    }
    // SAFETY: data_ptr..total_len is the contiguous AVCC payload.
    let data = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), total_len) };

    // Walk 4-byte-length-prefixed NAL units, rewriting each prefix as a
    // start code.
    let mut i = 0;
    while i + 4 <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if nal_len == 0 || i + nal_len > data.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[i..i + nal_len]);
        i += nal_len;
    }

    (!out.is_empty()).then_some(out)
}
