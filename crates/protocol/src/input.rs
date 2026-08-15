//! Input events (spec 05/07). Pen, touch, and gamepad variants are defined
//! at v1 even though some injection backends land later: reserving wire
//! variants is free, retrofitting them is a protocol rev.

use serde::{Deserialize, Serialize};

/// One input event, client-timestamped (client clock, µs) for latency
/// telemetry.
///
/// Postcard encodes a variant by its *position*, so new variants append. An
/// insertion would silently renumber every variant after it, and a client one
/// commit behind the host would land its mouse clicks on some other arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum InputEvent {
    /// HID usage codes (usage page 0x07), not OS keycodes.
    Key {
        usage: u16,
        down: bool,
        ts_us: u64,
    },
    MouseMove(MouseMove),
    MouseButton {
        button: MouseButton,
        down: bool,
        ts_us: u64,
    },
    MouseWheel {
        dx: f32,
        dy: f32,
        ts_us: u64,
    },
    /// Full-state snapshot, self-healing on the reliable stream (spec 07).
    Gamepad(GamepadInput),
    /// Higher-rate motion (gyro/accel), separate so pads without motion
    /// cost nothing.
    GamepadMotion {
        seat: u8,
        gyro: [f32; 3],
        accel: [f32; 3],
        ts_us: u64,
    },
    Touch(TouchEvent),
    Pen(PenEvent),
    /// The client's controller for `seat` went away — unplug the host's
    /// virtual pad, so a game sees a real removal rather than a pad frozen at
    /// neutral. Rides the reliable input stream; there is no `Connected`
    /// counterpart, because the first [`InputEvent::Gamepad`] plugs the seat.
    GamepadDisconnect {
        seat: u8,
        ts_us: u64,
    },
    /// A finger on the pad's own touch surface. Distinct from
    /// [`InputEvent::Touch`], which is a touchscreen: the host routes this to
    /// the virtual pad's touchpad, and a game reads the two differently.
    GamepadTouch {
        seat: u8,
        /// Which finger, stable for the life of the contact.
        pointer: u8,
        phase: TouchPhase,
        /// Normalized [0,1] across the pad's surface, origin top-left.
        x: f32,
        y: f32,
        /// [0,1]; 1.0 when the surface reports contact without pressure.
        pressure: f32,
        ts_us: u64,
    },
    /// The pad's charge level, for hosts that present it to the game.
    GamepadBattery {
        seat: u8,
        state: BatteryState,
        /// 0..=100, or `None` when the pad reports a state but no level.
        percent: Option<u8>,
        ts_us: u64,
    },
}

/// Where a contact is in its life. `Cancel` is not `Up`: the contact ended
/// without the user lifting (a palm rejected, the surface losing focus), and a
/// game that treats it as a release will fire the action the user aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// Charge state, as pads report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BatteryState {
    /// The pad has no battery — wired, or a virtual pad.
    NotPresent,
    Discharging,
    Charging,
    /// Charging complete while still on the cable.
    Full,
    /// The pad has a battery but will not say more.
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MouseMove {
    /// Relative deltas (pointer-locked games).
    Relative { dx: f32, dy: f32, ts_us: u64 },
    /// Normalized [0,1] absolute position (desktop use).
    Absolute { x: f32, y: f32, ts_us: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Full controller state for one seat (spec 07). See [`gamepad`] for the
/// meaning of every bit and axis.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GamepadInput {
    pub seat: u8,
    pub buttons: u32,
    /// LX, LY, RX, RY, LT, RT + 2 reserved.
    pub axes: [i16; 8],
    pub ts_us: u64,
}

/// Canonical gamepad semantics for [`GamepadInput`].
///
/// The low 16 bits of `buttons` are XInput's `wButtons` verbatim. That choice
/// costs nothing on the wire and makes the Windows host's mapping a mask
/// rather than a translation table; every other backend gets one unambiguous
/// definition to match instead of inventing its own.
pub mod gamepad {
    /// Bits of `GamepadInput::buttons` that XInput defines. The high 16 are
    /// reserved (0) — a place for pads XInput can't describe (paddles,
    /// touchpad click) without a protocol rev.
    pub const XINPUT_MASK: u32 = 0xFFFF;

    pub const DPAD_UP: u32 = 0x0001;
    pub const DPAD_DOWN: u32 = 0x0002;
    pub const DPAD_LEFT: u32 = 0x0004;
    pub const DPAD_RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_STICK: u32 = 0x0040;
    pub const RIGHT_STICK: u32 = 0x0080;
    pub const LEFT_SHOULDER: u32 = 0x0100;
    pub const RIGHT_SHOULDER: u32 = 0x0200;
    pub const GUIDE: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;

    /// Indices into `GamepadInput::axes`.
    ///
    /// Sticks span the full `i16` range with **+Y pointing up** (the XInput
    /// convention, the opposite of screen coordinates). Triggers are
    /// unipolar: `0..=i16::MAX`, never negative.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(usize)]
    pub enum Axis {
        LeftX = 0,
        LeftY = 1,
        RightX = 2,
        RightY = 3,
        LeftTrigger = 4,
        RightTrigger = 5,
    }

    impl Axis {
        #[must_use]
        pub const fn index(self) -> usize {
            self as usize
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchEvent {
    pub contacts: Vec<TouchContact>,
    pub ts_us: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TouchContact {
    pub id: u32,
    /// Normalized [0,1].
    pub x: f32,
    pub y: f32,
    pub down: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PenEvent {
    /// Normalized [0,1].
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub buttons: u8,
    pub eraser: bool,
    pub in_contact: bool,
    pub ts_us: u64,
}

/// What a `RenderSource` did with an event (spec 09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDisposition {
    /// Consumed in-process (emulators) — must not reach the OS.
    Consumed,
    /// Source wants OS-level injection to handle it.
    PassToOs,
}

#[cfg(test)]
mod tests {
    use super::gamepad::{self, Axis};
    use super::{GamepadInput, InputEvent};

    /// Postcard writes the variant's position as the first byte. Pin the ones
    /// that exist so an insertion fails here rather than on someone's desk.
    #[test]
    fn wire_positions_are_stable() {
        let position = |event: &InputEvent| crate::encode_msg(event).unwrap()[0];
        let pad = GamepadInput {
            seat: 0,
            buttons: 0,
            axes: [0; 8],
            ts_us: 0,
        };
        assert_eq!(
            position(&InputEvent::Key {
                usage: 0,
                down: true,
                ts_us: 0
            }),
            0
        );
        assert_eq!(position(&InputEvent::Gamepad(pad)), 4);
        assert_eq!(
            position(&InputEvent::GamepadDisconnect { seat: 0, ts_us: 0 }),
            8
        );
        // Appended after `GamepadDisconnect`, never inserted before it.
        assert_eq!(
            position(&InputEvent::GamepadTouch {
                seat: 0,
                pointer: 0,
                phase: super::TouchPhase::Down,
                x: 0.0,
                y: 0.0,
                pressure: 1.0,
                ts_us: 0,
            }),
            9
        );
        assert_eq!(
            position(&InputEvent::GamepadBattery {
                seat: 0,
                state: super::BatteryState::Discharging,
                percent: Some(50),
                ts_us: 0,
            }),
            10
        );
    }

    /// A cancelled contact must not decode as a release: the game would fire
    /// the action the user aborted.
    #[test]
    fn a_cancelled_contact_survives_the_wire_as_itself() {
        let bytes = crate::encode_msg(&InputEvent::GamepadTouch {
            seat: 1,
            pointer: 2,
            phase: super::TouchPhase::Cancel,
            x: 0.25,
            y: 0.75,
            pressure: 0.5,
            ts_us: 7,
        })
        .unwrap();
        let back: InputEvent = crate::decode_msg(&bytes).unwrap();
        let InputEvent::GamepadTouch {
            phase, x, pointer, ..
        } = back
        else {
            panic!("wrong variant");
        };
        assert_eq!(phase, super::TouchPhase::Cancel);
        assert_eq!(pointer, 2);
        assert!((x - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn a_battery_without_a_level_is_distinguishable_from_an_empty_one() {
        for (state, percent) in [
            (super::BatteryState::Unknown, None),
            (super::BatteryState::Discharging, Some(0)),
        ] {
            let bytes = crate::encode_msg(&InputEvent::GamepadBattery {
                seat: 0,
                state,
                percent,
                ts_us: 0,
            })
            .unwrap();
            let back: InputEvent = crate::decode_msg(&bytes).unwrap();
            let InputEvent::GamepadBattery {
                state: got_state,
                percent: got_percent,
                ..
            } = back
            else {
                panic!("wrong variant");
            };
            assert_eq!(got_state, state);
            assert_eq!(got_percent, percent);
        }
    }

    #[test]
    fn gamepad_disconnect_round_trips() {
        let bytes =
            crate::encode_msg(&InputEvent::GamepadDisconnect { seat: 3, ts_us: 99 }).unwrap();
        let back: InputEvent = crate::decode_msg(&bytes).unwrap();
        assert!(matches!(
            back,
            InputEvent::GamepadDisconnect { seat: 3, ts_us: 99 }
        ));
    }

    /// The masks are XInput's `wButtons` verbatim; drift here silently
    /// remaps every button on the Windows host.
    #[test]
    fn button_masks_match_xinput() {
        assert_eq!(gamepad::DPAD_UP, 0x0001);
        assert_eq!(gamepad::START, 0x0010);
        assert_eq!(gamepad::LEFT_SHOULDER, 0x0100);
        assert_eq!(gamepad::A, 0x1000);
        assert_eq!(gamepad::Y, 0x8000);
        // Every defined button lives in the low 16 bits.
        for mask in [gamepad::DPAD_UP, gamepad::GUIDE, gamepad::Y] {
            assert_eq!(mask & !gamepad::XINPUT_MASK, 0);
        }
    }

    #[test]
    fn axis_indices_are_the_documented_order() {
        assert_eq!(Axis::LeftX.index(), 0);
        assert_eq!(Axis::LeftY.index(), 1);
        assert_eq!(Axis::RightX.index(), 2);
        assert_eq!(Axis::RightY.index(), 3);
        assert_eq!(Axis::LeftTrigger.index(), 4);
        assert_eq!(Axis::RightTrigger.index(), 5);
    }
}
