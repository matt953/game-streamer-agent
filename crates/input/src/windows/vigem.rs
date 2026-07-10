//! Virtual Xbox 360 pads through the ViGEmBus kernel driver.
//!
//! Windows has no user-mode "inject gamepad" call the way it has `SendInput`
//! for keyboard and mouse, so a driver is unavoidable. ViGEmBus is a bridge:
//! its upstream is archived, and the production path is a self-signed fork or
//! our own driver (roadmap OQ-7.2). That is why nothing here escapes the
//! [`VirtualGamepad`] trait.

use std::collections::HashMap;
use std::sync::Arc;

use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

use gsa_protocol::input::GamepadInput;
use gsa_protocol::input::gamepad::{self, Axis};

use crate::VirtualGamepad;

/// `ERROR_NO_MORE_ITEMS`. For a few milliseconds after a target is plugged in
/// the driver rejects reports with this — even though `wait_ready` has already
/// returned Ok, and even after an earlier report succeeded. It is the only
/// transient failure; every other error means the pad is really gone.
const ERROR_NO_MORE_ITEMS: u32 = 259;

/// Retry budget for that window: ~20 ms, against a measured need of ~10 ms.
/// Never spent once a pad is established, so it costs the steady-state path
/// nothing.
const READY_RETRIES: u32 = 10;
const READY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

pub struct VigemGamepad {
    /// Shared by every seat's target; the driver connection is one handle.
    client: Arc<Client>,
    seats: HashMap<u8, Xbox360Wired<Arc<Client>>>,
}

impl std::fmt::Debug for VigemGamepad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VigemGamepad")
            .field("seats", &self.seats.len())
            .finish()
    }
}

impl VigemGamepad {
    /// Connect to the ViGEmBus service. Fails with `Error::BusNotFound` when
    /// the driver isn't installed, which is the caller's cue to warn once and
    /// carry on without controller support.
    pub fn connect() -> Result<Self, vigem_client::Error> {
        Ok(Self {
            client: Arc::new(Client::connect()?),
            seats: HashMap::new(),
        })
    }

    /// The target for `seat`, plugging a fresh virtual pad on first use.
    /// `None` if the driver refused to plug one in.
    fn seat(&mut self, seat: u8) -> Option<&mut Xbox360Wired<Arc<Client>>> {
        if !self.seats.contains_key(&seat) {
            let mut target = Xbox360Wired::new(self.client.clone(), TargetId::XBOX360_WIRED);
            if let Err(e) = target.plugin() {
                tracing::warn!(seat, error = ?e, "could not plug in a virtual pad");
                return None;
            }
            // The pad isn't enumerable by XInput until Windows finishes
            // bringing the device up; reports sent before that are lost.
            if let Err(e) = target.wait_ready() {
                tracing::warn!(seat, error = ?e, "virtual pad never became ready");
                return None;
            }
            tracing::info!(seat, "virtual Xbox 360 pad plugged in");
            self.seats.insert(seat, target);
        }
        self.seats.get_mut(&seat)
    }
}

impl VirtualGamepad for VigemGamepad {
    fn set_state(&mut self, input: &GamepadInput) {
        let report = report(input);
        let seat = input.seat;
        let Some(target) = self.seat(seat) else {
            return;
        };
        update(seat, target, &report);
    }
}

/// Submit a report, riding out the driver's post-plug-in readiness window.
///
/// Without the retry the first snapshot after a seat appears is dropped, so a
/// controller's opening button press goes nowhere until something else on the
/// pad moves — and the wire format's full-state self-healing can't save it,
/// because a held button produces no further snapshots.
fn update(seat: u8, target: &mut Xbox360Wired<Arc<Client>>, report: &XGamepad) {
    for _ in 0..READY_RETRIES {
        match target.update(report) {
            Ok(()) => return,
            Err(vigem_client::Error::WinError(ERROR_NO_MORE_ITEMS)) => {
                std::thread::sleep(READY_INTERVAL);
            }
            Err(e) => {
                tracing::warn!(seat, error = ?e, "virtual pad update failed");
                return;
            }
        }
    }
    tracing::warn!(seat, "virtual pad never became ready; report dropped");
}

/// `GamepadInput` → XUSB report. Near-identity by construction: the wire
/// format *is* the XInput layout (spec 07), so only the triggers rescale.
fn report(input: &GamepadInput) -> XGamepad {
    let axis = |a: Axis| input.axes[a.index()];
    XGamepad {
        buttons: XButtons {
            raw: (input.buttons & gamepad::XINPUT_MASK) as u16,
        },
        left_trigger: trigger(axis(Axis::LeftTrigger)),
        right_trigger: trigger(axis(Axis::RightTrigger)),
        thumb_lx: axis(Axis::LeftX),
        thumb_ly: axis(Axis::LeftY),
        thumb_rx: axis(Axis::RightX),
        thumb_ry: axis(Axis::RightY),
    }
}

/// Unipolar trigger `0..=i16::MAX` → XInput's `0..=255`.
fn trigger(value: i16) -> u8 {
    (value.max(0) >> 7) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(buttons: u32, axes: [i16; 8]) -> GamepadInput {
        GamepadInput {
            seat: 0,
            buttons,
            axes,
            ts_us: 0,
        }
    }

    #[test]
    fn buttons_pass_through_and_reserved_bits_are_dropped() {
        let r = report(&input(gamepad::A | gamepad::DPAD_LEFT, [0; 8]));
        assert_eq!(r.buttons.raw, 0x1004);
        // A future high-bit button must not corrupt wButtons.
        let r = report(&input(0xDEAD_0000 | gamepad::B, [0; 8]));
        assert_eq!(r.buttons.raw, 0x2000);
    }

    #[test]
    fn triggers_rescale_and_clamp_at_zero() {
        assert_eq!(trigger(0), 0);
        assert_eq!(trigger(i16::MAX), 255);
        assert_eq!(trigger(i16::MAX / 2), 127);
        // Triggers are unipolar; a negative is a client bug, not -1.0.
        assert_eq!(trigger(-4000), 0);
    }

    #[test]
    fn sticks_pass_through_untouched() {
        let axes = [-32768, 32767, 100, -100, 0, 0, 0, 0];
        let r = report(&input(0, axes));
        assert_eq!((r.thumb_lx, r.thumb_ly), (-32768, 32767));
        assert_eq!((r.thumb_rx, r.thumb_ry), (100, -100));
    }
}
