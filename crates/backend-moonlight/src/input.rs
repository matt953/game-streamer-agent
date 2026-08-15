//! Turning our input events into the host's wire format.
//!
//! Two things make this fiddlier than it looks. Endianness is **mixed** —
//! the envelope's size field is big-endian, the type is little-endian, mouse
//! and key bodies disagree with each other — so every field is written
//! explicitly rather than by deriving a layout. And keyboards are described
//! by Windows virtual-key codes here, while we carry HID usages internally,
//! so keys are translated rather than passed through.
//!
//! Gamepad state is a **snapshot**, not a stream of edges: each packet
//! carries the full button/stick state *and* the set of pads that should
//! remain plugged in. A packet with a stale set silently unplugs a
//! controller, so the set is tracked here rather than left to callers.

use crate::control::{message, msg};
use gsa_protocol::input::{InputEvent, MouseButton, MouseMove, gamepad};

/// Input message kinds, as the host numbers them.
mod kind {
    pub const KEY_DOWN: u32 = 0x0000_0003;
    pub const KEY_UP: u32 = 0x0000_0004;
    pub const MOUSE_MOVE_ABS: u32 = 0x0000_0005;
    pub const MOUSE_MOVE_REL: u32 = 0x0000_0007;
    pub const MOUSE_BUTTON_DOWN: u32 = 0x0000_0008;
    pub const MOUSE_BUTTON_UP: u32 = 0x0000_0009;
    pub const MOUSE_SCROLL: u32 = 0x0000_000a;
    pub const CONTROLLER_MULTI: u32 = 0x0000_000c;
    pub const MOUSE_HSCROLL: u32 = 0x5500_0001;
}

/// Reference surface for absolute pointer positions.
///
/// The host scales whatever we send against the width and height we declare,
/// so the units are ours to choose; the largest positive `i16` gives the
/// finest resolution the field can carry.
const ABS_REFERENCE: i16 = i16::MAX;

/// Wheel units per detent, matching the desktop convention the host expects.
const WHEEL_DETENT: f32 = 120.0;

/// Constants the wire carries in every controller packet. Hosts do not read
/// them; real clients send them, so we do too rather than discover which
/// host one day starts checking.
const PAD_HEADER: u16 = 0x001a;
const PAD_MID: u16 = 0x0014;
const PAD_TAIL_A: u16 = 0x009c;
const PAD_TAIL_B: u16 = 0x0055;

/// Encodes input events, carrying the small amount of state the wire format
/// requires the client to remember.
#[derive(Debug, Default)]
pub struct InputEncoder {
    /// Modifier bits to stamp on every key event while they are held — the
    /// host re-applies them per event rather than tracking them itself.
    modifiers: u8,
    /// Which controller slots should stay plugged in.
    active_pads: u16,
}

impl InputEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one event, or `None` for events this protocol has no place for.
    pub fn encode(&mut self, event: &InputEvent) -> Option<Vec<u8>> {
        match event {
            InputEvent::Key { usage, down, .. } => {
                let vk = hid_usage_to_virtual_key(*usage)?;
                self.track_modifier(*usage, *down);
                let mut body = Vec::with_capacity(6);
                // Flags: unused by hosts, and real clients send zero.
                body.push(0);
                // Clients set the high bit; hosts mask it off again.
                body.extend_from_slice(&(0x8000u16 | u16::from(vk)).to_le_bytes());
                body.push(self.modifiers);
                body.extend_from_slice(&[0, 0]);
                Some(input_message(
                    if *down { kind::KEY_DOWN } else { kind::KEY_UP },
                    &body,
                ))
            }
            InputEvent::MouseMove(MouseMove::Relative { dx, dy, .. }) => {
                let mut body = Vec::with_capacity(4);
                body.extend_from_slice(&clamp_i16(*dx).to_be_bytes());
                body.extend_from_slice(&clamp_i16(*dy).to_be_bytes());
                Some(input_message(kind::MOUSE_MOVE_REL, &body))
            }
            InputEvent::MouseMove(MouseMove::Absolute { x, y, .. }) => {
                let mut body = Vec::with_capacity(10);
                body.extend_from_slice(&normalised_to_abs(*x).to_be_bytes());
                body.extend_from_slice(&normalised_to_abs(*y).to_be_bytes());
                body.extend_from_slice(&[0, 0]);
                body.extend_from_slice(&ABS_REFERENCE.to_be_bytes());
                body.extend_from_slice(&ABS_REFERENCE.to_be_bytes());
                Some(input_message(kind::MOUSE_MOVE_ABS, &body))
            }
            InputEvent::MouseButton { button, down, .. } => Some(input_message(
                if *down {
                    kind::MOUSE_BUTTON_DOWN
                } else {
                    kind::MOUSE_BUTTON_UP
                },
                &[mouse_button(*button)],
            )),
            InputEvent::MouseWheel { dx, dy, .. } => {
                // Vertical is the common case; a horizontal-only event uses
                // the separate kind, so a diagonal scroll sends the vertical
                // part and drops the rest rather than inventing two events.
                if *dy != 0.0 {
                    let amount = clamp_i16(*dy * WHEEL_DETENT);
                    let mut body = Vec::with_capacity(6);
                    body.extend_from_slice(&amount.to_be_bytes());
                    // Hosts ignore the second copy; real clients duplicate it.
                    body.extend_from_slice(&amount.to_be_bytes());
                    body.extend_from_slice(&[0, 0]);
                    Some(input_message(kind::MOUSE_SCROLL, &body))
                } else if *dx != 0.0 {
                    let amount = clamp_i16(*dx * WHEEL_DETENT);
                    Some(input_message(kind::MOUSE_HSCROLL, &amount.to_be_bytes()))
                } else {
                    None
                }
            }
            InputEvent::Gamepad(pad) => {
                self.active_pads |= 1u16 << (pad.seat & 0x0f);
                Some(self.controller_message(pad))
            }
            InputEvent::GamepadDisconnect { seat, .. } => {
                self.active_pads &= !(1u16 << (seat & 0x0f));
                // Removal is signalled by a normal state packet whose active
                // set no longer includes this slot, so send a neutral one.
                let idle = gsa_protocol::input::GamepadInput {
                    seat: *seat,
                    buttons: 0,
                    axes: [0; 8],
                    ts_us: 0,
                };
                Some(self.controller_message(&idle))
            }
            // Touch, pen and motion have wire kinds we have not implemented;
            // dropping them is better than sending a malformed packet.
            _ => None,
        }
    }

    fn controller_message(&self, pad: &gsa_protocol::input::GamepadInput) -> Vec<u8> {
        let axis = |a: gamepad::Axis| pad.axes[a.index()];
        let mut body = Vec::with_capacity(26);
        body.extend_from_slice(&PAD_HEADER.to_le_bytes());
        body.extend_from_slice(&u16::from(pad.seat & 0x0f).to_le_bytes());
        body.extend_from_slice(&self.active_pads.to_le_bytes());
        body.extend_from_slice(&PAD_MID.to_le_bytes());
        // Our low 16 button bits are XInput's, which is the same numbering
        // this wire uses — so the mapping is an identity, not a table.
        body.extend_from_slice(&((pad.buttons & 0xffff) as u16).to_le_bytes());
        body.push(trigger_to_u8(axis(gamepad::Axis::LeftTrigger)));
        body.push(trigger_to_u8(axis(gamepad::Axis::RightTrigger)));
        for a in [
            gamepad::Axis::LeftX,
            gamepad::Axis::LeftY,
            gamepad::Axis::RightX,
            gamepad::Axis::RightY,
        ] {
            body.extend_from_slice(&axis(a).to_le_bytes());
        }
        body.extend_from_slice(&PAD_TAIL_A.to_le_bytes());
        // The extended buttons (paddles, touchpad, misc) live here; our event
        // model reserves its high bits, so they pass through unchanged.
        body.extend_from_slice(&((pad.buttons >> 16) as u16).to_le_bytes());
        body.extend_from_slice(&PAD_TAIL_B.to_le_bytes());
        input_message(kind::CONTROLLER_MULTI, &body)
    }

    /// Keep the modifier byte current. Hosts expect it stamped on every key
    /// event while a modifier is held, not just when it changes.
    fn track_modifier(&mut self, usage: u16, down: bool) {
        let bit = match usage {
            0xe0 | 0xe4 => 0x02, // control
            0xe1 | 0xe5 => 0x01, // shift
            0xe2 | 0xe6 => 0x04, // alt
            0xe3 | 0xe7 => 0x08, // meta
            _ => return,
        };
        if down {
            self.modifiers |= bit;
        } else {
            self.modifiers &= !bit;
        }
    }
}

/// Wrap one input body in the control-message envelope.
///
/// The size field counts the type word as well as the body, and is
/// big-endian while the type beside it is little-endian — a quirk of the
/// format, not a mistake here.
fn input_message(input_type: u32, body: &[u8]) -> Vec<u8> {
    let data_size = (4 + body.len()) as u32;
    let mut payload = Vec::with_capacity(8 + body.len());
    payload.extend_from_slice(&data_size.to_be_bytes());
    payload.extend_from_slice(&input_type.to_le_bytes());
    payload.extend_from_slice(body);
    message(msg::INPUT_DATA, &payload)
}

fn clamp_i16(v: f32) -> i16 {
    v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Map a 0..1 position onto the reference surface we declare to the host.
fn normalised_to_abs(v: f32) -> i16 {
    (v.clamp(0.0, 1.0) * f32::from(ABS_REFERENCE)) as i16
}

/// Triggers are unipolar `0..=i16::MAX` for us and a byte on the wire.
fn trigger_to_u8(v: i16) -> u8 {
    (v.max(0) >> 7) as u8
}

fn mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
        // The enum is non-exhaustive; an unknown button is better sent as a
        // left-click than as a value the host will reject outright.
        _ => 1,
    }
}

/// HID usage (page 0x07) to Windows virtual-key code.
///
/// The host speaks virtual keys; we carry HID usages because they are what
/// platforms hand us. Unmapped usages return `None` and are dropped rather
/// than sent as some arbitrary key.
fn hid_usage_to_virtual_key(usage: u16) -> Option<u8> {
    let vk = match usage {
        0x04..=0x1d => 0x41 + (usage - 0x04) as u8, // a-z
        0x1e..=0x26 => 0x31 + (usage - 0x1e) as u8, // 1-9
        0x27 => 0x30,                               // 0
        0x28 => 0x0d,                               // enter
        0x29 => 0x1b,                               // escape
        0x2a => 0x08,                               // backspace
        0x2b => 0x09,                               // tab
        0x2c => 0x20,                               // space
        0x2d => 0xbd,
        0x2e => 0xbb,
        0x2f => 0xdb,
        0x30 => 0xdd,
        0x31 | 0x32 => 0xdc,
        0x33 => 0xba,
        0x34 => 0xde,
        0x35 => 0xc0,
        0x36 => 0xbc,
        0x37 => 0xbe,
        0x38 => 0xbf,
        0x39 => 0x14,                               // caps lock
        0x3a..=0x45 => 0x70 + (usage - 0x3a) as u8, // F1-F12
        0x46 => 0x2c,
        0x47 => 0x91,
        0x48 => 0x13,
        0x49 => 0x2d,
        0x4a => 0x24,
        0x4b => 0x21,
        0x4c => 0x2e,
        0x4d => 0x23,
        0x4e => 0x22,
        0x4f => 0x27,
        0x50 => 0x25,
        0x51 => 0x28,
        0x52 => 0x26,
        0x53 => 0x90,
        0x54 => 0x6f,
        0x55 => 0x6a,
        0x56 => 0x6d,
        0x57 => 0x6b,
        0x58 => 0x0d,
        0x59..=0x61 => 0x61 + (usage - 0x59) as u8, // keypad 1-9
        0x62 => 0x60,
        0x63 => 0x6e,
        0x64 => 0xe2,
        0xe0 => 0xa2,
        0xe1 => 0xa0,
        0xe2 => 0xa4,
        0xe3 => 0x5b,
        0xe4 => 0xa3,
        0xe5 => 0xa1,
        0xe6 => 0xa5,
        0xe7 => 0x5c,
        _ => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::{InputEncoder, hid_usage_to_virtual_key};
    use gsa_protocol::input::{GamepadInput, InputEvent, MouseButton, MouseMove, gamepad};

    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn relative_mouse_matches_the_documented_bytes() {
        // Published wire example: dx = -1, dy = 0.
        let mut e = InputEncoder::new();
        let out = e
            .encode(&InputEvent::MouseMove(MouseMove::Relative {
                dx: -1.0,
                dy: 0.0,
                ts_us: 0,
            }))
            .unwrap();
        assert_eq!(hex(&out), "06020c000000000807000000ffff0000");
    }

    #[test]
    fn mouse_button_matches_the_documented_bytes() {
        let mut e = InputEncoder::new();
        let out = e
            .encode(&InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
                ts_us: 0,
            })
            .unwrap();
        assert_eq!(hex(&out), "06020900000000050800000001");
    }

    #[test]
    fn a_key_carries_the_high_bit_and_held_modifiers() {
        let mut e = InputEncoder::new();
        // Left alt down: it is both the key and the modifier.
        let out = e
            .encode(&InputEvent::Key {
                usage: 0xe2,
                down: true,
                ts_us: 0,
            })
            .unwrap();
        // Published example for left-alt down with the alt modifier set.
        assert_eq!(hex(&out), "06020e000000000a0300000000a480040000");
    }

    #[test]
    fn modifiers_persist_across_later_keys() {
        let mut e = InputEncoder::new();
        e.encode(&InputEvent::Key {
            usage: 0xe1, // left shift
            down: true,
            ts_us: 0,
        });
        let out = e
            .encode(&InputEvent::Key {
                usage: 0x04, // 'a'
                down: true,
                ts_us: 0,
            })
            .unwrap();
        // Modifier byte sits after the key code; shift is 0x01.
        assert_eq!(out[out.len() - 3], 0x01);
        e.encode(&InputEvent::Key {
            usage: 0xe1,
            down: false,
            ts_us: 0,
        });
        let out = e
            .encode(&InputEvent::Key {
                usage: 0x04,
                down: true,
                ts_us: 0,
            })
            .unwrap();
        assert_eq!(
            out[out.len() - 3],
            0x00,
            "released modifier must stop being sent"
        );
    }

    #[test]
    fn controller_matches_a_captured_packet() {
        let mut e = InputEncoder::new();
        let out = e
            .encode(&InputEvent::Gamepad(GamepadInput {
                seat: 0,
                buttons: gamepad::A,
                axes: [0; 8],
                ts_us: 0,
            }))
            .unwrap();
        // Captured from a real client: pad 0, mask 0x0001, button A.
        assert_eq!(
            hex(&out),
            "060222000000001e0c0000001a000000010014000010000000000000000000009c0000005500"
        );
    }

    #[test]
    fn disconnecting_clears_the_pad_from_the_active_set() {
        let mut e = InputEncoder::new();
        e.encode(&InputEvent::Gamepad(GamepadInput {
            seat: 0,
            buttons: 0,
            axes: [0; 8],
            ts_us: 0,
        }));
        let out = e
            .encode(&InputEvent::GamepadDisconnect { seat: 0, ts_us: 0 })
            .unwrap();
        // The active mask lives 4 bytes into the body; a cleared bit is what
        // tells the host to unplug the pad.
        let mask = u16::from_le_bytes([out[16], out[17]]);
        assert_eq!(mask, 0, "a stale mask would leave the pad plugged in");
    }

    #[test]
    fn triggers_and_sticks_survive_the_conversion() {
        let mut e = InputEncoder::new();
        let mut axes = [0i16; 8];
        axes[gamepad::Axis::LeftTrigger.index()] = i16::MAX;
        axes[gamepad::Axis::LeftX.index()] = -32768;
        let out = e
            .encode(&InputEvent::Gamepad(GamepadInput {
                seat: 0,
                buttons: 0,
                axes,
                ts_us: 0,
            }))
            .unwrap();
        // Trigger byte follows the button word; full pull must reach 255.
        assert_eq!(out[22], 0xff);
        assert_eq!(i16::from_le_bytes([out[24], out[25]]), -32768);
    }

    #[test]
    fn unmapped_keys_are_dropped_not_guessed() {
        assert_eq!(hid_usage_to_virtual_key(0x04), Some(0x41)); // 'a'
        assert_eq!(hid_usage_to_virtual_key(0x1e), Some(0x31)); // '1'
        assert_eq!(hid_usage_to_virtual_key(0x3a), Some(0x70)); // F1
        assert_eq!(hid_usage_to_virtual_key(0xff), None);
        let mut e = InputEncoder::new();
        assert!(
            e.encode(&InputEvent::Key {
                usage: 0xff,
                down: true,
                ts_us: 0
            })
            .is_none()
        );
    }
}
