//! Media clock: microsecond timestamps relative to a process-local epoch.
//!
//! Frames carry the *agent's* clock. Clients estimate the offset between
//! their clock and the agent's via ping/pong (spec 04) and map timestamps
//! into their own domain for latency accounting.

use std::time::Instant;

/// Process-local microsecond clock anchored at construction.
#[derive(Debug, Clone)]
pub struct MediaClock {
    epoch: Instant,
}

impl MediaClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Microseconds since this clock's epoch.
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }
}

impl Default for MediaClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Truncate a microsecond timestamp to the 32-bit wire form (spec 04).
/// Wraps every ~71.6 minutes; receivers only compare nearby timestamps.
#[must_use]
pub fn wire_ts(us: u64) -> u32 {
    us as u32
}

/// Difference `later - earlier` between two wrapped 32-bit timestamps,
/// correct across a single wrap.
#[must_use]
pub fn wire_ts_delta_us(later: u32, earlier: u32) -> u32 {
    later.wrapping_sub(earlier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_across_wrap() {
        assert_eq!(wire_ts_delta_us(5, u32::MAX - 4), 10);
        assert_eq!(wire_ts_delta_us(1000, 400), 600);
    }
}
