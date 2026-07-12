//! Delay-gradient bitrate controller (spec 04 ABR).
//!
//! Watches the trend of the client-reported one-way delay: rising delay means a
//! queue is building on the path, so back off *before* loss; stable/low delay
//! means there's room to probe up. The output target is bounded by a ceiling
//! (the client's requested quality cap) and a floor.

/// Delay above the baseline (µs) that counts as a filling queue → back off.
const OVERUSE_HIGH_US: f64 = 30_000.0;
/// Delay above the baseline (µs) below which the path is considered clear.
const OVERUSE_LOW_US: f64 = 10_000.0;
/// Minimum gap between decreases — react fast but not on every sample.
const DECREASE_COOLDOWN_US: u64 = 300_000;
/// Stable-and-clear duration required before probing the rate up.
const PROBE_INTERVAL_US: u64 = 2_000_000;
/// Multiplicative step down on congestion (−15%) and up when clear (+8%).
const DECREASE_FACTOR: f64 = 0.85;
const INCREASE_FACTOR: f64 = 1.08;

#[derive(Debug)]
pub struct AbrController {
    ceiling_bps: u32,
    floor_bps: u32,
    target_bps: u32,
    baseline_us: f64,
    fast_us: f64,
    last_change_us: u64,
    stable_since_us: u64,
    initialized: bool,
}

impl AbrController {
    #[must_use]
    pub fn new(ceiling_bps: u32, now_us: u64) -> Self {
        Self {
            ceiling_bps,
            floor_bps: 500_000,
            target_bps: ceiling_bps,
            baseline_us: 0.0,
            fast_us: 0.0,
            last_change_us: now_us,
            stable_since_us: now_us,
            initialized: false,
        }
    }

    #[must_use]
    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// Set the quality cap (the manual bitrate); ABR never exceeds it.
    pub fn set_ceiling(&mut self, ceiling_bps: u32) {
        self.ceiling_bps = ceiling_bps;
        self.target_bps = self.target_bps.min(ceiling_bps);
    }

    /// Re-seat the target (e.g. when ABR is toggled on, start from where the
    /// stream currently is).
    pub fn sync_target(&mut self, target_bps: u32) {
        self.target_bps = target_bps.clamp(self.floor_bps, self.ceiling_bps);
    }

    /// Feed one client-reported one-way-delay sample; returns the new target.
    pub fn on_delay(&mut self, delay_us: u32, now_us: u64) -> u32 {
        let d = f64::from(delay_us);
        if !self.initialized {
            self.baseline_us = d;
            self.fast_us = d;
            self.initialized = true;
            return self.target_bps;
        }
        // Fast delay estimate; baseline follows the quiescent (low) delay —
        // drops with it immediately, rises only slowly.
        self.fast_us = 0.6 * self.fast_us + 0.4 * d;
        if self.fast_us < self.baseline_us {
            self.baseline_us = self.fast_us;
        } else {
            self.baseline_us += 0.01 * (self.fast_us - self.baseline_us);
        }
        let overuse = self.fast_us - self.baseline_us;
        let since_change = now_us.saturating_sub(self.last_change_us);

        if overuse > OVERUSE_HIGH_US {
            if since_change > DECREASE_COOLDOWN_US {
                self.target_bps =
                    ((f64::from(self.target_bps) * DECREASE_FACTOR) as u32).max(self.floor_bps);
                self.last_change_us = now_us;
            }
            self.stable_since_us = now_us;
        } else if overuse < OVERUSE_LOW_US {
            if now_us.saturating_sub(self.stable_since_us) > PROBE_INTERVAL_US
                && since_change > PROBE_INTERVAL_US
            {
                self.target_bps =
                    ((f64::from(self.target_bps) * INCREASE_FACTOR) as u32).min(self.ceiling_bps);
                self.last_change_us = now_us;
            }
        } else {
            // Neutral band: not clear enough to probe up.
            self.stable_since_us = now_us;
        }
        self.target_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_when_delay_climbs() {
        let mut abr = AbrController::new(20_000_000, 0);
        // Settle at a low baseline.
        for i in 0..5 {
            abr.on_delay(20_000, i * 100_000);
        }
        // Delay climbs well above baseline → target must fall below the ceiling.
        let mut t = now_ms(1000);
        for _ in 0..10 {
            abr.on_delay(120_000, t);
            t += 100_000;
        }
        assert!(abr.target_bps() < 20_000_000, "target {}", abr.target_bps());
    }

    #[test]
    fn probes_up_when_clear_but_never_past_ceiling() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.sync_target(5_000_000); // start below the ceiling
        let mut t = 0u64;
        for _ in 0..200 {
            abr.on_delay(15_000, t); // low, stable delay
            t += 100_000;
        }
        assert!(abr.target_bps() > 5_000_000, "should probe up");
        assert!(abr.target_bps() <= 20_000_000, "must not exceed ceiling");
    }

    #[test]
    fn ceiling_clamps_the_target() {
        let mut abr = AbrController::new(20_000_000, 0);
        abr.set_ceiling(3_000_000);
        assert!(abr.target_bps() <= 3_000_000);
    }

    fn now_ms(ms: u64) -> u64 {
        ms * 1000
    }
}
