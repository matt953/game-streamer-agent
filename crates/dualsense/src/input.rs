//! Input report → [`InputEvent`]s.
//!
//! The report is the real DualSense's, byte offsets as in the `dualsense-tester`
//! reference and altc's `altc-input`. USB delivers report id `0x01` and the body
//! starts at data offset 0; Bluetooth delivers `0x31` with one header byte first,
//! so every field shifts by one. WebHID hands the body without the report id, so
//! the shift is decided by the connection, not by reading a leading byte.

use gsa_protocol::input::{BatteryState, GamepadInput, InputEvent, TouchPhase, gamepad};

/// How the pad is attached, which sets the report's field offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    /// Report id `0x01`; body at offset 0.
    Usb,
    /// Report id `0x31`; body at offset 1.
    Bluetooth,
}

impl Connection {
    /// The connection a report id implies, or `None` for an id this codec
    /// does not read (a plain-USB `0x01` and a BT `0x31` are the full ones).
    #[must_use]
    pub fn from_report_id(id: u8) -> Option<Self> {
        match id {
            0x01 => Some(Self::Usb),
            0x31 => Some(Self::Bluetooth),
            _ => None,
        }
    }

    fn base(self) -> usize {
        match self {
            Self::Usb => 0,
            Self::Bluetooth => 1,
        }
    }
}

/// One tracked touch contact, so a moving finger reads as Move and a lifted
/// one as Up exactly once.
#[derive(Debug, Clone, Copy, Default)]
struct Contact {
    /// The pad's 7-bit contact id while down; `None` when the slot is empty.
    id: Option<u8>,
    x: f32,
    y: f32,
}

/// Turns a stream of DualSense input reports into [`InputEvent`]s for one
/// seat, emitting only what changed so a 250 Hz pad does not flood the
/// reliable input channel.
#[derive(Debug)]
pub struct Parser {
    seat: u8,
    last_buttons: Option<u32>,
    last_axes: [i16; 8],
    have_axes: bool,
    contacts: [Contact; 2],
    last_battery: Option<(BatteryState, Option<u8>)>,
}

/// The DualSense touchpad's reported resolution.
const TOUCH_W: f32 = 1920.0;
const TOUCH_H: f32 = 1080.0;

/// A stick byte (0..255, centre 0x80) as an XInput axis (`i16`, +up).
fn stick_axis(v: u8, invert: bool) -> i16 {
    let centered = i32::from(v) - 128;
    let scaled = (centered * 32767 / 127).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    let scaled = if invert { -scaled } else { scaled };
    scaled.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// A trigger byte (0..255) as a unipolar XInput axis (0..=i16::MAX).
fn trigger_axis(v: u8) -> i16 {
    (i32::from(v) * 32767 / 255) as i16
}

impl Parser {
    #[must_use]
    pub fn new(seat: u8) -> Self {
        Self {
            seat,
            last_buttons: None,
            last_axes: [0; 8],
            have_axes: false,
            contacts: [Contact::default(); 2],
            last_battery: None,
        }
    }

    /// Parse one report body (without the report id), appending the events it
    /// produced. `ts_us` stamps them on the client clock.
    pub fn parse(&mut self, conn: Connection, data: &[u8], ts_us: u64, out: &mut Vec<InputEvent>) {
        let b = conn.base();
        // The battery byte is the furthest field read; a short report is not
        // one this codec understands.
        if data.len() < b + 53 {
            return;
        }
        self.parse_pad(b, data, ts_us, out);
        self.parse_touch(b, data, ts_us, out);
        self.parse_battery(b, data, ts_us, out);
    }

    fn parse_pad(&mut self, b: usize, data: &[u8], ts_us: u64, out: &mut Vec<InputEvent>) {
        let buttons = decode_buttons(data[b + 7], data[b + 8], data[b + 9]);
        let axes = [
            stick_axis(data[b], false),     // LX
            stick_axis(data[b + 1], true),  // LY (+up)
            stick_axis(data[b + 2], false), // RX
            stick_axis(data[b + 3], true),  // RY
            trigger_axis(data[b + 4]),      // LT
            trigger_axis(data[b + 5]),      // RT
            0,
            0,
        ];
        // Sticks drift a bit at rest; only a real move past a small deadzone
        // is a change worth a reliable message.
        let axes_changed = !self.have_axes
            || axes
                .iter()
                .zip(self.last_axes.iter())
                .any(|(a, p)| (i32::from(*a) - i32::from(*p)).abs() > 256);
        if self.last_buttons != Some(buttons) || axes_changed {
            self.last_buttons = Some(buttons);
            self.last_axes = axes;
            self.have_axes = true;
            out.push(InputEvent::Gamepad(GamepadInput {
                seat: self.seat,
                buttons,
                axes,
                ts_us,
            }));
        }
    }

    fn parse_touch(&mut self, b: usize, data: &[u8], ts_us: u64, out: &mut Vec<InputEvent>) {
        // The two 4-byte touch points sit at body offset 32 and 36 (plus the
        // Bluetooth shift `b`): the reference DualSense report puts `touchData`
        // at 32 + base. Byte 0 of a point is the contact id with bit 7 set
        // while no finger is down; the next three pack x (12 bits) and y (12
        // bits). Reading these one byte late scrambled position, which broke
        // touchpad pan and pinch-zoom.
        for (slot, off) in [(0usize, b + 32), (1usize, b + 36)] {
            let raw = &data[off..off + 4];
            let down = raw[0] & 0x80 == 0;
            let id = raw[0] & 0x7f;
            let x = f32::from(u16::from(raw[1]) | (u16::from(raw[2] & 0x0f) << 8)) / TOUCH_W;
            let y = f32::from((u16::from(raw[2]) >> 4) | (u16::from(raw[3]) << 4)) / TOUCH_H;
            let was = self.contacts[slot];
            if down {
                let phase = if was.id == Some(id) {
                    TouchPhase::Move
                } else {
                    // A slot that jumps straight to a new id lifts the old
                    // contact first.
                    if was.id.is_some() {
                        out.push(touch(self.seat, slot, TouchPhase::Up, was.x, was.y, ts_us));
                    }
                    TouchPhase::Down
                };
                self.contacts[slot] = Contact { id: Some(id), x, y };
                out.push(touch(self.seat, slot, phase, x, y, ts_us));
            } else if was.id.is_some() {
                self.contacts[slot] = Contact::default();
                out.push(touch(self.seat, slot, TouchPhase::Up, was.x, was.y, ts_us));
            }
        }
    }

    fn parse_battery(&mut self, b: usize, data: &[u8], ts_us: u64, out: &mut Vec<InputEvent>) {
        let byte = data[b + 52];
        let charge = byte & 0x0f;
        let (state, percent) = match (byte >> 4) & 0x0f {
            0x0 => (BatteryState::Discharging, Some((charge.min(10)) * 10)),
            0x1 => (BatteryState::Charging, Some((charge.min(10)) * 10)),
            0x2 => (BatteryState::Full, Some(100)),
            _ => (BatteryState::Unknown, None),
        };
        if self.last_battery != Some((state, percent)) {
            self.last_battery = Some((state, percent));
            out.push(InputEvent::GamepadBattery {
                seat: self.seat,
                state,
                percent,
                ts_us,
            });
        }
    }
}

fn touch(seat: u8, pointer: usize, phase: TouchPhase, x: f32, y: f32, ts_us: u64) -> InputEvent {
    InputEvent::GamepadTouch {
        seat,
        pointer: pointer as u8,
        phase,
        x,
        y,
        pressure: 1.0,
        ts_us,
    }
}

/// The three DualSense button bytes as XInput `buttons`, with the touchpad
/// click and mic-mute in the extended high word. Byte layout is the pad's own
/// and matches altc's `altc-input` masks, so the round trip is the identity.
fn decode_buttons(b0: u8, b1: u8, b2: u8) -> u32 {
    let mut out = 0u32;
    // b0 low nibble: hat (8 = neutral); high nibble: face buttons.
    match b0 & 0x0f {
        0 => out |= gamepad::DPAD_UP,
        1 => out |= gamepad::DPAD_UP | gamepad::DPAD_RIGHT,
        2 => out |= gamepad::DPAD_RIGHT,
        3 => out |= gamepad::DPAD_RIGHT | gamepad::DPAD_DOWN,
        4 => out |= gamepad::DPAD_DOWN,
        5 => out |= gamepad::DPAD_DOWN | gamepad::DPAD_LEFT,
        6 => out |= gamepad::DPAD_LEFT,
        7 => out |= gamepad::DPAD_LEFT | gamepad::DPAD_UP,
        _ => {}
    }
    if b0 & 0x10 != 0 {
        out |= gamepad::X; // square
    }
    if b0 & 0x20 != 0 {
        out |= gamepad::A; // cross
    }
    if b0 & 0x40 != 0 {
        out |= gamepad::B; // circle
    }
    if b0 & 0x80 != 0 {
        out |= gamepad::Y; // triangle
    }
    // b1: shoulders, sticks, create/options. L2/R2 are the analog triggers;
    // their digital bits are redundant with the trigger axes.
    if b1 & 0x01 != 0 {
        out |= gamepad::LEFT_SHOULDER;
    }
    if b1 & 0x02 != 0 {
        out |= gamepad::RIGHT_SHOULDER;
    }
    if b1 & 0x10 != 0 {
        out |= gamepad::BACK; // create
    }
    if b1 & 0x20 != 0 {
        out |= gamepad::START; // options
    }
    if b1 & 0x40 != 0 {
        out |= gamepad::LEFT_STICK; // L3
    }
    if b1 & 0x80 != 0 {
        out |= gamepad::RIGHT_STICK; // R3
    }
    // b2: PS home, touchpad click, mic mute.
    if b2 & 0x01 != 0 {
        out |= gamepad::GUIDE;
    }
    if b2 & 0x02 != 0 {
        out |= gamepad::TOUCHPAD;
    }
    if b2 & 0x04 != 0 {
        out |= gamepad::MISC; // mic-mute button
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal USB report body with neutral sticks and a battery byte.
    fn report() -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x80; // LX centre
        r[1] = 0x80;
        r[2] = 0x80;
        r[3] = 0x80;
        r[7] = 0x08; // hat neutral, no face buttons
        r[52] = 0x08; // discharging, 80%
        // Both touch slots empty (not-touching bit set).
        r[32] = 0x80;
        r[36] = 0x80;
        r
    }

    fn buttons_of(events: &[InputEvent]) -> Option<u32> {
        events.iter().find_map(|e| match e {
            InputEvent::Gamepad(g) => Some(g.buttons),
            _ => None,
        })
    }

    #[test]
    fn neutral_report_reads_as_centred_sticks_and_no_buttons() {
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        p.parse(Connection::Usb, &report(), 1, &mut out);
        let pad = out.iter().find_map(|e| match e {
            InputEvent::Gamepad(g) => Some(*g),
            _ => None,
        });
        let pad = pad.expect("a pad snapshot");
        assert_eq!(pad.buttons, 0);
        assert!(
            pad.axes[..4].iter().all(|a| a.abs() < 300),
            "{:?}",
            pad.axes
        );
        assert_eq!(pad.axes[4], 0);
        assert_eq!(pad.axes[5], 0);
    }

    #[test]
    fn face_shoulder_and_extended_buttons_map_to_xinput() {
        let mut r = report();
        r[7] = 0x08 | 0x20 | 0x80; // cross + triangle
        r[8] = 0x01 | 0x20; // L1 + options
        r[9] = 0x02 | 0x04; // touchpad click + mic mute
        r[4] = 0xff; // full left trigger
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        p.parse(Connection::Usb, &r, 1, &mut out);
        let b = buttons_of(&out).unwrap();
        assert_eq!(b & gamepad::A, gamepad::A);
        assert_eq!(b & gamepad::Y, gamepad::Y);
        assert_eq!(b & gamepad::LEFT_SHOULDER, gamepad::LEFT_SHOULDER);
        assert_eq!(b & gamepad::START, gamepad::START);
        assert_eq!(b & gamepad::TOUCHPAD, gamepad::TOUCHPAD);
        assert_eq!(b & gamepad::MISC, gamepad::MISC);
        let lt = out.iter().find_map(|e| match e {
            InputEvent::Gamepad(g) => Some(g.axes[4]),
            _ => None,
        });
        assert!(lt.unwrap() > 32000);
    }

    #[test]
    fn only_changes_are_emitted() {
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        p.parse(Connection::Usb, &report(), 1, &mut out);
        assert!(!out.is_empty(), "first report establishes state");
        out.clear();
        p.parse(Connection::Usb, &report(), 2, &mut out);
        assert!(out.is_empty(), "an unchanged report emits nothing");
    }

    #[test]
    fn a_finger_reads_down_then_move_then_up() {
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        let mut r = report();
        // Slot 0 down: id 5, x=480, y=270 (touchData at body offset 32).
        r[32] = 0x05;
        r[33] = 480u16 as u8;
        r[34] = ((480u16 >> 8) as u8) | (((270u16 & 0x0f) as u8) << 4);
        r[35] = (270u16 >> 4) as u8;
        p.parse(Connection::Usb, &r, 1, &mut out);
        let down = out.iter().find_map(|e| match e {
            InputEvent::GamepadTouch { phase, x, .. } => Some((*phase, *x)),
            _ => None,
        });
        let (phase, x) = down.expect("a touch");
        assert_eq!(phase, TouchPhase::Down);
        assert!((x - 0.25).abs() < 0.01, "x={x}");
        // Move the same id.
        out.clear();
        r[33] = 960u16 as u8;
        r[34] = ((960u16 >> 8) as u8) | (((270u16 & 0x0f) as u8) << 4);
        p.parse(Connection::Usb, &r, 2, &mut out);
        assert!(out.iter().any(|e| matches!(
            e,
            InputEvent::GamepadTouch {
                phase: TouchPhase::Move,
                ..
            }
        )));
        // Lift.
        out.clear();
        r[32] = 0x80;
        p.parse(Connection::Usb, &r, 3, &mut out);
        assert!(out.iter().any(|e| matches!(
            e,
            InputEvent::GamepadTouch {
                phase: TouchPhase::Up,
                ..
            }
        )));
    }

    #[test]
    fn bluetooth_shifts_every_field_by_one() {
        let mut usb = report();
        usb[7] = 0x08 | 0x20; // cross
        let mut bt = vec![0u8; 78];
        bt[0] = 0x00; // BT header byte
        bt[1..1 + usb.len().min(77)].copy_from_slice(&usb[..usb.len().min(77)]);
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        p.parse(Connection::Bluetooth, &bt, 1, &mut out);
        assert_eq!(buttons_of(&out).unwrap() & gamepad::A, gamepad::A);
    }

    #[test]
    fn battery_charge_and_state_decode() {
        let mut r = report();
        r[52] = 0x1a; // charging (0x1), 10 units → but 0x0a>10 clamps
        let mut p = Parser::new(0);
        let mut out = Vec::new();
        p.parse(Connection::Usb, &r, 1, &mut out);
        let bat = out.iter().find_map(|e| match e {
            InputEvent::GamepadBattery { state, percent, .. } => Some((*state, *percent)),
            _ => None,
        });
        assert_eq!(bat, Some((BatteryState::Charging, Some(100))));
    }
}
