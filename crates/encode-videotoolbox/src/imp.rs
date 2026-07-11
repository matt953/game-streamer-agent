//! VideoToolbox hardware H.264/HEVC encoder (spec 03). Consumes the
//! IOSurface-backed CVPixelBuffer captured upstream with no color
//! conversion, and emits Annex-B bitstream (parameter sets inlined on IDR)
//! ready for the datagram path.

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
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_H264_Main_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use gsa_capture_api::GpuFrame;
use gsa_capture_macos::IoSurfaceFrame;
use gsa_core::id::FrameId;
use gsa_core::media::{Codec, FrameKind, H264Profile, PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::{EncodeConfig, EncodedChunk, Encoder, EncoderCaps, FrameDirectives};

/// H.264 codec FourCC ('avc1').
const CODEC_H264: CMVideoCodecType = u32::from_be_bytes(*b"avc1");
/// HEVC codec FourCC ('hvc1').
const CODEC_HEVC: CMVideoCodecType = u32::from_be_bytes(*b"hvc1");
/// CFNumber type for a 32-bit signed int (kCFNumberSInt32Type).
const CF_NUMBER_SINT32: CFNumberType = CFNumberType(3);
/// CFNumber type for a 64-bit float (kCFNumberFloat64Type).
const CF_NUMBER_FLOAT64: CFNumberType = CFNumberType(6);
/// Keyframe cadence: at least one IDR per this many seconds (self-heal).
const KEYFRAME_INTERVAL_SECS: f64 = 1.0;

pub struct VideoToolboxEncoder {
    clock: MediaClock,
    session: Option<CFRetained<VTCompressionSession>>,
    mode: VideoMode,
    /// The codec the open session emits; picks the Annex-B parsing rules the
    /// output handler applies (H.264 vs HEVC NAL layout + parameter sets).
    codec: Codec,
    /// Assigned in the output handler when a chunk is *emitted* — so frames
    /// VideoToolbox drops/skips leave no hole in the numbering (a hole would
    /// trip the client's loss recovery). Shared with the handler closures.
    next_frame_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    force_idr: bool,
    tx: mpsc::Sender<EncodedChunk>,
    rx: mpsc::Receiver<EncodedChunk>,
}

impl std::fmt::Debug for VideoToolboxEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolboxEncoder")
            .field("open", &self.session.is_some())
            .finish()
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
            mode: VideoMode {
                width: 0,
                height: 0,
                fps: 0,
            },
            codec: Codec::H264,
            next_frame_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            force_idr: true,
            tx,
            rx,
        }
    }
}

impl Encoder for VideoToolboxEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            name: "videotoolbox",
            // HEVC first: preferred where the client can decode it.
            codecs: vec![Codec::Hevc, Codec::H264],
            input_formats: vec![PixelFormat::Nv12],
            max_width: 7680,
            max_height: 4320,
            supports_slices: false,
            supports_intra_refresh: false,
            supports_ref_invalidation: false,
            max_h264_profile: H264Profile::High,
        }
    }

    fn open(&mut self, cfg: EncodeConfig) -> Result<()> {
        let codec_type = match cfg.codec {
            Codec::H264 => CODEC_H264,
            Codec::Hevc => CODEC_HEVC,
            other => {
                return Err(Error::Encode(format!(
                    "VideoToolbox does H264 and HEVC, got {other:?}"
                )));
            }
        };
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
                codec_type,
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

        // SAFETY: the property keys and profile-level values are `&'static`
        // CFString constants exported by VideoToolbox.
        let (real_time, no_reorder, avg_bitrate, kf_interval, kf_duration, profile_key) = unsafe {
            (
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                kVTCompressionPropertyKey_ProfileLevel,
            )
        };
        // H.264 profile is negotiated per client (spec 03); HEVC uses Main (the
        // `h264_profile` field doesn't apply). SAFETY: `&'static` constants.
        let profile_value = unsafe {
            match cfg.codec {
                Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
                _ => match cfg.h264_profile {
                    H264Profile::ConstrainedBaseline => {
                        kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel
                    }
                    H264Profile::Main => kVTProfileLevel_H264_Main_AutoLevel,
                    H264Profile::High => kVTProfileLevel_H264_High_AutoLevel,
                },
            }
        };
        set_bool(&session, real_time, true)?;
        set_bool(&session, no_reorder, false)?;
        set_i32(&session, avg_bitrate, cfg.bitrate_bps as i32)?;
        // Periodic keyframe heals static regions (no intra-refresh until NVENC,
        // M4). A duration bound (not frame count) is what tracks wall-clock,
        // since capture is on-change; the frame-count bound is a high-motion
        // backstop. On-demand keyframes fire on top via `force_idr`.
        set_f64(&session, kf_duration, KEYFRAME_INTERVAL_SECS)?;
        set_i32(&session, kf_interval, (cfg.mode.fps.max(1) as i32) * 4)?;
        // SAFETY: valid session + key + CFString value.
        unsafe { set_property(&session, profile_key, Some(profile_value as &CFType))? };

        self.session = Some(session);
        self.mode = cfg.mode;
        self.codec = cfg.codec;
        self.next_frame_id
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.force_idr = true;
        Ok(())
    }

    fn submit(&mut self, frame: &GpuFrame, directives: FrameDirectives) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::Encode("encoder not open".into()))?;
        let io = frame
            .handle
            .downcast_platform::<IoSurfaceFrame>()
            .ok_or_else(|| Error::Encode("VideoToolbox needs an IOSurface frame".into()))?;
        let image_buffer: &CVImageBuffer = io.pixel_buffer();

        let is_idr = directives.idr || self.force_idr;
        self.force_idr = false;
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

        let frame_props = if is_idr {
            Some(force_keyframe_dict()?)
        } else {
            None
        };

        let tx = self.tx.clone();
        let clock = self.clock.clone();
        let frame_ids = self.next_frame_id.clone();
        let codec = self.codec;
        let handler = RcBlock::new(
            move |status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                if status != 0 || sample.is_null() {
                    return;
                }
                // SAFETY: non-null sample buffer from VideoToolbox.
                let sample = unsafe { &*sample };
                // Keyframe status is read from the bitstream (IDR NAL), not
                // the directive — VideoToolbox also emits keyframes on its
                // own periodic interval.
                if let Some((kind, data)) = sample_to_annex_b(sample, codec) {
                    // Only consume a frame id for frames actually emitted;
                    // no-reordering means handlers fire in submit order.
                    let frame_id =
                        FrameId(frame_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                    let _ = tx.send(EncodedChunk {
                        frame_id,
                        kind,
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
            return Err(Error::Encode(format!(
                "VTCompressionSessionEncodeFrame: {status}"
            )));
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
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::Encode("encoder not open".into()))?;
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
        if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    }
    .map(|b| b as &CFType);
    // SAFETY: valid session + key + CFBoolean value.
    unsafe { set_property(session, key, cf) }
}

fn set_i32(session: &VTCompressionSession, key: &CFString, value: i32) -> Result<()> {
    // SAFETY: value_ptr points at a live i32 for the duration of the call.
    let number = unsafe {
        CFNumber::new(
            None,
            CF_NUMBER_SINT32,
            (&value as *const i32).cast::<c_void>(),
        )
    }
    .ok_or_else(|| Error::Encode("CFNumberCreate failed".into()))?;
    // SAFETY: valid session + key + CFNumber value.
    unsafe { set_property(session, key, Some(&number)) }
}

fn set_f64(session: &VTCompressionSession, key: &CFString, value: f64) -> Result<()> {
    // SAFETY: value_ptr points at a live f64 for the duration of the call.
    let number = unsafe {
        CFNumber::new(
            None,
            CF_NUMBER_FLOAT64,
            (&value as *const f64).cast::<c_void>(),
        )
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
        if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
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

/// Convert a compressed CMSampleBuffer (length-prefixed) to Annex-B, detecting
/// keyframes from the bitstream (an IDR-slice NAL) and inlining the parameter
/// sets from the format description when it's a keyframe. `codec` selects the
/// NAL layout: H.264 (type in `nal[0] & 0x1f`; SPS+PPS) or HEVC (type in
/// `(nal[0] >> 1) & 0x3f`; VPS+SPS+PPS). Returns the detected [`FrameKind`] and
/// the Annex-B bytes.
fn sample_to_annex_b(sample: &CMSampleBuffer, codec: Codec) -> Option<(FrameKind, Vec<u8>)> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];

    // Does this NAL start an IDR access unit? H.264: slice type 5. HEVC:
    // IDR_W_RADL (19) or IDR_N_LP (20).
    let is_idr_nal = |nal: &[u8]| -> bool {
        let Some(&b) = nal.first() else { return false };
        match codec {
            Codec::Hevc => matches!((b >> 1) & 0x3f, 19 | 20),
            _ => b & 0x1f == 5,
        }
    };

    // SAFETY: compressed samples carry a data buffer.
    let block = unsafe { sample.data_buffer() }?;
    let mut total_len: usize = 0;
    let mut len_at: usize = 0;
    let mut data_ptr: *mut c_char = ptr::null_mut();
    // SAFETY: valid block buffer; out-params live locals.
    let status = unsafe { block.data_pointer(0, &mut len_at, &mut total_len, &mut data_ptr) };
    if status != 0 || data_ptr.is_null() {
        return None;
    }
    // SAFETY: data_ptr..total_len is the contiguous length-prefixed payload.
    let data = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), total_len) };

    // Walk 4-byte-length-prefixed NAL units → Annex-B start codes, noting
    // whether any is an IDR slice (→ keyframe).
    let mut slices = Vec::with_capacity(total_len + 8);
    let mut is_keyframe = false;
    let mut i = 0;
    while i + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if nal_len == 0 || i + nal_len > data.len() {
            break;
        }
        let nal = &data[i..i + nal_len];
        if is_idr_nal(nal) {
            is_keyframe = true;
        }
        slices.extend_from_slice(&START_CODE);
        slices.extend_from_slice(nal);
        i += nal_len;
    }
    if slices.is_empty() {
        return None;
    }

    // Keyframes carry parameter sets out-of-band (in the format description);
    // inline them ahead of the slice so the client's decoder can resync
    // standalone. H.264 has 2 (SPS, PPS); HEVC has 3 (VPS, SPS, PPS).
    let mut out = Vec::with_capacity(slices.len() + 96);
    let fmt = if is_keyframe {
        // SAFETY: compressed samples carry a format description.
        unsafe { sample.format_description() }
    } else {
        None
    };
    if let Some(fmt) = fmt {
        let ps_count = if codec == Codec::Hevc { 3 } else { 2 };
        for idx in 0..ps_count {
            let mut ps_ptr: *const u8 = ptr::null();
            let mut ps_size: usize = 0;
            // SAFETY: valid format description; out-params are live locals.
            let status = unsafe {
                match codec {
                    Codec::Hevc => {
                        objc2_core_media::CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                            &fmt,
                            idx,
                            &mut ps_ptr,
                            &mut ps_size,
                            ptr::null_mut(),
                            ptr::null_mut(),
                        )
                    }
                    _ => objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &fmt,
                        idx,
                        &mut ps_ptr,
                        &mut ps_size,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    ),
                }
            };
            if status != 0 || ps_ptr.is_null() || ps_size == 0 {
                continue;
            }
            // SAFETY: ps_ptr..ps_size is valid while `fmt` is retained.
            let ps = unsafe { std::slice::from_raw_parts(ps_ptr, ps_size) };
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(ps);
        }
    }
    out.extend_from_slice(&slices);

    let kind = if is_keyframe {
        FrameKind::Idr
    } else {
        FrameKind::P
    };
    Some((kind, out))
}
