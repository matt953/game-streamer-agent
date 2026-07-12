//! One streaming pipeline: source thread → depth-1 ring → encode thread →
//! packetize/send task (spec 01 threading model). Pixels never enter the
//! async world; the tokio side only ever sees encoded chunks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use gsa_capture_api::{RenderSource, SourceConfig, frame_channel};
use gsa_core::media::{Codec, H264Profile, VideoMode};
use gsa_core::time::wire_ts;
use gsa_core::{Error, Result};
use gsa_encode_api::{EncodeConfig, Encoder, FrameDirectives};
use gsa_protocol::datagram::{VideoDatagramHeader, chunk_video_frame};

/// Fallback when quinn hasn't discovered the path MTU yet.
const DEFAULT_MAX_DATAGRAM: usize = 1200;

/// Upper bound on waiting for an async encoder to emit a submitted frame.
/// Normal delivery is ~encode latency (single-digit ms); this only trips on
/// a dropped or failed frame, so it's generous.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

/// Video pacing (spec 04): spread a frame's datagrams over a few ms so a large
/// frame doesn't burst ~60 packets into the network at once — the measured
/// cause of the p99 tail once hardware encode made the encoder cheap. Paced as
/// a leaky bucket at a multiple of the encode bitrate; a first cut until the M3
/// congestion controller supplies a real send-rate estimate.
const PACING_GAIN: f64 = 8.0;
/// Never hold one frame's send back beyond this — bounds added latency on a
/// big IDR (the rest bursts once the budget is spent).
const PACING_CAP: Duration = Duration::from_millis(3);
/// Frames of at most this many datagrams don't burst; send them immediately.
const PACING_MIN_DATAGRAMS: usize = 8;

pub struct PipelineHandle {
    stop: Arc<AtomicBool>,
    source: Box<dyn RenderSource>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    /// Audio sub-pipeline, present when the source captures audio (spec 07).
    audio: Option<crate::audio_pipeline::AudioPipelineHandle>,
    pub frames_sent: Arc<AtomicU64>,
    /// Set to force the next encoded frame to be an IDR (client loss
    /// recovery, spec 04).
    force_idr: Arc<AtomicBool>,
    /// Live encode target bitrate (bps). The encode thread applies changes via
    /// `Encoder::update_rate` and the pacer reads it for its send-rate; this is
    /// the actuator the manual knob and the ABR controller (spec 04) both drive.
    bitrate: Arc<AtomicU32>,
}

impl PipelineHandle {
    /// Request a keyframe on the next frame (client couldn't decode).
    pub fn request_keyframe(&self) {
        self.force_idr.store(true, Ordering::Release);
    }

    /// Set the live encode target bitrate (bps). Takes effect on the next
    /// encoded frame; a no-op if unchanged.
    pub fn set_bitrate(&self, bitrate_bps: u32) {
        self.bitrate.store(bitrate_bps, Ordering::Relaxed);
    }

    /// The current live target bitrate (bps).
    pub fn bitrate(&self) -> u32 {
        self.bitrate.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for PipelineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineHandle")
            .field("stopped", &self.stop.load(Ordering::Relaxed))
            .finish()
    }
}

/// Start a pipeline streaming `source` through `encoder` onto `conn`'s
/// datagrams. Returns immediately; work happens on dedicated threads plus
/// one tokio task.
pub fn start(
    mut source: Box<dyn RenderSource>,
    mut encoder: Box<dyn Encoder>,
    conn: quinn::Connection,
    mode: VideoMode,
    bitrate_bps: u32,
    codec: Codec,
    h264_profile: H264Profile,
) -> Result<PipelineHandle> {
    let (sink, rx) = frame_channel();
    encoder.open(EncodeConfig {
        codec,
        mode,
        bitrate_bps,
        h264_profile,
    })?;
    source.start(SourceConfig { mode }, sink)?;

    // If the source captures audio, run the audio pipeline beside the video one.
    let audio = match source.audio() {
        Some(rx) => Some(crate::audio_pipeline::start(rx, conn.clone())?),
        None => {
            tracing::info!("source captures no audio; video only");
            None
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let frames_sent = Arc::new(AtomicU64::new(0));
    let force_idr = Arc::new(AtomicBool::new(false));
    let bitrate = Arc::new(AtomicU32::new(bitrate_bps));
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();

    let stop_enc = stop.clone();
    let force_idr_enc = force_idr.clone();
    let bitrate_enc = bitrate.clone();
    let encode_thread = std::thread::Builder::new()
        .name("gsa-encode".into())
        .spawn(move || {
            // Applied vs. target bitrate: re-arm the encoder only when the
            // live target changes (an `update_rate` may cost one IDR, spec 03).
            let mut applied_bitrate = bitrate_bps;
            // Encode only real captures — never re-encode a held frame to
            // synthesize a keyframe. Capture backends recycle their pooled
            // IOSurfaces after the callback, so a held handle can read back
            // stale pixels. A pending keyframe rides the next real capture.
            while !stop_enc.load(Ordering::Acquire) {
                let Some(frame) = rx.recv_latest(Duration::from_millis(100)) else {
                    if rx.is_closed() {
                        break;
                    }
                    continue;
                };
                // Apply a pending bitrate change before encoding this frame.
                let target_bitrate = bitrate_enc.load(Ordering::Relaxed);
                if target_bitrate != applied_bitrate {
                    match encoder.update_rate(target_bitrate) {
                        Ok(()) => {
                            applied_bitrate = target_bitrate;
                            tracing::debug!(bitrate = target_bitrate, "encode bitrate updated");
                        }
                        Err(e) => tracing::warn!(error = %e, "update_rate failed"),
                    }
                }
                let directives = FrameDirectives {
                    idr: force_idr_enc.swap(false, Ordering::AcqRel),
                    ..Default::default()
                };
                if let Err(e) = encoder.submit(&frame, directives) {
                    tracing::error!(error = %e, "encode submit failed; stopping pipeline");
                    break;
                }
                // Block for this frame's bitstream (async hw encoders deliver
                // it ~encode-latency later) so it goes out immediately instead
                // of waiting for the next captured frame to wake this loop.
                // The bound only trips on a genuinely dropped/failed frame.
                match encoder.next_chunk(DRAIN_TIMEOUT) {
                    Ok(Some(chunk)) => {
                        if chunk_tx.send(chunk).is_err() {
                            return; // sender task gone (connection closed)
                        }
                    }
                    Ok(None) => {} // dropped frame or timeout; move on
                    Err(e) => {
                        tracing::error!(error = %e, "encoder drain failed");
                        break;
                    }
                }
                // Sweep up any additional chunks without blocking.
                while let Ok(Some(chunk)) = encoder.poll_bitstream() {
                    if chunk_tx.send(chunk).is_err() {
                        return;
                    }
                }
            }
            encoder.close();
            tracing::debug!("encode thread exited");
        })
        .map_err(|e| Error::Session(format!("spawn encode thread: {e}")))?;

    let frames_ctr = frames_sent.clone();
    let bitrate_pace = bitrate.clone();
    tokio::spawn(async move {
        let mut logged = 0u64;
        while let Some(chunk) = chunk_rx.recv().await {
            let max = conn.max_datagram_size().unwrap_or(DEFAULT_MAX_DATAGRAM);
            let header = VideoDatagramHeader {
                session_epoch: 0,
                frame_id: chunk.frame_id.wire(),
                kind: chunk.kind,
                chunk_index: 0,
                chunk_count: 1,
                capture_ts_us: wire_ts(chunk.capture_ts_us),
            };
            let datagrams = match chunk_video_frame(header, &chunk.data, max) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, "packetize failed");
                    continue;
                }
            };
            // Pace big frames; small ones (static desktop) go out immediately.
            // Read the live bitrate so pacing tracks ABR / manual changes.
            let br = bitrate_pace.load(Ordering::Relaxed);
            let pace_bytes_per_sec = (datagrams.len() > PACING_MIN_DATAGRAMS && br > 0)
                .then(|| f64::from(br) / 8.0 * PACING_GAIN);
            let frame_start = std::time::Instant::now();
            let mut deadline = frame_start;
            for d in datagrams {
                if let Some(rate) = pace_bytes_per_sec {
                    let now = std::time::Instant::now();
                    if deadline > now && now.duration_since(frame_start) < PACING_CAP {
                        // tokio's timer floor is ~1 ms, so only sleep once the
                        // schedule slack is worth it; sub-ms slack accumulates.
                        let wait = (deadline - now).min(PACING_CAP);
                        if wait >= Duration::from_millis(1) {
                            tokio::time::sleep(wait).await;
                        }
                    }
                    deadline += Duration::from_secs_f64(d.len() as f64 / rate);
                }
                if let Err(e) = conn.send_datagram(bytes::Bytes::from(d)) {
                    match e {
                        quinn::SendDatagramError::ConnectionLost(_) => {
                            tracing::info!("client disconnected; stopping video sender")
                        }
                        other => tracing::warn!(error = %other, "video sender stopped on error"),
                    }
                    return;
                }
            }
            let sent = frames_ctr.fetch_add(1, Ordering::Relaxed) + 1;
            // Sampled latency span (spec 01: "where did the milliseconds go").
            if sent - logged >= 120 {
                logged = sent;
                let encode_ms =
                    (chunk.encode_done_ts_us.saturating_sub(chunk.capture_ts_us)) as f64 / 1000.0;
                tracing::debug!(
                    frames = sent,
                    encode_ms,
                    size = chunk.data.len(),
                    "pipeline sample"
                );
            }
        }
        tracing::debug!("sender task exited");
    });

    Ok(PipelineHandle {
        stop,
        source,
        encode_thread: Some(encode_thread),
        audio,
        frames_sent,
        force_idr,
        bitrate,
    })
}

impl PipelineHandle {
    /// Stop source and encoder; joins the encode thread.
    pub fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.source.stop()?; // closes the sink → encode thread drains out
        if let Some(mut audio) = self.audio.take() {
            audio.stop(); // capture sink now dropped → audio thread exits
        }
        if let Some(t) = self.encode_thread.take() {
            t.join()
                .map_err(|_| Error::Session("encode thread panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
