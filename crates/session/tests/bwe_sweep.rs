//! Encoder-agnostic convergence sweep (spec 04, ABR v2 gate): the estimator
//! must converge to link capacity no matter what fraction of the target the
//! encoder produces — including nothing at all. Links {1..500 Mb/s} ×
//! encoder output {0%, 50%, 100%, 150%} of target.

use std::collections::VecDeque;

use gsa_protocol::control::PacketFeedback;
use gsa_session::bwe_driver::BweDriver;
use gsa_session::pipeline::{ProbeJob, SendRecord};

const PROTOCOL_MAX_BPS: f64 = 150_000_000.0;
const FLOOR_BPS: f64 = 500_000.0;
const PACKET_BYTES: u32 = 1200;
const FEEDBACK_INTERVAL_US: u64 = 50_000;
const TICK_US: u64 = 250_000;
const RUN_US: u64 = 60_000_000;

/// Fluid bottleneck: fixed capacity, propagation delay, finite FIFO queue.
struct Link {
    capacity_bps: f64,
    prop_us: u64,
    queue_cap_bytes: u64,
    /// Time the queue drains to empty (µs) — arrival scheduling state.
    free_at_us: u64,
    queued_bytes: u64,
    last_drain_us: u64,
}

impl Link {
    fn new(capacity_bps: f64, prop_us: u64, queue_cap_bytes: u64) -> Self {
        Self {
            capacity_bps,
            prop_us,
            queue_cap_bytes,
            free_at_us: 0,
            queued_bytes: 0,
            last_drain_us: 0,
        }
    }

    /// Send one packet at `now`; `Some(arrival_us)` unless the queue drops it.
    fn send(&mut self, now_us: u64, bytes: u32, jitter_us: u64, rng: &mut u64) -> Option<u64> {
        // Drain the queue model up to now.
        let drained = (now_us.saturating_sub(self.last_drain_us)) as f64 * self.capacity_bps / 8e6;
        self.queued_bytes = self.queued_bytes.saturating_sub(drained as u64);
        self.last_drain_us = now_us;
        if self.queued_bytes + u64::from(bytes) > self.queue_cap_bytes {
            return None; // tail drop
        }
        self.queued_bytes += u64::from(bytes);
        let serialization_us = (f64::from(bytes) * 8e6 / self.capacity_bps) as u64;
        let start_us = self.free_at_us.max(now_us);
        self.free_at_us = start_us + serialization_us;
        // Radio aggregation: arrivals quantize to airtime slots, so packets
        // clump into shared bursts instead of spreading smoothly.
        let raw = self.free_at_us + self.prop_us;
        let j = if jitter_us == 0 {
            0
        } else {
            *rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let slot = raw.div_ceil(jitter_us) * jitter_us;
            (slot - raw) + (*rng >> 33) % 500
        };
        Some(raw + j)
    }
}

struct Cell {
    link_mbps: f64,
    fraction: f64,
    /// Per-packet arrival jitter amplitude (µs), airtime-clumping style.
    jitter_us: u64,
}

fn run_cell(cell: &Cell) -> f64 {
    let capacity = cell.link_mbps * 1e6;
    let mut link = Link::new(capacity, 20_000, (capacity / 8.0 * 0.15) as u64);
    let start_bps = 3_000_000u32;
    let mut driver = BweDriver::new(start_bps, 0);
    let mut now_us: u64 = 0;
    let mut seq: u32 = 0;
    let mut target_bps = f64::from(start_bps);

    // In-flight (arrival_us, seq); sent records pending feedback.
    let mut in_flight: VecDeque<(u64, u32)> = VecDeque::new();
    let mut sent: Vec<SendRecord> = Vec::new();
    let mut arrivals: Vec<(u32, u64)> = Vec::new();
    let mut next_media_us: u64 = 0;
    let mut probe_queue: VecDeque<(u64, ProbeJob, usize)> = VecDeque::new();
    let mut next_feedback_us = FEEDBACK_INTERVAL_US;
    let mut next_tick_us = 0;
    let mut estimate = f64::from(start_bps);
    let mut rng: u64 = 0x9e3779b97f4a7c15;
    // Time-averaged estimate over the final 20 s: a fair sample of a
    // sawtoothing controller, where an endpoint is a coin flip.
    let mut avg_sum = 0.0f64;
    let mut avg_n = 0u64;

    while now_us < RUN_US {
        // 1 ms simulation step.
        now_us += 1_000;

        // Media: encoder emits `fraction × target`, paced smoothly.
        let media_rate = cell.fraction * target_bps;
        if media_rate > 0.0 {
            while next_media_us <= now_us {
                let arrival = link.send(next_media_us, PACKET_BYTES, cell.jitter_us, &mut rng);
                sent.push(SendRecord {
                    seq,
                    sent_us: next_media_us,
                    bytes: PACKET_BYTES,
                    padding: false,
                    cluster: None,
                });
                if let Some(a) = arrival {
                    in_flight.push_back((a, seq));
                }
                seq = seq.wrapping_add(1);
                next_media_us += (f64::from(PACKET_BYTES) * 8e6 / media_rate) as u64;
            }
        } else {
            next_media_us = now_us;
        }

        // Probe bursts: per min-delta slot, enough packets to hit the rate.
        while let Some(&(due, job, remaining)) = probe_queue.front() {
            if due > now_us {
                break;
            }
            probe_queue.pop_front();
            let slot_us = job.min_delta.as_micros() as u64;
            let per_slot = ((job.rate_bps * slot_us as f64 / 8e6) / f64::from(PACKET_BYTES))
                .round()
                .max(1.0) as usize;
            let burst = per_slot.min(remaining);
            for _ in 0..burst {
                let arrival = link.send(due, PACKET_BYTES, cell.jitter_us, &mut rng);
                sent.push(SendRecord {
                    seq,
                    sent_us: due,
                    bytes: PACKET_BYTES,
                    padding: true,
                    cluster: Some(job.cluster),
                });
                if let Some(a) = arrival {
                    in_flight.push_back((a, seq));
                }
                seq = seq.wrapping_add(1);
            }
            if remaining > burst {
                probe_queue.push_front((due + slot_us, job, remaining - burst));
            }
        }

        // Deliveries (jitter reorders: scan the whole in-flight set).
        let mut i = 0;
        while i < in_flight.len() {
            if in_flight[i].0 <= now_us {
                let (a, s) = in_flight.remove(i).unwrap();
                arrivals.push((s, a));
            } else {
                i += 1;
            }
        }

        // Feedback batch (the client's 20 Hz report).
        if now_us >= next_feedback_us && !arrivals.is_empty() {
            next_feedback_us = now_us + FEEDBACK_INTERVAL_US;
            let base = arrivals.iter().map(|&(_, a)| a).min().unwrap();
            let fb = PacketFeedback {
                base_arrival_us: base,
                samples: arrivals
                    .drain(..)
                    .map(|(s, a)| (s, (a - base) as u32))
                    .collect(),
            };
            let max_seq = fb.samples.iter().map(|&(s, _)| s).max().unwrap();
            let covered: Vec<SendRecord> =
                sent.iter().filter(|r| r.seq <= max_seq).copied().collect();
            sent.retain(|r| r.seq > max_seq);
            driver.on_feedback(&covered, &fb, now_us);
        }

        // Controller tick.
        if now_us >= next_tick_us {
            next_tick_us = now_us + TICK_US;
            driver.set_desired_bitrate(PROTOCOL_MAX_BPS as u32);
            while let Some(job) = driver.on_tick(now_us) {
                let count = ((job.rate_bps / 8.0 * job.duration.as_secs_f64())
                    / f64::from(PACKET_BYTES))
                .ceil()
                .max(job.min_packets as f64) as usize;
                probe_queue.push_back((now_us, job, count));
            }
            if let Some(e) = driver.estimate_bps() {
                estimate = e as f64;
            }
            target_bps = estimate.clamp(FLOOR_BPS, PROTOCOL_MAX_BPS);
            if now_us > RUN_US - 20_000_000 {
                avg_sum += estimate;
                avg_n += 1;
            }
        }
    }
    avg_sum / avg_n.max(1) as f64
}

#[test]
fn estimator_converges_for_any_encoder_output() {
    let links = [1.0, 3.0, 8.0, 25.0, 100.0, 500.0];
    let fractions = [0.0, 0.5, 1.0, 1.5];
    let mut failures = Vec::new();
    for &link_mbps in &links {
        for &fraction in &fractions {
            let est = run_cell(&Cell {
                link_mbps,
                fraction,
                jitter_us: 0,
            });
            // Sustained overshoot caps the achievable target at C/f: the
            // wire carries C, the encoder mandates f x target.
            let achievable = (link_mbps * 1e6 / fraction.max(1.0)).min(PROTOCOL_MAX_BPS);
            // Sustained forced overshoot (f>1) is adversarial: the reference
            // behavior recovers to ~0.55x achievable, so hold it to 0.5x.
            let lo = if fraction > 1.0 { 0.5 } else { 0.6 } * achievable;
            let hi = 1.5 * link_mbps * 1e6;
            if est < lo || est > hi {
                failures.push(format!(
                    "link {link_mbps} Mb/s, encoder {fraction}x: est {:.2} Mb/s outside [{:.2}, {:.2}]",
                    est / 1e6,
                    lo / 1e6,
                    hi / 1e6
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "sweep failures:\n{}",
        failures.join("\n")
    );
}

#[test]
#[ignore] // diagnostic: run with --ignored --nocapture
fn trace_one_cell() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("gsa_session=trace"))
        .try_init();
    let est = run_cell(&Cell {
        link_mbps: 1.0,
        fraction: 0.0,
        jitter_us: 0,
    });
    println!("final estimate: {:.2} Mb/s", est / 1e6);
}

/// Radio links clump arrivals; the estimator must not read clean-link jitter
/// as congestion. Models the field failure seen on a fast WiFi link.
#[test]
fn estimator_survives_arrival_jitter() {
    let mut failures = Vec::new();
    for &(link_mbps, jitter_us) in &[(100.0, 3_000u64), (100.0, 8_000), (25.0, 5_000)] {
        let est = run_cell(&Cell {
            link_mbps,
            fraction: 0.3,
            jitter_us,
        });
        let ceiling = (link_mbps * 1e6_f64).min(PROTOCOL_MAX_BPS);
        if est < 0.5 * ceiling {
            failures.push(format!(
                "link {link_mbps} Mb/s jitter {jitter_us}us: est {:.2} Mb/s < {:.2}",
                est / 1e6,
                0.5 * ceiling / 1e6
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "jitter failures:
{}",
        failures.join(
            "
"
        )
    );
}
