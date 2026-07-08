//! Clock sync + latency accounting (spec 04).
//!
//! The agent stamps frames with *its* clock; we estimate
//! `offset = agent_clock - client_clock` from ping/pong midpoints (best of
//! N: the sample with the lowest RTT carries the least queueing noise) and
//! then latency of a frame = estimated-agent-now − capture_ts.

use gsa_core::time::wire_ts_delta_us;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct ClockSync {
    /// (rtt_us, offset_us) of the best sample so far.
    best: Option<(u64, i64)>,
}

impl ClockSync {
    /// Record one ping/pong: client send/receive times + agent timestamp.
    pub fn record(&mut self, sent_us: u64, received_us: u64, agent_ts_us: u64) {
        let rtt = received_us.saturating_sub(sent_us);
        let midpoint = sent_us + rtt / 2;
        let offset = agent_ts_us as i64 - midpoint as i64;
        if self.best.is_none_or(|(best_rtt, _)| rtt < best_rtt) {
            self.best = Some((rtt, offset));
        }
    }

    /// `agent_clock - client_clock` in µs, if synced.
    #[must_use]
    pub fn offset_us(&self) -> Option<i64> {
        self.best.map(|(_, o)| o)
    }

    /// Latency of a frame captured at `capture_ts_wire` (agent clock,
    /// truncated) observed at client time `client_now_us`.
    #[must_use]
    pub fn frame_latency_us(&self, client_now_us: u64, capture_ts_wire: u32) -> Option<u32> {
        let offset = self.offset_us()?;
        let agent_now = client_now_us.checked_add_signed(offset)?;
        let latency = wire_ts_delta_us(agent_now as u32, capture_ts_wire);
        // A "latency" of > 10 s is a wrap artifact or broken sync — report
        // nothing rather than garbage.
        (latency < 10_000_000).then_some(latency)
    }
}

#[derive(Debug, Default)]
pub struct LatencyStats {
    frames_complete: u64,
    frames_decoded: u64,
    latencies_us: Vec<u32>,
    decode_us: Vec<u32>,
}

impl LatencyStats {
    pub fn on_frame_complete(&mut self) {
        self.frames_complete += 1;
    }

    pub fn on_frame_decoded(&mut self, latency_us: Option<u32>, decode_us: u32) {
        self.frames_decoded += 1;
        if let Some(l) = latency_us {
            self.latencies_us.push(l);
        }
        self.decode_us.push(decode_us);
    }

    #[must_use]
    pub fn summary(&self, frames_dropped: u64) -> StatsSummary {
        StatsSummary {
            frames_complete: self.frames_complete,
            frames_decoded: self.frames_decoded,
            frames_dropped_incomplete: frames_dropped,
            latency_ms_p50: percentile(&self.latencies_us, 50).map(us_to_ms),
            latency_ms_p95: percentile(&self.latencies_us, 95).map(us_to_ms),
            latency_ms_p99: percentile(&self.latencies_us, 99).map(us_to_ms),
            decode_ms_p50: percentile(&self.decode_us, 50).map(us_to_ms),
        }
    }
}

/// JSON-friendly aggregate (client-dev `--json`, CI latency ledger).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSummary {
    pub frames_complete: u64,
    pub frames_decoded: u64,
    pub frames_dropped_incomplete: u64,
    pub latency_ms_p50: Option<f64>,
    pub latency_ms_p95: Option<f64>,
    pub latency_ms_p99: Option<f64>,
    pub decode_ms_p50: Option<f64>,
}

fn us_to_ms(us: u32) -> f64 {
    f64::from(us) / 1000.0
}

fn percentile(samples: &[u32], p: u32) -> Option<u32> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = (p as usize * (sorted.len() - 1)).div_euclid(100);
    Some(sorted[rank])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sync_prefers_lowest_rtt() {
        let mut cs = ClockSync::default();
        cs.record(0, 10_000, 1_000_000); // rtt 10 ms, offset ≈ 995 ms
        cs.record(20_000, 22_000, 1_021_500); // rtt 2 ms, offset ≈ 1000.5 ms
        let off = cs.offset_us().unwrap();
        assert!((1_000_000..=1_001_000).contains(&off), "offset {off}");
    }

    #[test]
    fn frame_latency_sane() {
        let mut cs = ClockSync::default();
        cs.record(1000, 1200, 501_100); // offset ≈ 500 000
        // Frame captured at agent-time 495 000, observed at client 3 000:
        // agent-now ≈ 503 000 → latency ≈ 8 000 µs.
        let lat = cs.frame_latency_us(3000, 495_000).unwrap();
        assert!((7_000..=9_000).contains(&lat), "latency {lat}");
    }

    #[test]
    fn percentiles() {
        let s: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&s, 50), Some(50));
        assert_eq!(percentile(&s, 99), Some(99));
        assert_eq!(percentile(&[], 50), None);
    }
}
