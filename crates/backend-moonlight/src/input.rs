//! Encoding [`InputEvent`]s into the host's input wire format.
//!
//! The format is **mixed-endian** and no layout may be derived: the envelope
//! is `u16` message type (LE), `u16` payload length (LE), `u32` data size
//! (BE), `u32` input type (LE), then the body. Bodies disagree with each
//! other too — mouse coordinates are big-endian, controller fields are
//! little-endian — so every field is written explicitly.
//!
//! Keyboards are described by Windows virtual-key codes, with the high bit
//! (0x8000) set on the code word. Internally we carry HID usages, so keys are
//! translated, not passed through.
//!
//! Gamepad state is a snapshot, not a stream of edges: each packet carries the
//! full button/stick state *and* the bitmask of pads that should remain
//! plugged in. A packet with a stale mask unplugs a controller, so the mask is
//! tracked here rather than left to callers.

use crate::control::{message, msg};
use crate::enet::Delivery;
use gsa_client_backend_api::{GamepadProfile, MotionSensor, PadCaps, PadKind};
use gsa_protocol::input::{BatteryState, InputEvent, MouseButton, MouseMove, TouchPhase, gamepad};

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
    pub const CONTROLLER_ARRIVAL: u32 = 0x5500_0004;
    pub const CONTROLLER_TOUCH: u32 = 0x5500_0005;
    pub const CONTROLLER_MOTION: u32 = 0x5500_0006;
    pub const CONTROLLER_BATTERY: u32 = 0x5500_0007;
}

/// Capability bits in a [`kind::CONTROLLER_ARRIVAL`] body. These are the
/// host's numbering, not ours; [`PadCaps`] is translated into them.
mod pad_cap {
    pub const ANALOG_TRIGGERS: u16 = 0x01;
    pub const RUMBLE: u16 = 0x02;
    pub const TRIGGER_RUMBLE: u16 = 0x04;
    pub const TOUCHPAD: u16 = 0x08;
    pub const ACCELEROMETER: u16 = 0x10;
    pub const GYRO: u16 = 0x20;
    pub const BATTERY: u16 = 0x40;
    pub const RGB_LED: u16 = 0x80;
}

/// Pad type values in a [`kind::CONTROLLER_ARRIVAL`] body.
mod pad_type {
    pub const UNKNOWN: u8 = 0x00;
    pub const XBOX: u8 = 0x01;
    pub const PLAYSTATION: u8 = 0x02;
    pub const NINTENDO: u8 = 0x03;
}

/// Motion-sample discriminator, shared by the client's samples and the host's
/// request for them.
mod motion_kind {
    pub const ACCEL: u8 = 0x01;
    pub const GYRO: u8 = 0x02;
}

/// Touch event types. A host may switch on this *or* infer contact from
/// pressure, so both must agree in every message we send.
mod touch_event {
    pub const DOWN: u8 = 0x01;
    pub const UP: u8 = 0x02;
    pub const MOVE: u8 = 0x03;
    pub const CANCEL: u8 = 0x04;
}

/// Percentage value meaning "the pad has a battery but will not say how full".
const BATTERY_PERCENT_UNKNOWN: u8 = 0xff;

/// Least pressure that reads as contact on a host that ignores the event type
/// and thresholds pressure instead. That host compares strictly greater than
/// 0.5, so a contact reporting exactly 0.5 would read as a release.
const CONTACT_PRESSURE: f32 = 0.51;

/// Reference surface width/height declared with every absolute position. The
/// host scales the coordinates against it, so the units are ours to choose;
/// `i16::MAX` is the finest resolution the field can carry.
const ABS_REFERENCE: i16 = i16::MAX;

/// Wheel units per detent, matching the desktop convention the host expects.
const WHEEL_DETENT: f32 = 120.0;

/// Fixed words every controller packet carries. Known hosts ignore them, but
/// real clients send them, so we match the wire rather than rely on that.
const PAD_HEADER: u16 = 0x001a;
const PAD_MID: u16 = 0x0014;
const PAD_TAIL_A: u16 = 0x009c;
const PAD_TAIL_B: u16 = 0x0055;

/// One encoded message and how it should ride the control channel.
///
/// Delivery is a property of the message, not of the caller: only the encoder
/// knows that a motion sample supersedes itself while a button edge does not.
#[derive(Debug, Clone)]
pub struct WireMessage {
    pub bytes: Vec<u8>,
    pub delivery: Delivery,
}

impl WireMessage {
    /// Mark this message as droppable. Valid only where the next sample makes
    /// a lost one irrelevant.
    #[must_use]
    fn unreliable(mut self) -> Self {
        self.delivery = Delivery::Unreliable;
        self
    }
}

/// Encodes input events, holding the state the wire format requires the client
/// to remember.
#[derive(Debug, Default)]
pub struct InputEncoder {
    /// Modifier bits stamped on every key event while held: the host applies
    /// them per event rather than tracking them itself.
    modifiers: u8,
    /// Bitmask of controller slots that should stay plugged in.
    active_pads: u16,
}

impl InputEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one event, or `None` if this protocol has no message for it.
    pub fn encode(&mut self, event: &InputEvent) -> Option<WireMessage> {
        match event {
            InputEvent::Key { usage, down, .. } => {
                let vk = hid_usage_to_virtual_key(*usage)?;
                self.track_modifier(*usage, *down);
                let mut body = Vec::with_capacity(6);
                // Flags byte: hosts ignore it, real clients send zero.
                body.push(0);
                // Clients set the high bit on the key code; hosts mask it off.
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
                // Vertical and horizontal are separate message kinds with no
                // combined form, so a diagonal scroll sends only its vertical
                // component.
                if *dy != 0.0 {
                    let amount = clamp_i16(*dy * WHEEL_DETENT);
                    let mut body = Vec::with_capacity(6);
                    body.extend_from_slice(&amount.to_be_bytes());
                    // The amount appears twice; hosts read only the first.
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
                // There is no unplug message: removal is a normal state packet
                // whose active mask no longer includes the slot.
                let idle = gsa_protocol::input::GamepadInput {
                    seat: *seat,
                    buttons: 0,
                    axes: [0; 8],
                    ts_us: 0,
                };
                Some(self.controller_message(&idle))
            }
            // Motion is deliberately absent here: one event carries two
            // sensors and the wire has one message per sensor, so the sink
            // expands it through `motion_message` rather than this returning
            // half of it.
            InputEvent::GamepadTouch {
                seat,
                pointer,
                phase,
                x,
                y,
                pressure,
                ..
            } => touch_message(*seat, *pointer, *phase, *x, *y, *pressure),
            InputEvent::GamepadBattery {
                seat,
                state,
                percent,
                ..
            } => Some(battery_message(*seat, *state, *percent)),
            // Touchscreen and pen events have wire kinds this backend does not
            // send: the scope here is gaming, and a pad's touchpad is a
            // different message from a screen's.
            _ => None,
        }
    }

    /// Announce what pad occupies `seat`, so the host builds a matching device.
    ///
    /// Nothing richer than buttons works before this: a host that has not been
    /// told what the pad is creates a plain Xbox device, and then drops motion,
    /// touch and battery for it without complaint.
    pub fn arrival_message(&self, seat: u8, profile: GamepadProfile) -> WireMessage {
        let mut body = Vec::with_capacity(8);
        body.push(seat & 0x0f);
        body.push(pad_type_byte(profile.kind));
        // Implementations disagree on this field's width — one reads a byte
        // here and takes the next byte as part of the button flags, another
        // requires a full 16-bit field and rejects a 7-byte body outright.
        // Writing 16 bits with a zero high byte satisfies both readings: every
        // defined capability fits in the low byte, so the byte-reader is still
        // correct and the word-reader gets its length.
        body.extend_from_slice(&wire_caps(profile.caps).to_le_bytes());
        // Which buttons the pad physically has. Advertising the standard set
        // is honest for every pad we support and costs nothing.
        body.extend_from_slice(&gsa_protocol::input::gamepad::XINPUT_MASK.to_le_bytes());
        input_message(kind::CONTROLLER_ARRIVAL, &body)
    }

    /// A motion sample for one sensor. Send only after the host asks.
    pub fn motion_message(&self, seat: u8, sensor: MotionSensor, values: [f32; 3]) -> WireMessage {
        let discriminator = match sensor {
            MotionSensor::Accel => motion_kind::ACCEL,
            MotionSensor::Gyro => motion_kind::GYRO,
        };
        motion_message(seat, discriminator, values)
    }
}

/// Translate our capability set into the host's bits.
///
/// Analog triggers are always claimed: every pad this client supports has
/// them, and the bit describes the pad rather than anything we choose.
fn wire_caps(caps: PadCaps) -> u16 {
    let mut bits = pad_cap::ANALOG_TRIGGERS;
    for (ours, theirs) in [
        (PadCaps::RUMBLE, pad_cap::RUMBLE),
        (PadCaps::TRIGGER_RUMBLE, pad_cap::TRIGGER_RUMBLE),
        (PadCaps::TOUCHPAD, pad_cap::TOUCHPAD),
        (PadCaps::ACCEL, pad_cap::ACCELEROMETER),
        (PadCaps::GYRO, pad_cap::GYRO),
        (PadCaps::BATTERY, pad_cap::BATTERY),
        (PadCaps::LED, pad_cap::RGB_LED),
    ] {
        if caps.contains(ours) {
            bits |= theirs;
        }
    }
    bits
}

/// Map a pad to the host's type byte.
///
/// Hosts disagree about how they decide to build a motion-capable device: one
/// requires this to say PlayStation and ignores the capability bits entirely,
/// another promotes an unknown pad that advertises motion, a third goes purely
/// on the bits. Reporting the pad honestly is what satisfies all of them —
/// there is no value that is safe to lie with.
fn pad_type_byte(kind: PadKind) -> u8 {
    match kind {
        PadKind::Xbox => pad_type::XBOX,
        PadKind::DualShock4 | PadKind::DualSense => pad_type::PLAYSTATION,
        PadKind::SwitchPro => pad_type::NINTENDO,
        _ => pad_type::UNKNOWN,
    }
}

/// Motion body: seat, sensor, two reserved bytes, then three little-endian
/// floats. Gyro is degrees per second; acceleration is m/s² **including
/// gravity**, on the axes the pad reports rather than the display's.
fn motion_message(seat: u8, discriminator: u8, values: [f32; 3]) -> WireMessage {
    let mut body = Vec::with_capacity(16);
    body.push(seat & 0x0f);
    body.push(discriminator);
    body.extend_from_slice(&[0, 0]);
    for value in values {
        body.extend_from_slice(&value.to_le_bytes());
    }
    // Motion supersedes itself many times a second; retransmitting a stale
    // sample would delay live input behind it for nothing.
    input_message(kind::CONTROLLER_MOTION, &body).unreliable()
}

/// Touchpad body: seat, event, two reserved bytes, pointer id, then x, y and
/// pressure as little-endian floats normalised to [0,1].
fn touch_message(
    seat: u8,
    pointer: u8,
    phase: TouchPhase,
    x: f32,
    y: f32,
    pressure: f32,
) -> Option<WireMessage> {
    // A host may switch on the event type or may ignore it entirely and
    // threshold pressure instead, so the two must never disagree: a "down"
    // carrying no pressure reads as a release on the second kind of host.
    let (event, pressure) = match phase {
        TouchPhase::Down => (
            touch_event::DOWN,
            pressure.clamp(0.0, 1.0).max(CONTACT_PRESSURE),
        ),
        TouchPhase::Move => (
            touch_event::MOVE,
            pressure.clamp(0.0, 1.0).max(CONTACT_PRESSURE),
        ),
        TouchPhase::Up => (touch_event::UP, 0.0),
        TouchPhase::Cancel => (touch_event::CANCEL, 0.0),
        // A phase added later is dropped rather than guessed: reporting it as
        // the wrong one would either strand a contact down or release one the
        // user is still holding.
        _ => return None,
    };
    let mut body = Vec::with_capacity(20);
    body.push(seat & 0x0f);
    body.push(event);
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&u32::from(pointer).to_le_bytes());
    for value in [x.clamp(0.0, 1.0), y.clamp(0.0, 1.0), pressure] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    Some(input_message(kind::CONTROLLER_TOUCH, &body))
}

/// Battery body: seat, state, percentage, one reserved byte.
fn battery_message(seat: u8, state: BatteryState, percent: Option<u8>) -> WireMessage {
    let body = vec![
        seat & 0x0f,
        match state {
            BatteryState::NotPresent => 0x01,
            BatteryState::Discharging => 0x02,
            BatteryState::Charging => 0x03,
            BatteryState::Full => 0x05,
            // Includes `Unknown`: hosts treat 0 as "no information".
            _ => 0x00,
        },
        percent.map_or(BATTERY_PERCENT_UNKNOWN, |p| p.min(100)),
        0,
    ];
    input_message(kind::CONTROLLER_BATTERY, &body)
}

impl InputEncoder {
    fn controller_message(&self, pad: &gsa_protocol::input::GamepadInput) -> WireMessage {
        let axis = |a: gamepad::Axis| pad.axes[a.index()];
        let mut body = Vec::with_capacity(26);
        body.extend_from_slice(&PAD_HEADER.to_le_bytes());
        body.extend_from_slice(&u16::from(pad.seat & 0x0f).to_le_bytes());
        body.extend_from_slice(&self.active_pads.to_le_bytes());
        body.extend_from_slice(&PAD_MID.to_le_bytes());
        // Our low 16 button bits use XInput numbering, which is what this wire
        // uses: the mapping is the identity, not a table.
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
        // Extended buttons (paddles, touchpad, misc). Our event model reserves
        // its high 16 bits for exactly these, so they pass through unchanged.
        body.extend_from_slice(&((pad.buttons >> 16) as u16).to_le_bytes());
        body.extend_from_slice(&PAD_TAIL_B.to_le_bytes());
        input_message(kind::CONTROLLER_MULTI, &body)
    }

    /// Update the modifier byte. It is stamped on every key event while a
    /// modifier is held, not only when it changes.
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
/// `data_size` counts the input-type word as well as the body, and is
/// big-endian while the input type beside it is little-endian. That mismatch
/// is the wire format, not a bug here.
fn input_message(input_type: u32, body: &[u8]) -> WireMessage {
    let data_size = (4 + body.len()) as u32;
    let mut payload = Vec::with_capacity(8 + body.len());
    payload.extend_from_slice(&data_size.to_be_bytes());
    payload.extend_from_slice(&input_type.to_le_bytes());
    payload.extend_from_slice(body);
    WireMessage {
        bytes: message(msg::INPUT_DATA, &payload),
        // Reliable unless a caller downgrades it: losing an edge event leaves
        // the host holding a key or a button that the user released.
        delivery: Delivery::Reliable,
    }
}

fn clamp_i16(v: f32) -> i16 {
    v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Map a 0..1 position onto [`ABS_REFERENCE`].
fn normalised_to_abs(v: f32) -> i16 {
    (v.clamp(0.0, 1.0) * f32::from(ABS_REFERENCE)) as i16
}

/// Triggers are unipolar `0..=i16::MAX` internally and a single byte on the
/// wire.
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
        // The enum is non-exhaustive and the wire has no "other" code; an
        // unknown button falls back to left rather than a rejected value.
        _ => 1,
    }
}

/// HID usage (page 0x07) to Windows virtual-key code.
///
/// The wire carries virtual keys; platforms hand us HID usages. Unmapped
/// usages return `None` and the event is dropped — there is no safe default
/// key to substitute.
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
mod arrival_tests {
    use super::{InputEncoder, wire_caps};
    use gsa_client_backend_api::{GamepadProfile, MotionSensor, PadCaps, PadKind};
    use gsa_protocol::input::{BatteryState, TouchPhase};

    fn body(message: &super::WireMessage) -> Vec<u8> {
        // Envelope: 4 bytes control header, 4 data size, 4 input type.
        message.bytes[12..].to_vec()
    }

    fn input_type(message: &super::WireMessage) -> u32 {
        u32::from_le_bytes([
            message.bytes[8],
            message.bytes[9],
            message.bytes[10],
            message.bytes[11],
        ])
    }

    /// Hosts disagree on the capabilities field's width: one reads a byte,
    /// another requires two and rejects a shorter body outright. Eight bytes
    /// with a zero high byte is the only encoding both accept.
    #[test]
    fn an_arrival_body_is_eight_bytes_so_either_reading_works() {
        let encoder = InputEncoder::new();
        let message = encoder.arrival_message(
            0,
            GamepadProfile::new(PadKind::DualSense, PadCaps::RUMBLE | PadCaps::MOTION),
        );
        let fields = body(&message);
        assert_eq!(input_type(&message), 0x5500_0004);
        assert_eq!(fields.len(), 8);
        // Byte 3 is the capabilities' high byte under one reading and part of
        // the button flags under the other; zero keeps both correct.
        assert_eq!(fields[3], 0);
        // Buttons occupy the last four bytes either way.
        assert_eq!(
            u32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]),
            0xffff
        );
    }

    /// A host that goes purely on the type byte gives motion to a PlayStation
    /// pad and to nothing else, so the pad must be reported honestly.
    #[test]
    fn pad_types_map_to_the_hosts_families() {
        let encoder = InputEncoder::new();
        let kind_byte =
            |kind| body(&encoder.arrival_message(0, GamepadProfile::new(kind, PadCaps::NONE)))[1];
        assert_eq!(kind_byte(PadKind::DualSense), 0x02);
        assert_eq!(kind_byte(PadKind::DualShock4), 0x02);
        assert_eq!(kind_byte(PadKind::Xbox), 0x01);
        assert_eq!(kind_byte(PadKind::SwitchPro), 0x03);
        assert_eq!(kind_byte(PadKind::Generic), 0x00);
    }

    #[test]
    fn capabilities_translate_to_the_hosts_bits() {
        // Analog triggers are always claimed: every pad here has them.
        assert_eq!(wire_caps(PadCaps::NONE), 0x01);
        assert_eq!(wire_caps(PadCaps::RUMBLE), 0x01 | 0x02);
        assert_eq!(
            wire_caps(PadCaps::GYRO | PadCaps::ACCEL),
            0x01 | 0x20 | 0x10
        );
        assert_eq!(
            wire_caps(PadCaps::TOUCHPAD | PadCaps::LED | PadCaps::BATTERY),
            0x01 | 0x08 | 0x80 | 0x40
        );
        // Every defined bit fits the low byte — which is why an 8-byte body
        // with a zero high byte satisfies the u8 reader too.
        assert_eq!(wire_caps(PadCaps::from_bits(u16::MAX)) & 0xff00, 0);
    }

    #[test]
    fn motion_is_three_little_endian_floats_and_never_retransmitted() {
        let encoder = InputEncoder::new();
        let message = encoder.motion_message(1, MotionSensor::Gyro, [1.0, -2.0, 0.5]);
        let fields = body(&message);
        assert_eq!(input_type(&message), 0x5500_0006);
        assert_eq!(fields.len(), 16);
        assert_eq!(fields[0], 1);
        assert_eq!(fields[1], 0x02, "gyro discriminator");
        assert_eq!(
            f32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]),
            1.0
        );
        assert_eq!(
            f32::from_le_bytes([fields[8], fields[9], fields[10], fields[11]]),
            -2.0
        );
        assert_eq!(
            f32::from_le_bytes([fields[12], fields[13], fields[14], fields[15]]),
            0.5
        );
        // A stale sample is worse than no sample.
        assert_eq!(message.delivery, crate::enet::Delivery::Unreliable);
        let accel = encoder.motion_message(1, MotionSensor::Accel, [0.0; 3]);
        assert_eq!(body(&accel)[1], 0x01, "acceleration discriminator");
    }

    /// One host switches on the event type; another ignores it and thresholds
    /// pressure. A contact must read as contact under both.
    #[test]
    fn a_touch_carries_contact_pressure_as_well_as_its_event_type() {
        let mut encoder = InputEncoder::new();
        let mut touch = |phase, pressure| {
            let event = gsa_protocol::input::InputEvent::GamepadTouch {
                seat: 0,
                pointer: 3,
                phase,
                x: 0.25,
                y: 0.5,
                pressure,
                ts_us: 0,
            };
            body(&encoder.encode(&event).expect("touch encodes"))
        };

        // A press reporting no pressure at all still has to register.
        let down = touch(TouchPhase::Down, 0.0);
        assert_eq!(down.len(), 20);
        assert_eq!(down[1], 0x01);
        assert_eq!(u32::from_le_bytes([down[4], down[5], down[6], down[7]]), 3);
        assert_eq!(
            f32::from_le_bytes([down[8], down[9], down[10], down[11]]),
            0.25
        );
        let pressure = f32::from_le_bytes([down[16], down[17], down[18], down[19]]);
        assert!(
            pressure > 0.5,
            "reads as contact on a pressure-thresholding host"
        );

        // A release must fall below the threshold as well as saying "up".
        let up = touch(TouchPhase::Up, 1.0);
        assert_eq!(up[1], 0x02);
        assert_eq!(f32::from_le_bytes([up[16], up[17], up[18], up[19]]), 0.0);

        // Cancel is its own event, not a release.
        assert_eq!(touch(TouchPhase::Cancel, 1.0)[1], 0x04);
    }

    #[test]
    fn battery_reports_an_absent_level_as_unknown_not_as_empty() {
        let mut encoder = InputEncoder::new();
        let mut battery = |state, percent| {
            let event = gsa_protocol::input::InputEvent::GamepadBattery {
                seat: 0,
                state,
                percent,
                ts_us: 0,
            };
            body(&encoder.encode(&event).expect("battery encodes"))
        };
        let unknown = battery(BatteryState::Discharging, None);
        assert_eq!(unknown.len(), 4);
        assert_eq!(unknown[1], 0x02);
        assert_eq!(unknown[2], 0xff, "0xff is 'no level', 0 would be 'empty'");
        assert_eq!(battery(BatteryState::Discharging, Some(0))[2], 0);
        assert_eq!(battery(BatteryState::Charging, Some(50))[2], 50);
        assert_eq!(battery(BatteryState::Full, Some(255))[2], 100, "clamped");
        assert_eq!(battery(BatteryState::NotPresent, None)[1], 0x01);
    }
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
        assert_eq!(hex(&out.bytes), "06020c000000000807000000ffff0000");
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
        assert_eq!(hex(&out.bytes), "06020900000000050800000001");
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
        assert_eq!(hex(&out.bytes), "06020e000000000a0300000000a480040000");
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
        assert_eq!(out.bytes[out.bytes.len() - 3], 0x01);
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
            out.bytes[out.bytes.len() - 3],
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
            hex(&out.bytes),
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
        let mask = u16::from_le_bytes([out.bytes[16], out.bytes[17]]);
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
        assert_eq!(out.bytes[22], 0xff);
        assert_eq!(i16::from_le_bytes([out.bytes[24], out.bytes[25]]), -32768);
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
