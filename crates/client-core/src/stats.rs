//! Clock sync + latency accounting (spec 04).
//!
//! The agent stamps frames with *its* clock; we estimate
//! `offset = agent_clock - client_clock` from ping/pong midpoints (best of
//! N: the sample with the lowest RTT carries the least queueing noise) and
//! then latency of a frame = estimated-agent-now − capture_ts.

use std::collections::VecDeque;

use gsa_core::time::wire_ts_delta_us;
use serde::{Deserialize, Serialize};

/// Latency samples are kept for the most recent `WINDOW` frames and reported
/// at two horizons: the full window (~60 s at 60 fps — a stable baseline,
/// generous enough that a typical CI/headless run is still whole-run) and a
/// short `RECENT` tail (~5 s — responsive to a spike as it happens). Both
/// shed startup transients (decoder warmup, first keyframe) over time.
const WINDOW: usize = 3600;
const RECENT: usize = 300;

/// Sliding window for the received-goodput bitrate (spec 04 observability + an
/// ABR input): the actual bits/s of encoded video arriving, distinct from the
/// requested target.
const BITRATE_WINDOW_US: u64 = 1_000_000; // ~1 s

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
    // Rolling windows (last `WINDOW` samples); the counters above stay total.
    latencies_us: VecDeque<u32>,
    decode_us: VecDeque<u32>,
    /// (client_us, au_bytes) for frames received within the last
    /// `BITRATE_WINDOW_US`; `recv_bytes` is their running byte total.
    recv_window: VecDeque<(u64, u64)>,
    recv_bytes: u64,
}

fn push_capped(buf: &mut VecDeque<u32>, v: u32) {
    if buf.len() == WINDOW {
        buf.pop_front();
    }
    buf.push_back(v);
}

impl LatencyStats {
    /// Record a reassembled access unit: `bytes` is its encoded size, `now_us`
    /// the client-clock arrival time (feeds the rolling received bitrate).
    pub fn on_frame_complete(&mut self, bytes: usize, now_us: u64) {
        self.frames_complete += 1;
        let bytes = bytes as u64;
        self.recv_window.push_back((now_us, bytes));
        self.recv_bytes += bytes;
        // Evict samples older than the window.
        while let Some(&(t, b)) = self.recv_window.front() {
            if now_us.saturating_sub(t) > BITRATE_WINDOW_US {
                self.recv_bytes -= b;
                self.recv_window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Rolling received video goodput (bits/s) over ~1 s — what the encoder is
    /// actually producing *and* surviving the network. `None` until there are
    /// ≥2 samples spanning time.
    #[must_use]
    fn recv_bitrate_bps(&self) -> Option<f64> {
        let &(oldest_us, oldest_bytes) = self.recv_window.front()?;
        let &(newest_us, _) = self.recv_window.back()?;
        // Burst pacing clusters arrivals: a raw first-to-last span shrinks
        // under clustering and inflates the rate. Floor it at half the window.
        let span_us = newest_us
            .saturating_sub(oldest_us)
            .max(BITRATE_WINDOW_US / 2);
        // Bytes in (oldest, newest] — drop the boundary sample so the span and
        // the byte total cover the same interval.
        let bytes = self.recv_bytes.saturating_sub(oldest_bytes);
        Some(bytes as f64 * 8.0 / (span_us as f64 / 1_000_000.0))
    }

    pub fn on_frame_decoded(&mut self, latency_us: Option<u32>, decode_us: u32) {
        self.frames_decoded += 1;
        if let Some(l) = latency_us {
            push_capped(&mut self.latencies_us, l);
        }
        push_capped(&mut self.decode_us, decode_us);
    }

    #[must_use]
    pub fn summary(&self, frames_dropped: u64, frames_recovered: u64) -> StatsSummary {
        let latencies: Vec<u32> = self.latencies_us.iter().copied().collect();
        let decodes: Vec<u32> = self.decode_us.iter().copied().collect();
        let recent: Vec<u32> = tail(&self.latencies_us, RECENT);
        StatsSummary {
            frames_complete: self.frames_complete,
            frames_decoded: self.frames_decoded,
            frames_dropped_incomplete: frames_dropped,
            frames_recovered,
            latency_ms_p50: percentile(&latencies, 50).map(us_to_ms),
            latency_ms_p95: percentile(&latencies, 95).map(us_to_ms),
            latency_ms_p99: percentile(&latencies, 99).map(us_to_ms),
            decode_ms_p50: percentile(&decodes, 50).map(us_to_ms),
            recent_latency_ms_p50: percentile(&recent, 50).map(us_to_ms),
            recent_latency_ms_p99: percentile(&recent, 99).map(us_to_ms),
            recv_mbps: self.recv_bitrate_bps().map(|bps| bps / 1_000_000.0),
        }
    }
}

/// Presentation-side health: fed by the embedder (or harness) each time a
/// frame is handed to the display. Cadence gaps are the ground truth for
/// smoothness — rates alone hide bad pacing.
#[derive(Debug, Default)]
pub struct PresentStats {
    presented: u64,
    last_present_us: Option<u64>,
    /// Rolling inter-present gaps (µs), last `WINDOW` samples.
    frametimes_us: VecDeque<u32>,
    /// Rolling capture→present latency (µs), when clock sync allows it.
    latencies_us: VecDeque<u32>,
    stutters: u64,
    /// Cadence breaks already present in the SOURCE (capture-ts gaps): the
    /// game hitched; the stream merely carried it.
    src_stutters: u64,
    last_capture_ts: Option<u32>,
    freezes: u64,
    freeze_us_total: u64,
    /// Episode tracking: clustered cadence breaks are one visible event
    /// (a "slideshow"), not N independent hiccups.
    episodes: u32,
    worst_episode_us: u64,
    first_present_us: Option<u64>,
    ep_breaks: u32,
    ep_start_us: u64,
    ep_last_break_us: u64,
    ep_counted: bool,
}

/// Breaks this close together belong to one episode: only dense clusters
/// read as a visible event; sparse breaks are individual stutters.
const EPISODE_GAP_US: u64 = 1_000_000;

/// Cadence breaks in the first moments of presentation are stream startup
/// ramp, not stream health.
const WARMUP_US: u64 = 2_000_000;
/// Breaks that make a cluster an episode (a lone freeze also qualifies).
const EPISODE_MIN_BREAKS: u32 = 3;

/// A gap this many times the rolling median cadence counts as a stutter
/// (a single missed beat at any frame rate).
const STUTTER_FACTOR: u32 = 2;
/// Gaps below this never count as stutters (high-fps noise floor).
const STUTTER_MIN_US: u32 = 40_000;
/// A gap this long is a freeze regardless of cadence.
const FREEZE_US: u32 = 250_000;

impl PresentStats {
    pub fn on_presented(&mut self, latency_us: Option<u32>, capture_ts_us: u32, now_us: u64) {
        if let Some(prev_ts) = self.last_capture_ts {
            let src_gap = capture_ts_us.wrapping_sub(prev_ts);
            if src_gap > STUTTER_MIN_US && src_gap < 10_000_000 {
                self.src_stutters += 1;
            }
        }
        self.last_capture_ts = Some(capture_ts_us);
        self.presented += 1;
        if let Some(l) = latency_us {
            push_capped(&mut self.latencies_us, l);
        }
        if self.first_present_us.is_none() {
            self.first_present_us = Some(now_us);
        }
        let warm = self
            .first_present_us
            .is_some_and(|t| now_us.saturating_sub(t) > WARMUP_US);
        if let Some(prev) = self.last_present_us {
            let gap = now_us.saturating_sub(prev).min(u64::from(u32::MAX)) as u32;
            let median = {
                let v: Vec<u32> = self.frametimes_us.iter().copied().collect();
                percentile(&v, 50)
            };
            let mut brk = false;
            let mut hard = false;
            if !warm {
                // startup ramp: track cadence, count nothing
            } else if gap >= FREEZE_US {
                self.freezes += 1;
                self.freeze_us_total += u64::from(gap);
                brk = true;
                hard = true;
            } else if let Some(m) = median
                && gap > m.saturating_mul(STUTTER_FACTOR)
                && gap > STUTTER_MIN_US
            {
                self.stutters += 1;
                brk = true;
            }
            if brk {
                if now_us.saturating_sub(self.ep_last_break_us) > EPISODE_GAP_US {
                    self.ep_breaks = 0;
                    self.ep_start_us = prev;
                    self.ep_counted = false;
                }
                self.ep_breaks += 1;
                self.ep_last_break_us = now_us;
                if (self.ep_breaks >= EPISODE_MIN_BREAKS || hard) && !self.ep_counted {
                    self.episodes += 1;
                    self.ep_counted = true;
                }
                if self.ep_counted {
                    self.worst_episode_us = self
                        .worst_episode_us
                        .max(now_us.saturating_sub(self.ep_start_us));
                }
            }
            push_capped(&mut self.frametimes_us, gap);
        }
        self.last_present_us = Some(now_us);
    }

    #[must_use]
    pub fn summary(&self) -> PresentSummary {
        let gaps: Vec<u32> = self.frametimes_us.iter().copied().collect();
        let sum: u64 = gaps.iter().map(|&g| u64::from(g)).sum();
        let fps_x100 = if sum > 0 {
            (gaps.len() as u64 * 100_000_000 / sum) as u32
        } else {
            0
        };
        // "1% low": the fps equivalent of the worst 1% of frame gaps.
        let low1_fps_x100 = percentile(&gaps, 99)
            .filter(|&p| p > 0)
            .map_or(0, |p| (100_000_000u64 / u64::from(p)) as u32);
        let lat: Vec<u32> = self.latencies_us.iter().copied().collect();
        PresentSummary {
            presented: self.presented,
            fps_x100,
            low1_fps_x100,
            latency_p50_us: percentile(&lat, 50).unwrap_or(0),
            latency_p95_us: percentile(&lat, 95).unwrap_or(0),
            latency_p99_us: percentile(&lat, 99).unwrap_or(0),
            stutters: self.stutters,
            src_stutters: self.src_stutters,
            freezes: self.freezes,
            freeze_ms_total: (self.freeze_us_total / 1000) as u32,
            episodes: self.episodes,
            worst_episode_ms: (self.worst_episode_us / 1000) as u32,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PresentSummary {
    pub presented: u64,
    /// Average presented fps × 100 over the rolling window.
    pub fps_x100: u32,
    /// fps equivalent of the worst 1% of frame gaps, × 100.
    pub low1_fps_x100: u32,
    /// Capture→present latency percentiles (µs); 0 when unmeasured.
    pub latency_p50_us: u32,
    pub latency_p95_us: u32,
    pub latency_p99_us: u32,
    pub stutters: u64,
    /// Cadence breaks already present in the source (game hitches).
    pub src_stutters: u64,
    pub freezes: u64,
    pub freeze_ms_total: u32,
    /// Clustered cadence breaks (a visible degradation event) and the
    /// longest one's duration.
    pub episodes: u32,
    pub worst_episode_ms: u32,
}

/// The last `n` samples (all of them if fewer), oldest-first.
fn tail(buf: &VecDeque<u32>, n: usize) -> Vec<u32> {
    buf.iter()
        .skip(buf.len().saturating_sub(n))
        .copied()
        .collect()
}

/// JSON-friendly aggregate (client-dev `--json`, CI latency ledger).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSummary {
    pub frames_complete: u64,
    pub frames_decoded: u64,
    pub frames_dropped_incomplete: u64,
    /// Frames completed only thanks to FEC parity reconstruction.
    #[serde(default)]
    pub frames_recovered: u64,
    pub latency_ms_p50: Option<f64>,
    pub latency_ms_p95: Option<f64>,
    pub latency_ms_p99: Option<f64>,
    pub decode_ms_p50: Option<f64>,
    /// Same latency, over the short `RECENT` tail — a "right now" read.
    pub recent_latency_ms_p50: Option<f64>,
    pub recent_latency_ms_p99: Option<f64>,
    /// Rolling received video goodput (Mb/s) over ~1 s — the actual bitrate the
    /// encoder is producing and that survives the network, vs. the target.
    #[serde(default)]
    pub recv_mbps: Option<f64>,
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
    fn present_stats_track_cadence_and_breaks() {
        let mut p = PresentStats::default();
        let mut now = 0u64;
        // Steady 60 fps past warmup, then one stutter and one freeze.
        for _ in 0..150 {
            now += 16_667;
            p.on_presented(Some(20_000), (now % 4_000_000_000) as u32, now);
        }
        now += 50_000; // ~3 missed beats
        p.on_presented(Some(20_000), (now % 4_000_000_000) as u32, now);
        now += 300_000; // freeze
        p.on_presented(Some(20_000), (now % 4_000_000_000) as u32, now);
        let s = p.summary();
        assert_eq!(s.stutters, 1);
        assert_eq!(s.freezes, 1);
        assert!(s.freeze_ms_total >= 300);
        // Average fps stays near 60 (window dominated by clean cadence).
        assert!((5_000..6_500).contains(&s.fps_x100), "fps {}", s.fps_x100);
        assert_eq!(s.latency_p50_us, 20_000);
    }

    #[test]
    fn clustered_breaks_form_one_episode() {
        let mut p = PresentStats::default();
        let mut now = 0u64;
        for _ in 0..150 {
            now += 16_667;
            p.on_presented(None, (now % 4_000_000_000) as u32, now);
        }
        // A 3-second slideshow: repeated ~150 ms gaps.
        for _ in 0..20 {
            now += 150_000;
            p.on_presented(None, (now % 4_000_000_000) as u32, now);
        }
        for _ in 0..100 {
            now += 16_667;
            p.on_presented(None, (now % 4_000_000_000) as u32, now);
        }
        let s = p.summary();
        assert_eq!(s.episodes, 1, "one episode, not many");
        assert!(s.worst_episode_ms >= 2_500, "worst {}", s.worst_episode_ms);
        // An isolated hiccup well after the cluster is not an episode.
        for _ in 0..300 {
            now += 16_667;
            p.on_presented(None, (now % 4_000_000_000) as u32, now);
        }
        now += 60_000;
        p.on_presented(None, (now % 4_000_000_000) as u32, now);
        assert_eq!(p.summary().episodes, 1);
    }

    #[test]
    fn present_stats_low1_matches_worst_gap_tail() {
        let mut p = PresentStats::default();
        let mut now = 0u64;
        for i in 0..200 {
            now += if i % 50 == 49 { 100_000 } else { 16_667 };
            p.on_presented(None, (now % 4_000_000_000) as u32, now);
        }
        let s = p.summary();
        // The worst-gap tail is the 100 ms hitches -> ~10 fps equivalent.
        assert!(
            (900..1_100).contains(&s.low1_fps_x100),
            "low1 {}",
            s.low1_fps_x100
        );
    }

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

    #[test]
    fn recv_bitrate_tracks_recent_goodput() {
        let mut s = LatencyStats::default();
        // 20 frames × 25 000 B, 50 ms apart: span = 950 ms → 4 Mb/s.
        for i in 0..20u64 {
            s.on_frame_complete(25_000, i * 50_000);
        }
        let mbps = s.summary(0, 0).recv_mbps.unwrap();
        assert!((3.6..=4.4).contains(&mbps), "recv_mbps {mbps}");
    }

    #[test]
    fn recv_bitrate_resists_burst_clustering() {
        let mut s = LatencyStats::default();
        // 10 frames in a 90 ms cluster: the raw span would read 20 Mb/s; the
        // half-window floor keeps it grounded.
        for i in 0..10u64 {
            s.on_frame_complete(25_000, i * 10_000);
        }
        let mbps = s.summary(0, 0).recv_mbps.unwrap();
        assert!(mbps < 5.0, "clustered arrivals inflated recv_mbps {mbps}");
    }

    #[test]
    fn recv_bitrate_evicts_stale_samples() {
        let mut s = LatencyStats::default();
        s.on_frame_complete(1_000_000, 0); // old, must age out
        s.on_frame_complete(25_000, 5_000_000); // 5 s later
        s.on_frame_complete(25_000, 5_010_000);
        // The 1 MB spike is >1 s old, so it doesn't inflate the rate.
        let mbps = s.summary(0, 0).recv_mbps.unwrap();
        assert!(mbps < 25.0, "stale sample leaked: {mbps}");
    }

    #[test]
    fn recent_window_tracks_the_tail_not_the_baseline() {
        let mut s = LatencyStats::default();
        for _ in 0..1000 {
            s.on_frame_decoded(Some(10_000), 1000); // 10 ms baseline
        }
        for _ in 0..RECENT {
            s.on_frame_decoded(Some(100_000), 1000); // 100 ms recent spike
        }
        let sum = s.summary(0, 0);
        // Full window is still dominated by the 1000 fast frames.
        assert!(sum.latency_ms_p50.unwrap() < 20.0);
        // The short window sees only the spike.
        assert!(sum.recent_latency_ms_p50.unwrap() > 90.0);
    }
}
