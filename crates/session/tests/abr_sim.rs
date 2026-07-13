//! Deterministic congestion simulator for the ABR controller (spec 04).
//!
//! Drives the real controller against a fluid bottleneck link (capacity,
//! propagation, finite buffer, random loss + jitter) and asserts on utilization
//! and queueing delay across scenarios and seeds — the local, reproducible
//! stand-in for the netem CI loop.

use gsa_session::abr::{AbrController, Sample};

/// Deterministic xorshift64* — no external RNG, fully reproducible per seed.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64) / (1u64 << 53) as f64
    }
    /// Symmetric jitter in ±`spread`.
    fn jitter(&mut self, spread: f64) -> f64 {
        (self.unit() * 2.0 - 1.0) * spread
    }
}

/// A capacity segment: `capacity_bps` holds until `until_s`.
#[derive(Clone, Copy)]
struct Segment {
    until_s: f64,
    capacity_bps: f64,
}

struct Scenario {
    name: &'static str,
    /// Piecewise-constant link capacity over time.
    capacity: &'static [Segment],
    /// One-way propagation (µs).
    prop_us: f64,
    /// Bottleneck buffer (bytes) — bufferbloat depth before tail-drop.
    buffer_bytes: f64,
    /// Random (non-congestive) loss fraction, e.g. Wi-Fi interference.
    random_loss: f64,
    /// One-way delay jitter amplitude (µs).
    jitter_us: f64,
    ceiling_bps: u32,
    start_bps: u32,
    duration_s: f64,
}

impl Scenario {
    fn capacity_at(&self, t: f64) -> f64 {
        self.capacity
            .iter()
            .find(|seg| t < seg.until_s)
            .map_or(self.capacity.last().unwrap().capacity_bps, |seg| {
                seg.capacity_bps
            })
    }
}

struct SimStats {
    /// Mean of (delivered / capacity) over the run, after a warmup.
    mean_utilization: f64,
    /// 95th-percentile one-way queueing delay (ms), after a warmup.
    p95_queue_ms: f64,
    /// Lowest target the controller ever held (bps) — collapse detector.
    min_target_bps: u32,
    /// Mean received goodput (Mb/s).
    mean_recv_mbps: f64,
}

/// Run the real controller against the modelled link. `dt` steps the fluid
/// link; the controller ticks every 250 ms as in production.
fn simulate(sc: &Scenario, seed: u64) -> SimStats {
    const DT: f64 = 0.005; // 5 ms fluid step
    const TICK: f64 = 0.25; // controller cadence
    let warmup_s = 3.0; // ignore the initial ramp in the aggregates

    let mut abr = AbrController::new(sc.ceiling_bps, 0);
    abr.sync_target(sc.start_bps);
    let mut rng = Rng::new(seed);

    let mut target = f64::from(sc.start_bps);
    let mut queue_bytes = 0.0f64;

    // Per-tick accumulators.
    let mut acc_delivered = 0.0;
    let mut acc_offered = 0.0;
    let mut acc_dropped = 0.0;

    let mut util_samples: Vec<f64> = Vec::new();
    let mut delay_samples: Vec<f64> = Vec::new();
    let mut recv_samples: Vec<f64> = Vec::new();
    let mut min_target = sc.start_bps;

    let mut t = 0.0f64;
    let mut next_tick = TICK;
    while t < sc.duration_s {
        let cap_bps = sc.capacity_at(t);
        let cap_bytes_per_s = cap_bps / 8.0;

        // Encoder offers `target` bits over dt; a fraction is lost to random
        // (non-congestive) loss before it even reaches the buffer.
        let offered = target / 8.0 * DT;
        let random_lost = offered * sc.random_loss;
        queue_bytes += offered - random_lost;

        // Tail-drop: anything over the buffer is lost.
        let overflow = (queue_bytes - sc.buffer_bytes).max(0.0);
        queue_bytes -= overflow;

        // Drain at link capacity.
        let drained = queue_bytes.min(cap_bytes_per_s * DT);
        queue_bytes -= drained;

        acc_delivered += drained;
        acc_offered += offered;
        acc_dropped += random_lost + overflow;

        if t >= warmup_s {
            util_samples.push(drained / (cap_bytes_per_s * DT).max(1.0));
            delay_samples.push(queue_bytes / cap_bytes_per_s * 1000.0); // ms
        }

        t += DT;
        if t >= next_tick {
            next_tick += TICK;
            let recv_bps = acc_delivered * 8.0 / TICK;
            let loss = if acc_offered > 0.0 {
                acc_dropped / acc_offered
            } else {
                0.0
            };
            // Queue sits on the forward path, so it inflates the round trip.
            let queue_delay_us = queue_bytes / cap_bytes_per_s * 1_000_000.0;
            let rtt_us = (2.0 * sc.prop_us + queue_delay_us + rng.jitter(sc.jitter_us)).max(1.0);

            let (new_target, _) = abr.on_sample(Sample {
                rtt_us: rtt_us as u32,
                loss,
                // The encoder fills the target (demanding content), so the
                // delivered rate is a valid estimate (not app-limited).
                estimate_bps: Some(recv_bps as u32),
                now_us: (t * 1_000_000.0) as u64,
            });
            target = f64::from(new_target);
            min_target = min_target.min(new_target);
            if t >= warmup_s {
                recv_samples.push(recv_bps / 1_000_000.0);
            }

            acc_delivered = 0.0;
            acc_offered = 0.0;
            acc_dropped = 0.0;
        }
    }

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let p95 = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[(v.len() as f64 * 0.95) as usize % v.len().max(1)]
    };
    SimStats {
        mean_utilization: mean(&util_samples),
        p95_queue_ms: p95(&mut delay_samples),
        min_target_bps: min_target,
        mean_recv_mbps: mean(&recv_samples),
    }
}

/// Assert an outcome holds across a spread of seeds (loss/jitter patterns), so
/// a single lucky run can't pass. Prints a table on failure.
fn assert_across_seeds(sc: &Scenario, min_util: f64, max_p95_ms: f64, min_target_floor_bps: u32) {
    let mut failures = Vec::new();
    let (mut worst_util, mut worst_p95, mut worst_target) = (1.0f64, 0.0f64, u32::MAX);
    for seed in 1..=20u64 {
        let st = simulate(sc, seed);
        worst_util = worst_util.min(st.mean_utilization);
        worst_p95 = worst_p95.max(st.p95_queue_ms);
        worst_target = worst_target.min(st.min_target_bps);
        let ok = st.mean_utilization >= min_util
            && st.p95_queue_ms <= max_p95_ms
            && st.min_target_bps >= min_target_floor_bps;
        if !ok {
            failures.push(format!(
                "  seed {seed:2}: util {:.2} (≥{min_util:.2}), p95 queue {:.0} ms (≤{max_p95_ms:.0}), min target {:.2} Mb/s (≥{:.2})",
                st.mean_utilization,
                st.p95_queue_ms,
                f64::from(st.min_target_bps) / 1e6,
                f64::from(min_target_floor_bps) / 1e6,
            ));
        }
    }
    eprintln!(
        "[{}] worst over 20 seeds: util {:.2}, p95 queue {:.0} ms, min target {:.2} Mb/s",
        sc.name,
        worst_util,
        worst_p95,
        f64::from(worst_target) / 1e6
    );
    assert!(
        failures.is_empty(),
        "[{}] {}/20 seeds failed:\n{}",
        sc.name,
        failures.len(),
        failures.join("\n")
    );
}

/// A steady bottleneck well under the ceiling: the controller must use most of
/// the link and keep the queue bounded, on every seed.
#[test]
fn steady_bottleneck_is_well_utilized_and_low_latency() {
    let sc = Scenario {
        name: "steady 5 Mb/s bottleneck",
        capacity: &[Segment {
            until_s: f64::INFINITY,
            capacity_bps: 5_000_000.0,
        }],
        prop_us: 30_000.0,
        buffer_bytes: 60_000.0, // ~100 ms at 5 Mb/s
        random_loss: 0.005,
        jitter_us: 8_000.0,
        ceiling_bps: 20_000_000,
        start_bps: 2_000_000,
        duration_s: 30.0,
    };
    assert_across_seeds(&sc, 0.85, 130.0, 1_500_000);
}

/// The link capacity halves mid-stream: the controller must follow it down,
/// draining the queue rather than sitting in a bloated standing queue.
#[test]
fn capacity_drop_is_tracked_without_a_standing_queue() {
    let sc = Scenario {
        name: "6→2 Mb/s capacity drop",
        capacity: &[
            Segment {
                until_s: 12.0,
                capacity_bps: 6_000_000.0,
            },
            Segment {
                until_s: f64::INFINITY,
                capacity_bps: 2_000_000.0,
            },
        ],
        prop_us: 25_000.0,
        buffer_bytes: 50_000.0,
        random_loss: 0.005,
        jitter_us: 8_000.0,
        ceiling_bps: 20_000_000,
        start_bps: 2_000_000,
        duration_s: 30.0,
    };
    // Utilization is averaged across the high and low phases; the key is a
    // bounded queue (no bufferbloat) and no collapse to the floor.
    assert_across_seeds(&sc, 0.80, 250.0, 1_200_000);
}

/// A clean, fat link: the controller must climb close to the ceiling and stay
/// there — probing must not stall.
#[test]
fn fat_clean_link_climbs_to_the_ceiling() {
    let sc = Scenario {
        name: "50 Mb/s clean link, 25 ceiling",
        capacity: &[Segment {
            until_s: f64::INFINITY,
            capacity_bps: 50_000_000.0,
        }],
        prop_us: 10_000.0,
        buffer_bytes: 200_000.0,
        random_loss: 0.001,
        jitter_us: 3_000.0,
        ceiling_bps: 25_000_000,
        start_bps: 2_000_000,
        duration_s: 30.0,
    };
    // The ceiling (25) is the binding limit, so utilization of the 50 Mb/s link
    // is ~0.5 by design; assert the target itself reached near the ceiling.
    for seed in 1..=20u64 {
        let st = simulate(&sc, seed);
        assert!(
            st.mean_recv_mbps >= 20.0,
            "seed {seed}: recv {:.1} Mb/s should approach the 25 ceiling",
            st.mean_recv_mbps
        );
    }
}

/// Lossy cellular-style link (2% random loss, high jitter) under a bottleneck:
/// must not collapse or bloat despite the noise.
#[test]
fn lossy_jittery_link_does_not_collapse() {
    let sc = Scenario {
        name: "4 Mb/s, 2% loss, 40 ms jitter",
        capacity: &[Segment {
            until_s: f64::INFINITY,
            capacity_bps: 4_000_000.0,
        }],
        prop_us: 40_000.0,
        buffer_bytes: 60_000.0,
        random_loss: 0.02,
        jitter_us: 40_000.0,
        ceiling_bps: 20_000_000,
        start_bps: 2_000_000,
        duration_s: 30.0,
    };
    assert_across_seeds(&sc, 0.80, 160.0, 1_000_000);
}
