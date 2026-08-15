//! Read a controller through the platform's own gamepad framework (macOS).
//!
//! [`crate::gamepad_capture`] reads buttons and sticks through a portable
//! library, which is all a cross-platform crate exposes. A DualSense has more
//! than that — motion, a touch surface, a battery — and only the OS framework
//! reports it, so this exists to drive the parts of the protocol the portable
//! path cannot reach. Where both work, this one wins; where this finds no
//! controller, the caller falls back.
//!
//! Two conversions matter, because the framework and the wire disagree:
//!
//! - Rotation arrives in **radians per second**, the wire carries **degrees
//!   per second**.
//! - Acceleration arrives in **G**, the wire carries **m/s² including
//!   gravity**. A pad lying still is therefore ~9.81 on one axis, not zero —
//!   a reading of zero at rest means the conversion was skipped.
//!
//! Sensors on some pads are **off until asked**; without that they report a
//! perfect, unchanging zero, which looks exactly like a pad held very still.

use gsa_client_core::{GamepadProfile, PadCaps, PadKind};
use gsa_protocol::input::{BatteryState, GamepadInput, InputEvent, TouchPhase, gamepad};
use objc2::rc::Retained;
use objc2_game_controller::{
    GCController, GCControllerDirectionPad, GCDevice, GCDeviceBatteryState, GCExtendedGamepad,
    GCMotion, GCTouchState,
};

/// Radians per second to degrees per second.
const RAD_TO_DEG: f32 = 180.0 / std::f32::consts::PI;

/// One G in m/s². The wire wants acceleration including gravity.
const G_TO_MS2: f32 = 9.806_65;

/// Sticks below this fraction of travel read as centred, matching the
/// portable path so the two feel the same.
const STICK_DEADZONE: f32 = 0.05;

const SEAT: u8 = 0;

pub struct GcCapture {
    controller: Retained<GCController>,
    pad: Retained<GCExtendedGamepad>,
    motion: Option<Retained<GCMotion>>,
    /// The pad's own touch surface, when it has one.
    touchpad: Option<Retained<GCControllerDirectionPad>>,
    profile: GamepadProfile,
    /// Last button/axis state sent, so a resting pad stays quiet.
    last: Option<(u32, [i16; 8])>,
    /// Whether a contact is currently down, to tell a first touch from a move.
    touching: bool,
    /// Last battery reading sent; charge moves slowly and would otherwise
    /// repeat every poll.
    last_battery: Option<(BatteryState, Option<u8>)>,
    /// Whether the first motion sample has been reported, as a unit check.
    logged_motion: bool,
}

impl std::fmt::Debug for GcCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcCapture")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl GcCapture {
    /// The first connected controller the framework reports, or `None`.
    pub fn new() -> Option<Self> {
        // SAFETY: every call here is a plain property read on a framework
        // object; the framework is initialised by the first `controllers`
        // call and requires only that we are on the main thread, which the
        // caller guarantees by polling from the event loop.
        unsafe {
            let controllers = GCController::controllers();
            let controller = controllers.iter().next()?;
            let pad = controller.extendedGamepad()?;

            let motion = controller.motion();
            if let Some(motion) = &motion {
                // Some pads report a flawless zero until the sensors are
                // switched on, which is indistinguishable from a pad held
                // perfectly still.
                if motion.sensorsRequireManualActivation() {
                    motion.setSensorsActive(true);
                }
            }
            let touchpad = dualsense_touchpad(&controller);

            let mut caps = PadCaps::RUMBLE;
            if motion.is_some() {
                caps |= PadCaps::MOTION;
            }
            if touchpad.is_some() {
                caps |= PadCaps::TOUCHPAD;
            }
            if controller.battery().is_some() {
                caps |= PadCaps::BATTERY;
            }
            let kind = pad_kind(&controller);
            if matches!(kind, PadKind::DualSense) {
                // Present on the hardware whether or not this host will drive
                // them; the capability describes the pad.
                caps |= PadCaps::LED | PadCaps::ADAPTIVE_TRIGGERS | PadCaps::TRIGGER_RUMBLE;
            }

            tracing::info!(
                ?kind,
                motion = motion.is_some(),
                touchpad = touchpad.is_some(),
                "controller opened through the platform framework"
            );
            Some(Self {
                controller,
                pad,
                motion,
                touchpad,
                profile: GamepadProfile::new(kind, caps),
                last: None,
                touching: false,
                last_battery: None,
                logged_motion: false,
            })
        }
    }

    /// What to announce to the host before sending anything else.
    pub fn profile(&self) -> GamepadProfile {
        self.profile
    }

    /// Everything that changed since the last poll.
    ///
    /// `motion` is the caller's gate: the host asks for motion explicitly, and
    /// sampling it unasked spends battery on samples nobody reads.
    pub fn poll(&mut self, motion: bool) -> Vec<InputEvent> {
        let mut events = Vec::new();
        // SAFETY: property reads on framework objects held alive by `self`.
        unsafe {
            let buttons = buttons(&self.pad);
            let axes = axes(&self.pad);
            if self.last != Some((buttons, axes)) {
                self.last = Some((buttons, axes));
                events.push(InputEvent::Gamepad(GamepadInput {
                    seat: SEAT,
                    buttons,
                    axes,
                    ts_us: now_us(),
                }));
            }

            if motion && let Some(m) = &self.motion {
                let rate = m.rotationRate();
                let accel = m.acceleration();
                // The first sample is worth seeing: a pad lying still should
                // read about 9.81 in total acceleration. Zero means the
                // sensors never woke; ~1.0 means the G conversion was missed.
                if !self.logged_motion {
                    self.logged_motion = true;
                    let (g, a) = to_wire_frame(
                        [rate.x as f32, rate.y as f32, rate.z as f32],
                        [accel.x as f32, accel.y as f32, accel.z as f32],
                    );
                    let magnitude = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
                    tracing::info!(
                        gyro_deg_s = format!("{:.1},{:.1},{:.1}", g[0], g[1], g[2]),
                        accel_ms2 = format!("{:.2},{:.2},{:.2}", a[0], a[1], a[2]),
                        magnitude = format!("{magnitude:.2}"),
                        "first motion sample"
                    );
                }
                let (gyro, accel) = to_wire_frame(
                    [rate.x as f32, rate.y as f32, rate.z as f32],
                    [accel.x as f32, accel.y as f32, accel.z as f32],
                );
                events.push(InputEvent::GamepadMotion {
                    seat: SEAT,
                    gyro,
                    accel,
                    ts_us: now_us(),
                });
            }

            if let Some(event) = self.poll_touch() {
                events.push(event);
            }
            if let Some(event) = self.poll_battery() {
                events.push(event);
            }
        }
        events
    }

    /// One touch event, or `None` while nothing is happening.
    ///
    /// # Safety
    /// Caller holds the framework objects alive.
    unsafe fn poll_touch(&mut self) -> Option<InputEvent> {
        let touchpad = self.touchpad.as_ref()?;
        // SAFETY: property reads on a live framework object.
        let (x, y, down) = unsafe {
            (
                touchpad.xAxis().value(),
                touchpad.yAxis().value(),
                // The surface reports its own contact state; the click button
                // is a different input, and a finger resting without pressing
                // must still track.
                self.controller
                    .extendedGamepad()
                    .and_then(|pad| touch_state(&pad))
                    .is_some_and(|state| state != GCTouchState::Up),
            )
        };

        let phase = match (self.touching, down) {
            (false, true) => TouchPhase::Down,
            (true, true) => TouchPhase::Move,
            (true, false) => TouchPhase::Up,
            (false, false) => return None,
        };
        self.touching = down;
        Some(InputEvent::GamepadTouch {
            seat: SEAT,
            pointer: 0,
            phase,
            // The surface reports -1..1 with the origin centred; the wire is
            // 0..1 from the top-left, and its Y grows downward.
            x: (f32::midpoint(x, 1.0)).clamp(0.0, 1.0),
            y: (f32::midpoint(-y, 1.0)).clamp(0.0, 1.0),
            pressure: 1.0,
            ts_us: now_us(),
        })
    }

    /// A battery reading, but only when it has actually changed.
    ///
    /// # Safety
    /// Caller holds the framework objects alive.
    unsafe fn poll_battery(&mut self) -> Option<InputEvent> {
        // SAFETY: property reads on a live framework object.
        let (state, percent) = unsafe {
            let battery = self.controller.battery()?;
            let state = match battery.batteryState() {
                GCDeviceBatteryState::Discharging => BatteryState::Discharging,
                GCDeviceBatteryState::Charging => BatteryState::Charging,
                GCDeviceBatteryState::Full => BatteryState::Full,
                _ => BatteryState::Unknown,
            };
            let level = battery.batteryLevel();
            // A negative level is the framework saying it does not know, which
            // is not the same as an empty battery.
            let percent = (level >= 0.0).then(|| (level * 100.0).round().clamp(0.0, 100.0) as u8);
            (state, percent)
        };
        if self.last_battery == Some((state, percent)) {
            return None;
        }
        self.last_battery = Some((state, percent));
        Some(InputEvent::GamepadBattery {
            seat: SEAT,
            state,
            percent,
            ts_us: now_us(),
        })
    }
}

/// Put a motion sample into the wire's frame and units.
///
/// The framework and the wire disagree on both. The axis mapping is not a
/// guess: it is the one a widely-used input library applies for this same
/// framework — gyro is reordered, acceleration is negated on every axis — and
/// it is what makes a pad lying flat report gravity where the wire expects it
/// rather than on some other axis with the sign inverted.
fn to_wire_frame(rate: [f32; 3], accel: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    (
        // Reordered, then radians per second to degrees per second.
        [
            rate[0] * RAD_TO_DEG,
            rate[2] * RAD_TO_DEG,
            -rate[1] * RAD_TO_DEG,
        ],
        // Negated, then G to m/s².
        [
            -accel[0] * G_TO_MS2,
            -accel[1] * G_TO_MS2,
            -accel[2] * G_TO_MS2,
        ],
    )
}

/// The pad's family, from what the framework says it is.
///
/// # Safety
/// `controller` must be live.
unsafe fn pad_kind(controller: &GCController) -> PadKind {
    // SAFETY: property read on a live framework object.
    let category = unsafe { controller.productCategory() }.to_string();
    // Matched loosely: the strings carry model detail ("DualSense Edge") that
    // does not change the family.
    let lower = category.to_lowercase();
    if lower.contains("dualsense") {
        PadKind::DualSense
    } else if lower.contains("dualshock") {
        PadKind::DualShock4
    } else if lower.contains("xbox") {
        PadKind::Xbox
    } else if lower.contains("switch") {
        PadKind::SwitchPro
    } else {
        PadKind::Generic
    }
}

/// The DualSense touch surface, when this controller has one.
///
/// # Safety
/// `controller` must be live.
unsafe fn dualsense_touchpad(
    controller: &GCController,
) -> Option<Retained<GCControllerDirectionPad>> {
    // SAFETY: a downcast the framework itself performs; `None` for every pad
    // that is not a DualSense.
    unsafe {
        let pad = controller.extendedGamepad()?;
        let dualsense = pad.downcast_ref::<objc2_game_controller::GCDualSenseGamepad>()?;
        Some(dualsense.touchpadPrimary())
    }
}

/// Whether the touch surface currently has a finger on it.
///
/// # Safety
/// `pad` must be live.
unsafe fn touch_state(pad: &GCExtendedGamepad) -> Option<GCTouchState> {
    // SAFETY: downcast + property read on a live framework object.
    unsafe {
        let dualsense = pad.downcast_ref::<objc2_game_controller::GCDualSenseGamepad>()?;
        let surface = dualsense.touchpadPrimary();
        let touchpad = surface.downcast_ref::<objc2_game_controller::GCControllerTouchpad>()?;
        Some(touchpad.touchState())
    }
}

/// Face-button positions map straight onto the wire's XInput numbering: a
/// pad's south button is A whatever its cap says.
///
/// # Safety
/// `pad` must be live.
unsafe fn buttons(pad: &GCExtendedGamepad) -> u32 {
    // SAFETY: property reads on a live framework object.
    unsafe {
        let mut bits = 0u32;
        let pressed = |button: &objc2_game_controller::GCControllerButtonInput| button.isPressed();
        for (down, mask) in [
            (pressed(&pad.buttonA()), gamepad::A),
            (pressed(&pad.buttonB()), gamepad::B),
            (pressed(&pad.buttonX()), gamepad::X),
            (pressed(&pad.buttonY()), gamepad::Y),
            (pressed(&pad.leftShoulder()), gamepad::LEFT_SHOULDER),
            (pressed(&pad.rightShoulder()), gamepad::RIGHT_SHOULDER),
            (pressed(&pad.buttonMenu()), gamepad::START),
            (pressed(&pad.dpad().up()), gamepad::DPAD_UP),
            (pressed(&pad.dpad().down()), gamepad::DPAD_DOWN),
            (pressed(&pad.dpad().left()), gamepad::DPAD_LEFT),
            (pressed(&pad.dpad().right()), gamepad::DPAD_RIGHT),
        ] {
            if down {
                bits |= mask;
            }
        }
        // Optional on some pads, so each is checked rather than assumed.
        for (button, mask) in [
            (pad.buttonOptions(), gamepad::BACK),
            (pad.buttonHome(), gamepad::GUIDE),
            (pad.leftThumbstickButton(), gamepad::LEFT_STICK),
            (pad.rightThumbstickButton(), gamepad::RIGHT_STICK),
        ] {
            if button.is_some_and(|b| b.isPressed()) {
                bits |= mask;
            }
        }
        bits
    }
}

/// # Safety
/// `pad` must be live.
unsafe fn axes(pad: &GCExtendedGamepad) -> [i16; 8] {
    // SAFETY: property reads on a live framework object.
    unsafe {
        let mut axes = [0i16; 8];
        let left = pad.leftThumbstick();
        let right = pad.rightThumbstick();
        // The framework and the wire agree that +Y is up, so sticks pass
        // through without a flip.
        axes[gamepad::Axis::LeftX.index()] = stick(left.xAxis().value());
        axes[gamepad::Axis::LeftY.index()] = stick(left.yAxis().value());
        axes[gamepad::Axis::RightX.index()] = stick(right.xAxis().value());
        axes[gamepad::Axis::RightY.index()] = stick(right.yAxis().value());
        axes[gamepad::Axis::LeftTrigger.index()] = trigger(pad.leftTrigger().value());
        axes[gamepad::Axis::RightTrigger.index()] = trigger(pad.rightTrigger().value());
        axes
    }
}

/// Bipolar stick, `-1.0..=1.0` → full `i16`.
fn stick(value: f32) -> i16 {
    if value.abs() < STICK_DEADZONE {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

/// Analog trigger, `0.0..=1.0` → `0..=i16::MAX`, never negative.
fn trigger(value: f32) -> i16 {
    (value.clamp(0.0, 1.0) * f32::from(i16::MAX)) as i16
}

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use super::{G_TO_MS2, RAD_TO_DEG};

    /// The framework and the wire disagree on units, and a missing conversion
    /// is invisible in a code review: gyro still moves, it is just wrong by a
    /// factor of 57.
    #[test]
    fn unit_conversions_match_the_wire_contract() {
        // Half a turn a second is 180 degrees a second.
        assert!((std::f32::consts::PI * RAD_TO_DEG - 180.0).abs() < 0.001);
        // A pad at rest reads one G, which the wire carries as ~9.81 m/s².
        assert!((G_TO_MS2 - 9.806_65).abs() < 0.001);
    }

    /// A pad lying flat reads gravity on one axis. Getting the frame wrong
    /// still produces plausible-looking numbers — they just point the wrong
    /// way, which reads as a game with inverted or swapped aim.
    #[test]
    fn a_pad_at_rest_reports_gravity_where_the_wire_expects_it() {
        // Measured from a DualSense lying flat: the framework reports very
        // nearly -1 G on its own Z axis.
        let (_, accel) = super::to_wire_frame([0.0; 3], [0.0, 0.0, -1.0]);
        assert!((accel[0]).abs() < 0.01);
        assert!((accel[1]).abs() < 0.01);
        assert!(
            (accel[2] - 9.806_65).abs() < 0.01,
            "gravity is positive on the third axis, not negative: {accel:?}"
        );
        // Total magnitude is one gravity however the pad is held.
        let (_, tilted) = super::to_wire_frame([0.0; 3], [0.5, 0.5, -0.707]);
        let magnitude =
            (tilted[0] * tilted[0] + tilted[1] * tilted[1] + tilted[2] * tilted[2]).sqrt();
        assert!((magnitude - 9.806_65).abs() < 0.2);
    }

    /// The gyro is reordered rather than negated wholesale; conflating the two
    /// swaps pitch and yaw, which a player feels immediately.
    #[test]
    fn gyro_axes_are_reordered_not_merely_flipped() {
        let (gyro, _) = super::to_wire_frame([1.0, 2.0, 3.0], [0.0; 3]);
        assert!((gyro[0] - RAD_TO_DEG).abs() < 0.01);
        assert!((gyro[1] - 3.0 * RAD_TO_DEG).abs() < 0.01);
        assert!((gyro[2] + 2.0 * RAD_TO_DEG).abs() < 0.01);
    }
}
