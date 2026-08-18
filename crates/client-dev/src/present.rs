//! What the display actually did with the frames we decoded.
//!
//! Delivery statistics answer "did the frames arrive evenly". They cannot
//! answer "did the user see them evenly", and those differ for two reasons
//! that have nothing to do with the network:
//!
//! 1. **The display is a grid.** A 60 Hz panel changes image every 16.67 ms.
//!    A frame ready 1 ms after a refresh waits 15.6 ms before anyone sees it,
//!    and that wait is added *after* every latency measurement upstream.
//! 2. **Rates beat against each other.** When the stream's cadence and the
//!    panel's do not divide evenly, frames periodically miss a refresh and are
//!    shown twice while the next is discarded — visible judder on a link with
//!    no jitter at all.
//!
//! So this ledger measures three things the delivery path cannot see: how long
//! a decoded frame waited to be shown, how far apart presents actually landed,
//! and how many frames were decoded and then never displayed.
//!
//! Kept deliberately separate from the pacing policy. A measurement that
//! changes with the thing it measures cannot falsify it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Samples kept for the percentiles — ten seconds at 60 fps.
const WINDOW: usize = 600;

#[derive(Debug, Default)]
pub struct PresentLedger {
    /// Microseconds a decoded frame waited between being ready and shown.
    waits: VecDeque<u32>,
    /// Microseconds between consecutive presents, repeats included. Tracks
    /// the display's own rhythm.
    intervals: VecDeque<u32>,
    /// Microseconds between *distinct* frames reaching the screen. This is
    /// the cadence a viewer perceives, and the only one judder shows up in: a
    /// 30 fps stream on a 60 Hz panel should sit flat at 33 ms, and a mixture
    /// of 17 and 50 is exactly the "average is fine, motion is not" failure.
    new_frame_intervals: VecDeque<u32>,
    /// Microseconds between frames becoming *ready* — the cadence as decoded,
    /// before the display has had any say.
    ///
    /// The comparison that matters: a refresh grid turns small arrival jitter
    /// into whole-refresh steps, so a stream arriving within a few ms of
    /// perfect can still be *shown* 16.7 ms early or late. If this spread is
    /// small while the presented spread is a refresh or more, the unevenness
    /// is ours — quantisation, not delivery — and smoothing the network would
    /// fix nothing.
    ready_intervals: VecDeque<u32>,
    last_present: Option<Instant>,
    last_new_frame: Option<Instant>,
    last_ready: Option<Instant>,
    /// Frames decoded and handed over.
    pub ready: u64,
    /// Frames actually put on screen.
    pub presented: u64,
    /// Frames replaced by a newer one before they were ever shown. Wasted
    /// decode, and a cadence the user never saw.
    pub superseded: u64,
    /// Presents that re-showed the frame already on screen, because no new one
    /// had arrived. The other half of a beat: one frame twice, the next never.
    pub repeats: u64,
}

impl PresentLedger {
    /// A frame has been decoded and is ready to show. `replaced_unshown` is
    /// true when it displaced one that never made it to the screen.
    pub fn on_ready(&mut self, replaced_unshown: bool, at: Instant) {
        self.ready += 1;
        if replaced_unshown {
            self.superseded += 1;
        }
        if let Some(previous) = self.last_ready {
            push(&mut self.ready_intervals, duration_us(at - previous));
        }
        self.last_ready = Some(at);
    }

    /// A frame reached the screen, `waited` after becoming ready.
    pub fn on_present(&mut self, waited: Duration, at: Instant) {
        self.presented += 1;
        push(&mut self.waits, duration_us(waited));
        if let Some(previous) = self.last_present {
            push(&mut self.intervals, duration_us(at - previous));
        }
        if let Some(previous) = self.last_new_frame {
            push(&mut self.new_frame_intervals, duration_us(at - previous));
        }
        self.last_present = Some(at);
        self.last_new_frame = Some(at);
    }

    /// The screen was redrawn with the frame already on it.
    pub fn on_repeat(&mut self, at: Instant) {
        self.repeats += 1;
        if let Some(previous) = self.last_present {
            push(&mut self.intervals, duration_us(at - previous));
        }
        self.last_present = Some(at);
    }

    /// A snapshot for reporting; `None` until enough samples to be worth
    /// reading, since a percentile over three of them is noise.
    ///
    /// Gated on *arrival* samples, not presents: a window the compositor has
    /// covered shows nothing and therefore presents nothing, but the frames
    /// still arrive and their cadence is still the question pacing is about.
    /// Requiring presents here made the harness useless whenever its window
    /// was behind another.
    #[must_use]
    pub fn summary(&self) -> Option<PresentSummary> {
        if self.ready_intervals.len() < 30 {
            return None;
        }
        Some(PresentSummary {
            wait_p50_us: percentile(&self.waits, 50),
            wait_p99_us: percentile(&self.waits, 99),
            interval_p50_us: percentile(&self.intervals, 50),
            interval_p99_us: percentile(&self.intervals, 99),
            frame_p50_us: percentile(&self.new_frame_intervals, 50),
            frame_p99_us: percentile(&self.new_frame_intervals, 99),
            frame_spread_us: percentile(&self.new_frame_intervals, 99)
                .saturating_sub(percentile(&self.new_frame_intervals, 1)),
            ready_p50_us: percentile(&self.ready_intervals, 50),
            ready_spread_us: percentile(&self.ready_intervals, 99)
                .saturating_sub(percentile(&self.ready_intervals, 1)),
            ready: self.ready,
            presented: self.presented,
            superseded: self.superseded,
            repeats: self.repeats,
        })
    }
}

/// What the ledger has to say, in one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentSummary {
    /// Typical wait between a frame being ready and being shown — the tax the
    /// display charges, invisible to every upstream latency measurement.
    pub wait_p50_us: u32,
    pub wait_p99_us: u32,
    /// Typical and worst gap between presents. On a healthy 60 fps stream on a
    /// 60 Hz panel both sit at ~16.7 ms; a p99 near double it is a beat.
    pub interval_p50_us: u32,
    pub interval_p99_us: u32,
    /// Gap between distinct frames appearing — the cadence actually seen.
    pub frame_p50_us: u32,
    pub frame_p99_us: u32,
    /// How far that cadence spreads, p1 to p99. A steady stream is near zero
    /// however fast it runs; judder is spread, not rate.
    pub frame_spread_us: u32,
    /// The same two figures for frames as decoded, before the display grid.
    pub ready_p50_us: u32,
    pub ready_spread_us: u32,
    pub ready: u64,
    pub presented: u64,
    pub superseded: u64,
    pub repeats: u64,
}

impl PresentSummary {
    /// Presents that showed nothing new, as a percentage.
    ///
    /// The clearest single number for a rate mismatch: a stream and a panel
    /// that agree produce nearly none.
    #[must_use]
    pub fn repeat_pct(&self) -> f64 {
        let total = self.presented + self.repeats;
        if total == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.repeats as f64 * 100.0 / total as f64
        }
    }

    /// Decoded frames the user never saw, as a percentage.
    #[must_use]
    pub fn superseded_pct(&self) -> f64 {
        if self.ready == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.superseded as f64 * 100.0 / self.ready as f64
        }
    }
}

fn push(window: &mut VecDeque<u32>, value: u32) {
    if window.len() == WINDOW {
        window.pop_front();
    }
    window.push_back(value);
}

#[allow(clippy::cast_possible_truncation)]
fn duration_us(d: Duration) -> u32 {
    d.as_micros().min(u128::from(u32::MAX)) as u32
}

fn percentile(window: &VecDeque<u32>, pct: usize) -> u32 {
    if window.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u32> = window.iter().copied().collect();
    sorted.sort_unstable();
    let index = (sorted.len() - 1) * pct / 100;
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(ledger: &mut PresentLedger, start: Instant, at_ms: u64, waited_ms: u64) {
        ledger.on_present(
            Duration::from_millis(waited_ms),
            start + Duration::from_millis(at_ms),
        );
    }

    /// A stream and a panel that agree: every interval one refresh apart, and
    /// nothing shown twice.
    #[test]
    fn a_matched_rate_shows_every_frame_once() {
        let start = Instant::now();
        let mut ledger = PresentLedger::default();
        for i in 0..60 {
            ledger.on_ready(false, start + Duration::from_millis(i * 17));
            present(&mut ledger, start, i * 17, 2);
        }
        let summary = ledger.summary().expect("enough samples");
        assert_eq!(summary.interval_p50_us, 17_000);
        assert_eq!(summary.interval_p99_us, 17_000);
        assert_eq!(summary.repeats, 0);
        assert!(summary.repeat_pct() < 0.01);
        assert!(summary.superseded_pct() < 0.01);
    }

    /// The failure this exists to catch: the average rate looks right while
    /// every fourth refresh shows nothing new and a decoded frame is thrown
    /// away. Delivery statistics call this healthy.
    #[test]
    fn a_beat_shows_up_as_repeats_and_wasted_frames() {
        let start = Instant::now();
        let mut ledger = PresentLedger::default();
        let mut at = 0;
        for i in 0..60 {
            if i % 4 == 3 {
                // No new frame this refresh: the old one is shown again, and
                // the next to arrive displaces one that was never seen.
                ledger.on_repeat(start + Duration::from_millis(at));
                ledger.on_ready(true, start + Duration::from_millis(at));
            } else {
                ledger.on_ready(false, start + Duration::from_millis(at));
                present(&mut ledger, start, at, 8);
            }
            at += 17;
        }
        let summary = ledger.summary().expect("enough samples");
        assert!(summary.repeats > 0, "the doubled frames must be counted");
        assert!(
            summary.repeat_pct() > 20.0,
            "a quarter of refreshes showed nothing new, got {:.1}%",
            summary.repeat_pct()
        );
        assert!(summary.superseded > 0, "and frames were decoded unseen");
    }

    /// The display tax: a frame ready just after a refresh waits nearly a
    /// whole period, and no upstream measurement can see it.
    #[test]
    fn the_wait_percentiles_expose_the_vsync_tax() {
        let start = Instant::now();
        let mut ledger = PresentLedger::default();
        for i in 0..60 {
            ledger.on_ready(false, start + Duration::from_millis(i * 17));
            // Mostly prompt, but one frame in ten just misses a refresh.
            let waited = if i % 10 == 0 { 16 } else { 1 };
            present(&mut ledger, start, i * 17, waited);
        }
        let summary = ledger.summary().expect("enough samples");
        assert_eq!(summary.wait_p50_us, 1_000);
        assert_eq!(summary.wait_p99_us, 16_000);
    }

    /// Percentiles over a handful of frames are noise, and a number that looks
    /// authoritative is worse than none.
    #[test]
    fn too_few_samples_report_nothing() {
        let start = Instant::now();
        let mut ledger = PresentLedger::default();
        for i in 0..10 {
            ledger.on_ready(false, start + Duration::from_millis(i * 17));
            present(&mut ledger, start, i * 17, 1);
        }
        assert!(ledger.summary().is_none());
    }
}
