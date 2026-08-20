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
    /// Where cadence breaks entered the stream: at capture, or in transit.
    arrival: stats::ArrivalCadence,
    /// The latency chain, stage by stage, composed the way the reference
    /// client's overlay is: durations and round trips, no clock sync.
    latency: stats::LatencyChain,
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
    /// Drift at the first release, and how far it has moved since — real
    /// milliseconds of latency gained, the one figure a steady backlog shows
    /// up in.
    first_release_drift_us: Option<u32>,
    latency_growth_us: i64,
    released_win: std::collections::VecDeque<u32>,
    /// The latency-for-smoothness trade this session is making.
    pacing: crate::PacingMode,
    /// One frame at the stream's rate, in microseconds — the unit the hold cap
    /// is expressed in. Learned from the capture stamps rather than assumed,
    /// since the negotiated rate and the rate a host actually produces differ.
    frame_interval_us: u32,
    /// Previous capture stamp, for measuring the stream's frame interval.
    last_capture_for_interval: Option<u32>,
    /// Recent capture gaps, for the interval percentile.
    interval_gaps: std::collections::VecDeque<u32>,
    /// Total time frames have been held back to smooth delivery, and how many
    /// were held. The latency side of the trade: smoothness gained is
    /// meaningless without the delay paid for it, and the hold happens before
    /// decode so nothing downstream can see it.
    hold_total_us: u64,
    held_frames: u64,
    /// Frames the de-jitter declined to pace because something was queued.
    dejitter_skipped_backlog: u64,
    /// Frames it did pace.
    dejitter_ran: u64,
    dejitter_active: bool,
    first_gate_us: Option<u64>,
    /// Frames decoded and then discarded because newer ones were already
    /// queued — the drop half of the pacing policy, enforced here so every
    /// embedder gets it.
    superseded: u64,
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
            arrival: stats::ArrivalCadence::default(),
            latency: stats::LatencyChain::default(),
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
            first_release_drift_us: None,
            latency_growth_us: 0,
            released_win: std::collections::VecDeque::new(),
            pacing: crate::PacingMode::default(),
            // Until measured, assume 60 fps: it is the commonest rate and the
            // figure is replaced within a second of frames arriving.
            frame_interval_us: 16_667,
            last_capture_for_interval: None,
            interval_gaps: std::collections::VecDeque::new(),
            hold_total_us: 0,
            held_frames: 0,
            dejitter_skipped_backlog: 0,
            dejitter_ran: 0,
            dejitter_active: false,
            first_gate_us: None,
            superseded: 0,
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

    /// How often the de-jitter paced a frame, against how often it stood down
    /// because frames were already queued.
    #[must_use]
    pub fn dejitter_duty(&self) -> (u64, u64) {
        (self.dejitter_ran, self.dejitter_skipped_backlog)
    }

    /// Spread of transit drift at release — what the de-jitter achieved, as
    /// opposed to [`Self::jitter_us`], which is what it was given.
    #[must_use]
    pub fn released_jitter_us(&self) -> u32 {
        self.released_jitter_us
    }

    /// Learn the stream's own frame interval from consecutive capture stamps.
    ///
    /// The negotiated rate is what was asked for, not what arrives: a host
    /// encodes on change, so a 60 fps request over 30 fps content delivers
    /// every 33 ms. The hold cap is a number of frames, so it has to be the
    /// real interval or the policy is not the one the mode names.
    /// Learn the stream's frame interval; returns this frame's capture gap.
    ///
    /// The interval is a low percentile of recent gaps, not an average. Under
    /// change-driven encoding a still screen makes consecutive stamps sit
    /// hundreds of milliseconds apart, and an average ingests those idle gaps
    /// as if they were the frame rate — inflating the one-frame hold budget
    /// into tens of milliseconds of real latency. The *shortest* common gap
    /// is the cadence the host actually produces at; idle time only ever
    /// lands in the upper tail, where a low percentile never looks.
    fn learn_frame_interval(&mut self, capture_ts_us: u32) -> Option<u32> {
        const WIN: usize = 64;
        let Some(previous) = self.last_capture_for_interval else {
            self.last_capture_for_interval = Some(capture_ts_us);
            return None;
        };
        self.last_capture_for_interval = Some(capture_ts_us);
        let gap = gsa_core::time::wire_ts_delta_us(capture_ts_us, previous);
        // A gap outside this range is a stall or a stamp that wrapped oddly,
        // not a cadence: 8 ms is 120 fps and 200 ms is 5 fps.
        if (8_000..=200_000).contains(&gap) {
            if self.interval_gaps.len() == WIN {
                self.interval_gaps.pop_front();
            }
            self.interval_gaps.push_back(gap);
            if self.interval_gaps.len() >= 8 {
                let mut sorted: Vec<u32> = self.interval_gaps.iter().copied().collect();
                sorted.sort_unstable();
                self.frame_interval_us = sorted[sorted.len() / 4];
            }
        }
        Some(gap)
    }

    /// Fold a released frame into the output measure.
    fn note_release(&mut self, capture_ts_us: u32) {
        const WIN: usize = 64;
        let drift = transit_drift_us(self.clock.now_us(), capture_ts_us);
        // How much further behind the stream we are than when it started.
        //
        // The offset between the host's clock and ours is unknown, so drift
        // has no absolute meaning — but it is *constant*, so it cancels from a
        // difference and this is real milliseconds of latency gained or lost.
        //
        // Needed because every other figure here is a spread, and a backlog
        // that fills once and never drains has no spread at all: a queue two
        // seconds deep looks identical to a perfect stream in jitter terms,
        // while the picture arrives two seconds after the sound.
        match self.first_release_drift_us {
            None => self.first_release_drift_us = Some(drift),
            Some(first) => {
                self.latency_growth_us = i64::from(drift) - i64::from(first);
            }
        }
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

    /// Latency gained since the session's first frame, in microseconds.
    ///
    /// Positive means the picture is further behind the host than it was at
    /// the start — a queue that filled and never drained. Negative means it
    /// caught up.
    #[must_use]
    pub fn latency_growth_us(&self) -> i64 {
        self.latency_growth_us
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

    /// Choose the latency-for-smoothness trade. Takes effect on the next
    /// frame; nothing is retained from the previous mode.
    pub fn set_pacing(&mut self, mode: crate::PacingMode) {
        self.pacing = mode;
        tracing::info!(mode = mode.label(), "pacing mode");
    }

    /// The trade currently in force.
    #[must_use]
    pub fn pacing(&self) -> crate::PacingMode {
        self.pacing
    }

    /// The stream's measured frame interval (µs), which the hold cap is a
    /// multiple of.
    #[must_use]
    pub fn frame_interval_us(&self) -> u32 {
        self.frame_interval_us
    }

    /// Mean microseconds a frame is held back, over the session.
    ///
    /// The price of smoothing, and the half of the trade that no downstream
    /// measurement can see: the hold happens before decode, so a display-side
    /// figure reports it as zero however long it was.
    #[must_use]
    pub fn mean_hold_us(&self) -> u32 {
        if self.held_frames == 0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (self.hold_total_us / self.held_frames) as u32
        }
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

    /// The transport measured a control-link round trip; add it to the chain.
    pub fn on_link_rtt(&mut self, rtt_us: u32) {
        self.latency.on_rtt(rtt_us);
    }

    /// A presenter measured how long a decoded frame waited to be shown.
    pub fn on_present_wait(&mut self, wait_us: u32) {
        self.latency.on_present_wait(wait_us);
    }

    /// Per-stage latency percentiles and the composed total.
    #[must_use]
    pub fn latency_chain(&self) -> stats::LatencySummary {
        self.latency.summary()
    }

    /// Frames decoded but discarded unseen under the drop policy.
    #[must_use]
    pub fn superseded(&self) -> u64 {
        self.superseded
    }

    /// Where cadence breaks came from, as frames arrived.
    #[must_use]
    pub fn arrival_cadence(&self) -> (u64, u64, u32) {
        (
            self.arrival.captured_late,
            self.arrival.delivered_late,
            self.arrival.worst_slip_us,
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
        if let Some(total) = latency_us {
            self.latency.on_measured_total(total);
        }
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
    ///
    /// The pacing mode's drop policy is enforced *here*, not by the presenter:
    /// in a mode that drops, a decoded frame with newer complete frames
    /// already queued behind it is superseded — decoded (the reference chain
    /// needs it) but never returned — so every embedder inherits the policy
    /// instead of re-implementing it per platform. In the modes that never
    /// drop, every decoded frame is returned in order.
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
                    // Newer frames are already waiting: showing this one would
                    // only delay them, so it is dropped now unless the mode
                    // promises every frame is seen. Decode still happened —
                    // skipping it would corrupt every later frame.
                    if self.pacing.drops_unshown() && !self.frames_rx.is_empty() {
                        self.superseded += 1;
                        continue;
                    }
                    let now = self.clock.now_us();
                    let decode_us = (now - decode_start) as u32;
                    self.latency.on_decode(decode_us);
                    let latency_us = self.absolute_latency_us(now, gated.capture_ts_us);
                    if let Some(total) = latency_us {
                        self.latency.on_measured_total(total);
                    }
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
        // Before any gating or pacing: this has to see the stream as it was
        // delivered, not as we chose to release it.
        self.arrival.on_arrival(f.capture_ts_us, arrival_us);
        if let Some(host_us) = f.host_latency_us {
            self.latency.on_host(host_us);
        }
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
        // The mode's own ceiling, in frame intervals. Was a flat 33 ms, which
        // is two frames at 60 fps and eight at 240 — the same number meaning a
        // different policy on every display.
        let dejitter_max_us = self.pacing.hold_cap_us(self.frame_interval_us);
        if !self.pacing.paces() {
            return;
        }
        /// Startup transients (clock sync settling, burst catch-up) must not
        /// be read as jitter.
        const WARMUP_US: u64 = 2_000_000;
        let now = self.clock.now_us();
        self.first_gate_us.get_or_insert(now);
        let capture_gap = self.learn_frame_interval(capture_ts_us);
        // A frame arriving after a content pause says nothing about the
        // link: the gap is the host's own idle time, and reading it as
        // jitter is what wakes the smoother on film content and static
        // screens. It is shown immediately and kept out of the window.
        let content_pause =
            capture_gap.is_none_or(|gap| gap > self.frame_interval_us.saturating_mul(2));
        if content_pause {
            self.latency.on_hold(0);
            return;
        }
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
        //
        // Counted because it decides whether the smoothing runs at all: a
        // bursty source keeps something queued most of the time, and the
        // de-jitter then stands down exactly when the link needs it.
        if backlog {
            self.dejitter_skipped_backlog = self.dejitter_skipped_backlog.saturating_add(1);
        }
        if backlog || !self.dejitter.load(std::sync::atomic::Ordering::Relaxed) {
            self.latency.on_hold(0);
            return;
        }
        self.dejitter_ran = self.dejitter_ran.saturating_add(1);
        let age = now.saturating_sub(self.first_gate_us.unwrap_or(now));
        if age < WARMUP_US {
            self.latency.on_hold(0);
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
            self.latency.on_hold(0);
            return;
        }
        // How far through its transit budget this frame already is. Measured
        // against `now` rather than `arrival_us` so time spent queued behind
        // the gate counts against the wait rather than being added to it.
        let drift_now = transit_drift_us(now, capture_ts_us);
        let target = p90.min(p10.saturating_add(dejitter_max_us));
        if drift_now >= target {
            self.latency.on_hold(0);
        }
        if drift_now < target {
            let wait = u64::from((target - drift_now).min(dejitter_max_us));
            self.hold_total_us += wait;
            self.held_frames += 1;
            #[allow(clippy::cast_possible_truncation)]
            self.latency.on_hold(wait as u32);
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

#[cfg(test)]
mod supersede_tests {
    use gsa_client_backend_api::{BackendFrame, CaptureClock, RecoverySink};

    /// A decoder that "decodes" every access unit into a one-pixel frame and
    /// remembers how many it was fed, so the test can tell decoded-then-
    /// discarded from never-decoded — only the second corrupts a stream.
    struct CountingDecoder {
        fed: usize,
    }

    impl crate::VideoDecoder for CountingDecoder {
        fn decode(&mut self, _au: &[u8]) -> gsa_core::Result<Option<crate::DecodedFrame>> {
            self.fed += 1;
            Ok(Some(crate::DecodedFrame {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
                order: crate::PixelOrder::Rgba,
                platform: None,
            }))
        }
    }

    #[derive(Debug)]
    struct NoRecovery;
    impl RecoverySink for NoRecovery {
        fn request_keyframe(&self) {}
    }

    fn session_with_frames(
        count: u32,
    ) -> (
        super::StreamSession,
        tokio::sync::mpsc::UnboundedSender<BackendFrame>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let session = super::StreamSession::with_capture_clock(
            rx,
            std::sync::Arc::new(NoRecovery),
            gsa_core::time::MediaClock::new(),
            crate::ClockSync::default(),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            CaptureClock::StreamPts,
        );
        // The pacing hold is not what is under test, and a paced release
        // would stall the drain this test measures.
        session
            .dejitter_flag()
            .store(false, std::sync::atomic::Ordering::Relaxed);
        for i in 0..count {
            let _ = tx.send(BackendFrame {
                data: vec![0u8; 16],
                frame_id: i + 1,
                keyframe: i == 0,
                capture_ts_us: 1_000_000 + i * 16_667,
                host_latency_us: Some(4_000),
                arrival_us: u64::from(1_000_000 + i * 16_667),
            });
        }
        (session, tx)
    }

    /// The drop policy lives in the session, not the presenter: with newer
    /// frames already queued, older decoded frames are discarded here and
    /// only the newest comes out. Every frame is still *decoded* — dropping
    /// one before the decoder would corrupt everything referencing it.
    #[tokio::test]
    async fn a_dropping_mode_returns_only_the_newest_queued_frame() {
        let (mut session, _tx) = session_with_frames(4);
        session.set_pacing(crate::PacingMode::LowestLatency);
        let mut decoder = CountingDecoder { fed: 0 };
        let out = session
            .recv_frame(&mut decoder)
            .await
            .expect("recv works")
            .expect("a frame");
        assert_eq!(out.frame_id, 4, "only the newest is worth showing");
        assert_eq!(decoder.fed, 4, "but every frame fed the reference chain");
        assert_eq!(session.superseded(), 3);
    }

    /// The mode that promises every frame is seen must get every frame, in
    /// order, however deep the backlog.
    #[tokio::test]
    async fn a_never_drop_mode_returns_every_frame_in_order() {
        let (mut session, _tx) = session_with_frames(4);
        session.set_pacing(crate::PacingMode::Smoothest);
        let mut decoder = CountingDecoder { fed: 0 };
        for expected in 1..=4 {
            let out = session
                .recv_frame(&mut decoder)
                .await
                .expect("recv works")
                .expect("a frame");
            assert_eq!(out.frame_id, expected);
        }
        assert_eq!(session.superseded(), 0);
    }

    /// Balanced-with-FPS-limit is the other never-drop mode — the reference
    /// client's definition, and the parity bug this guards against.
    #[tokio::test]
    async fn the_limited_mode_also_never_drops() {
        let (mut session, _tx) = session_with_frames(3);
        session.set_pacing(crate::PacingMode::BalancedFpsLimit);
        let mut decoder = CountingDecoder { fed: 0 };
        let out = session
            .recv_frame(&mut decoder)
            .await
            .expect("recv works")
            .expect("a frame");
        assert_eq!(out.frame_id, 1, "the oldest, not the newest");
        assert_eq!(session.superseded(), 0);
    }
}

#[cfg(test)]
mod interval_tests {
    use gsa_client_backend_api::{CaptureClock, RecoverySink};

    #[derive(Debug)]
    struct NoRecovery;
    impl RecoverySink for NoRecovery {
        fn request_keyframe(&self) {}
    }

    fn session() -> super::StreamSession {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        super::StreamSession::with_capture_clock(
            rx,
            std::sync::Arc::new(NoRecovery),
            gsa_core::time::MediaClock::new(),
            crate::ClockSync::default(),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            CaptureClock::StreamPts,
        )
    }

    /// The failure this guards: change-driven encoding makes a still screen
    /// look like a slow frame rate, and an averaged interval then inflates
    /// the one-frame hold budget into tens of milliseconds of real latency.
    /// Idle gaps land in the upper tail; the learned cadence must not move.
    #[test]
    fn idle_gaps_do_not_inflate_the_learned_interval() {
        let mut s = session();
        let mut ts = 1_000_000u32;
        // A 24 fps film with a still shot every second or so: nine real
        // frames, then a 180 ms pause, repeated.
        for _ in 0..12 {
            for _ in 0..9 {
                ts = ts.wrapping_add(41_667);
                let _ = s.learn_frame_interval(ts);
            }
            ts = ts.wrapping_add(180_000);
            let _ = s.learn_frame_interval(ts);
        }
        let learned = s.frame_interval_us();
        assert!(
            (40_000..=44_000).contains(&learned),
            "24 fps with stills must still learn ~41.7 ms, got {learned}"
        );
    }

    /// A steady stream still learns its actual rate — the percentile must not
    /// bias a clean cadence downward either.
    #[test]
    fn a_steady_cadence_is_learned_exactly() {
        let mut s = session();
        let mut ts = 1_000_000u32;
        for _ in 0..40 {
            ts = ts.wrapping_add(8_333);
            let _ = s.learn_frame_interval(ts);
        }
        let learned = s.frame_interval_us();
        assert!(
            (8_000..=8_700).contains(&learned),
            "120 fps must learn ~8.3 ms, got {learned}"
        );
    }
}
