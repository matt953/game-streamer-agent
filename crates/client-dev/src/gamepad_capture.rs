//! Read a physical controller with gilrs and translate it into the canonical
//! `GamepadInput` (client capture side of spec 07).
//!
//! Dev-client only: real clients read their own controllers. This exists so
//! the maintainer can drive the Windows host's virtual pad from a Mac.

use gilrs::{Axis, Button, Gilrs, GilrsBuilder};
use gsa_protocol::input::{GamepadInput, InputEvent, gamepad};

/// Sticks below this fraction of full deflection read as centred. Small on
/// purpose: the game applies the real deadzone, and all this has to do is
/// stop a resting stick from emitting a snapshot every poll.
const STICK_DEADZONE: f32 = 0.05;

/// Which `GamepadInput::seat` this client occupies. Co-op seats arrive with
/// multi-client sessions (spec 07); a single dev client is always seat 0.
const SEAT: u8 = 0;

pub struct GamepadCapture {
    gilrs: Gilrs,
    /// Last state sent, so we only speak when something changed.
    last: Option<(u32, [i16; 8])>,
    /// Whether the active pad's name has been logged (reset on disconnect).
    announced: bool,
}

impl std::fmt::Debug for GamepadCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GamepadCapture").finish_non_exhaustive()
    }
}

impl GamepadCapture {
    /// `None` if the platform has no usable gamepad subsystem — the client
    /// then simply streams without controller support.
    pub fn new() -> Option<Self> {
        // gilrs's default filters impose a deadzone of their own — measured at
        // ~23% of travel on XInput — which compresses every mid-range stick and
        // trigger reading before we ever see it. The host is a game, not a UI:
        // it wants the raw curve, with only [`STICK_DEADZONE`] to kill jitter.
        match GilrsBuilder::new().with_default_filters(false).build() {
            Ok(gilrs) => Some(Self {
                gilrs,
                last: None,
                announced: false,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "no gamepad support on this client");
                None
            }
        }
    }

    /// Drain gilrs's event queue and return an event when the first connected
    /// pad has changed since the last call, or when it has gone away.
    ///
    /// Full state rather than deltas: the wire format is self-healing, so a
    /// dropped snapshot costs one poll interval rather than a stuck button.
    pub fn poll(&mut self) -> Option<InputEvent> {
        // The gamepad state gilrs exposes only advances as events are drained.
        while self.gilrs.next_event().is_some() {}

        // gamepads() can be empty until the first drain (macOS populates it
        // lazily), so announce on first sight here rather than at construction.
        // `is_connected` filters out a pad gilrs keeps listed after it drops —
        // otherwise a powered-off controller reads as still present.
        let Some((_, pad)) = self.gilrs.gamepads().find(|(_, p)| p.is_connected()) else {
            // Pad gone: tell the host to unplug its virtual one. No neutral
            // release first — unplugging releases everything, and a snapshot
            // sent after the pad is gone would only re-plug the seat.
            if self.announced {
                tracing::info!("gamepad disconnected");
                self.announced = false;
                self.last = None;
                return Some(InputEvent::GamepadDisconnect {
                    seat: SEAT,
                    ts_us: now_us(),
                });
            }
            return None;
        };
        let buttons = buttons(&pad);
        let axes = axes(&pad);
        let announce = (!self.announced).then(|| pad.name().to_string());
        if let Some(name) = announce {
            tracing::info!(name, "gamepad connected");
            self.announced = true;
        }
        if self.last == Some((buttons, axes)) {
            return None;
        }
        self.last = Some((buttons, axes));
        Some(InputEvent::Gamepad(GamepadInput {
            seat: SEAT,
            buttons,
            axes,
            ts_us: now_us(),
        }))
    }
}

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// gilrs's face-button names are positional (South/East/...), which is exactly
/// what we want: an Xbox pad's A and a DualSense's Cross both read as South,
/// and both should land on XInput's A.
fn buttons(pad: &gilrs::Gamepad<'_>) -> u32 {
    let mut bits = 0u32;
    for (button, mask) in [
        (Button::South, gamepad::A),
        (Button::East, gamepad::B),
        (Button::West, gamepad::X),
        (Button::North, gamepad::Y),
        (Button::LeftTrigger, gamepad::LEFT_SHOULDER),
        (Button::RightTrigger, gamepad::RIGHT_SHOULDER),
        (Button::Select, gamepad::BACK),
        (Button::Start, gamepad::START),
        (Button::Mode, gamepad::GUIDE),
        (Button::LeftThumb, gamepad::LEFT_STICK),
        (Button::RightThumb, gamepad::RIGHT_STICK),
        (Button::DPadUp, gamepad::DPAD_UP),
        (Button::DPadDown, gamepad::DPAD_DOWN),
        (Button::DPadLeft, gamepad::DPAD_LEFT),
        (Button::DPadRight, gamepad::DPAD_RIGHT),
    ] {
        if pad.is_pressed(button) {
            bits |= mask;
        }
    }
    // Pads whose d-pad is a hat report it as an axis, not four buttons.
    let (hx, hy) = (pad.value(Axis::DPadX), pad.value(Axis::DPadY));
    if hx > 0.5 {
        bits |= gamepad::DPAD_RIGHT;
    } else if hx < -0.5 {
        bits |= gamepad::DPAD_LEFT;
    }
    if hy > 0.5 {
        bits |= gamepad::DPAD_UP;
    } else if hy < -0.5 {
        bits |= gamepad::DPAD_DOWN;
    }
    bits
}

fn axes(pad: &gilrs::Gamepad<'_>) -> [i16; 8] {
    let mut axes = [0i16; 8];
    // gilrs and XInput agree that +Y is up, so the sticks pass through.
    axes[gamepad::Axis::LeftX.index()] = stick(pad.value(Axis::LeftStickX));
    axes[gamepad::Axis::LeftY.index()] = stick(pad.value(Axis::LeftStickY));
    axes[gamepad::Axis::RightX.index()] = stick(pad.value(Axis::RightStickX));
    axes[gamepad::Axis::RightY.index()] = stick(pad.value(Axis::RightStickY));
    axes[gamepad::Axis::LeftTrigger.index()] = trigger(pad, Button::LeftTrigger2);
    axes[gamepad::Axis::RightTrigger.index()] = trigger(pad, Button::RightTrigger2);
    axes
}

/// Bipolar stick, `-1.0..=1.0` → full `i16`.
fn stick(value: f32) -> i16 {
    if value.abs() < STICK_DEADZONE {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

/// Analog trigger, `0.0..=1.0` → `0..=i16::MAX`. gilrs models triggers as
/// buttons with a value; a digital trigger reports 0.0 or 1.0.
fn trigger(pad: &gilrs::Gamepad<'_>, button: Button) -> i16 {
    let value = pad
        .button_data(button)
        .map_or(0.0, gilrs::ev::state::ButtonData::value);
    (value.clamp(0.0, 1.0) * f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticks_scale_and_deadzone() {
        assert_eq!(stick(0.0), 0);
        assert_eq!(stick(0.01), 0); // resting jitter
        assert_eq!(stick(1.0), i16::MAX);
        assert_eq!(stick(-1.0), -i16::MAX);
        assert!(stick(0.5) > 0);
        // Out-of-range input must not wrap into the opposite direction.
        assert_eq!(stick(3.0), i16::MAX);
        assert_eq!(stick(-3.0), -i16::MAX);
    }
}
