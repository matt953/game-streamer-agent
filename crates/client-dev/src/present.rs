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

    /// Whether presents landed on a fixed refresh grid, given the rate the
    /// panel would run at if it were not varying.
    ///
    /// This is the only measurement here that can tell VRR from its absence.
    /// Every other figure is identical either way whenever the content rate
    /// divides the panel's: 60 fps on a fixed 120 Hz panel shows each frame
    /// twice and produces exactly the 16.67 ms cadence an adapting panel
    /// would. Content whose rate *varies* is what separates them, because a
    /// fixed panel can only ever hold a frame for a whole number of refreshes.
    #[must_use]
    pub fn grid_fit(&self, refresh_hz: f64) -> Option<GridFit> {
        if refresh_hz <= 0.0 {
            return None;
        }
        let period_us = 1_000_000.0 / refresh_hz;
        // An interval shorter than half a refresh cannot be a multiple of one,
        // and dividing by a near-zero nearest-multiple gives a meaningless
        // residual. Those are presents the panel coalesced, not grid samples.
        let residuals: Vec<f64> = self
            .intervals
            .iter()
            .map(|us| f64::from(*us))
            .filter(|us| *us > period_us / 2.0)
            .map(|us| {
                let ratio = us / period_us;
                // Distance to the nearest whole refresh, as a fraction of one.
                // Zero means dead on a boundary, 0.5 means as far off as it is
                // possible to be.
                (ratio - ratio.round()).abs()
            })
            .collect();
        if residuals.len() < 30 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let mean = residuals.iter().sum::<f64>() / residuals.len() as f64;
        Some(GridFit {
            refresh_hz,
            mean_residual: mean,
            samples: residuals.len(),
        })
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

/// How far presents sat from a fixed refresh grid.
///
/// A frame can only leave a fixed panel on a refresh boundary, so consecutive
/// presents differ by a whole number of refreshes and the residual is near
/// zero. A panel changing its own rate refreshes when asked, so the residual
/// is scattered across the whole range — averaging a quarter of a refresh, the
/// mean of a uniform spread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridFit {
    /// The fixed rate tested against.
    pub refresh_hz: f64,
    /// Mean distance to the nearest whole refresh, in refreshes: 0 is perfectly
    /// on the grid, 0.25 is the uniform scatter of no grid at all.
    pub mean_residual: f64,
    pub samples: usize,
}

impl GridFit {
    /// Halfway between "on the grid" (0) and "uniform scatter" (0.25). Chosen
    /// as the midpoint rather than tuned, so the verdict is not fitted to the
    /// one display in front of us.
    const OFF_GRID: f64 = 0.125;

    /// Whether the presents were pinned to a fixed grid.
    ///
    /// Only meaningful for content whose rate does *not* divide the refresh
    /// rate. Frames arriving at exactly half the refresh rate land on the grid
    /// whether or not the panel is capable of leaving it.
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.mean_residual < Self::OFF_GRID
    }

    /// What the residual says, in a word.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        if self.is_fixed() {
            "pinned"
        } else {
            "adapting"
        }
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

    /// Build a ledger whose presents sit `intervals_us` apart.
    fn with_intervals(intervals: impl Iterator<Item = u32>) -> PresentLedger {
        let start = Instant::now();
        let mut ledger = PresentLedger::default();
        let mut at = Duration::ZERO;
        for gap in intervals {
            at += Duration::from_micros(u64::from(gap));
            ledger.on_present(Duration::from_millis(1), start + at);
        }
        ledger
    }

    const REFRESH_120HZ_US: u32 = 8_333;

    /// A panel that cannot change its rate can only hold a frame for a whole
    /// number of refreshes, however unevenly the content arrives. Every
    /// interval is therefore a multiple of one, and the residual collapses.
    #[test]
    fn a_pinned_panel_puts_every_present_on_a_refresh_boundary() {
        // Varying content — two refreshes then three — which is exactly what a
        // fixed panel does with a frame rate it cannot match.
        let ledger =
            with_intervals((0..200).map(|i| REFRESH_120HZ_US * if i % 3 == 0 { 3 } else { 2 }));
        let fit = ledger.grid_fit(120.0).expect("enough samples");
        assert!(
            fit.mean_residual < 0.01,
            "multiples of a refresh must sit on the grid, got {:.3}",
            fit.mean_residual
        );
        assert!(fit.is_fixed());
        assert_eq!(fit.verdict(), "pinned");
    }

    /// A panel changing its own rate refreshes when it is asked to, so the
    /// intervals owe nothing to the grid and scatter across it.
    #[test]
    fn an_adapting_panel_scatters_across_the_grid() {
        // Intervals spread continuously over a refresh rather than snapping to
        // one, which is the whole signature of the panel following the content.
        let ledger = with_intervals(
            (0..200).map(|i: u32| REFRESH_120HZ_US * 2 + (i * 997) % REFRESH_120HZ_US),
        );
        let fit = ledger.grid_fit(120.0).expect("enough samples");
        assert!(
            fit.mean_residual > 0.2,
            "a uniform scatter averages a quarter of a refresh, got {:.3}",
            fit.mean_residual
        );
        assert!(!fit.is_fixed());
        assert_eq!(fit.verdict(), "adapting");
    }

    /// The limitation, asserted so nobody reads a verdict this test forbids:
    /// content at a rate that divides the refresh rate lands on the grid
    /// whether or not the panel could leave it. Steady 60 fps on a 120 Hz
    /// panel proves nothing, which is why this needs a varying workload.
    #[test]
    fn content_that_divides_the_refresh_rate_cannot_tell_them_apart() {
        let ledger = with_intervals(std::iter::repeat_n(REFRESH_120HZ_US * 2, 200));
        let fit = ledger.grid_fit(120.0).expect("enough samples");
        assert!(
            fit.is_fixed(),
            "60 fps reads as pinned even on a panel that is adapting to it"
        );
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

/// Whether a content rate and a display rate can live together.
///
/// A display can only change image on its own refresh boundaries, so a frame
/// occupies a whole number of them. When the rates divide evenly that number
/// is constant and motion is smooth. When they do not, the phase drifts until
/// a frame has to be held one refresh longer than its neighbours — and that
/// hitch repeats forever, on a period set by how badly they disagree.
///
/// This is the decision VRR exists to remove: a display that can change its
/// own rate to match the content never has to hold a frame over. The same
/// arithmetic tells an app whether asking for a rate change is worth it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateMatch {
    pub display_hz: f64,
    pub content_fps: f64,
    /// Refreshes each frame occupies, when the rates divide evenly.
    pub refreshes_per_frame: Option<u32>,
    /// Seconds between forced hitches when they do not. `None` when they fit.
    pub beat_period_s: Option<f64>,
}

impl RateMatch {
    /// Rates agree when a frame always occupies the same number of refreshes.
    ///
    /// The tolerance is not slack for its own sake: 59.94 against 60 is a real
    /// pairing that beats once every ~17 seconds, and calling that "matched"
    /// would hide the most common judder there is.
    const TOLERANCE: f64 = 0.001;

    #[must_use]
    pub fn new(display_hz: f64, content_fps: f64) -> Self {
        if display_hz <= 0.0 || content_fps <= 0.0 {
            return Self {
                display_hz,
                content_fps,
                refreshes_per_frame: None,
                beat_period_s: None,
            };
        }
        let ratio = display_hz / content_fps;
        let nearest = ratio.round();
        let drift = (ratio - nearest).abs();
        // Fewer than one refresh per frame means the display cannot keep up at
        // all; there is no whole number of refreshes to hold a frame for.
        let fits = drift < Self::TOLERANCE && nearest >= 1.0;
        Self {
            display_hz,
            content_fps,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            refreshes_per_frame: fits.then_some(nearest as u32),
            // Each frame slips `drift` of a refresh, so `1/drift` frames pass
            // before a whole one has accumulated and must be held over.
            beat_period_s: (!fits && drift > 0.0).then(|| 1.0 / drift / content_fps),
        }
    }

    /// Whether the pairing needs no frame ever held over.
    #[must_use]
    pub fn fits(&self) -> bool {
        self.refreshes_per_frame.is_some()
    }

    /// A display rate that would fit, for a panel that can be asked.
    ///
    /// The content rate itself always fits, and a whole multiple of it fits
    /// while giving the panel more chances to show a frame on time.
    #[must_use]
    pub fn suggested_display_hz(&self) -> f64 {
        if self.fits() {
            return self.display_hz;
        }
        let multiple = (self.display_hz / self.content_fps).round().max(1.0);
        self.content_fps * multiple
    }
}

#[cfg(test)]
mod rate_tests {
    use super::RateMatch;

    /// The everyday good case: 30 fps content on a 60 Hz panel holds every
    /// frame for exactly two refreshes, forever.
    #[test]
    fn rates_that_divide_evenly_never_hold_a_frame_over() {
        let m = RateMatch::new(60.0, 30.0);
        assert!(m.fits());
        assert_eq!(m.refreshes_per_frame, Some(2));
        assert_eq!(m.beat_period_s, None);
        assert!((m.suggested_display_hz() - 60.0).abs() < 0.001);

        assert!(RateMatch::new(120.0, 60.0).fits());
        assert!(RateMatch::new(60.0, 60.0).fits());
        assert!(RateMatch::new(120.0, 30.0).fits());
    }

    /// The most common judder in the world, and the reason the tolerance is
    /// tight: 59.94 fps on a 60 Hz panel looks matched to a rounder check and
    /// hitches about every 17 seconds.
    #[test]
    fn the_ntsc_pairing_is_caught_rather_than_rounded_away() {
        let m = RateMatch::new(60.0, 59.94);
        assert!(!m.fits(), "59.94 does not divide 60");
        let beat = m.beat_period_s.expect("a beat period");
        assert!(
            (16.0..18.0).contains(&beat),
            "expected a hitch roughly every 17 s, got {beat:.1}"
        );
    }

    /// A worse mismatch must beat more often, not less — otherwise the number
    /// cannot be used to decide whether a rate change is worth asking for.
    #[test]
    fn a_worse_mismatch_hitches_more_often() {
        let mild = RateMatch::new(60.0, 59.94).beat_period_s.expect("beat");
        let bad = RateMatch::new(60.0, 50.0).beat_period_s.expect("beat");
        assert!(
            bad < mild,
            "50 on 60 should hitch far more often than 59.94"
        );
    }

    /// What to ask a panel for, when it can be asked.
    #[test]
    fn the_suggestion_is_a_rate_the_content_fits() {
        // 50 fps on a 60 Hz panel: ask for 100, which fits at 2 refreshes.
        let m = RateMatch::new(60.0, 50.0);
        assert!(!m.fits());
        let suggested = m.suggested_display_hz();
        assert!(RateMatch::new(suggested, 50.0).fits());

        // And 24 fps film, the other classic, on the same panel.
        let film = RateMatch::new(60.0, 24.0);
        assert!(!film.fits());
        assert!(RateMatch::new(film.suggested_display_hz(), 24.0).fits());
    }

    /// Nonsense in must not produce confident nonsense out.
    #[test]
    fn a_rate_of_zero_claims_nothing() {
        assert!(!RateMatch::new(0.0, 60.0).fits());
        assert!(RateMatch::new(0.0, 60.0).beat_period_s.is_none());
        assert!(!RateMatch::new(60.0, 0.0).fits());
    }
}

/// What the display will do about its own refresh rate.
///
/// Read rather than assumed: a monitor set to a variable range only actually
/// varies under conditions the app does not control — Apple requires
/// full-screen for Adaptive-Sync — so "the user turned VRR on" and "this
/// session is getting VRR" are different facts. Attributing a measurement to
/// VRR without checking is how a windowed run gets reported as a VRR result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayRefresh {
    /// Shortest and longest a frame may stay on screen, in seconds.
    pub min_interval_s: f64,
    pub max_interval_s: f64,
    /// The ceiling the system reports, in Hz.
    pub max_fps: f64,
}

impl DisplayRefresh {
    /// Whether the display can vary its rate at all.
    ///
    /// Apple's own rule: the two intervals are equal on a display that cannot.
    /// The comparison needs a tolerance because these are floating seconds and
    /// an exact match on a fixed panel is not guaranteed to be bit-identical.
    #[must_use]
    pub fn is_variable(&self) -> bool {
        (self.max_interval_s - self.min_interval_s).abs() > 1e-6
    }

    /// The rate range, in Hz, for reporting.
    #[must_use]
    pub fn range_hz(&self) -> (f64, f64) {
        let hz = |interval: f64| if interval > 0.0 { 1.0 / interval } else { 0.0 };
        // The shortest interval is the *highest* rate.
        (hz(self.max_interval_s), hz(self.min_interval_s))
    }
}

/// Ask the main display what it can do.
///
/// `None` when there is no screen to ask — a display that is asleep or absent
/// does not enumerate, and inventing a rate for it would be worse than saying
/// nothing.
#[cfg(target_os = "macos")]
#[must_use]
pub fn display_refresh() -> Option<DisplayRefresh> {
    use objc2_app_kit::NSScreen;
    let mtm = objc2_foundation::MainThreadMarker::new()?;
    let screen = NSScreen::mainScreen(mtm)?;
    Some(DisplayRefresh {
        min_interval_s: screen.minimumRefreshInterval(),
        max_interval_s: screen.maximumRefreshInterval(),
        #[allow(clippy::cast_precision_loss)]
        max_fps: screen.maximumFramesPerSecond() as f64,
    })
}

#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn display_refresh() -> Option<DisplayRefresh> {
    None
}

#[cfg(test)]
mod refresh_tests {
    use super::DisplayRefresh;

    /// Apple's rule, and the whole basis of the check: a fixed display reports
    /// the same interval for both bounds.
    #[test]
    fn a_fixed_display_reports_one_interval_twice() {
        let fixed = DisplayRefresh {
            min_interval_s: 1.0 / 120.0,
            max_interval_s: 1.0 / 120.0,
            max_fps: 120.0,
        };
        assert!(!fixed.is_variable());
        let (low, high) = fixed.range_hz();
        assert!((low - 120.0).abs() < 0.01 && (high - 120.0).abs() < 0.01);
    }

    /// And a variable one reports a range — 48 to 120 Hz being the case in
    /// front of us.
    #[test]
    fn a_variable_display_reports_a_range() {
        let variable = DisplayRefresh {
            min_interval_s: 1.0 / 120.0,
            max_interval_s: 1.0 / 48.0,
            max_fps: 120.0,
        };
        assert!(variable.is_variable());
        // The shortest interval is the fastest rate, which is easy to invert
        // by accident and would report the range backwards.
        let (low, high) = variable.range_hz();
        assert!((low - 48.0).abs() < 0.01, "low end is 48 Hz, got {low}");
        assert!(
            (high - 120.0).abs() < 0.01,
            "high end is 120 Hz, got {high}"
        );
    }
}
