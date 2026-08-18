//! A network worse than the one on the desk.
//!
//! Pacing only matters on links that misbehave, and a LAN to a host three
//! metres away does not. Measured against Apollo, transit jitter sits around
//! 3 ms — far too clean to exercise any de-jitter, which means a pacing change
//! tested only there is untestable rather than tested.
//!
//! So this sits between the backend and the session and delays frames on
//! purpose, restamping arrival to when the frame *would* have turned up. The
//! restamp is the point: `BackendFrame::arrival_us` is documented as the true
//! reception time, and pacing logic reads it, so a simulated delay that left
//! the old stamp in place would be invisible to the very code it exists to
//! test.
//!
//! **Order is preserved.** Real paths reorder, but every backend in this
//! codebase is required to release in order and the reference gate assumes it,
//! so shuffling here would test a contract we do not have. What this models is
//! an ordered path with variable delay — which is what a Wi-Fi hop mostly is.

use gsa_client_core::BackendFrame;

/// Deterministic so two runs can be compared.
///
/// A random seed would make every measurement a different experiment, and the
/// entire point is A/B against the same imposed conditions.
#[derive(Debug, Clone, Copy)]
pub struct Jitter {
    /// Largest extra delay to add, in microseconds. Frames get a uniform draw
    /// from zero to this.
    peak_us: u32,
    state: u64,
}

impl Jitter {
    /// `peak_ms` of added delay, or `None` to leave the stream alone.
    #[must_use]
    pub fn new(peak_ms: u32, seed: u64) -> Option<Self> {
        (peak_ms > 0).then_some(Self {
            peak_us: peak_ms.saturating_mul(1000),
            // Zero is a fixed point of xorshift, so it can never be the seed.
            state: seed | 1,
        })
    }

    /// The next delay to impose.
    pub fn next_delay_us(&mut self) -> u32 {
        // xorshift64: small, deterministic, and good enough to spread delays.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        #[allow(clippy::cast_possible_truncation)]
        {
            (self.state % u64::from(self.peak_us)) as u32
        }
    }
}

/// Wrap a frame stream so each frame arrives late by a varying amount.
///
/// Returns a receiver to hand the session in place of the backend's own.
pub fn delayed(
    mut frames: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
    mut jitter: Jitter,
    clock: gsa_core::time::MediaClock,
) -> tokio::sync::mpsc::UnboundedReceiver<BackendFrame> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(mut frame) = frames.recv().await {
            let delay = jitter.next_delay_us();
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_micros(u64::from(delay))).await;
            }
            // The frame really did arrive now, as far as anything downstream
            // is concerned. Leaving the original stamp would hide the delay
            // from the pacing code under test.
            frame.arrival_us = clock.now_us();
            if tx.send(frame).is_err() {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs must impose the same conditions, or an A/B compares two
    /// different experiments and proves nothing.
    #[test]
    fn the_same_seed_gives_the_same_delays() {
        let draw = |seed| {
            let mut j = Jitter::new(20, seed).expect("enabled");
            (0..16).map(|_| j.next_delay_us()).collect::<Vec<_>>()
        };
        assert_eq!(draw(1), draw(1));
        assert_ne!(draw(1), draw(2), "a different seed is a different link");
    }

    /// Delays must stay inside the budget asked for: a "20 ms jitter" run that
    /// silently imposes 200 ms is measuring something else entirely.
    #[test]
    fn delays_stay_within_the_requested_peak() {
        let mut jitter = Jitter::new(20, 7).expect("enabled");
        let draws: Vec<u32> = (0..2000).map(|_| jitter.next_delay_us()).collect();
        assert!(draws.iter().all(|&d| d < 20_000), "within peak");
        // And it must actually use the range, or the link is not being
        // stressed however large the number on the command line is.
        assert!(*draws.iter().max().expect("draws") > 17_000, "reaches high");
        assert!(*draws.iter().min().expect("draws") < 3_000, "reaches low");
    }

    /// Zero means off, so the flag's absence costs nothing.
    #[test]
    fn zero_disables_it_entirely() {
        assert!(Jitter::new(0, 1).is_none());
    }
}
