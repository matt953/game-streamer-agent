//! One streaming pipeline: source thread → depth-1 ring → encode thread →
//! packetize/send task (spec 01 threading model). Pixels never enter the
//! async world; the tokio side only ever sees encoded chunks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gsa_capture_api::{RenderSource, SourceConfig, frame_channel};
use gsa_core::media::{Codec, VideoMode};
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

pub struct PipelineHandle {
    stop: Arc<AtomicBool>,
    source: Box<dyn RenderSource>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    pub frames_sent: Arc<AtomicU64>,
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
) -> Result<PipelineHandle> {
    let (sink, rx) = frame_channel();
    encoder.open(EncodeConfig {
        codec: Codec::H264,
        mode,
        bitrate_bps,
    })?;
    source.start(SourceConfig { mode }, sink)?;

    let stop = Arc::new(AtomicBool::new(false));
    let frames_sent = Arc::new(AtomicU64::new(0));
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();

    let stop_enc = stop.clone();
    let encode_thread = std::thread::Builder::new()
        .name("gsa-encode".into())
        .spawn(move || {
            while !stop_enc.load(Ordering::Acquire) {
                let Some(frame) = rx.recv_latest(Duration::from_millis(100)) else {
                    if rx.is_closed() {
                        break;
                    }
                    continue;
                };
                if let Err(e) = encoder.submit(&frame, FrameDirectives::default()) {
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
    // TODO(M3): real pacer — spread chunks over 1-3 ms instead of bursting
    // (spec 04). At M0/loopback bursting is fine.
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
            for d in datagrams {
                if let Err(e) = conn.send_datagram(bytes::Bytes::from(d)) {
                    tracing::info!(error = %e, "datagram send failed; sender exiting");
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
        frames_sent,
    })
}

impl PipelineHandle {
    /// Stop source and encoder; joins the encode thread.
    pub fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.source.stop()?; // closes the sink → encode thread drains out
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
