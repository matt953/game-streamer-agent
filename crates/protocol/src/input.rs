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

/// Full controller state for one seat (spec 07).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GamepadInput {
    pub seat: u8,
    pub buttons: u32,
    /// LX, LY, RX, RY, LT, RT + 2 reserved.
    pub axes: [i16; 8],
    pub ts_us: u64,
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
