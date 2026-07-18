//! Software H.264 encoder via openh264 (spec 03 "software fallback").
//! Debug/CI backend only: it lets the full pipeline run with no GPU and
//! provides bitstream-conformance smoke coverage. Never auto-selected in
//! release sessions once hardware backends exist.

mod convert;

use bytes::Bytes;
use openh264::encoder::{
    BitRate, Encoder as OhEncoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::YUVBuffer;

use gsa_capture_api::{GpuFrame, GpuHandle};
use gsa_core::id::FrameId;
use gsa_core::media::{Codec, FrameKind, H264Profile, PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::{EncodeConfig, EncodedChunk, Encoder, EncoderCaps, FrameDirectives};

pub struct SwEncoder {
    clock: MediaClock,
    state: Option<Open>,
    force_idr: bool,
}

struct Open {
    encoder: OhEncoder,
    mode: VideoMode,
    next_frame_id: FrameId,
    pending: Option<EncodedChunk>,
}

impl std::fmt::Debug for SwEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwEncoder")
            .field("open", &self.state.is_some())
            .finish()
    }
}

impl SwEncoder {
    #[must_use]
    pub fn new(clock: MediaClock) -> Self {
        Self {
            clock,
            state: None,
            force_idr: false,
        }
    }

    fn build_encoder(cfg: &EncodeConfig) -> Result<OhEncoder> {
        // Low-latency contract (spec 03): no B-frames (openh264 has none),
        // 1-in-1-out, bitrate-mode rate control, no periodic IDR (period 0;
        // IDRs arrive only via force_intra_frame).
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(cfg.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(cfg.mode.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            // Unsupported in screen-content mode; off explicitly to keep
            // openh264 from warning on every open.
            .adaptive_quantization(false)
            .background_detection(false);
        OhEncoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| Error::Encode(format!("openh264 init: {e}")))
    }
}

impl Encoder for SwEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            name: "openh264-sw",
            codecs: vec![Codec::H264],
            input_formats: vec![PixelFormat::Bgra8],
            max_width: 3840,
            max_height: 2160,
            supports_slices: false,
            supports_intra_refresh: false,
            supports_ref_invalidation: false,
            // openh264 encodes Constrained Baseline only.
            max_h264_profile: H264Profile::ConstrainedBaseline,
        }
    }

    fn open(&mut self, cfg: EncodeConfig) -> Result<()> {
        if cfg.codec != Codec::H264 {
            return Err(Error::Encode(format!(
                "sw backend only does H264, got {:?}",
                cfg.codec
            )));
        }
        let encoder = Self::build_encoder(&cfg)?;
        self.state = Some(Open {
            encoder,
            mode: cfg.mode,
            next_frame_id: FrameId::ZERO,
            pending: None,
        });
        self.force_idr = true; // first frame is always an IDR
        Ok(())
    }

    fn submit(&mut self, frame: &GpuFrame, directives: FrameDirectives) -> Result<()> {
        let open = self
            .state
            .as_mut()
            .ok_or_else(|| Error::Encode("encoder not open".into()))?;
        if frame.format != PixelFormat::Bgra8 {
            return Err(Error::Encode(format!(
                "sw backend needs Bgra8, got {:?}",
                frame.format
            )));
        }
        let GpuHandle::Cpu(cpu) = &frame.handle else {
            return Err(Error::Encode("sw backend needs a CPU frame".into()));
        };

        let yuv: YUVBuffer = convert::bgra_to_i420(
            &cpu.data,
            cpu.stride,
            frame.width as usize,
            frame.height as usize,
        )?;

        if directives.idr || self.force_idr {
            open.encoder.force_intra_frame();
            self.force_idr = false;
        }

        let encoded = open
            .encoder
            .encode(&yuv)
            .map_err(|e| Error::Encode(format!("openh264 encode: {e}")))?;

        let kind = match encoded.frame_type() {
            FrameType::IDR | FrameType::I => FrameKind::Idr,
            FrameType::Skip => return Ok(()), // rate control skipped it; no output
            _ => FrameKind::P,
        };
        let data = Bytes::from(encoded.to_vec());
        if data.is_empty() {
            return Ok(());
        }

        let frame_id = open.next_frame_id;
        open.next_frame_id = frame_id.next();
        open.pending = Some(EncodedChunk {
            frame_id,
            kind,
            data,
            capture_ts_us: frame.capture_ts_us,
            encode_done_ts_us: self.clock.now_us(),
        });
        Ok(())
    }

    fn poll_bitstream(&mut self) -> Result<Option<EncodedChunk>> {
        Ok(self.state.as_mut().and_then(|o| o.pending.take()))
    }

    fn next_chunk(&mut self, _timeout: std::time::Duration) -> Result<Option<EncodedChunk>> {
        // Synchronous: the chunk was produced during `submit`.
        self.poll_bitstream()
    }

    fn update_rate(&mut self, bitrate_bps: u32) -> Result<()> {
        // openh264's safe wrapper has no live rate update; reopen (debug
        // backend — an IDR hiccup is acceptable here, spec 03 contract
        // notes backends may reopen internally but must re-IDR).
        let open = self
            .state
            .as_mut()
            .ok_or_else(|| Error::Encode("encoder not open".into()))?;
        let cfg = EncodeConfig {
            codec: Codec::H264,
            mode: open.mode,
            bitrate_bps,
            h264_profile: H264Profile::ConstrainedBaseline,
        };
        open.encoder = Self::build_encoder(&cfg)?;
        self.force_idr = true;
        Ok(())
    }

    fn force_idr(&mut self) {
        self.force_idr = true;
    }

    fn invalidate_refs(&mut self, _last_good_wire: u32) -> bool {
        false // unsupported: session layer falls back to force_idr (spec 04)
    }

    fn close(&mut self) {
        self.state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsa_capture_api::CpuFrame;
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;
    use std::sync::Arc;

    fn bgra_frame(w: usize, h: usize, index: u32, ts: u64) -> GpuFrame {
        let stride = w * 4;
        let mut buf = vec![0x30u8; stride * h];
        gsa_core::pattern::write_marker_bgra(&mut buf, stride, index);
        GpuFrame {
            handle: GpuHandle::Cpu(CpuFrame {
                data: Arc::new(buf),
                stride,
            }),
            format: PixelFormat::Bgra8,
            width: w as u32,
            height: h as u32,
            capture_ts_us: ts,
            dirty_rects: None,
        }
    }

    /// The M0 conformance smoke (spec 03): encode a marked frame, decode it
    /// with the reference decoder, read the marker back out of the luma.
    #[test]
    fn encode_decode_round_trip_preserves_marker() {
        let mode = VideoMode {
            width: 320,
            height: 240,
            fps: 30,
        };
        let mut enc = SwEncoder::new(MediaClock::new());
        enc.open(EncodeConfig {
            codec: Codec::H264,
            mode,
            bitrate_bps: 2_000_000,
            h264_profile: H264Profile::ConstrainedBaseline,
        })
        .unwrap();

        let mut dec = Decoder::new().unwrap();
        let mut decoded_markers = Vec::new();

        for i in 0..8u32 {
            enc.submit(
                &bgra_frame(320, 240, 0xa5a5_0000 | i, u64::from(i)),
                FrameDirectives::default(),
            )
            .unwrap();
            let chunk = enc.poll_bitstream().unwrap().expect("chunk per frame");
            assert_eq!(chunk.frame_id, FrameId(u64::from(i)));
            if i == 0 {
                assert_eq!(chunk.kind, FrameKind::Idr, "first frame must be IDR");
            }
            if let Ok(Some(yuv)) = dec.decode(&chunk.data) {
                let (ys, _, _) = yuv.strides();
                if let Some(m) =
                    gsa_core::pattern::read_marker_luma(yuv.y(), ys, yuv.dimensions().0)
                {
                    decoded_markers.push(m);
                }
            }
        }
        assert!(!decoded_markers.is_empty(), "decoder produced no frames");
        for (n, m) in decoded_markers.iter().enumerate() {
            assert_eq!(
                *m & 0xffff_0000,
                0xa5a5_0000,
                "marker high bits corrupt at {n}"
            );
        }
    }

    #[test]
    fn force_idr_produces_idr() {
        let mode = VideoMode {
            width: 320,
            height: 240,
            fps: 30,
        };
        let mut enc = SwEncoder::new(MediaClock::new());
        enc.open(EncodeConfig {
            codec: Codec::H264,
            mode,
            bitrate_bps: 2_000_000,
            h264_profile: H264Profile::ConstrainedBaseline,
        })
        .unwrap();
        for i in 0..3u32 {
            enc.submit(&bgra_frame(320, 240, i, 0), FrameDirectives::default())
                .unwrap();
            let _ = enc.poll_bitstream().unwrap();
        }
        enc.force_idr();
        enc.submit(&bgra_frame(320, 240, 99, 0), FrameDirectives::default())
            .unwrap();
        let chunk = enc.poll_bitstream().unwrap().unwrap();
        assert_eq!(chunk.kind, FrameKind::Idr);
    }
}
