//! One controller vocabulary for every backend (spec 07, spec 16).
//!
//! Backends disagree about what a pad is. A Moonlight host takes rumble and,
//! if the client sends it, motion; a console's own protocol carries the pad
//! whole — touchpad, adaptive triggers, LED. The rule this module exists to
//! enforce is **model the richest backend and gate by capability**: the types
//! describe everything a pad can do, and [`PadCaps`] decides what a given
//! session actually carries. Never define the vocabulary as the intersection
//! of what today's backends support — that makes every later backend a
//! breaking change.
//!
//! Mapping is therefore one-way and lossy by design: a client announces its
//! real pad, and a backend drops what its wire cannot express. It must never
//! substitute something else.

/// What a pad is, so the host can present a matching virtual device.
///
/// The host preserves native identity where it can and normalises to
/// [`PadKind::Xbox`] where it cannot: a game that reads the pad's identity
/// shows the right button prompts, and one that does not is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PadKind {
    /// Layout known, identity not: an on-screen pad, or a controller no
    /// backend recognises.
    #[default]
    Generic,
    Xbox,
    DualShock4,
    DualSense,
    SwitchPro,
}

/// USB vendor ids. A pad's family is unambiguous from these; its *name* is
/// not, and the ambiguity is not theoretical — Android calls a DualSense
/// "Wireless Controller" and an Xbox pad "Xbox Wireless Controller", so any
/// substring match on the former silently claims the latter.
mod vendor {
    pub const SONY: u32 = 0x054C;
    pub const MICROSOFT: u32 = 0x045E;
    pub const NINTENDO: u32 = 0x057E;
}

/// Product ids worth distinguishing within a vendor.
mod product {
    /// Confirmed against hardware. Other Sony pads fall to DualShock 4, which
    /// claims less: a DS4 has a touchpad and a lightbar but no adaptive
    /// triggers, so a DualSense misread as a DS4 loses a feature, while the
    /// reverse promises one that is not there.
    pub const DUALSENSE: u32 = 0x0CE6;
}

impl PadKind {
    /// Identify a pad from what a platform can tell us about it.
    ///
    /// Shared deliberately: every client learns the same pad the same way, and
    /// the rules are testable here rather than written twice in two languages
    /// against two sets of half-remembered ids. Pass `0` for an id the
    /// platform does not expose — Apple, for instance, gives a name and a type
    /// but no USB ids, and should use its own type information first where it
    /// has it, since that is better evidence than any heuristic.
    ///
    /// Anything unrecognised is an **Xbox** pad, not a generic one: it is the
    /// layout every host emulates best, and it claims no feature a stranger
    /// pad might lack.
    #[must_use]
    pub fn identify(vendor_id: u32, product_id: u32, name: &str) -> Self {
        let name = name.to_lowercase();
        match vendor_id {
            vendor::SONY => {
                if product_id == product::DUALSENSE || name.contains("dualsense") {
                    Self::DualSense
                } else {
                    Self::DualShock4
                }
            }
            vendor::MICROSOFT => Self::Xbox,
            vendor::NINTENDO => Self::SwitchPro,
            // Names only where ids are absent or belong to an adapter. Most
            // specific first, because the generic names overlap.
            _ if name.contains("dualsense") => Self::DualSense,
            _ if name.contains("dualshock") => Self::DualShock4,
            _ if name.contains("xbox") => Self::Xbox,
            _ if name.contains("pro controller") => Self::SwitchPro,
            _ => Self::Xbox,
        }
    }

    /// Features implied by the family alone, whatever the platform reports.
    ///
    /// A client ORs these with what it can actually observe (sensors, a
    /// vibrator, a battery): the hardware in the box is a fact about the
    /// model, while what an OS chooses to expose is a fact about the OS.
    #[must_use]
    pub fn implied_caps(self) -> PadCaps {
        match self {
            Self::DualSense => {
                PadCaps::RUMBLE
                    | PadCaps::TRIGGER_RUMBLE
                    | PadCaps::MOTION
                    | PadCaps::TOUCHPAD
                    | PadCaps::ADAPTIVE_TRIGGERS
                    | PadCaps::LED
                    | PadCaps::BATTERY
            }
            Self::DualShock4 => {
                PadCaps::RUMBLE
                    | PadCaps::MOTION
                    | PadCaps::TOUCHPAD
                    | PadCaps::LED
                    | PadCaps::BATTERY
            }
            // Rumble is the only thing every remaining pad reliably has.
            _ => PadCaps::RUMBLE,
        }
    }
}

/// Features a pad has, or a wire can carry — one vocabulary for both.
///
/// A client announces what its pad **has** ([`GamepadProfile::caps`]); a
/// session reports what it can **carry** ([`crate::SessionCaps::pads`]).
/// Enable a feature only where both agree: capture that nothing transmits
/// costs battery for nothing, and feedback the pad cannot produce is silence
/// the user reads as a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PadCaps(u16);

impl PadCaps {
    pub const NONE: Self = Self(0);
    /// Dual-motor body rumble.
    pub const RUMBLE: Self = Self(1 << 0);
    /// Separate motors in the triggers.
    pub const TRIGGER_RUMBLE: Self = Self(1 << 1);
    /// Angular rate.
    pub const GYRO: Self = Self(1 << 2);
    /// Linear acceleration.
    pub const ACCEL: Self = Self(1 << 3);
    /// Absolute finger positions on a touch surface.
    pub const TOUCHPAD: Self = Self(1 << 4);
    /// Programmable trigger resistance ([`TriggerEffect`]).
    pub const ADAPTIVE_TRIGGERS: Self = Self(1 << 5);
    /// Host-set light colour.
    pub const LED: Self = Self(1 << 6);
    /// The pad reports its charge level.
    pub const BATTERY: Self = Self(1 << 7);

    /// Both motion sensors, which pads carry together in practice.
    pub const MOTION: Self = Self(Self::GYRO.0 | Self::ACCEL.0);

    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// True when every capability in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for PadCaps {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PadCaps {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// What both ends can do: the only correct basis for enabling a feature.
impl std::ops::BitAnd for PadCaps {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A pad as the client sees it, announced to the backend per seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadProfile {
    pub kind: PadKind,
    pub caps: PadCaps,
}

impl GamepadProfile {
    #[must_use]
    pub const fn new(kind: PadKind, caps: PadCaps) -> Self {
        Self { kind, caps }
    }
}

/// Which sensor a motion sample or request refers to. They are separate
/// streams: a host may want one and not the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionSensor {
    /// Angular rate, degrees per second.
    Gyro,
    /// Linear acceleration, m/s², **including gravity** — a pad at rest reads
    /// ~9.81 on one axis rather than zero.
    Accel,
}

/// One trigger's resistance profile.
///
/// Positions are along the pull, 0 released to 255 fully pressed, so they mean
/// the same thing on any pad deep enough to have them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TriggerEffect {
    /// Free travel.
    #[default]
    Off,
    /// This trigger is not part of the change: whatever it is already doing
    /// continues. Distinct from [`TriggerEffect::Off`], which cancels an
    /// effect the game may still want.
    Unchanged,
    /// A vendor effect passed through untouched.
    ///
    /// Some hosts forward the game's own effect blob rather than describing
    /// the effect, so there is nothing to interpret: a client holding the
    /// matching physical pad writes these bytes to it directly. Synthesising
    /// an approximation from them is worse than rendering nothing, because the
    /// parameters are a per-effect bit-packed structure and a wrong reading
    /// produces a trigger that fights the user.
    Raw { effect: u8, params: [u8; 10] },
    /// Constant resistance from `start` onwards.
    Feedback { start: u8, strength: u8 },
    /// Resistance between `start` and `end`, then a release past it — a
    /// trigger with a break.
    Weapon { start: u8, end: u8, strength: u8 },
    /// Vibration from `start`, at `frequency` Hz.
    Vibration {
        start: u8,
        strength: u8,
        frequency: u8,
    },
}

/// A trigger effect decoded to the shape trigger hardware takes: ten
/// discrete positions along the pull, each with its own strength.
///
/// The DualSense's own effect parameters are zone tables over ten positions,
/// and Apple's GameController API takes exactly ten positional values — so
/// this loses nothing on the only path that renders it today. Values are
/// normalized 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum DecodedTriggerEffect {
    Off,
    /// Leave whatever the trigger is already doing.
    Unchanged,
    /// Resistance per position.
    Feedback {
        strengths: [f32; 10],
    },
    /// Vibration amplitude per position, at a normalized frequency.
    Vibration {
        amplitudes: [f32; 10],
        frequency: f32,
    },
    /// Resistance from `start` to `end`, then release — a trigger break.
    Weapon {
        start: f32,
        end: f32,
        strength: f32,
    },
    /// An opcode this decoder does not know. Render nothing: a wrong
    /// reading produces a trigger that fights the user.
    Unknown {
        effect: u8,
    },
}

impl TriggerEffect {
    /// Decode to positional form, including the raw DualSense opcodes.
    ///
    /// The raw formats are zone tables: a 10-bit active-zone mask in the
    /// first two bytes, then ten 3-bit values packed LSB-first from the
    /// third byte. Verified against live captures — uniform-force blobs
    /// unpack to the same value in every zone, which a wrong bit layout
    /// cannot produce.
    #[must_use]
    pub fn decoded(self) -> DecodedTriggerEffect {
        /// DualSense trigger effect opcodes, as they arrive on the wire.
        const OFF: u8 = 0x00;
        const RELEASE: u8 = 0x05;
        const FEEDBACK: u8 = 0x21;
        const WEAPON: u8 = 0x25;
        const VIBRATION: u8 = 0x26;

        // Zone fields store strength − 1 (the sender never emits strength
        // 0 — that would be Off), so a field of 3 means 4 of 8. Rendering
        // field/7 instead is a full step weak on every zone, which is
        // exactly what "works but does not feel like local" reports.
        let zones = |params: [u8; 10]| -> [f32; 10] {
            let mask = u16::from_le_bytes([params[0], params[1]]);
            let packed = u32::from_le_bytes([params[2], params[3], params[4], params[5]]);
            let mut out = [0.0f32; 10];
            for (i, slot) in out.iter_mut().enumerate() {
                if mask & (1 << i) != 0 {
                    let field = (packed >> (3 * i)) & 0x7;
                    #[allow(clippy::cast_precision_loss)]
                    {
                        *slot = (field + 1) as f32 / 8.0;
                    }
                }
            }
            out
        };

        match self {
            Self::Off => DecodedTriggerEffect::Off,
            Self::Unchanged => DecodedTriggerEffect::Unchanged,
            Self::Feedback { start, strength } => {
                let mut strengths = [0.0f32; 10];
                let first = usize::from(start) * 10 / 256;
                for slot in strengths.iter_mut().skip(first) {
                    *slot = f32::from(strength) / 255.0;
                }
                DecodedTriggerEffect::Feedback { strengths }
            }
            Self::Weapon {
                start,
                end,
                strength,
            } => DecodedTriggerEffect::Weapon {
                start: f32::from(start) / 255.0,
                end: f32::from(end) / 255.0,
                strength: f32::from(strength) / 255.0,
            },
            Self::Vibration {
                start,
                strength,
                frequency,
            } => {
                let mut amplitudes = [0.0f32; 10];
                let first = usize::from(start) * 10 / 256;
                for slot in amplitudes.iter_mut().skip(first) {
                    *slot = f32::from(strength) / 255.0;
                }
                DecodedTriggerEffect::Vibration {
                    amplitudes,
                    frequency: f32::from(frequency) / 255.0,
                }
            }
            Self::Raw { effect, params } => match effect {
                // 0x05 is the official Off — neutral position, no params.
                OFF | RELEASE => DecodedTriggerEffect::Off,
                FEEDBACK => DecodedTriggerEffect::Feedback {
                    strengths: zones(params),
                },
                VIBRATION => DecodedTriggerEffect::Vibration {
                    amplitudes: zones(params),
                    frequency: f32::from(params[8]) / 255.0,
                },
                WEAPON => {
                    let mask = u16::from_le_bytes([params[0], params[1]]);
                    let first = (0..10).find(|i| mask & (1 << i) != 0).unwrap_or(0);
                    let last = (0..10).rfind(|i| mask & (1 << i) != 0).unwrap_or(9);
                    #[allow(clippy::cast_precision_loss)]
                    DecodedTriggerEffect::Weapon {
                        start: first as f32 / 9.0,
                        end: last as f32 / 9.0,
                        strength: f32::from((params[2] & 0x7) + 1) / 8.0,
                    }
                }
                other => DecodedTriggerEffect::Unknown { effect: other },
            },
        }
    }
}

/// Something the host asks a pad to do. Backend-neutral: a variant here means
/// the same on every wire, and a backend emits only what its
/// [`crate::SessionCaps::pads`] claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GamepadFeedback {
    /// Body rumble, 16-bit amplitudes, low- and high-frequency motor.
    Rumble { seat: u8, low: u16, high: u16 },
    /// Trigger motors, independent of body rumble.
    TriggerRumble { seat: u8, left: u16, right: u16 },
    /// Trigger resistance until the next change; not a one-shot.
    AdaptiveTriggers {
        seat: u8,
        left: TriggerEffect,
        right: TriggerEffect,
    },
    /// Light colour, sRGB.
    Led { seat: u8, rgb: [u8; 3] },
}

impl GamepadFeedback {
    /// Which seat this is for — every variant names one.
    #[must_use]
    pub const fn seat(&self) -> u8 {
        match *self {
            Self::Rumble { seat, .. }
            | Self::TriggerRumble { seat, .. }
            | Self::AdaptiveTriggers { seat, .. }
            | Self::Led { seat, .. } => seat,
        }
    }

    /// The capability a pad needs to render this. An embedder that lacks it
    /// drops the effect rather than approximating it with another motor.
    #[must_use]
    pub const fn requires(&self) -> PadCaps {
        match *self {
            Self::Rumble { .. } => PadCaps::RUMBLE,
            Self::TriggerRumble { .. } => PadCaps::TRIGGER_RUMBLE,
            Self::AdaptiveTriggers { .. } => PadCaps::ADAPTIVE_TRIGGERS,
            Self::Led { .. } => PadCaps::LED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DecodedTriggerEffect, GamepadFeedback, PadCaps, PadKind, TriggerEffect};

    /// The zone unpack against live captures: a host sending uniform force
    /// produces the same 3-bit value in every zone, which a wrong bit layout
    /// cannot fake across four different blobs.
    #[test]
    fn captured_dualsense_blobs_decode_to_uniform_zones() {
        // HZD bow draw, two strengths (captured live).
        let strong = TriggerEffect::Raw {
            effect: 0x21,
            params: [255, 3, 219, 182, 109, 27, 0, 0, 0, 0],
        };
        let DecodedTriggerEffect::Feedback { strengths } = strong.decoded() else {
            panic!("feedback opcode must decode to feedback");
        };
        for s in strengths {
            // Field 3 means strength 4 of 8: the fields store strength − 1.
            assert!((s - 0.5).abs() < 1e-6, "expected uniform 4/8, got {s}");
        }

        let vibration = TriggerEffect::Raw {
            effect: 0x26,
            params: [255, 3, 73, 146, 36, 9, 0, 0, 50, 0],
        };
        let DecodedTriggerEffect::Vibration {
            amplitudes,
            frequency,
        } = vibration.decoded()
        else {
            panic!("vibration opcode must decode to vibration");
        };
        for a in amplitudes {
            assert!((a - 0.25).abs() < 1e-6, "expected uniform 2/8, got {a}");
        }
        assert!((frequency - 50.0 / 255.0).abs() < 1e-6);
    }

    /// Opcode 5 is the official Off; a genuinely unknown opcode renders
    /// nothing rather than a guess.
    #[test]
    fn release_is_off_and_unknown_stays_unknown() {
        let release = TriggerEffect::Raw {
            effect: 0x05,
            params: [0; 10],
        };
        assert_eq!(release.decoded(), DecodedTriggerEffect::Off);
        let mystery = TriggerEffect::Raw {
            effect: 0x33,
            params: [1; 10],
        };
        assert_eq!(
            mystery.decoded(),
            DecodedTriggerEffect::Unknown { effect: 0x33 }
        );
    }

    /// The trap that produced a real bug: an Xbox pad announced as a
    /// PlayStation one because its name contains the other's alias, so the
    /// host built a DS4 and the client claimed a lightbar that does not exist.
    #[test]
    fn a_pad_named_like_another_is_identified_by_its_vendor() {
        // Real ids, from hardware.
        assert_eq!(
            PadKind::identify(0x045E, 0x0B13, "Xbox Wireless Controller"),
            PadKind::Xbox
        );
        assert_eq!(
            PadKind::identify(0x054C, 0x0CE6, "Wireless Controller"),
            PadKind::DualSense
        );
    }

    #[test]
    fn a_sony_pad_that_is_not_a_known_dualsense_claims_less_rather_than_more() {
        // Unknown Sony product: a DS4 has no adaptive triggers, so this loses
        // a feature rather than promising a missing one.
        let kind = PadKind::identify(0x054C, 0x9999, "Wireless Controller");
        assert_eq!(kind, PadKind::DualShock4);
        assert!(!kind.implied_caps().contains(PadCaps::ADAPTIVE_TRIGGERS));
        assert!(kind.implied_caps().contains(PadCaps::TOUCHPAD));
        // A named Edge is still a DualSense even with an id we do not know.
        assert_eq!(
            PadKind::identify(0x054C, 0x0DF2, "DualSense Edge Wireless Controller"),
            PadKind::DualSense
        );
    }

    #[test]
    fn names_identify_a_pad_when_ids_are_missing() {
        // Apple exposes a name and a type but no USB ids.
        assert_eq!(
            PadKind::identify(0, 0, "Xbox One Controller"),
            PadKind::Xbox
        );
        assert_eq!(
            PadKind::identify(0, 0, "DualShock 4 Wireless Controller"),
            PadKind::DualShock4
        );
        assert_eq!(
            PadKind::identify(0, 0, "Pro Controller"),
            PadKind::SwitchPro
        );
    }

    /// An unknown pad is announced as an Xbox one: the layout hosts emulate
    /// best, claiming nothing a stranger pad might lack.
    #[test]
    fn an_unknown_pad_is_an_xbox_pad() {
        let kind = PadKind::identify(0x1234, 0x5678, "Generic Controller");
        assert_eq!(kind, PadKind::Xbox);
        assert_eq!(kind.implied_caps(), PadCaps::RUMBLE);
    }

    #[test]
    fn capabilities_combine_and_test_as_a_set() {
        let pad = PadCaps::RUMBLE | PadCaps::MOTION | PadCaps::TOUCHPAD;
        assert!(pad.contains(PadCaps::GYRO));
        assert!(pad.contains(PadCaps::GYRO | PadCaps::ACCEL));
        assert!(!pad.contains(PadCaps::ADAPTIVE_TRIGGERS));
        assert!(!pad.contains(PadCaps::RUMBLE | PadCaps::LED));
        assert!(PadCaps::NONE.is_empty());
    }

    #[test]
    fn a_feature_is_live_only_where_pad_and_wire_agree() {
        // The pad has motion and adaptive triggers; the wire carries motion
        // and rumble. Only motion may be enabled.
        let pad = PadCaps::MOTION | PadCaps::ADAPTIVE_TRIGGERS;
        let wire = PadCaps::MOTION | PadCaps::RUMBLE;
        let live = pad & wire;
        assert!(live.contains(PadCaps::MOTION));
        assert!(!live.contains(PadCaps::ADAPTIVE_TRIGGERS));
        assert!(!live.contains(PadCaps::RUMBLE));
    }

    #[test]
    fn every_effect_names_its_seat_and_its_requirement() {
        let effects = [
            GamepadFeedback::Rumble {
                seat: 1,
                low: 0,
                high: 0,
            },
            GamepadFeedback::TriggerRumble {
                seat: 1,
                left: 0,
                right: 0,
            },
            GamepadFeedback::AdaptiveTriggers {
                seat: 1,
                left: TriggerEffect::Off,
                right: TriggerEffect::Weapon {
                    start: 40,
                    end: 160,
                    strength: 200,
                },
            },
            GamepadFeedback::Led {
                seat: 1,
                rgb: [0, 0, 0],
            },
        ];
        for effect in effects {
            assert_eq!(effect.seat(), 1);
            // A requirement of NONE would let an unsupported effect through
            // the capability gate.
            assert!(!effect.requires().is_empty());
        }
    }
}
