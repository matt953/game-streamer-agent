//! Everything that happens to a frame *after* it is whole (spec 16).
//!
//! The protocol-independent half of the client: hold early frames to a jitter
//! target, keep the reference chain honest, and measure what reached the
//! glass. A backend supplies complete access units with true arrival stamps
//! and a [`RecoverySink`]; everything downstream of that is here and shared by
//! every backend.

use gsa_client_backend_api::{BackendFrame, CaptureClock, RecoverySink};
use gsa_core::Result;
use gsa_core::time::MediaClock;

use crate::decode::VideoDecoder;
use crate::stats::{ClockSync, LatencyStats, StatsSummary};
use crate::{EncodedFrame, FrameOutput, PresentedSink, stats};

/// How far a frame's arrival sits from its capture stamp.
///
/// Not a latency: the two clocks differ by an unknown constant, so the value
/// on its own means nothing. That constant is the same for every frame, so it
/// cancels from any difference — which makes the *spread* of this quantity the
/// jitter, and a target on it a capture-anchored release. Both are real under
/// a stream clock, where latency is not.
fn transit_drift_us(client_us: u64, capture_ts_us: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    gsa_core::time::wire_ts_delta_us(client_us as u32, capture_ts_us)
}

/// Drives one running stream: gate, de-jitter, and health accounting.
pub struct StreamSession {
    clock: MediaClock,
    clock_sync: ClockSync,
    /// What the backend's capture stamps mean. Under
    /// [`CaptureClock::StreamPts`] only *differences* are real, so de-jitter
    /// still works but glass-to-glass latency does not exist and is reported
    /// as unmeasured rather than as a plausible wrong number.
    capture_clock: CaptureClock,
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
    /// Last frame actually delivered. A repair request must cite a frame the
    /// decoder holds, and frames skipped while frozen were never decoded, so
    /// this trails `last_frame_id` during a freeze.
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
    /// Spread of transit drift as frames were *released* (µs), and its window.
    ///
    /// The output measure. `last_jitter_us` is the spread on arrival — the
    /// smoother's input, which it cannot change — so judging the de-jitter by
    /// that shows nothing however well it works. This is the same quantity
    /// taken after the hold, and it is the one that should shrink.
    released_jitter_us: u32,
    released_win: std::collections::VecDeque<u32>,
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
        Self::with_capture_clock(
            frames_rx,
            recovery,
            clock,
            clock_sync,
            dropped,
            recovered,
            CaptureClock::HostSynced,
        )
    }

    /// As [`StreamSession::new`], with an explicit [`CaptureClock`] for
    /// backends whose stamps are a stream clock.
    #[must_use]
    pub fn with_capture_clock(
        frames_rx: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
        recovery: std::sync::Arc<dyn RecoverySink>,
        clock: MediaClock,
        clock_sync: ClockSync,
        dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
        recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
        capture_clock: CaptureClock,
    ) -> Self {
        let presented = tokio::sync::mpsc::unbounded_channel();
        Self {
            clock,
            clock_sync,
            capture_clock,
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
            // Before the first keyframe the decoder has no reference, so
            // predicted frames arriving ahead of it must be skipped.
            awaiting_idr: true,
            recovery_point: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            decode_error: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dejitter: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            jitter_win: std::collections::VecDeque::new(),
            last_jitter_us: 0,
            released_jitter_us: 0,
            released_win: std::collections::VecDeque::new(),
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

    /// Spread of transit drift at release — what the de-jitter achieved, as
    /// opposed to [`Self::jitter_us`], which is what it was given.
    #[must_use]
    pub fn released_jitter_us(&self) -> u32 {
        self.released_jitter_us
    }

    /// Fold a released frame into the output measure.
    fn note_release(&mut self, capture_ts_us: u32) {
        const WIN: usize = 64;
        let drift = transit_drift_us(self.clock.now_us(), capture_ts_us);
        if self.released_win.len() == WIN {
            self.released_win.pop_front();
        }
        self.released_win.push_back(drift);
        if self.released_win.len() >= WIN / 2 {
            let mut sorted: Vec<u32> = self.released_win.iter().copied().collect();
            sorted.sort_unstable();
            self.released_jitter_us = sorted[sorted.len() * 9 / 10] - sorted[sorted.len() / 10];
        }
    }

    /// Whether glass-to-glass latency exists for this backend. False under
    /// [`CaptureClock::StreamPts`]; a HUD must then show "—", not 0.
    #[must_use]
    pub fn latency_is_absolute(&self) -> bool {
        self.capture_clock == CaptureClock::HostSynced
    }

    /// Latency against the capture stamp, or `None` when the stamp is a stream
    /// clock and no absolute answer exists.
    fn absolute_latency_us(&self, now_us: u64, capture_ts_us: u32) -> Option<u32> {
        self.latency_is_absolute()
            .then(|| self.clock_sync.frame_latency_us(now_us, capture_ts_us))
            .flatten()
    }

    /// Shared de-jitter switch for the embedder (default enabled).
    #[must_use]
    pub fn dejitter_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.dejitter.clone()
    }

    /// Shared flag the embedder sets when its decoder rejects a frame. The
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

    /// Direct form of [`PresentedSink::presented`], for callers that own the
    /// session rather than a sink handle.
    pub fn frame_presented(&mut self, capture_ts_us: u32) {
        let now = self.clock.now_us();
        let latency = self.absolute_latency_us(now, capture_ts_us);
        self.present.on_presented(latency, capture_ts_us, now);
    }

    /// Fold queued presentation reports into the health stats. They are
    /// stamped on the display thread, so each is aged back to its own instant
    /// rather than counted as having happened now.
    fn drain_presented(&mut self) {
        while let Ok((capture_ts, at)) = self.presented_rx.try_recv() {
            let now = self
                .clock
                .now_us()
                .saturating_sub(at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            let latency = self.absolute_latency_us(now, capture_ts);
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
        let latency_us = self.absolute_latency_us(now, gated.capture_ts_us);
        // Decode happens app-side and is unmeasurable here: record 0.
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
                    let latency_us = self.absolute_latency_us(now, gated.capture_ts_us);
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
                    // An undecodable (loss-damaged) frame is never fatal:
                    // freeze and ask for a repair, then keep receiving.
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

    /// Reference-chain gate for one frame (spec 04). On a break, hold the last
    /// good picture and skip predicted frames until a keyframe or a
    /// host-announced recovery point resyncs: decoding against a stale
    /// reference corrupts the output.
    async fn gate(&mut self, f: BackendFrame, backlog: bool) -> Result<Option<BackendFrame>> {
        let arrival_us = f.arrival_us;
        self.stats.on_frame_complete(f.data.len(), arrival_us);
        // A decoder-rejected frame breaks the chain even when delivery looked
        // clean, so it is handled exactly like a gap.
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
                // Backends are required to release in order, so this means
                // ordering is broken upstream.
                tracing::warn!(last, got = f.frame_id, "OUT-OF-ORDER frame gated");
            } else if delta != 1 && !is_idr && !self.awaiting_idr {
                tracing::debug!(gap_after = last, got = f.frame_id, "frame gap; freezing");
                self.awaiting_idr = true;
                self.request_keyframe_throttled();
            }
        }

        // Frozen. A host-announced recovery point resumes decoding without a
        // keyframe: frames from it on reference nothing that is missing.
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
                // Skip the predicted frame, but advance the id so the next
                // one is not re-flagged as a fresh gap.
                self.last_frame_id = Some(f.frame_id);
                self.request_keyframe_throttled();
                return Ok(None);
            }
        }

        // The frame counts as consumed whatever the caller does with it;
        // advance so the next one is not misread as a gap.
        self.last_frame_id = Some(f.frame_id);
        self.last_delivered_id = Some(f.frame_id);
        self.dejitter_release(f.capture_ts_us, arrival_us, backlog)
            .await;
        // After the hold, not before: this is the measure of what the pacing
        // achieved rather than what it was handed.
        self.measure_release(f.capture_ts_us);
        Ok(Some(f))
    }

    /// Request a repair, rate-limited so a burst of gaps or decode errors does
    /// not spam the host. Throttling lives here rather than in the backend, so
    /// every protocol gets it.
    fn request_keyframe_throttled(&mut self) {
        const MIN_INTERVAL_US: u64 = 250_000;
        let now = self.clock.now_us();
        if now.saturating_sub(self.last_keyframe_request_us) < MIN_INTERVAL_US {
            return;
        }
        self.last_keyframe_request_us = now;
        // With a known-good frame the host can invalidate references instead
        // of sending a full IDR (spec 04 rung 2).
        match self.last_delivered_id {
            Some(last_good) => self.recovery.request_recovery(last_good),
            None => self.recovery.request_keyframe(),
        }
    }

    /// Absorb delay variance by holding early frames to a capture-anchored
    /// target (the window's p90 transit), so the spread is spent waiting
    /// rather than stuttering.
    ///
    /// The target must be anchored to capture time and never to the previous
    /// release: an anchor on the previous release compounds its own error and
    /// drifts, while a capture anchor forces the release rate to equal the
    /// capture rate. Late frames never wait, a backlog drains unpaced, and a
    /// clean link waits not at all.
    ///
    /// **The signal is transit drift, not latency.** `arrival - capture` is
    /// offset by however far the two clocks differ, and under a stream clock
    /// that offset is unknowable — but it is *constant*, so it cancels out of
    /// every difference. The spread of the drift is therefore exactly the
    /// jitter, and holding a frame to a drift target spaces releases by the
    /// capture interval, which is what smooth motion is.
    ///
    /// Keying this to absolute latency instead is what made it dead code on
    /// every stream-clock backend: the latency was always `None`, the window
    /// never filled, and it returned before measuring anything.
    async fn dejitter_release(&mut self, capture_ts_us: u32, arrival_us: u64, backlog: bool) {
        const WIN: usize = 32;
        const JITTER_ON_US: u32 = 12_000;
        /// Disengage threshold, deliberately below `JITTER_ON_US`: the
        /// hysteresis stops a link hovering at the engage point from flapping.
        const JITTER_OFF_US: u32 = 8_000;
        const DEJITTER_MAX_US: u32 = 33_000;
        /// Startup transients (clock sync settling, burst catch-up) must not
        /// be read as jitter.
        const WARMUP_US: u64 = 2_000_000;
        let now = self.clock.now_us();
        self.first_gate_us.get_or_insert(now);
        // Measured on this clock, not from `arrival_us`. Every stage stamps
        // with a `MediaClock` of its own, and each one starts its epoch when
        // it is built — so a drift window filled from the backend's stamps and
        // compared against a target on this clock is two different time bases
        // subtracted from each other. The spread then reads as whatever the
        // gap between the epochs happens to be.
        let drift = transit_drift_us(now, capture_ts_us);
        let _ = arrival_us;
        if self.jitter_win.len() == WIN {
            self.jitter_win.pop_front();
        }
        self.jitter_win.push_back(drift);
        if self.jitter_win.len() < WIN / 2 {
            return;
        }
        let mut lat: Vec<u32> = self.jitter_win.iter().copied().collect();
        lat.sort_unstable();
        let (p10, p90) = (lat[lat.len() / 10], lat[lat.len() * 9 / 10]);
        let jitter = p90 - p10;
        self.last_jitter_us = jitter;
        // Pacing is only sound with nothing else queued: holding a frame while
        // others wait builds a standing backlog that never drains.
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
        // How far through its transit budget this frame already is. Measured
        // against `now` rather than `arrival_us` so time spent queued behind
        // the gate counts against the wait rather than being added to it.
        let drift_now = transit_drift_us(now, capture_ts_us);
        let target = p90.min(p10.saturating_add(DEJITTER_MAX_US));
        if drift_now < target {
            let wait = u64::from((target - drift_now).min(DEJITTER_MAX_US));
            tokio::time::sleep(std::time::Duration::from_micros(wait)).await;
        }
    }

    /// Record what the release actually achieved, held or not.
    fn measure_release(&mut self, capture_ts_us: u32) {
        self.note_release(capture_ts_us);
    }
}

#[cfg(test)]
mod dejitter_signal_tests {
    use super::transit_drift_us;

    /// The whole reason this signal works under a stream clock: the offset
    /// between the two clocks is unknown, but it is the *same* for every
    /// frame, so it vanishes from any difference. If it did not, the jitter
    /// figure would be an arbitrary constant and the de-jitter would hold
    /// frames for a made-up length of time.
    #[test]
    fn an_unknown_clock_offset_cancels_out_of_the_spread() {
        let captures: Vec<u32> = (0..8).map(|i| 1_000_000 + i * 33_333).collect();
        let jitter = [0i64, 5_000, -3_000, 1_000, 4_000, -2_000, 0, 3_000];

        let spread_for = |offset: i64| {
            let drifts: Vec<u32> = captures
                .iter()
                .zip(jitter)
                .map(|(&capture, wobble)| {
                    let arrival = i64::from(capture) + offset + 20_000 + wobble;
                    #[allow(clippy::cast_sign_loss)]
                    transit_drift_us(arrival as u64, capture)
                })
                .collect();
            drifts.iter().max().unwrap() - drifts.iter().min().unwrap()
        };

        let baseline = spread_for(0);
        assert_eq!(baseline, 8_000, "5 ms early to 3 ms late");
        assert_eq!(spread_for(500_000), baseline);
        assert_eq!(spread_for(-250_000), baseline);
    }

    /// Stream timestamps wrap; a frame either side of the wrap must not read
    /// as a four-thousand-second transit and freeze the pacing.
    #[test]
    fn the_drift_survives_a_timestamp_wrap() {
        let capture = u32::MAX - 1_000;
        let arrival = u64::from(u32::MAX) + 4_000;
        assert_eq!(transit_drift_us(arrival, capture), 5_000);
    }
}
