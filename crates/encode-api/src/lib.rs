//! Encoder contract (spec 03). Every backend — VideoToolbox, NVENC, AMF,
//! VPL, software — implements this trait under the same low-latency rules:
//! 1 frame in → bitstream out immediately; no B-frames, no lookahead, no
//! periodic IDR (keyframes on demand only).

use bytes::Bytes;
use gsa_capture_api::GpuFrame;
use gsa_core::Result;
use gsa_core::id::FrameId;
use gsa_core::media::{Codec, FrameKind, H264Profile, PixelFormat, VideoMode};

/// What a backend can do; drives session negotiation (spec 05) and the
/// loss-recovery ladder (spec 04).
#[derive(Debug, Clone)]
pub struct EncoderCaps {
    pub name: &'static str,
    pub codecs: Vec<Codec>,
    pub input_formats: Vec<PixelFormat>,
    pub max_width: u32,
    pub max_height: u32,
    pub supports_slices: bool,
    pub supports_intra_refresh: bool,
    pub supports_ref_invalidation: bool,
    /// Highest H.264 profile this backend emits.
    pub max_h264_profile: H264Profile,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    pub codec: Codec,
    pub mode: VideoMode,
    pub bitrate_bps: u32,
    /// Negotiated H.264 profile (ignored for other codecs).
    pub h264_profile: H264Profile,
}

/// Per-frame instructions from the session layer (loss recovery).
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDirectives {
    /// Force an IDR on this frame (new subscriber, unrecoverable loss).
    pub idr: bool,
    /// Reference-invalidate: encode referencing nothing older than this
    /// (backends without support fall back via `invalidate_refs`).
    pub invalidate_before: Option<FrameId>,
}

/// One encoded access unit, ready for packetization.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    pub frame_id: FrameId,
    pub kind: FrameKind,
    /// Annex-B (H.264/HEVC) or OBU (AV1) bitstream.
    pub data: Bytes,
    /// Copied from the input frame; the latency chain's origin.
    pub capture_ts_us: u64,
    /// Agent clock when encoding finished (encode-stage latency).
    pub encode_done_ts_us: u64,
}

pub trait Encoder: Send {
    fn caps(&self) -> EncoderCaps;

    fn open(&mut self, cfg: EncodeConfig) -> Result<()>;

    /// Submit one frame with directives. Implementations must not queue
    /// more than one frame of latency.
    fn submit(&mut self, frame: &GpuFrame, directives: FrameDirectives) -> Result<()>;

    /// Drain the next encoded chunk if one is ready (non-blocking).
    fn poll_bitstream(&mut self) -> Result<Option<EncodedChunk>>;

    /// Block up to `timeout` for the next encoded chunk.
    ///
    /// Hardware encoders (e.g. VideoToolbox) deliver a frame's bitstream
    /// asynchronously, shortly after `submit`. The pipeline calls this right
    /// after submitting so the chunk goes out the instant it's ready rather
    /// than waiting for the next captured frame to wake the loop. Synchronous
    /// encoders return the already-ready chunk immediately.
    fn next_chunk(&mut self, timeout: std::time::Duration) -> Result<Option<EncodedChunk>>;

    /// ABR bitrate update without reopening (spec 03). Backends that can't
    /// do it live may reopen internally (must re-IDR).
    fn update_rate(&mut self, bitrate_bps: u32) -> Result<()>;

    /// Request an IDR on the next submitted frame.
    fn force_idr(&mut self);

    /// Loss recovery (spec 04): frames newer than `last_good_wire` never
    /// reached the client — later frames must not reference them. Returns
    /// `false` when unsupported — caller must `force_idr` instead.
    fn invalidate_refs(&mut self, last_good_wire: u32) -> bool;

    fn close(&mut self);
}
