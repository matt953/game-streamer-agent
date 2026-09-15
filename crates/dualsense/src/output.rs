//! [`Effects`] into the output report that makes the pad do them.
//!
//! The report is the real controller's, and the framing is SDL's
//! (`SDL_hidapi_ps5.c`), because SDL is what every desktop client uses to
//! drive a DualSense: over USB a 48-byte report `0x02`, over Bluetooth a
//! 78-byte report `0x31` with a tag, a magic byte and a CRC32 the pad checks
//! before acting on anything. Between the two framings sits the same 47-byte
//! block, which is also exactly what altc's `altc-input` decodes on the far
//! side when it emulates a pad — this is the inverse of that, and the two are
//! tested against each other rather than against numbers of our own.
//!
//! No I/O: the caller moves the bytes, over WebHID in a browser or hidraw on
//! a desktop.

use crate::input::Connection;

/// The 47-byte block both framings carry.
const COMMON_LEN: usize = 47;
/// Total bytes of each framing, report id included.
const USB_LEN: usize = 48;
const BT_LEN: usize = 78;
/// Where the common block sits in each framing.
const USB_COMMON_AT: usize = 1;
const BT_COMMON_AT: usize = 3;

/// Output report ids.
const REPORT_USB: u8 = 0x02;
const REPORT_BT: u8 = 0x31;

/// `ucEnableBits1`: the motor levels are the legacy emulation's.
const FLAG1_RUMBLE: u8 = 0x01;
/// `ucEnableBits1`: silence the pad's audio haptics while the motors are
/// being driven. Without it the pad plays both, and the rumble is mush.
const FLAG1_MUTE_HAPTICS: u8 = 0x02;
/// `ucEnableBits3`: the motor levels are the improved emulation's, which is
/// what firmware 2.24 and newer want.
const FLAG3_RUMBLE: u8 = 0x04;

/// `ucEnableBits1`: the trigger blocks are to be applied. The same values
/// the stream uses to say which triggers a message addresses.
const FLAG1_RIGHT_TRIGGER: u8 = 0x04;
const FLAG1_LEFT_TRIGGER: u8 = 0x08;

/// Offsets in the common block.
const RUMBLE_RIGHT: usize = 2;
const RUMBLE_LEFT: usize = 3;
const RIGHT_TRIGGER: usize = 10;
const LEFT_TRIGGER: usize = 21;
const ENABLE_BITS_3: usize = 38;

/// One trigger's effect as the pad takes it: the effect type, then ten
/// parameter bytes whose meaning depends on the type. Passed through from
/// the game untouched — the parameters are bit-packed per effect, and a
/// wrong reading produces a trigger that fights the player.
pub const TRIGGER_BLOCK_LEN: usize = 11;
/// A block that turns the trigger's resistance off.
pub const TRIGGER_OFF: [u8; TRIGGER_BLOCK_LEN] = [0; TRIGGER_BLOCK_LEN];

/// The byte the pad's Bluetooth CRC is seeded with: the HID header, which is
/// part of the calculation but not part of the report.
const BT_CRC_TAG: u8 = 0xA2;

/// The firmware from which the pad has the improved rumble emulation.
const ENHANCED_RUMBLE_FIRMWARE: u16 = 0x0224;

/// What the pad should be doing. A whole report is built from this every
/// time, because the report carries every effect at once: sending one that
/// mentions only the motors would stop anything else the pad was doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effects {
    /// Body rumble as the stream carries it: the low-frequency (heavy) motor
    /// and the high-frequency (light) one, full scale `u16`.
    pub rumble: (u16, u16),
    /// Each trigger's effect block, once the game has set one. `None` is a
    /// trigger nobody has spoken to yet, which is left alone rather than told
    /// anything; [`TRIGGER_OFF`] is a trigger the game has released.
    pub left_trigger: Option<[u8; TRIGGER_BLOCK_LEN]>,
    pub right_trigger: Option<[u8; TRIGGER_BLOCK_LEN]>,
}

/// One report, as a HID transport wants it: the id, and the bytes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputReport {
    pub report_id: u8,
    pub data: Vec<u8>,
}

/// Builds output reports for one pad.
#[derive(Debug, Default)]
pub struct OutputEncoder {
    /// Whether this pad takes the improved rumble emulation. Assumed until
    /// its firmware says otherwise, as SDL assumes it for a pad whose
    /// firmware it could not read.
    enhanced_rumble: bool,
}

impl OutputEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enhanced_rumble: true,
        }
    }

    /// Tell the encoder this pad's firmware version, from feature report
    /// `0x20` without its report id.
    ///
    /// Optional: a pad whose firmware cannot be read is driven the improved
    /// way, which is what SDL does with the same unknown. Returns the version
    /// it read, for a client that wants to log what it is talking to.
    pub fn set_firmware(&mut self, body: &[u8]) -> Option<u16> {
        // SDL reads the pair at 44 and 45 of a report that still has its id.
        let version = u16::from_le_bytes([*body.get(43)?, *body.get(44)?]);
        self.enhanced_rumble = version == 0 || version >= ENHANCED_RUMBLE_FIRMWARE;
        Some(version)
    }

    /// The report that makes the pad do `effects`, framed for `conn`.
    #[must_use]
    pub fn report(&mut self, conn: Connection, effects: &Effects) -> OutputReport {
        let mut common = [0u8; COMMON_LEN];
        let (low, high) = effects.rumble;
        if low != 0 || high != 0 {
            // The inverse of the widening a reader applies, so a level
            // survives the round trip rather than drifting by a count.
            let level = |v: u16| (v / 257) as u8;
            if self.enhanced_rumble {
                common[ENABLE_BITS_3] |= FLAG3_RUMBLE;
                common[RUMBLE_LEFT] = level(low);
                common[RUMBLE_RIGHT] = level(high);
            } else {
                common[0] |= FLAG1_RUMBLE;
                // Halved, as SDL halves it: the legacy emulation is stronger
                // than the motors every other pad has, and a game's idea of
                // "half strength" should feel like half strength here too.
                common[RUMBLE_LEFT] = level(low) >> 1;
                common[RUMBLE_RIGHT] = level(high) >> 1;
            }
            common[0] |= FLAG1_MUTE_HAPTICS;
        }
        // A trigger the game has set is carried in every report, flagged, so
        // a report sent for the motors does not read to the pad as a trigger
        // going quiet. Resending the block it already has changes nothing.
        if let Some(block) = effects.right_trigger {
            common[0] |= FLAG1_RIGHT_TRIGGER;
            common[RIGHT_TRIGGER..RIGHT_TRIGGER + TRIGGER_BLOCK_LEN].copy_from_slice(&block);
        }
        if let Some(block) = effects.left_trigger {
            common[0] |= FLAG1_LEFT_TRIGGER;
            common[LEFT_TRIGGER..LEFT_TRIGGER + TRIGGER_BLOCK_LEN].copy_from_slice(&block);
        }
        // Leaving the rumble bits clear is what gives the pad its audio
        // haptics back, so a zeroed report is how rumble stops.
        match conn {
            Connection::Usb => {
                let mut frame = [0u8; USB_LEN];
                frame[0] = REPORT_USB;
                frame[USB_COMMON_AT..USB_COMMON_AT + COMMON_LEN].copy_from_slice(&common);
                OutputReport {
                    report_id: REPORT_USB,
                    data: frame[1..].to_vec(),
                }
            }
            Connection::Bluetooth => {
                let mut frame = [0u8; BT_LEN];
                frame[0] = REPORT_BT;
                // Tag and sequence, then the magic byte the pad expects.
                frame[1] = 0x00;
                frame[2] = 0x10;
                frame[BT_COMMON_AT..BT_COMMON_AT + COMMON_LEN].copy_from_slice(&common);
                let crc = crc32(&[BT_CRC_TAG], 0);
                let end = BT_LEN - 4;
                let crc = crc32(&frame[..end], crc);
                frame[end..].copy_from_slice(&crc.to_le_bytes());
                OutputReport {
                    report_id: REPORT_BT,
                    data: frame[1..].to_vec(),
                }
            }
        }
    }
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Reflected CRC32 (poly `0xEDB88320`) with a chainable seed, which is how
/// the pad's Bluetooth framing wants it: the message CRC continues from the
/// CRC of the single tag byte.
fn crc32(buf: &[u8], seed: u32) -> u32 {
    let mut c = seed ^ 0xFFFF_FFFF;
    for &b in buf {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::{Connection, Effects, OutputEncoder, crc32};

    #[test]
    fn usb_rumble_lands_in_the_motor_bytes() {
        let mut encoder = OutputEncoder::new();
        let report = encoder.report(
            Connection::Usb,
            &Effects {
                rumble: (0xFFFF, 0x8080),
                ..Effects::default()
            },
        );
        assert_eq!(report.report_id, 0x02);
        assert_eq!(report.data.len(), 47, "the common block, without its id");
        // Offsets 2 and 3 of the common block, which is the whole payload here.
        assert_eq!(report.data[3], 255, "left motor is the low-frequency one");
        assert_eq!(report.data[2], 0x80);
        assert_eq!(
            report.data[38] & 0x04,
            0x04,
            "improved emulation by default"
        );
        assert_eq!(report.data[0] & 0x02, 0x02, "audio haptics muted");
    }

    #[test]
    fn old_firmware_takes_the_legacy_emulation_at_half_strength() {
        let mut encoder = OutputEncoder::new();
        let mut firmware = [0u8; 63];
        firmware[43..45].copy_from_slice(&0x0223u16.to_le_bytes());
        assert_eq!(encoder.set_firmware(&firmware), Some(0x0223));
        let report = encoder.report(
            Connection::Usb,
            &Effects {
                rumble: (0xFFFF, 0),
                ..Effects::default()
            },
        );
        assert_eq!(report.data[0] & 0x01, 0x01, "legacy bit");
        assert_eq!(report.data[38] & 0x04, 0, "not the improved bit");
        assert_eq!(report.data[3], 127);

        // And the firmware that introduced the improved emulation takes it.
        firmware[43..45].copy_from_slice(&0x0224u16.to_le_bytes());
        encoder.set_firmware(&firmware);
        let report = encoder.report(
            Connection::Usb,
            &Effects {
                rumble: (0xFFFF, 0),
                ..Effects::default()
            },
        );
        assert_eq!(report.data[38] & 0x04, 0x04);
        assert_eq!(report.data[3], 255);
    }

    /// A pad refuses a Bluetooth report whose CRC does not check out, so this
    /// is the difference between rumble and silence over Bluetooth.
    #[test]
    fn bluetooth_is_framed_and_signed_the_way_the_pad_checks() {
        let mut encoder = OutputEncoder::new();
        let report = encoder.report(
            Connection::Bluetooth,
            // Levels a reader gives back unchanged: the widening is ×257, so
            // these are exact on the way out and the way back.
            &Effects {
                rumble: (0x4040, 0x2020),
                ..Effects::default()
            },
        );
        assert_eq!(report.report_id, 0x31);
        assert_eq!(report.data.len(), 77);
        // Tag, magic, then the common block three bytes into the frame.
        assert_eq!(report.data[0], 0x00);
        assert_eq!(report.data[1], 0x10);
        assert_eq!(report.data[2 + 3], 0x40, "left motor");
        assert_eq!(report.data[2 + 2], 0x20, "right motor");
        assert_eq!(report.data[2 + 38] & 0x04, 0x04);

        // Recompute the way the pad does: the HID header byte, then the whole
        // report including its id, up to the CRC itself.
        let mut frame = vec![0x31u8];
        frame.extend_from_slice(&report.data);
        let end = frame.len() - 4;
        let want = crc32(&frame[..end], crc32(&[0xA2], 0));
        assert_eq!(&frame[end..], &want.to_le_bytes());
    }

    /// The canonical CRC-32 check value. The pad's framing is only as good as
    /// this being the same CRC-32 everyone else means — a private variant
    /// would pass every test written against itself and be refused by the
    /// controller.
    #[test]
    fn the_crc_is_the_standard_one() {
        assert_eq!(crc32(b"123456789", 0), 0xCBF4_3926);
        // And seeding is chaining: the tag byte then the rest is one CRC over
        // both, which is what lets the pad's header take part in it.
        assert_eq!(crc32(b"56789", crc32(b"1234", 0)), crc32(b"123456789", 0),);
    }

    #[test]
    fn a_trigger_effect_lands_in_its_block_and_is_flagged() {
        let mut encoder = OutputEncoder::new();
        let mut weapon = [0u8; 11];
        weapon[0] = 0x25;
        weapon[1..4].copy_from_slice(&[0x11, 0x22, 0x33]);
        let report = encoder.report(
            Connection::Usb,
            &Effects {
                right_trigger: Some(weapon),
                ..Effects::default()
            },
        );
        assert_eq!(report.data[0] & 0x04, 0x04, "right trigger flagged");
        assert_eq!(report.data[0] & 0x08, 0, "left trigger left alone");
        assert_eq!(&report.data[10..21], &weapon, "the block, verbatim");
        assert!(report.data[21..32].iter().all(|&b| b == 0));

        // Both, and the motors, ride one report: it carries every effect at
        // once, so nothing the pad was doing is dropped by mentioning another.
        let report = encoder.report(
            Connection::Usb,
            &Effects {
                rumble: (0xFFFF, 0),
                left_trigger: Some(super::TRIGGER_OFF),
                right_trigger: Some(weapon),
            },
        );
        assert_eq!(report.data[0] & 0x0c, 0x0c);
        assert_eq!(report.data[3], 255);
        assert_eq!(&report.data[10..21], &weapon);
    }

    #[test]
    fn silence_clears_every_rumble_bit() {
        let mut encoder = OutputEncoder::new();
        let report = encoder.report(Connection::Usb, &Effects::default());
        assert!(
            report.data.iter().all(|&b| b == 0),
            "a report that mentions nothing is what gives the pad back to itself"
        );
    }
}
