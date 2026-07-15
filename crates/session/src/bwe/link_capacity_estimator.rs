use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tracing::{debug, info, trace, warn};

use crate::bwe::prelude::Bitrate;

/// Smoothed link-capacity memory fed by probe results and by the delivered
/// rate observed at overuse. Probes move it fast (they are direct capacity
/// measurements); overuse samples move it slowly. A running deviation gives
/// confidence bounds so a single optimistic sample cannot own the memory.
///
/// Estimates decay over time (default 60s) since network conditions change
/// and old measurements become less reliable.
#[derive(Default)]
pub struct LinkCapacityEstimator {
    /// Smoothed capacity estimate (kbps), if any samples arrived yet.
    estimate_kbps: Option<f64>,

    /// Normalized variance of samples around the estimate (kbps units).
    deviation_kbps: f64,

    /// Time of the last accepted sample, for decay tracking.
    last_estimate_time: Option<Instant>,
}

impl LinkCapacityEstimator {
    /// Default duration before capacity estimate resets (60 seconds)
    const DEFAULT_RESET_WINDOW: Duration = Duration::from_secs(60);

    /// Sample weight for probe results.
    const PROBE_ALPHA: f64 = 0.5;

    /// Sample weight for delivered rate observed at overuse.
    const OVERUSE_ALPHA: f64 = 0.05;

    /// Bounds keeping the normalized variance sane.
    const MIN_DEVIATION: f64 = 0.4;
    const MAX_DEVIATION: f64 = 2.5;

    /// Create a new LinkCapacityEstimator with default settings
    pub fn new() -> Self {
        Self {
            deviation_kbps: Self::MAX_DEVIATION,
            ..Self::default()
        }
    }

    /// Fold a successful probe result into the capacity memory.
    pub fn update_from_probe(&mut self, probe_estimate: Bitrate, now: Instant) {
        if !probe_estimate.is_valid() {
            return;
        }
        self.update(probe_estimate, Self::PROBE_ALPHA, now);
        trace!(
            "Link capacity estimate now {:?} kbps after probe",
            self.estimate_kbps
        );
    }

    /// Fold the delivered rate observed while overusing into the memory:
    /// congestion means delivery ran at (roughly) capacity.
    pub fn update_from_overuse(&mut self, acked: Bitrate, now: Instant) {
        if !acked.is_valid() {
            return;
        }
        self.update(acked, Self::OVERUSE_ALPHA, now);
    }

    fn update(&mut self, sample: Bitrate, alpha: f64, now: Instant) {
        let sample_kbps = sample.as_f64() / 1000.0;
        let estimate = match self.estimate_kbps {
            None => sample_kbps,
            Some(e) => (1.0 - alpha) * e + alpha * sample_kbps,
        };
        let norm = estimate.max(1.0);
        let error = estimate - sample_kbps;
        self.deviation_kbps = ((1.0 - alpha) * self.deviation_kbps + alpha * error * error / norm)
            .clamp(Self::MIN_DEVIATION, Self::MAX_DEVIATION);
        self.estimate_kbps = Some(estimate);
        self.last_estimate_time = Some(now);
    }

    fn live_estimate_kbps(&self, now: Instant) -> Option<f64> {
        let estimate = self.estimate_kbps?;
        let last_time = self.last_estimate_time?;
        if now.saturating_duration_since(last_time) > Self::DEFAULT_RESET_WINDOW {
            trace!("Link capacity estimate expired");
            return None;
        }
        Some(estimate)
    }

    fn std_dev_kbps(&self, estimate_kbps: f64) -> f64 {
        (self.deviation_kbps * estimate_kbps).sqrt()
    }

    /// Smoothed capacity estimate, if available and not expired.
    pub fn capacity_estimate(&self, now: Instant) -> Option<Bitrate> {
        Some(Bitrate::kbps(self.live_estimate_kbps(now)?.max(0.0) as u64))
    }

    /// Conservative capacity (estimate − 3σ): what the link almost surely
    /// still carries. Feeds the crush floor.
    pub fn lower_bound(&self, now: Instant) -> Option<Bitrate> {
        let e = self.live_estimate_kbps(now)?;
        Some(Bitrate::kbps(
            (e - 3.0 * self.std_dev_kbps(e)).max(0.0) as u64
        ))
    }

    /// Optimistic capacity (estimate + 3σ): anything above this is an
    /// outlier sample, not the link.
    pub fn upper_bound(&self, now: Instant) -> Option<Bitrate> {
        let e = self.live_estimate_kbps(now)?;
        Some(Bitrate::kbps(
            (e + 3.0 * self.std_dev_kbps(e)).max(0.0) as u64
        ))
    }

    /// Reset the capacity estimate.
    pub fn reset(&mut self) {
        if self.estimate_kbps.is_some() {
            trace!("Link capacity estimate reset");
        }
        self.estimate_kbps = None;
        self.deviation_kbps = Self::MAX_DEVIATION;
        self.last_estimate_time = None;
    }

    /// Check if we currently have a valid capacity estimate
    #[cfg(test)]
    pub fn has_estimate(&self) -> bool {
        self.estimate_kbps.is_some() && self.last_estimate_time.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_no_estimate() {
        let estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        assert_eq!(estimator.capacity_estimate(now), None);
        assert!(!estimator.has_estimate());
    }

    #[test]
    fn stores_probe_result() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();
        let probe_result = Bitrate::mbps(10);

        estimator.update_from_probe(probe_result, now);

        assert_eq!(estimator.capacity_estimate(now), Some(probe_result));
        assert!(estimator.has_estimate());
    }

    #[test]
    fn smooths_probe_samples_instead_of_taking_the_max() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        estimator.update_from_probe(Bitrate::mbps(20), now);
        estimator.update_from_probe(Bitrate::mbps(30), now);

        // One optimistic sample pulls the estimate up but does not own it.
        let est = estimator.capacity_estimate(now).unwrap();
        assert!(est > Bitrate::mbps(20) && est < Bitrate::mbps(30));
    }

    #[test]
    fn overuse_samples_move_the_estimate_slowly() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        estimator.update_from_probe(Bitrate::mbps(20), now);
        estimator.update_from_overuse(Bitrate::mbps(10), now);

        // alpha 0.05: one overuse sample barely dents the memory.
        let est = estimator.capacity_estimate(now).unwrap();
        assert!(est > Bitrate::mbps(19));
    }

    #[test]
    fn bounds_straddle_the_estimate() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        estimator.update_from_probe(Bitrate::mbps(20), now);

        let est = estimator.capacity_estimate(now).unwrap();
        assert!(estimator.lower_bound(now).unwrap() < est);
        assert!(estimator.upper_bound(now).unwrap() > est);
    }

    #[test]
    fn estimate_expires_after_reset_window() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        estimator.update_from_probe(Bitrate::mbps(10), now);
        assert_eq!(estimator.capacity_estimate(now), Some(Bitrate::mbps(10)));

        // Check just before expiration
        let almost_expired = now + Duration::from_secs(59);
        assert_eq!(
            estimator.capacity_estimate(almost_expired),
            Some(Bitrate::mbps(10))
        );

        // Check after expiration
        let expired = now + Duration::from_secs(61);
        assert_eq!(estimator.capacity_estimate(expired), None);
    }

    #[test]
    fn reset_clears_estimate() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        estimator.update_from_probe(Bitrate::mbps(10), now);
        assert!(estimator.has_estimate());

        estimator.reset();

        assert!(!estimator.has_estimate());
        assert_eq!(estimator.capacity_estimate(now), None);
    }

    #[test]
    fn ignores_invalid_probes() {
        let mut estimator = LinkCapacityEstimator::new();
        let now = Instant::now();

        // Set a valid estimate first
        estimator.update_from_probe(Bitrate::mbps(10), now);

        // Try to update with invalid bitrate
        estimator.update_from_probe(Bitrate::NEG_INFINITY, now);

        // Should keep the valid estimate
        assert_eq!(estimator.capacity_estimate(now), Some(Bitrate::mbps(10)));
    }
}
