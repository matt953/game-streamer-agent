//! Translate winit input into protocol `InputEvent`s (client capture side of
//! spec 07). Covers the common desktop/gaming key set; unmapped keys are
//! dropped.

use gsa_client_core::{InputEvent, MouseButton, MouseMove};
use winit::event::{ElementState, MouseButton as WMouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

/// Monotonic-ish client timestamp for input latency telemetry.
fn now_us() -> u64 {
    // A dedicated clock isn't needed here — the agent stamps capture time;
    // this is only for the client's own input-rate diagnostics.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// winit `KeyCode` → USB HID usage (page 0x07). The agent maps HID → its OS
/// keycode, so clients stay layout- and OS-independent.
pub fn key_event(physical: PhysicalKey, state: ElementState) -> Option<InputEvent> {
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    let usage = hid_usage(code)?;
    Some(InputEvent::Key {
        usage,
        down: state == ElementState::Pressed,
        ts_us: now_us(),
    })
}

pub fn mouse_button(button: WMouseButton, state: ElementState) -> Option<InputEvent> {
    let b = match button {
        WMouseButton::Left => MouseButton::Left,
        WMouseButton::Right => MouseButton::Right,
        WMouseButton::Middle => MouseButton::Middle,
        WMouseButton::Back => MouseButton::Back,
        WMouseButton::Forward => MouseButton::Forward,
        WMouseButton::Other(_) => return None,
    };
    Some(InputEvent::MouseButton {
        button: b,
        down: state == ElementState::Pressed,
        ts_us: now_us(),
    })
}

/// Absolute cursor move, normalized to [0,1] over the presented content rect.
pub fn mouse_move_abs(x: f32, y: f32) -> InputEvent {
    InputEvent::MouseMove(MouseMove::Absolute {
        x: x.clamp(0.0, 1.0),
        y: y.clamp(0.0, 1.0),
        ts_us: now_us(),
    })
}

pub fn mouse_wheel(delta: MouseScrollDelta) -> InputEvent {
    let (dx, dy) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, y),
        MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 40.0, p.y as f32 / 40.0),
    };
    InputEvent::MouseWheel {
        dx,
        dy,
        ts_us: now_us(),
    }
}

#[allow(clippy::match_same_arms)]
fn hid_usage(code: KeyCode) -> Option<u16> {
    let usage = match code {
        KeyCode::KeyA => 0x04,
        KeyCode::KeyB => 0x05,
        KeyCode::KeyC => 0x06,
        KeyCode::KeyD => 0x07,
        KeyCode::KeyE => 0x08,
        KeyCode::KeyF => 0x09,
        KeyCode::KeyG => 0x0A,
        KeyCode::KeyH => 0x0B,
        KeyCode::KeyI => 0x0C,
        KeyCode::KeyJ => 0x0D,
        KeyCode::KeyK => 0x0E,
        KeyCode::KeyL => 0x0F,
        KeyCode::KeyM => 0x10,
        KeyCode::KeyN => 0x11,
        KeyCode::KeyO => 0x12,
        KeyCode::KeyP => 0x13,
        KeyCode::KeyQ => 0x14,
        KeyCode::KeyR => 0x15,
        KeyCode::KeyS => 0x16,
        KeyCode::KeyT => 0x17,
        KeyCode::KeyU => 0x18,
        KeyCode::KeyV => 0x19,
        KeyCode::KeyW => 0x1A,
        KeyCode::KeyX => 0x1B,
        KeyCode::KeyY => 0x1C,
        KeyCode::KeyZ => 0x1D,
        KeyCode::Digit1 => 0x1E,
        KeyCode::Digit2 => 0x1F,
        KeyCode::Digit3 => 0x20,
        KeyCode::Digit4 => 0x21,
        KeyCode::Digit5 => 0x22,
        KeyCode::Digit6 => 0x23,
        KeyCode::Digit7 => 0x24,
        KeyCode::Digit8 => 0x25,
        KeyCode::Digit9 => 0x26,
        KeyCode::Digit0 => 0x27,
        KeyCode::Enter => 0x28,
        KeyCode::Escape => 0x29,
        KeyCode::Backspace => 0x2A,
        KeyCode::Tab => 0x2B,
        KeyCode::Space => 0x2C,
        KeyCode::Minus => 0x2D,
        KeyCode::Equal => 0x2E,
        KeyCode::BracketLeft => 0x2F,
        KeyCode::BracketRight => 0x30,
        KeyCode::Backslash => 0x31,
        KeyCode::Semicolon => 0x33,
        KeyCode::Quote => 0x34,
        KeyCode::Backquote => 0x35,
        KeyCode::Comma => 0x36,
        KeyCode::Period => 0x37,
        KeyCode::Slash => 0x38,
        KeyCode::CapsLock => 0x39,
        KeyCode::F1 => 0x3A,
        KeyCode::F2 => 0x3B,
        KeyCode::F3 => 0x3C,
        KeyCode::F4 => 0x3D,
        KeyCode::F5 => 0x3E,
        KeyCode::F6 => 0x3F,
        KeyCode::F7 => 0x40,
        KeyCode::F8 => 0x41,
        KeyCode::F9 => 0x42,
        KeyCode::F10 => 0x43,
        KeyCode::F11 => 0x44,
        KeyCode::F12 => 0x45,
        KeyCode::ArrowRight => 0x4F,
        KeyCode::ArrowLeft => 0x50,
        KeyCode::ArrowDown => 0x51,
        KeyCode::ArrowUp => 0x52,
        KeyCode::ControlLeft => 0xE0,
        KeyCode::ShiftLeft => 0xE1,
        KeyCode::AltLeft => 0xE2,
        KeyCode::SuperLeft => 0xE3,
        KeyCode::ControlRight => 0xE4,
        KeyCode::ShiftRight => 0xE5,
        KeyCode::AltRight => 0xE6,
        KeyCode::SuperRight => 0xE7,
        _ => return None,
    };
    Some(usage)
}
