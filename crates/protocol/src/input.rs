//! Input events (spec 05/07). Pen, touch, and gamepad variants are defined
//! at v1 even though some injection backends land later: reserving wire
//! variants is free, retrofitting them is a protocol rev.

use serde::{Deserialize, Serialize};

/// One input event, client-timestamped (client clock, µs) for latency
/// telemetry.
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
