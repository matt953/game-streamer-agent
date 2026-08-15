//! Everything that happens to a frame *after* it is whole (spec 16).
//!
//! This is the half of the client that is not protocol-specific: hold early
//! frames to a jitter target, keep the reference chain honest, and measure
//! what actually reached the glass. A backend delivers complete access units
//! with true arrival stamps and a way to ask for repairs; the rest is here,
//! shared, so a second protocol inherits the field tuning instead of
//! reinventing it.

use gsa_client_backend_api::{BackendFrame, RecoverySink};
use gsa_core::Result;
use gsa_core::time::MediaClock;

use crate::decode::VideoDecoder;
use crate::stats::{ClockSync, LatencyStats, StatsSummary};
use crate::{EncodedFrame, FrameOutput, PresentedSink, stats};

/// Drives one running stream: gate, de-jitter, and health accounting.
pub struct StreamSession {
    clock: MediaClock,
    clock_sync: ClockSync,
    /// Complete frames from the backend, stamped at true arrival.
    frames_rx: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
    /// How the gate asks the host to repair a broken reference chain.
    recovery: std::sync::Arc<dyn RecoverySink>,
    stats: LatencyStats,
    present: stats::PresentStats,
    presented_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, std::time::Instant)>,
    presented_tx: tokio::sync::mpsc::UnboundedSender<(u32, std::time::Instant)>,
    /// Backend-maintained counters for frames it could not deliver whole.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frame id of the last frame handed on (gap detection).
    last_frame_id: Option<u32>,
    /// Last frame actually DELIVERED: a repair request must cite a frame the
    /// decoder truly has, and frames skipped while frozen were never decoded.
    last_delivered_id: Option<u32>,
    last_keyframe_request_us: u64,
    /// True while the reference chain is broken; predicted frames are skipped
    /// until a keyframe — or a host-announced recovery point — resyncs.
    awaiting_idr: bool,
    /// Host-announced first-safe frame id + 1 (0 = none).
    recovery_point: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set by the embedder when its decoder rejected a delivered frame.
    decode_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    dejitter: std::sync::Arc<std::sync::atomic::AtomicBool>,
    jitter_win: std::collections::VecDeque<u32>,
    last_jitter_us: u32,
    dejitter_active: bool,
    first_gate_us: Option<u64>,
}

impl std::fmt::Debug for StreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSession")
            .field("awaiting_idr", &self.awaiting_idr)
            .field("last_frame_id", &self.last_frame_id)
            .finish_non_exhaustive()
    }
}

impl StreamSession {
    #[must_use]
    pub fn new(
        frames_rx: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
        recovery: std::sync::Arc<dyn RecoverySink>,
        clock: MediaClock,
        clock_sync: ClockSync,
        dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
        recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let presented = tokio::sync::mpsc::unbounded_channel();
        Self {
            clock,
            clock_sync,
            frames_rx,
            recovery,
            stats: LatencyStats::default(),
            present: stats::PresentStats::default(),
            presented_rx: presented.1,
            presented_tx: presented.0,
            dropped,
            recovered,
            last_frame_id: None,
            last_delivered_id: None,
            last_keyframe_request_us: 0,
            // Until the first keyframe the decoder has no reference; skip any
            // predicted frames that arrive ahead of it.
            awaiting_idr: true,
            recovery_point: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            decode_error: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dejitter: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            jitter_win: std::collections::VecDeque::new(),
            last_jitter_us: 0,
            dejitter_active: false,
            first_gate_us: None,
        }
    }

    /// Keep the clock offset estimate current (backends that can measure it).
    pub fn clock_sync_mut(&mut self) -> &mut ClockSync {
        &mut self.clock_sync
    }

    #[must_use]
    pub fn clock(&self) -> &MediaClock {
        &self.clock
    }

    /// Where a backend publishes host-announced recovery points.
    #[must_use]
    pub fn recovery_point(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.recovery_point.clone()
    }

    /// Last measured latency spread (µs) — the de-jitter signal, for reports.
    #[must_use]
    pub fn jitter_us(&self) -> u32 {
        self.last_jitter_us
    }

    /// Shared de-jitter switch for the embedder (default enabled).
    #[must_use]
    pub fn dejitter_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.dejitter.clone()
    }

    /// Shared flag the embedder sets when its decoder rejects a frame; the
    /// gate treats it as a reference break and requests repair.
    #[must_use]
    pub fn decode_error_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.decode_error.clone()
    }

    /// Handle for the embedder's display path to report presented frames.
    #[must_use]
    pub fn presented_sink(&self) -> PresentedSink {
        PresentedSink {
            tx: self.presented_tx.clone(),
        }
    }

    /// Direct form of [`PresentedSink::presented`] for harnesses that own the
    /// session.
    pub fn frame_presented(&mut self, capture_ts_us: u32) {
        let now = self.clock.now_us();
        let latency = self.clock_sync.frame_latency_us(now, capture_ts_us);
        self.present.on_presented(latency, capture_ts_us, now);
    }

    /// Fold queued presentation reports (stamped on the display thread) into
    /// the health stats.
    fn drain_presented(&mut self) {
        while let Ok((capture_ts, at)) = self.presented_rx.try_recv() {
            let now = self
                .clock
                .now_us()
                .saturating_sub(at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            let latency = self.clock_sync.frame_latency_us(now, capture_ts);
            self.present.on_presented(latency, capture_ts, now);
        }
    }

    #[must_use]
    pub fn stats(&self) -> StatsSummary {
        self.stats.summary(
            self.dropped.load(std::sync::atomic::Ordering::Relaxed),
            self.recovered.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Presentation-side health summary (fed by [`PresentedSink`]).
    pub fn present_stats(&mut self) -> stats::PresentSummary {
        self.drain_presented();
        self.present.summary()
    }

    /// Next complete **encoded** access unit past the gate, for embedders
    /// that decode on the platform. `None` when the stream ends.
    pub async fn recv_encoded(&mut self) -> Result<Option<EncodedFrame>> {
        let Some(gated) = self.next_gated_frame().await? else {
            return Ok(None);
        };
        let now = self.clock.now_us();
        let latency_us = self.clock_sync.frame_latency_us(now, gated.capture_ts_us);
        // Decode happens app-side; record it as zero in the stats window.
        self.stats.on_frame_decoded(latency_us, 0);
        Ok(Some(EncodedFrame {
            data: gated.data,
            frame_id: gated.frame_id,
            keyframe: gated.keyframe,
            capture_ts_us: gated.capture_ts_us,
            latency_us,
        }))
    }

    /// Receive frames until one decodes. `None` when the stream ends.
    pub async fn recv_frame(
        &mut self,
        decoder: &mut dyn VideoDecoder,
    ) -> Result<Option<FrameOutput>> {
        loop {
            let Some(gated) = self.next_gated_frame().await? else {
                return Ok(None);
            };
            let decode_start = self.clock.now_us();
            match decoder.decode(&gated.data) {
                Ok(Some(frame)) => {
                    let now = self.clock.now_us();
                    let decode_us = (now - decode_start) as u32;
                    let latency_us = self.clock_sync.frame_latency_us(now, gated.capture_ts_us);
                    self.stats.on_frame_decoded(latency_us, decode_us);
                    return Ok(Some(FrameOutput {
                        frame,
                        frame_id: gated.frame_id,
                        capture_ts_us: gated.capture_ts_us,
                        latency_us,
                        decode_us,
                    }));
                }
                Ok(None) => {
                    // Decoder accepted the data but produced no frame
                    // (parameter sets / buffering).
                }
                Err(e) => {
                    // Undecodable (loss-damaged) frame: never fatal. Freeze
                    // and ask for a healing keyframe.
                    tracing::debug!(error = %e, "decode error; freezing until keyframe");
                    self.awaiting_idr = true;
                    self.request_keyframe_throttled();
                }
            }
        }
    }

    /// Next frame past the loss-recovery gate. `None` when the stream ends.
    async fn next_gated_frame(&mut self) -> Result<Option<BackendFrame>> {
        loop {
            let Some(frame) = self.frames_rx.recv().await else {
                return Ok(None);
            };
            let backlog = !self.frames_rx.is_empty();
            if let Some(out) = self.gate(frame, backlog).await? {
                return Ok(Some(out));
            }
        }
    }

    /// Reference-chain gate for one released frame (spec 04): on a break,
    /// hold the last good picture and skip predicted frames until a keyframe
    /// resyncs — decoding against a stale reference corrupts the output.
    async fn gate(&mut self, f: BackendFrame, backlog: bool) -> Result<Option<BackendFrame>> {
        let arrival_us = f.arrival_us;
        self.stats.on_frame_complete(f.data.len(), arrival_us);
        // A decoder-rejected frame breaks the chain even though delivery
        // looked clean: freeze and request repair like any gap.
        if self
            .decode_error
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            && !self.awaiting_idr
        {
            tracing::debug!("embedder reported decode error; freezing");
            self.awaiting_idr = true;
            self.request_keyframe_throttled();
        }
        let is_idr = f.keyframe;
        if is_idr {
            self.awaiting_idr = false;
        }
        if let Some(last) = self.last_frame_id {
            let delta = f.frame_id.wrapping_sub(last);
            if delta == 0 || delta > u32::MAX / 2 {
                // Backends release in order, so this should be unreachable;
                // if it fires, ordering is broken upstream.
                tracing::warn!(last, got = f.frame_id, "OUT-OF-ORDER frame gated");
            } else if delta != 1 && !is_idr && !self.awaiting_idr {
                tracing::debug!(gap_after = last, got = f.frame_id, "frame gap; freezing");
                self.awaiting_idr = true;
                self.request_keyframe_throttled();
            }
        }

        // Frozen: a host-announced recovery point resumes decoding without a
        // keyframe — frames from it on reference nothing we are missing.
        if self.awaiting_idr && !is_idr {
            let rp = self
                .recovery_point
                .load(std::sync::atomic::Ordering::Acquire);
            let safe = rp > 0 && {
                let first_safe = (rp - 1) as u32;
                f.frame_id.wrapping_sub(first_safe) < u32::MAX / 2
            };
            if safe {
                self.awaiting_idr = false;
            } else {
                // Skip predicted frames (a broken reference is the
                // corruption); advance the id so the gap isn't re-flagged.
                self.last_frame_id = Some(f.frame_id);
                self.request_keyframe_throttled();
                return Ok(None);
            }
        }

        // The frame is consumed regardless of what the caller does with it;
        // advance so the next frame isn't misread as another gap.
        self.last_frame_id = Some(f.frame_id);
        self.last_delivered_id = Some(f.frame_id);
        self.dejitter_release(f.capture_ts_us, arrival_us, backlog)
            .await;
        Ok(Some(f))
    }

    /// Request a healing keyframe, rate-limited so a burst of gaps or errors
    /// doesn't spam the host. Throttling lives here, not in the backend, so
    /// every protocol inherits it.
    fn request_keyframe_throttled(&mut self) {
        const MIN_INTERVAL_US: u64 = 250_000;
        let now = self.clock.now_us();
        if now.saturating_sub(self.last_keyframe_request_us) < MIN_INTERVAL_US {
            return;
        }
        self.last_keyframe_request_us = now;
        // With a known-good frame the host can clean references instead of
        // resetting the world with an IDR (spec 04 rung 2).
        match self.last_delivered_id {
            Some(last_good) => self.recovery.request_recovery(last_good),
            None => self.recovery.request_keyframe(),
        }
    }

    /// Absorb delay variance by holding early frames to a capture-anchored
    /// latency target — the window's p90 — so the spread is spent waiting,
    /// not stuttering. Anchoring to capture time (never to the previous
    /// release) makes drift structurally impossible: the release rate equals
    /// the capture rate. Late frames never wait, a backlog is drained
    /// unpaced, and a clean link pays nothing.
    async fn dejitter_release(&mut self, capture_ts_us: u32, arrival_us: u64, backlog: bool) {
        const WIN: usize = 32;
        const JITTER_ON_US: u32 = 12_000;
        /// Hysteresis: a link hovering at the engage threshold must not flap
        /// the mode (and its log line) every few frames.
        const JITTER_OFF_US: u32 = 8_000;
        const DEJITTER_MAX_US: u32 = 33_000;
        /// Startup transient (clock sync settling, burst catch-up) must not
        /// read as jitter.
        const WARMUP_US: u64 = 2_000_000;
        let now = self.clock.now_us();
        self.first_gate_us.get_or_insert(now);
        if let Some(lat) = self.clock_sync.frame_latency_us(arrival_us, capture_ts_us) {
            if self.jitter_win.len() == WIN {
                self.jitter_win.pop_front();
            }
            self.jitter_win.push_back(lat);
        }
        if self.jitter_win.len() < WIN / 2 {
            return;
        }
        let mut lat: Vec<u32> = self.jitter_win.iter().copied().collect();
        lat.sort_unstable();
        let (p10, p90) = (lat[lat.len() / 10], lat[lat.len() * 9 / 10]);
        let jitter = p90 - p10;
        self.last_jitter_us = jitter;
        // Pacing is only sound at the queue head with nothing waiting:
        // holding a frame while more are already queued builds a standing
        // backlog that can never drain — the opposite of smoothing.
        if backlog || !self.dejitter.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let age = now.saturating_sub(self.first_gate_us.unwrap_or(now));
        if age < WARMUP_US {
            return;
        }
        let high = if self.dejitter_active {
            jitter >= JITTER_OFF_US
        } else {
            jitter >= JITTER_ON_US
        };
        if high != self.dejitter_active {
            self.dejitter_active = high;
            tracing::debug!(jitter_us = jitter, active = high, "dejitter mode");
        }
        if !high {
            return;
        }
        let Some(lat_now) = self.clock_sync.frame_latency_us(now, capture_ts_us) else {
            return;
        };
        let target = p90.min(p10.saturating_add(DEJITTER_MAX_US));
        if lat_now < target {
            let wait = u64::from((target - lat_now).min(DEJITTER_MAX_US));
            tokio::time::sleep(std::time::Duration::from_micros(wait)).await;
        }
    }
}
