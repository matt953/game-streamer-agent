//! Congestion-reactive bitrate controller (spec 04 ABR).
//!
//! Driven by the agent's own QUIC path signals — RTT and packet loss — sampled
//! on a fixed tick, so it reacts even when the client's feedback stalls (a
//! slideshow no longer starves it). Loss triggers an immediate hard backoff; a
//! rising RTT above the path's baseline (queue building) triggers a softer one;
//! a clear, loss-free path is probed upward toward the ceiling, held down for a
//! while after each cut so it doesn't charge straight back into the wall.

/// RTT above baseline that counts as a filling queue (µs).
const OVERUSE_HIGH_US: f64 = 40_000.0;
/// RTT above baseline below which the path is considered clear (µs).
const OVERUSE_LOW_US: f64 = 15_000.0;
/// Loss fraction above which we cut hard, and below which it's safe to probe up.
const LOSS_HIGH: f64 = 0.02;
const LOSS_LOW: f64 = 0.005;
/// Multiplicative steps: hard cut on loss, softer on delay, gentle probe up.
const DECREASE_LOSS: f64 = 0.7;
const DECREASE_DELAY: f64 = 0.85;
const INCREASE: f64 = 1.05;
/// Minimum gap between cuts.
const DECREASE_COOLDOWN_US: u64 = 250_000;
/// No probing up for this long after a cut.
const HOLD_DOWN_US: u64 = 3_000_000;
/// Probe up at most this often once clear.
const PROBE_INTERVAL_US: u64 = 1_000_000;
/// Baseline (propagation RTT) upward decay per sample — slow, so sustained
/// congestion isn't "forgotten" and the controller keeps reacting to it.
const BASELINE_DECAY: f64 = 0.002;

#[derive(Debug)]
pub struct AbrController {
    ceiling_bps: u32,
    floor_bps: u32,
    target_bps: u32,
    /// Estimated propagation RTT (µs): drops to any new minimum immediately,
    /// creeps up only slowly — a decaying minimum, not a mean.
    baseline_rtt_us: f64,
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
            baseline_rtt_us: 0.0,
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

    /// Set the quality cap (the manual bitrate); ABR never exceeds it.
    pub fn set_ceiling(&mut self, ceiling_bps: u32) {
        self.ceiling_bps = ceiling_bps;
        self.target_bps = self.target_bps.min(ceiling_bps);
    }

    /// Re-seat the target (e.g. when ABR is toggled on, start where we are).
    pub fn sync_target(&mut self, target_bps: u32) {
        self.target_bps = target_bps.clamp(self.floor_bps, self.ceiling_bps);
    }

    /// One control step from the agent's path signals (`rtt_us`, `loss` fraction
    /// over the last interval). Returns the new target bitrate.
    pub fn on_sample(&mut self, rtt_us: u32, loss: f64, now_us: u64) -> u32 {
        let rtt = f64::from(rtt_us);
        if !self.initialized {
            self.baseline_rtt_us = rtt;
            self.initialized = true;
            return self.target_bps;
        }
        // Decaying-minimum baseline: follows a drop immediately, rises slowly.
        if rtt < self.baseline_rtt_us {
            self.baseline_rtt_us = rtt;
        } else {
            self.baseline_rtt_us += BASELINE_DECAY * (rtt - self.baseline_rtt_us);
        }
        let overuse = rtt - self.baseline_rtt_us;
        let since_decrease = now_us.saturating_sub(self.last_decrease_us);

        if loss > LOSS_HIGH {
            // Loss is unambiguous congestion — cut hard, immediately.
            self.decrease(DECREASE_LOSS);
            self.last_decrease_us = now_us;
        } else if overuse > OVERUSE_HIGH_US {
            if since_decrease > DECREASE_COOLDOWN_US {
                self.decrease(DECREASE_DELAY);
                self.last_decrease_us = now_us;
            }
        } else if overuse < OVERUSE_LOW_US
            && loss < LOSS_LOW
            && since_decrease > HOLD_DOWN_US
            && now_us.saturating_sub(self.last_probe_us) > PROBE_INTERVAL_US
        {
            self.increase(INCREASE);
            self.last_probe_us = now_us;
        }
        self.target_bps
    }

    fn decrease(&mut self, factor: f64) {
        self.target_bps = ((f64::from(self.target_bps) * factor) as u32).max(self.floor_bps);
    }

    fn increase(&mut self, factor: f64) {
        self.target_bps = ((f64::from(self.target_bps) * factor) as u32).min(self.ceiling_bps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    /// Feed samples until `t`, stepping 250 ms, returning the final target.
    fn run(abr: &mut AbrController, rtt_us: u32, loss: f64, from_ms: u64, count: u64) -> u64 {
        let mut t = from_ms;
        for _ in 0..count {
            abr.on_sample(rtt_us, loss, t * MS);
            t += 250;
        }
        t
    }

    #[test]
    fn loss_cuts_hard_and_immediately() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.on_sample(20_000, 0.0, 0); // init baseline
        abr.on_sample(20_000, 0.10, 250 * MS); // 10% loss
        assert!(abr.target_bps() < 20_000_000, "target {}", abr.target_bps());
    }

    #[test]
    fn rising_rtt_backs_off() {
        let mut abr = AbrController::new(20_000_000, 0);
        let t = run(&mut abr, 20_000, 0.0, 0, 5); // baseline ~20 ms
        run(&mut abr, 120_000, 0.0, t, 5); // +100 ms queue
        assert!(abr.target_bps() < 20_000_000);
    }

    #[test]
    fn sustained_congestion_is_not_forgotten() {
        let mut abr = AbrController::new(20_000_000, 0);
        let t = run(&mut abr, 20_000, 0.0, 0, 5);
        // Hold a high RTT for ~30 s: the baseline must not creep up enough to
        // stop the backoffs — target should fall to the floor.
        run(&mut abr, 200_000, 0.0, t, 120);
        assert_eq!(abr.target_bps(), 500_000, "target {}", abr.target_bps());
    }

    #[test]
    fn probes_up_when_clear_but_holds_down_after_a_cut() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.sync_target(5_000_000);
        // A cut, then immediately clear: must NOT probe up during hold-down (3 s).
        abr.on_sample(20_000, 0.0, 0);
        abr.on_sample(20_000, 0.10, 250 * MS); // cut at 0.25 s
        let after_cut = abr.target_bps();
        let t = run(&mut abr, 15_000, 0.0, 500, 8); // clear, but within 3 s
        assert_eq!(abr.target_bps(), after_cut, "probed up during hold-down");
        // Well past hold-down + probe interval → climbs, capped at the ceiling.
        run(&mut abr, 15_000, 0.0, t, 200);
        assert!(
            abr.target_bps() > after_cut,
            "should probe up after hold-down"
        );
        assert!(abr.target_bps() <= 20_000_000, "must not exceed ceiling");
    }

    #[test]
    fn ceiling_clamps_the_target() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.set_ceiling(3_000_000);
        assert!(abr.target_bps() <= 3_000_000);
    }
}
