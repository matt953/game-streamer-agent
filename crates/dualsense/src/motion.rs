//! The pad's inertial sensors: raw report words into the units the wire wants.
//!
//! A DualSense reports gyro and accelerometer as raw `i16` counts whose scale
//! is the individual pad's, not the model's. Feature report `0x05` carries
//! that pad's own factory calibration, and applying it is what makes two
//! controllers agree. The arithmetic here is SDL's (`SDL_hidapi_ps5.c`),
//! because SDL is what every desktop Moonlight client runs and so is what the
//! host's games were tuned against — including its refusal of a calibration
//! that reads as nonsense, and the fallback scaling it uses instead.
//!
//! The units are the stream protocol's: **deg/s** for gyro and **m/s²
//! including gravity** for acceleration. SDL itself reports gyro in rad/s and
//! the desktop client multiplies by 180/π on the way to the wire; doing both
//! would be a round trip through a constant, so the conversion is folded in
//! here and the wire units are produced directly.

/// Raw counts per degree per second, before a pad's own sensitivity.
const GYRO_RES_PER_DEGREE: f32 = 1024.0;
/// Raw counts per g, before a pad's own sensitivity.
const ACCEL_RES_PER_G: f32 = 8192.0;
/// Standard gravity, the same constant SDL scales acceleration by.
const STANDARD_GRAVITY: f32 = 9.806_65;

/// The calibration report's own length, without its report id. Shorter than
/// this and the fields SDL reads are not all present.
const CALIBRATION_MIN: usize = 34;

/// One sensor axis: the zero the pad reports at rest, and what one count is
/// worth on this particular pad.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Axis {
    bias: f32,
    sensitivity: f32,
}

/// A pad's factory calibration for its six sensor axes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// Gyro pitch/yaw/roll, then accelerometer x/y/z.
    axes: [Axis; 6],
}

impl Default for Calibration {
    fn default() -> Self {
        Self::UNCALIBRATED
    }
}

impl Calibration {
    /// What to use before the pad's own numbers are known, or when they are
    /// not believable: SDL's fallback, which scales gyro counts by 64 and
    /// takes accelerometer counts as they come.
    pub const UNCALIBRATED: Self = Self {
        axes: [
            Axis {
                bias: 0.0,
                sensitivity: 64.0,
            },
            Axis {
                bias: 0.0,
                sensitivity: 64.0,
            },
            Axis {
                bias: 0.0,
                sensitivity: 64.0,
            },
            Axis {
                bias: 0.0,
                sensitivity: 1.0,
            },
            Axis {
                bias: 0.0,
                sensitivity: 1.0,
            },
            Axis {
                bias: 0.0,
                sensitivity: 1.0,
            },
        ],
    };

    /// Read feature report `0x05`, without its report id.
    ///
    /// A short report, or one whose numbers do not describe a working sensor,
    /// gives [`Calibration::UNCALIBRATED`] rather than an error: a pad with a
    /// bad calibration still has to work, and SDL treats it the same way.
    #[must_use]
    pub fn parse(body: &[u8]) -> Self {
        if body.len() < CALIBRATION_MIN {
            return Self::UNCALIBRATED;
        }
        let word = |at: usize| i16::from_le_bytes([body[at], body[at + 1]]);
        let (pitch_bias, yaw_bias, roll_bias) = (word(0), word(2), word(4));
        let (pitch_plus, pitch_minus) = (word(6), word(8));
        let (yaw_plus, yaw_minus) = (word(10), word(12));
        let (roll_plus, roll_minus) = (word(14), word(16));
        let (speed_plus, speed_minus) = (word(18), word(20));

        // Every denominator here is a difference of a plus and a minus
        // extreme, which is zero only on a pad whose calibration is missing
        // rather than merely odd. Dividing there would give an infinity that
        // the plausibility check below would catch — but only after the
        // division, and a NaN compares false against every bound.
        let span = |plus: i16, minus: i16| f32::from(plus) - f32::from(minus);
        let numerator = (f32::from(speed_plus) + f32::from(speed_minus)) * GYRO_RES_PER_DEGREE;
        let gyro = |bias: i16, plus: i16, minus: i16| {
            let range = span(plus, minus);
            (range != 0.0).then(|| Axis {
                bias: f32::from(bias),
                sensitivity: numerator / range,
            })
        };
        let accel = |plus: i16, minus: i16| {
            let range = span(plus, minus);
            (range != 0.0).then(|| Axis {
                // The zero sits halfway between the two extremes, wherever
                // the pair happens to straddle it.
                bias: f32::from(plus) - range / 2.0,
                sensitivity: 2.0 * ACCEL_RES_PER_G / range,
            })
        };
        let axes = [
            gyro(pitch_bias, pitch_plus, pitch_minus),
            gyro(yaw_bias, yaw_plus, yaw_minus),
            gyro(roll_bias, roll_plus, roll_minus),
            accel(word(22), word(24)),
            accel(word(26), word(28)),
            accel(word(30), word(32)),
        ];
        let mut out = Self::UNCALIBRATED;
        for (slot, axis) in out.axes.iter_mut().zip(axes) {
            match axis {
                Some(axis) => *slot = axis,
                None => return Self::UNCALIBRATED,
            }
        }
        if out.is_plausible() {
            out
        } else {
            Self::UNCALIBRATED
        }
    }

    /// Whether these numbers describe a sensor that could exist.
    ///
    /// Pads ship with calibration that is occasionally garbage, and a wild
    /// sensitivity turns a still controller into one spinning at thousands of
    /// degrees a second. The bounds are SDL's: a zero no further than 1024
    /// counts off, and a sensitivity within half of the nominal one.
    fn is_plausible(&self) -> bool {
        self.axes.iter().enumerate().all(|(i, axis)| {
            let nominal = if i < 3 { 64.0 } else { 1.0 };
            axis.bias.abs() <= 1024.0 && (1.0 - axis.sensitivity / nominal).abs() <= 0.5
        })
    }

    /// Gyro pitch, yaw and roll in degrees per second.
    #[must_use]
    pub fn gyro_deg_s(&self, raw: [i16; 3]) -> [f32; 3] {
        std::array::from_fn(|i| {
            let axis = self.axes[i];
            (f32::from(raw[i]) - axis.bias) * axis.sensitivity / GYRO_RES_PER_DEGREE
        })
    }

    /// Acceleration x, y and z in m/s², gravity included.
    #[must_use]
    pub fn accel_ms2(&self, raw: [i16; 3]) -> [f32; 3] {
        std::array::from_fn(|i| {
            let axis = self.axes[i + 3];
            (f32::from(raw[i]) - axis.bias) * axis.sensitivity / ACCEL_RES_PER_G * STANDARD_GRAVITY
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Calibration;

    /// The calibration report of a real DualSense, read over hidraw from the
    /// pad on this desk (`CUH-ZCT1W`, USB). Not a hand-written fixture: a
    /// fixture written to match the parser proves only that the parser did
    /// not change.
    const REAL_PAD: [u8; 40] = [
        0x00, 0x00, 0x01, 0x00, 0x03, 0x00, 0x63, 0x22, 0x9f, 0xdd, 0x4e, 0x22, 0xb7, 0xdd, 0x0d,
        0x23, 0xfb, 0xdc, 0x1c, 0x02, 0x1c, 0x02, 0x05, 0x20, 0x1c, 0xe0, 0x26, 0x20, 0x2e, 0xe0,
        0xe5, 0x1f, 0x07, 0xe0, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// One input report from the same pad, lying still on the desk. At rest
    /// the only force on it is gravity, so the three acceleration axes must
    /// come to one g between them however they are scaled — the check that
    /// catches a swapped axis, a byte-order slip or a wrong divisor, none of
    /// which a fixture of my own numbers would catch.
    const AT_REST: [i16; 3] = [-185, 8108, 1319];
    const AT_REST_GYRO: [i16; 3] = [2, 4, 0];

    #[test]
    fn a_real_pads_calibration_puts_gravity_at_one_g() {
        let calibration = Calibration::parse(&REAL_PAD);
        assert_ne!(
            calibration,
            Calibration::UNCALIBRATED,
            "this pad's numbers are good and must be used"
        );
        let accel = calibration.accel_ms2(AT_REST);
        let magnitude = accel.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (magnitude - 9.80665).abs() < 0.25,
            "a still pad feels one g, got {magnitude} from {accel:?}"
        );
        // Lying flat, gravity is almost all on one axis.
        assert!(accel[1] > 9.0, "{accel:?}");

        // And a pad that is not turning is not turning: the bias is what makes
        // the small counts at rest read as zero rather than a slow drift.
        let gyro = calibration.gyro_deg_s(AT_REST_GYRO);
        assert!(
            gyro.iter().all(|v| v.abs() < 1.0),
            "a still pad has no rotation, got {gyro:?}"
        );
    }

    #[test]
    fn a_calibration_that_cannot_be_true_is_refused() {
        // Too short to hold the fields at all.
        assert_eq!(Calibration::parse(&[0; 20]), Calibration::UNCALIBRATED);
        // All zeroes: every span is zero, which would divide by nothing.
        assert_eq!(Calibration::parse(&[0; 40]), Calibration::UNCALIBRATED);
        // Plausible in shape, absurd in scale: a sensitivity far from nominal
        // would have a still pad spinning.
        let mut wild = REAL_PAD;
        wild[6..8].copy_from_slice(&100i16.to_le_bytes());
        wild[8..10].copy_from_slice(&(-100i16).to_le_bytes());
        assert_eq!(Calibration::parse(&wild), Calibration::UNCALIBRATED);
    }

    #[test]
    fn without_calibration_the_fallback_still_gives_real_units() {
        // SDL's fallback, which every pad gets until its report is read: gyro
        // counts times 64, acceleration counts as they come.
        let c = Calibration::UNCALIBRATED;
        assert!((c.gyro_deg_s([1024, 0, 0])[0] - 64.0).abs() < 0.001);
        assert!((c.accel_ms2([8192, 0, 0])[0] - 9.80665).abs() < 0.001);
        // The same real pad at rest is still about one g this way, which is
        // what makes the fallback usable rather than merely non-fatal.
        let magnitude = c
            .accel_ms2(AT_REST)
            .iter()
            .map(|v| v * v)
            .sum::<f32>()
            .sqrt();
        assert!((magnitude - 9.80665).abs() < 0.5, "{magnitude}");
    }
}
