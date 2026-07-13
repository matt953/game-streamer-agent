//! Congestion-reactive bitrate controller (spec 04 ABR).
//!
//! Sender-side, ticked every 250 ms. Aims the target at [`ESTIMATE_HEADROOM`] ×
//! the receiver's smoothed delivered rate (the bandwidth ground truth — a queue
//! can't inflate what actually arrived, unlike `cwnd/rtt`), climbing one
//! [`INCREASE`] step per tick while a rate is being measured. Backs off on loss
//! over [`LOSS_HIGH`], or on a sustained rising RTT trend ([`Trendline`]).

/// Target cap = this × the smoothed delivered rate (headroom to probe for more).
const ESTIMATE_HEADROOM: f64 = 1.5;
/// Multiplicative step per probe toward the cap.
const INCREASE: f64 = 1.25;
/// EWMA weight on the delivered rate; the lag doubles as the estimate's hysteresis.
const ESTIMATE_ALPHA: f64 = 0.5;
/// Trendline window in samples (250 ms each ≈ 2 s).
const TREND_WINDOW: usize = 8;
/// Delay slope (µs added RTT per second) above which the queue is building → cut.
const OVERUSE_SLOPE_US_PER_S: f64 = 40_000.0;
/// Don't probe up while the standing queue (RTT above baseline) exceeds this.
const PROBE_MAX_DELAY_US: f64 = 60_000.0;
/// Consecutive over-use samples required before a delay cut (debounces spikes).
const OVERUSE_TICKS: u32 = 2;
/// Loss fraction above which we cut hard; below it, loss doesn't block probing.
const LOSS_HIGH: f64 = 0.10;
/// Multiplicative cuts: hard on loss, softer on delay.
const DECREASE_LOSS: f64 = 0.7;
const DECREASE_DELAY: f64 = 0.85;
/// Minimum gap between cuts.
const DECREASE_COOLDOWN_US: u64 = 250_000;
/// No ramping up for this long after a cut.
const HOLD_DOWN_US: u64 = 3_000_000;
/// Minimum gap between probes up.
const PROBE_INTERVAL_US: u64 = 250_000;
/// Upward decay of the propagation-RTT baseline per sample (slow).
const BASELINE_DECAY: f64 = 0.002;
/// EWMA weight on RTT before the trendline, so a lone spike is one bump not an outlier.
const TREND_SMOOTH_ALPHA: f64 = 0.4;

/// GCC-style delay-trend detector: least-squares slope of a smoothed RTT over a
/// short window. A sustained positive slope means the queue is growing.
#[derive(Debug)]
struct Trendline {
    /// (time seconds, smoothed rtt µs) over the last [`TREND_WINDOW`] samples.
    samples: std::collections::VecDeque<(f64, f64)>,
    smoothed_us: Option<f64>,
}

impl Trendline {
    fn new() -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(TREND_WINDOW),
            smoothed_us: None,
        }
    }

    fn push(&mut self, now_us: u64, rtt_us: f64) {
        let smoothed = match self.smoothed_us {
            Some(prev) => prev + TREND_SMOOTH_ALPHA * (rtt_us - prev),
            None => rtt_us,
        };
        self.smoothed_us = Some(smoothed);
        self.samples
            .push_back((now_us as f64 / 1_000_000.0, smoothed));
        while self.samples.len() > TREND_WINDOW {
            self.samples.pop_front();
        }
    }

    /// Whether the smoothed delay rose on the latest sample (queue still filling).
    fn rising(&self) -> bool {
        let n = self.samples.len();
        n >= 2 && self.samples[n - 1].1 > self.samples[n - 2].1
    }

    /// Slope of RTT vs time (µs per second). Zero until the window is at least
    /// half full, so a couple of early samples can't swing it.
    fn slope_us_per_s(&self) -> f64 {
        let n = self.samples.len();
        if n < TREND_WINDOW / 2 {
            return 0.0;
        }
        let nf = n as f64;
        let (mut sum_t, mut sum_d) = (0.0, 0.0);
        for &(t, d) in &self.samples {
            sum_t += t;
            sum_d += d;
        }
        let (mean_t, mean_d) = (sum_t / nf, sum_d / nf);
        let (mut num, mut den) = (0.0, 0.0);
        for &(t, d) in &self.samples {
            num += (t - mean_t) * (d - mean_d);
            den += (t - mean_t) * (t - mean_t);
        }
        if den <= 0.0 { 0.0 } else { num / den }
    }
}

/// One tick's path signals. `estimate_bps` is the receiver-reported delivered
/// rate, `None` when stale or app-limited.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub rtt_us: u32,
    pub loss: f64,
    pub estimate_bps: Option<u32>,
    pub now_us: u64,
}

/// What the last sample did to the target (logged by the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    CutLoss,
    CutDelay,
    Ramp,
    ClampedToEstimate,
}

#[derive(Debug)]
pub struct AbrController {
    ceiling_bps: u32,
    floor_bps: u32,
    target_bps: u32,
    /// Smoothed delivered rate (bps); survives app-limited spells.
    estimate_ewma_bps: Option<f64>,
    /// Estimated propagation RTT (µs): drops to any new minimum immediately,
    /// creeps up only slowly — a decaying minimum, not a mean.
    baseline_rtt_us: f64,
    /// Delay-trend detector (queue building vs transient jitter).
    trend: Trendline,
    /// Consecutive samples the trend has been in over-use (debounces spikes).
    overuse_ticks: u32,
    initialized: bool,
    last_decrease_us: u64,
    last_probe_us: u64,
}

impl AbrController {
    #[must_use]
    pub fn new(ceiling_bps: u32, now_us: u64) -> Self {
        Self {
            ceiling_bps,
            floor_bps: 500_000,
            target_bps: ceiling_bps,
            estimate_ewma_bps: None,
            baseline_rtt_us: 0.0,
            trend: Trendline::new(),
            overuse_ticks: 0,
            initialized: false,
            last_decrease_us: now_us,
            last_probe_us: now_us,
        }
    }

    #[must_use]
    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// The quality cap (the manual bitrate) — restored as the target when ABR
    /// is turned off.
    #[must_use]
    pub fn ceiling_bps(&self) -> u32 {
        self.ceiling_bps
    }

    /// Headroom-scaled smoothed delivered rate (bps) — the dynamic cap — if
    /// the stream has exercised the pipe enough to measure one.
    #[must_use]
    pub fn estimate_cap_bps(&self) -> Option<u32> {
        self.estimate_ewma_bps
            .map(|e| (e * ESTIMATE_HEADROOM) as u32)
    }

    /// The current delay trend (µs of added RTT per second) — the congestion
    /// signal, exposed for telemetry/logging.
    #[must_use]
    pub fn delay_slope_us_per_s(&self) -> f64 {
        self.trend.slope_us_per_s()
    }

    /// Set the quality cap (the manual bitrate); ABR never exceeds it.
    pub fn set_ceiling(&mut self, ceiling_bps: u32) {
        self.ceiling_bps = ceiling_bps;
        self.target_bps = self.target_bps.min(ceiling_bps);
    }

    /// Re-seat the target (e.g. when ABR is toggled on, start where we are).
    pub fn sync_target(&mut self, target_bps: u32) {
        self.target_bps = target_bps.clamp(self.floor_bps, self.ceiling_bps);
    }

    /// One control step. Returns the new target and what moved it.
    pub fn on_sample(&mut self, s: Sample) -> (u32, Decision) {
        let rtt = f64::from(s.rtt_us);
        if !self.initialized {
            self.baseline_rtt_us = rtt;
            self.initialized = true;
            return (self.target_bps, Decision::Hold);
        }
        // Decaying-minimum baseline: follows a drop immediately, rises slowly.
        if rtt < self.baseline_rtt_us {
            self.baseline_rtt_us = rtt;
        } else {
            self.baseline_rtt_us += BASELINE_DECAY * (rtt - self.baseline_rtt_us);
        }
        self.trend.push(s.now_us, rtt);

        // Refresh the smoothed delivered rate only from meaningful samples.
        if let Some(est) = s.estimate_bps {
            let est = f64::from(est);
            self.estimate_ewma_bps = Some(match self.estimate_ewma_bps {
                Some(ewma) => ewma + ESTIMATE_ALPHA * (est - ewma),
                None => est,
            });
        }
        let cap = self
            .estimate_cap_bps()
            .map_or(self.ceiling_bps, |e| e.min(self.ceiling_bps))
            .max(self.floor_bps);

        let slope = self.trend.slope_us_per_s();
        let overuse = rtt - self.baseline_rtt_us; // standing queue, gates the probe
        let since_decrease = s.now_us.saturating_sub(self.last_decrease_us);

        if slope > OVERUSE_SLOPE_US_PER_S && self.trend.rising() {
            self.overuse_ticks += 1;
        } else {
            self.overuse_ticks = 0;
        }

        let mut decision = Decision::Hold;
        if s.loss > LOSS_HIGH {
            self.decrease(DECREASE_LOSS);
            self.last_decrease_us = s.now_us;
            decision = Decision::CutLoss;
        } else if self.overuse_ticks >= OVERUSE_TICKS {
            if since_decrease > DECREASE_COOLDOWN_US {
                self.decrease(DECREASE_DELAY);
                self.last_decrease_us = s.now_us;
                decision = Decision::CutDelay;
            }
        } else if self.estimate_ewma_bps.is_some()
            && slope < 0.5 * OVERUSE_SLOPE_US_PER_S
            && overuse < PROBE_MAX_DELAY_US
            && since_decrease > HOLD_DOWN_US
            && s.now_us.saturating_sub(self.last_probe_us) > PROBE_INTERVAL_US
            && self.target_bps < cap
        {
            // Climb only while an estimate exists — app-limited content leaves
            // nothing to probe for.
            self.target_bps = ((f64::from(self.target_bps) * INCREASE) as u32)
                .min(cap)
                .max(self.floor_bps);
            self.last_probe_us = s.now_us;
            decision = Decision::Ramp;
        }
        if self.target_bps > cap {
            self.target_bps = cap;
            decision = Decision::ClampedToEstimate;
        }
        (self.target_bps, decision)
    }

    fn decrease(&mut self, factor: f64) {
        self.target_bps = ((f64::from(self.target_bps) * factor) as u32).max(self.floor_bps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    /// Feed identical samples, stepping 250 ms, returning the end time.
    fn run(
        abr: &mut AbrController,
        rtt_us: u32,
        loss: f64,
        est: Option<u32>,
        from_ms: u64,
        count: u64,
    ) -> u64 {
        let mut t = from_ms;
        for _ in 0..count {
            abr.on_sample(Sample {
                rtt_us,
                loss,
                estimate_bps: est,
                now_us: t * MS,
            });
            t += 250;
        }
        t
    }

    #[test]
    fn loss_cuts_hard_and_immediately() {
        let mut abr = AbrController::new(20_000_000, 0);
        run(&mut abr, 20_000, 0.0, None, 0, 1);
        let (target, d) = abr.on_sample(Sample {
            rtt_us: 20_000,
            loss: 0.15,
            estimate_bps: None,
            now_us: 250 * MS,
        });
        assert_eq!(d, Decision::CutLoss);
        assert!(target < 20_000_000, "target {target}");
    }

    #[test]
    fn rising_rtt_backs_off() {
        let mut abr = AbrController::new(20_000_000, 0);
        let t = run(&mut abr, 20_000, 0.0, None, 0, 5); // baseline ~20 ms
        run(&mut abr, 200_000, 0.0, None, t, 5); // +180 ms queue
        assert!(abr.target_bps() < 20_000_000);
    }

    #[test]
    fn sustained_build_cuts_toward_the_floor() {
        let mut abr = AbrController::new(20_000_000, 0);
        let mut t = run(&mut abr, 20_000, 0.0, None, 0, 5); // clear baseline
        // A queue that keeps growing (RTT climbing every sample) is real
        // congestion — the trend stays positive, so it keeps cutting.
        let mut rtt = 20_000u32;
        for _ in 0..60 {
            rtt += 15_000;
            abr.on_sample(Sample {
                rtt_us: rtt,
                loss: 0.0,
                estimate_bps: None,
                now_us: t * MS,
            });
            t += 250;
        }
        assert!(
            abr.target_bps() < 2_000_000,
            "sustained build should drive the target down, got {}",
            abr.target_bps()
        );
    }

    #[test]
    fn transient_spike_does_not_cut() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.sync_target(10_000_000);
        let mut t = run(&mut abr, 20_000, 0.0, None, 0, 6); // clear
        let before = abr.target_bps();
        // One lone 300 ms spike amid a clear path: the windowed trend stays
        // flat, so it must not cut.
        abr.on_sample(Sample {
            rtt_us: 300_000,
            loss: 0.0,
            estimate_bps: None,
            now_us: t * MS,
        });
        t += 250;
        run(&mut abr, 20_000, 0.0, None, t, 4); // back to clear
        assert!(
            abr.target_bps() >= before,
            "a lone spike must not cut: {} -> {}",
            before,
            abr.target_bps()
        );
    }

    #[test]
    fn ramps_when_clear_but_holds_down_after_a_cut() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.sync_target(5_000_000);
        // A 10 Mb/s delivered estimate throughout — the climb only happens when
        // the pipe is actually being measured.
        let est = Some(10_000_000);
        run(&mut abr, 20_000, 0.0, est, 0, 1);
        abr.on_sample(Sample {
            rtt_us: 20_000,
            loss: 0.15,
            estimate_bps: est,
            now_us: 250 * MS,
        });
        let after_cut = abr.target_bps();
        let t = run(&mut abr, 20_000, 0.0, est, 500, 8); // clear, within 3 s
        assert_eq!(abr.target_bps(), after_cut, "ramped during hold-down");
        run(&mut abr, 20_000, 0.0, est, t, 200);
        assert!(abr.target_bps() > after_cut, "should ramp after hold-down");
        assert!(abr.target_bps() <= 20_000_000, "must not exceed ceiling");
    }

    #[test]
    fn estimate_caps_the_ramp_and_a_shrinking_pipe_clamps() {
        let mut abr = AbrController::new(50_000_000, 0);
        abr.sync_target(2_000_000);
        // Clear path, 10 Mb/s delivered: ramp must stop at the headroom × it.
        let t = run(&mut abr, 20_000, 0.0, Some(10_000_000), 0, 60);
        assert_eq!(abr.target_bps(), 15_000_000, "target {}", abr.target_bps());
        // Delivery collapses to 4 Mb/s: once the smoothed estimate follows,
        // the target clamps to ~headroom × the new rate without a loss/delay cut.
        run(&mut abr, 20_000, 0.0, Some(4_000_000), t, 20);
        let target = abr.target_bps();
        assert!(
            (5_400_000..=6_600_000).contains(&target),
            "target {target} should track ~1.5 x 4 Mb/s"
        );
    }

    #[test]
    fn app_limited_samples_leave_the_cap_alone() {
        let mut abr = AbrController::new(50_000_000, 0);
        abr.sync_target(2_000_000);
        let t = run(&mut abr, 20_000, 0.0, Some(10_000_000), 0, 60);
        let capped = abr.target_bps();
        // A long app-limited spell (estimate None) must not release the cap.
        run(&mut abr, 20_000, 0.0, None, t, 60);
        assert_eq!(abr.target_bps(), capped, "cap released while app-limited");
    }

    #[test]
    fn oscillating_jitter_does_not_cut() {
        let mut abr = AbrController::new(20_000_000, 0);
        // Alternate 20/60 ms RTT — 5G-like jitter, no trend. The trendline
        // averages the swing to ~zero slope, so the noisy-but-clear path must
        // not trigger a cut.
        let mut t = 0u64;
        for i in 0..40 {
            let rtt = if i % 2 == 0 { 20_000 } else { 60_000 };
            abr.on_sample(Sample {
                rtt_us: rtt,
                loss: 0.0,
                estimate_bps: None,
                now_us: t * MS,
            });
            t += 250;
        }
        assert_eq!(abr.target_bps(), 20_000_000, "cut on pure jitter");
    }

    #[test]
    fn ceiling_clamps_the_target() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.set_ceiling(3_000_000);
        assert!(abr.target_bps() <= 3_000_000);
    }
}
