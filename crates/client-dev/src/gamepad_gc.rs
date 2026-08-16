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
use objc2_foundation::ns_string;
use objc2_game_controller::{
    GCController, GCControllerDirectionPad, GCDevice, GCDeviceBatteryState, GCExtendedGamepad,
    GCMotion,
};

/// Radians per second to degrees per second.
const RAD_TO_DEG: f32 = 180.0 / std::f32::consts::PI;

/// One G in m/s². The wire wants acceleration including gravity.
const G_TO_MS2: f32 = 9.806_65;

/// Polls to allow for the motion sensors to start reporting before treating
/// silence as a fault. At the poll rate this is a fraction of a second.
const MOTION_WAKE_POLLS: u32 = 60;

/// Sticks below this fraction of travel read as centred, matching the
/// portable path so the two feel the same.
const STICK_DEADZONE: f32 = 0.05;

const SEAT: u8 = 0;

/// Contacts the framework tracks separately on a DualSense surface.
const MAX_CONTACTS: usize = 2;

/// At-rest reads before a contact counts as lifted. The surface reports an
/// exact (0, 0) with nothing on it, and briefly mid-drag when an axis updates
/// ahead of its pair.
const RELEASE_SAMPLES: u8 = 2;

pub struct GcCapture {
    controller: Retained<GCController>,
    pad: Retained<GCExtendedGamepad>,
    motion: Option<Retained<GCMotion>>,
    /// One direction pad per touch contact, empty without a touch surface.
    touchpads: Vec<Retained<GCControllerDirectionPad>>,
    profile: GamepadProfile,
    /// Last button/axis state sent, so a resting pad stays quiet.
    last: Option<(u32, [i16; 8])>,
    /// Whether each contact is down, to tell a first touch from a move.
    touching: [bool; MAX_CONTACTS],
    /// Consecutive at-rest reads per contact, so one stray sample mid-drag
    /// does not lift the finger.
    at_rest: [u8; MAX_CONTACTS],
    /// Where each contact last actually was, to report a lift there rather
    /// than at the origin.
    last_touch: [(f32, f32); MAX_CONTACTS],
    /// Last battery reading sent; charge moves slowly and would otherwise
    /// repeat every poll.
    last_battery: Option<(BatteryState, Option<u8>)>,
    /// Whether a real motion sample has been reported, as a unit check.
    logged_motion: bool,
    /// Polls spent waiting for the sensors to produce anything.
    motion_polls: u32,
    /// The pad's motors, for playing what the host asks for.
    motors: Option<crate::haptics::Rumble>,
}

impl std::fmt::Debug for GcCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcCapture")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl GcCapture {
    /// Whether the framework can see any controller at all.
    ///
    /// Cheap, and the basis for keeping the portable path quiet: while this is
    /// true the platform path owns the pad, even in the instants before it has
    /// finished opening it. A time window cannot do this job — a controller
    /// plugged in mid-session arrives long after any startup grace period.
    pub fn any_controller() -> bool {
        // SAFETY: a class-method property read on the framework.
        unsafe { GCController::controllers() }.count() > 0
    }

    /// The first connected controller the framework reports, or `None`.
    pub fn new() -> Option<Self> {
        // SAFETY: every call here is a plain property read on a framework
        // object; the framework is initialised by the first `controllers`
        // call and requires only that we are on the main thread, which the
        // caller guarantees by polling from the event loop.
        unsafe {
            // Prefer the most capable pad rather than whichever the framework
            // lists first — that order follows connection time, so with two
            // controllers attached the harness would exercise whichever
            // happened to be switched on first, which is never what is wanted
            // when one of them has motion and a touch surface and the other
            // does not.
            let controllers = GCController::controllers();
            let controller = controllers
                .iter()
                .filter(|c| c.extendedGamepad().is_some())
                .max_by_key(|c| {
                    usize::from(c.motion().is_some())
                        + usize::from(!touch_surfaces(c).is_empty())
                        + usize::from(c.battery().is_some())
                })?;
            let pad = controller.extendedGamepad()?;

            let motion = controller.motion();
            if let Some(motion) = &motion {
                // Switch the sensors on unconditionally rather than only when
                // the framework says activation is required: the flag reports
                // whether the pad *needs* asking, not whether the sensors are
                // already running, and an inactive sensor reports a flawless
                // zero that looks exactly like a pad held perfectly still.
                motion.setSensorsActive(true);
                tracing::info!(
                    requires_activation = motion.sensorsRequireManualActivation(),
                    active = motion.sensorsActive(),
                    has_rotation = motion.hasRotationRate(),
                    has_gravity = motion.hasGravityAndUserAcceleration(),
                    "motion sensors"
                );
            }
            let touchpads = touch_surfaces(&controller);

            let mut caps = PadCaps::RUMBLE;
            if motion.is_some() {
                caps |= PadCaps::MOTION;
            }
            if !touchpads.is_empty() {
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
                touchpads = touchpads.len(),
                "controller opened through the platform framework"
            );
            let motors = crate::haptics::Rumble::new(&controller);
            Some(Self {
                controller,
                pad,
                motion,
                touchpads,
                profile: GamepadProfile::new(kind, caps),
                last: None,
                touching: [false; MAX_CONTACTS],
                at_rest: [0; MAX_CONTACTS],
                last_touch: [(0.0, 0.0); MAX_CONTACTS],
                last_battery: None,
                logged_motion: false,
                motors,
                motion_polls: 0,
            })
        }
    }

    /// What to announce to the host before sending anything else.
    pub fn profile(&self) -> GamepadProfile {
        self.profile
    }

    /// Play the host's rumble amplitudes on the pad. A zero pair stops, and
    /// must be honoured: the host ends an effect explicitly rather than giving
    /// it a duration.
    pub fn rumble(&mut self, low: u16, high: u16) {
        if let Some(motors) = &mut self.motors {
            motors.set(low, high);
        }
    }

    /// How many controllers the framework can see, for reporting the ones
    /// this harness ignores. They can arrive at any time, so this is checked
    /// while running rather than once at startup.
    pub fn controller_count() -> usize {
        // SAFETY: a class-method property read on the framework.
        unsafe { GCController::controllers() }.count()
    }

    /// Whether this controller is still attached.
    ///
    /// The framework hands out an object that stays valid after the pad goes
    /// away; polling it keeps returning the last state, which reads as a
    /// controller being held perfectly still. Presence has to be checked
    /// against the framework's own list.
    pub fn is_connected(&self) -> bool {
        // SAFETY: a class-method property read on the framework.
        unsafe { GCController::controllers() }
            .iter()
            .any(|c| std::ptr::eq(&*c, &*self.controller))
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
                // Report the first sample that actually carries data, not the
                // first sample full stop: sensors take a moment to spin up
                // after being switched on, so an immediate reading is all
                // zeros and says nothing. A pad lying still reads ~9.81 in
                // total acceleration — zero means the sensors never woke,
                // ~1.0 means the G conversion was missed.
                if !self.logged_motion {
                    let (g, a) = to_wire_frame(
                        [rate.x as f32, rate.y as f32, rate.z as f32],
                        [accel.x as f32, accel.y as f32, accel.z as f32],
                    );
                    let magnitude = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
                    // Gravity is always present, so a live sensor can never
                    // read zero for long.
                    if magnitude > 1.0 {
                        self.logged_motion = true;
                    } else if self.motion_polls < MOTION_WAKE_POLLS {
                        self.motion_polls += 1;
                        // Still waking; say nothing yet.
                        return events;
                    } else {
                        self.logged_motion = true;
                        tracing::warn!(
                            active = m.sensorsActive(),
                            has_rotation = m.hasRotationRate(),
                            "motion reads zero after {MOTION_WAKE_POLLS} polls: the sensors \
                             never woke, so the host is being sent nothing useful"
                        );
                        return events;
                    }
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

            self.poll_touch(&mut events);
            if let Some(event) = self.poll_battery() {
                events.push(event);
            }
        }
        events
    }

    /// Touch events for whichever contacts changed this poll.
    ///
    /// # Safety
    /// Caller holds the framework objects alive.
    unsafe fn poll_touch(&mut self, events: &mut Vec<InputEvent>) {
        for pointer in 0..self.touchpads.len() {
            // SAFETY: property reads on a live framework object.
            let (x, y) = unsafe {
                let surface = &self.touchpads[pointer];
                (surface.xAxis().value(), surface.yAxis().value())
            };

            let mut contact = Contact {
                touching: self.touching[pointer],
                at_rest: self.at_rest[pointer],
                position: self.last_touch[pointer],
            };
            let phase = contact.read(x, y);
            self.touching[pointer] = contact.touching;
            self.at_rest[pointer] = contact.at_rest;
            self.last_touch[pointer] = contact.position;
            let Some(phase) = phase else { continue };
            // A lift reports the origin, which is the middle of the surface —
            // sending that would drag the contact to the centre on the way up.
            let (x, y) = contact.position;
            events.push(InputEvent::GamepadTouch {
                seat: SEAT,
                pointer: pointer as u8,
                phase,
                // The surface reports -1..1 with the origin centred; the wire is
                // 0..1 from the top-left, and its Y grows downward.
                x: (f32::midpoint(x, 1.0)).clamp(0.0, 1.0),
                y: (f32::midpoint(-y, 1.0)).clamp(0.0, 1.0),
                pressure: 1.0,
                ts_us: now_us(),
            });
        }
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

/// One finger on the touch surface, tracked across polls.
///
/// The framework reports no contact state for this pad, so the reading itself
/// has to say whether a finger is there. Measured against the hardware:
///
/// - An untouched surface reads exactly `(0, 0)`.
/// - The axes do not update in the same sample. A contact begins as `(x, 0)`
///   and ends as `(0, y)`, and those half-updated samples last exactly one
///   sample. Requiring **both** axes rules them out, so an edge touch is never
///   recorded at the centre; accepting *either* reports every edge lift at
///   0.5, 0.5.
#[derive(Debug, Default, Clone, Copy)]
struct Contact {
    touching: bool,
    at_rest: u8,
    /// The last position with a finger genuinely on it. A lift reads the
    /// origin, which is the middle of the surface.
    position: (f32, f32),
}

impl Contact {
    /// Fold one reading in, returning the phase to report if it changed.
    fn read(&mut self, x: f32, y: f32) -> Option<TouchPhase> {
        let down = x != 0.0 && y != 0.0;
        if down {
            self.at_rest = 0;
            self.position = (x, y);
        } else if self.touching {
            self.at_rest += 1;
            if self.at_rest < RELEASE_SAMPLES {
                return None;
            }
        }
        let phase = match (self.touching, down) {
            (false, true) => TouchPhase::Down,
            (true, true) => TouchPhase::Move,
            (true, false) => TouchPhase::Up,
            (false, false) => return None,
        };
        self.touching = down;
        Some(phase)
    }
}

/// The pad's touch contacts, one direction pad each, empty for a pad without a
/// touch surface.
///
/// The framework offers a `GCControllerTouchpad` with real contact state, but
/// never for this pad: `touchpads` is empty on both macOS and iOS, and
/// `GCDualSenseGamepad::touchpadPrimary` is a direction pad — a sibling of
/// `GCControllerTouchpad` rather than a subclass, so no downcast between the
/// two can ever succeed. Measured against the hardware, these direction pads
/// carry absolute positions across the whole surface and rest at exactly
/// (0, 0), which is the only contact signal available.
///
/// # Safety
/// `controller` must be live.
unsafe fn touch_surfaces(controller: &GCController) -> Vec<Retained<GCControllerDirectionPad>> {
    // SAFETY: property reads on a live framework object.
    unsafe {
        let dpads = controller.physicalInputProfile().dpads();
        [ns_string!("Touchpad 1"), ns_string!("Touchpad 2")]
            .into_iter()
            .filter_map(|key| dpads.objectForKey(key))
            .collect()
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
    use super::{Contact, G_TO_MS2, RAD_TO_DEG, TouchPhase};

    /// Both axes must be non-zero for a contact. The surface updates them a
    /// sample apart, so accepting either one reports an edge touch at the
    /// centre of the pad — a lift in the corner arrives as 0.5, 0.5.
    #[test]
    fn a_half_updated_sample_is_not_a_contact() {
        let mut contact = Contact::default();
        // A touch in the top-right corner: x lands first, y a sample later.
        assert_eq!(contact.read(0.9, 0.0), None);
        assert_eq!(contact.read(0.9, 0.8), Some(TouchPhase::Down));
        // Release: x drops first, and that sample must not be recorded as the
        // position or the lift is reported from the middle of the surface.
        assert_eq!(contact.read(0.0, 0.8), None);
        assert_eq!(contact.read(0.0, 0.0), Some(TouchPhase::Up));
        assert_eq!(contact.position, (0.9, 0.8));
    }

    /// A finger resting still keeps reporting, and a lift needs more than the
    /// single at-rest sample the surface produces mid-transition.
    #[test]
    fn a_contact_survives_one_at_rest_sample() {
        let mut contact = Contact::default();
        assert_eq!(contact.read(0.5, 0.5), Some(TouchPhase::Down));
        assert_eq!(contact.read(0.0, 0.5), None);
        assert_eq!(contact.read(0.5, 0.5), Some(TouchPhase::Move));
        assert!(contact.touching);
    }

    /// An untouched surface must stay silent rather than emit contacts at the
    /// origin, which is the middle of the pad.
    #[test]
    fn an_untouched_surface_reports_nothing() {
        let mut contact = Contact::default();
        for _ in 0..10 {
            assert_eq!(contact.read(0.0, 0.0), None);
        }
        assert!(!contact.touching);
    }

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
