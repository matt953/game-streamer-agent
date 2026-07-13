//! One streaming pipeline: source thread → depth-1 ring → encode thread →
//! packetize/send task (spec 01 threading model). Pixels never enter the
//! async world; the tokio side only ever sees encoded chunks.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

/// Pace the send at the live target × this gain (WebRTC's pacing factor), so an
/// IDR drains within a few frame intervals instead of jamming the send queue.
const PACING_GAIN: f64 = 2.5;
/// Pacing rate floor (bytes/s): keeps startup and IDR recovery moving at a low target.
const PACING_FLOOR_BYTES_PER_SEC: f64 = 250_000.0; // 2 Mb/s
/// Send-queue depth (frames) past which the backlog is dropped and an IDR forced.
const SEND_BACKLOG_CAP: usize = 4;

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
    /// Live encode target bitrate (bps) — the actuator the manual knob and the
    /// ABR controller both drive; the encode thread and pacer read it.
    bitrate: Arc<AtomicU32>,
    /// Rolling emitted video bitrate (bps) on the send path — actual encoder
    /// output, pushed to the client.
    emitted_bitrate: Arc<AtomicU32>,
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

    /// The rolling emitted (actual encoder output) bitrate (bps), 0 until enough
    /// frames have been sent to measure it.
    pub fn emitted_bitrate_bps(&self) -> u32 {
        self.emitted_bitrate.load(Ordering::Relaxed)
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
    let force_idr_send = force_idr.clone();
    let bitrate_pace = bitrate.clone();
    let emitted_bitrate = Arc::new(AtomicU32::new(0));
    let emitted_send = emitted_bitrate.clone();
    tokio::spawn(async move {
        let mut logged = 0u64;
        let mut frames_dropped = 0u64;
        // Rolling (send_time, encoded bytes) window for the emitted bitrate.
        let mut emit_window: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut emit_bytes = 0u64;
        while let Some(chunk) = chunk_rx.recv().await {
            // Pacing fell behind the encoder: drop the stale backlog and force
            // an IDR so decode resyncs from the freshest frame.
            if chunk_rx.len() > SEND_BACKLOG_CAP {
                let mut dropped = 1u64; // this chunk is the oldest of the pile
                while chunk_rx.try_recv().is_ok() {
                    dropped += 1;
                }
                frames_dropped += dropped;
                force_idr_send.store(true, Ordering::Release);
                tracing::debug!(dropped, "send backlog dropped; IDR forced");
                continue;
            }
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
            // Pace at the live target so ABR's bound applies to the wire too.
            let br = f64::from(bitrate_pace.load(Ordering::Relaxed));
            let rate = (br / 8.0 * PACING_GAIN).max(PACING_FLOOR_BYTES_PER_SEC);
            let frame_start = std::time::Instant::now();
            let mut deadline = frame_start;
            for d in datagrams {
                let now = std::time::Instant::now();
                if deadline > now {
                    // tokio's timer floor is ~1 ms, so only sleep once the
                    // schedule slack is worth it; sub-ms slack accumulates.
                    let wait = deadline - now;
                    if wait >= Duration::from_millis(1) {
                        tokio::time::sleep(wait).await;
                    }
                }
                deadline += Duration::from_secs_f64(d.len() as f64 / rate);
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
            // Roll the emitted-bitrate window with this frame's encoded size.
            {
                let now = Instant::now();
                let n = chunk.data.len() as u64;
                emit_window.push_back((now, n));
                emit_bytes += n;
                while let Some(&(t, b)) = emit_window.front() {
                    if now.duration_since(t) > Duration::from_secs(1) {
                        emit_bytes -= b;
                        emit_window.pop_front();
                    } else {
                        break;
                    }
                }
                // Bytes in (oldest, newest] over the span.
                if let (Some(&(oldest, oldest_b)), Some(&(newest, _))) =
                    (emit_window.front(), emit_window.back())
                {
                    let span = newest.duration_since(oldest).as_secs_f64();
                    if span > 0.0 {
                        let bps = ((emit_bytes - oldest_b) as f64 * 8.0 / span) as u32;
                        emitted_send.store(bps, Ordering::Relaxed);
                    }
                }
            }
            // Sampled latency span (spec 01: "where did the milliseconds go").
            if sent - logged >= 120 {
                logged = sent;
                let encode_ms =
                    (chunk.encode_done_ts_us.saturating_sub(chunk.capture_ts_us)) as f64 / 1000.0;
                tracing::debug!(
                    frames = sent,
                    encode_ms,
                    size = chunk.data.len(),
                    pace_mbps = rate * 8.0 / 1_000_000.0,
                    send_spread_ms = frame_start.elapsed().as_secs_f64() * 1000.0,
                    queue = chunk_rx.len(),
                    dropped = frames_dropped,
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
        emitted_bitrate,
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
