//! How much delay to trade for how much smoothness.
//!
//! A frame crosses two gaps between the host and an eye: a network that
//! delivers unevenly, and a display that only changes image on its own
//! refresh boundaries. Absorbing the first costs latency; refusing to absorb
//! it costs judder. There is no setting that avoids both, so this is a choice
//! rather than a default — and one users of other clients already know, so the
//! names and meanings deliberately match theirs.
//!
//! The modes describe *policy*. What each one implies is expressed here, once,
//! so that a platform cannot quietly mean something different by "Balanced".

/// The trade a session makes between latency and smoothness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacingMode {
    /// Show every frame the moment it decodes, discarding one that has not
    /// reached the screen yet.
    ///
    /// Nothing is ever held, so this is the lowest latency available and the
    /// most judder: whatever unevenness the network produced is shown.
    LowestLatency,
    /// Hold a frame until it is due, buffering at most one frame interval.
    ///
    /// The default, and the same trade the reference client recommends: one
    /// frame of buffer absorbs ordinary network jitter and the drift between
    /// the host's cadence and the display's, without the several frames of lag
    /// that deeper buffering costs.
    #[default]
    Balanced,
    /// Balanced, and never ask the host for more frames than the display can
    /// show — one below its refresh rate — and never drop one.
    ///
    /// Staying below the refresh rate is what makes not dropping affordable:
    /// the display always has a slot free, so a frame never has to be
    /// discarded to keep up. Dropping as well would defeat the point of the
    /// mode. Costs a frame per second of content, and misbehaves on a display
    /// whose rate varies, since "the refresh rate" is then not one number.
    BalancedFpsLimit,
    /// Never discard a frame, whatever it costs.
    ///
    /// Frames queue when they arrive faster than they can be shown, and
    /// latency grows with the queue. For watching rather than playing.
    Smoothest,
}

impl PacingMode {
    /// The longest a frame may be held to smooth delivery, in microseconds.
    ///
    /// Expressed in frame intervals rather than milliseconds: a fixed figure
    /// means two frames at 60 fps and half a frame at 240, so the same number
    /// would be a different policy on every display.
    #[must_use]
    pub fn hold_cap_us(self, frame_interval_us: u32) -> u32 {
        match self {
            // Nothing is held at all.
            Self::LowestLatency => 0,
            // One frame, matching the reference client's recommended default.
            Self::Balanced | Self::BalancedFpsLimit => frame_interval_us,
            // Deep enough to ride out a stall, at the latency that implies.
            Self::Smoothest => frame_interval_us.saturating_mul(4),
        }
    }

    /// Whether a frame that has not been shown may be dropped when a newer one
    /// arrives.
    ///
    /// Dropping keeps latency from growing; keeping guarantees every frame is
    /// seen. The two modes that never drop do it for different reasons:
    /// [`Self::Smoothest`] pays for it in queue depth, while
    /// [`Self::BalancedFpsLimit`] pays for it by asking for fewer frames in
    /// the first place.
    #[must_use]
    pub fn drops_unshown(self) -> bool {
        !matches!(self, Self::Smoothest | Self::BalancedFpsLimit)
    }

    /// Whether any holding happens at all.
    #[must_use]
    pub fn paces(self) -> bool {
        !matches!(self, Self::LowestLatency)
    }

    /// The frame rate to ask the host for, given what the display can show.
    ///
    /// Only one mode changes it: staying a frame below the refresh rate is
    /// what stops the two cadences beating against each other.
    #[must_use]
    pub fn requested_fps(self, wanted_fps: u32, display_hz: Option<u32>) -> u32 {
        match (self, display_hz) {
            (Self::BalancedFpsLimit, Some(hz)) if hz > 1 => wanted_fps.min(hz - 1),
            _ => wanted_fps,
        }
    }

    /// The name users of other clients already know.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::LowestLatency => "Prefer Lowest Latency",
            Self::Balanced => "Balanced",
            Self::BalancedFpsLimit => "Balanced with FPS Limit",
            Self::Smoothest => "Prefer Smoothest Video",
        }
    }

    /// Parse a short form, for a command line or a stored setting.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "lowest-latency" => Some(Self::LowestLatency),
            "balanced" => Some(Self::Balanced),
            "balanced-fps-limit" => Some(Self::BalancedFpsLimit),
            "smoothest" => Some(Self::Smoothest),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT_60: u32 = 16_667;
    const AT_30: u32 = 33_333;

    /// The cap is a number of frames, not a number of milliseconds. A fixed
    /// figure would be two frames of lag at 60 fps and a third of one at 240 —
    /// the same setting meaning a different policy on every display.
    #[test]
    fn the_hold_is_measured_in_frames_not_milliseconds() {
        assert_eq!(PacingMode::Balanced.hold_cap_us(AT_60), AT_60);
        assert_eq!(PacingMode::Balanced.hold_cap_us(AT_30), AT_30);
        // Which is the whole point: one setting, one frame, any rate.
        assert!(PacingMode::Balanced.hold_cap_us(AT_30) > PacingMode::Balanced.hold_cap_us(AT_60));
    }

    /// The fastest mode must hold nothing whatsoever — a "low latency" mode
    /// that quietly buffers is the failure this names.
    #[test]
    fn the_fastest_mode_holds_nothing_and_paces_nothing() {
        assert_eq!(PacingMode::LowestLatency.hold_cap_us(AT_60), 0);
        assert!(!PacingMode::LowestLatency.paces());
        // And it drops rather than queue, so latency cannot creep.
        assert!(PacingMode::LowestLatency.drops_unshown());
    }

    /// Two modes never drop a frame, and they buy that differently: the
    /// smoothest one queues deeper, the limiting one asks for fewer frames.
    /// The reference client defines both that way — "limits the FPS value to
    /// the display refresh rate - 1 and never drops frames" — and a limiting
    /// mode that dropped would defeat its own purpose.
    #[test]
    fn the_modes_that_never_drop_are_the_smoothest_and_the_limited_one() {
        assert!(!PacingMode::Smoothest.drops_unshown());
        assert!(!PacingMode::BalancedFpsLimit.drops_unshown());
        assert!(PacingMode::Smoothest.hold_cap_us(AT_60) > PacingMode::Balanced.hold_cap_us(AT_60));

        for mode in [PacingMode::LowestLatency, PacingMode::Balanced] {
            assert!(mode.drops_unshown(), "{} must drop", mode.label());
        }
    }

    /// Only the limiting mode touches the requested rate, and it stays one
    /// below the display so the two cadences cannot beat.
    #[test]
    fn only_the_limiting_mode_lowers_the_requested_rate() {
        assert_eq!(
            PacingMode::BalancedFpsLimit.requested_fps(60, Some(60)),
            59,
            "one below the refresh rate"
        );
        // Never raises a rate the user did not ask for.
        assert_eq!(
            PacingMode::BalancedFpsLimit.requested_fps(30, Some(120)),
            30
        );
        // Without a known display rate there is nothing to stay below.
        assert_eq!(PacingMode::BalancedFpsLimit.requested_fps(60, None), 60);

        for mode in [
            PacingMode::LowestLatency,
            PacingMode::Balanced,
            PacingMode::Smoothest,
        ] {
            assert_eq!(mode.requested_fps(60, Some(60)), 60, "{}", mode.label());
        }
    }

    /// Names round-trip, because they are persisted settings and a typo would
    /// silently reset a user's choice to the default.
    #[test]
    fn every_mode_round_trips_through_its_short_name() {
        for mode in [
            PacingMode::LowestLatency,
            PacingMode::Balanced,
            PacingMode::BalancedFpsLimit,
            PacingMode::Smoothest,
        ] {
            let short = match mode {
                PacingMode::LowestLatency => "lowest-latency",
                PacingMode::Balanced => "balanced",
                PacingMode::BalancedFpsLimit => "balanced-fps-limit",
                PacingMode::Smoothest => "smoothest",
            };
            assert_eq!(PacingMode::from_name(short), Some(mode));
        }
        assert_eq!(PacingMode::from_name("nonsense"), None);
        assert_eq!(PacingMode::default(), PacingMode::Balanced);
    }
}
