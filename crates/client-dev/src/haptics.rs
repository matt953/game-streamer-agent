//! Driving a controller's rumble motors on macOS.
//!
//! The host sends a low- and a high-frequency amplitude; a pad exposes its two
//! motors as separate haptic localities, so each amplitude drives its own
//! engine. A pad with a single actuator falls back to one engine driven by the
//! stronger of the two, which is closer to the intent than silence.
//!
//! Effects are **continuous and explicitly stopped**: the host says "rumble at
//! this strength" and later "stop", with no duration in between. A fixed-length
//! effect would either cut a long rumble short or keep buzzing after the game
//! stopped asking.
//!
//! `createEngineWithLocality:` is called through `msg_send!` rather than the
//! generated binding, which gates the method to iOS and its siblings. Apple's
//! own header declares the whole interface `API_AVAILABLE(macos(11.0), ...)`
//! with no macOS exclusion, so the gate is the binding's, not the platform's.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, msg_send};
use objc2_core_haptics::{
    CHHapticEngine, CHHapticEvent, CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity,
    CHHapticEventParameterIDHapticSharpness, CHHapticEventTypeHapticContinuous, CHHapticPattern,
    CHHapticPatternPlayer,
};
use objc2_foundation::NSArray;
use objc2_game_controller::{
    GCController, GCHapticsLocality, GCHapticsLocalityLeftHandle, GCHapticsLocalityRightHandle,
};

/// Longest a single effect runs without being renewed. The host's stop message
/// normally ends it first; this only bounds the damage if that message is lost,
/// so it is short enough not to annoy and long enough not to stutter.
const MAX_EFFECT_SECS: f64 = 2.0;

/// Amplitudes below this are treated as off. The wire carries 16 bits, and the
/// bottom of that range is imperceptible on a real motor.
const MIN_AMPLITUDE: f32 = 0.01;

/// One motor: an engine and whatever it is currently playing.
struct Motor {
    engine: Retained<CHHapticEngine>,
    player: Option<Retained<ProtocolObject<dyn CHHapticPatternPlayer>>>,
    /// Current amplitude, so an unchanged value does not restart the effect —
    /// which is audible as a stutter, and the host repeats an amplitude while
    /// a rumble continues.
    amplitude: f32,
}

pub struct Rumble {
    low: Option<Motor>,
    high: Option<Motor>,
}

impl std::fmt::Debug for Rumble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rumble")
            .field("low", &self.low.is_some())
            .field("high", &self.high.is_some())
            .finish()
    }
}

impl Rumble {
    /// Open the controller's motors, or `None` if it has none.
    pub fn new(controller: &GCController) -> Option<Self> {
        // SAFETY: property read on a live framework object.
        let haptics = unsafe { controller.haptics() }?;

        let engine_for = |locality: &GCHapticsLocality| -> Option<Retained<CHHapticEngine>> {
            // SAFETY: `createEngineWithLocality:` on `GCDeviceHaptics`, which
            // the SDK declares available on macOS 11+. It returns an owned
            // engine or nil, which `Option<Retained<_>>` models.
            unsafe { msg_send![&*haptics, createEngineWithLocality: locality] }
        };
        // SAFETY: framework string constants.
        let (left, right) = unsafe {
            (
                engine_for(GCHapticsLocalityLeftHandle),
                engine_for(GCHapticsLocalityRightHandle),
            )
        };

        let motor = |engine: Option<Retained<CHHapticEngine>>| -> Option<Motor> {
            let engine = engine?;
            // SAFETY: starting an engine we just created.
            if let Err(e) = unsafe { engine.startAndReturnError() } {
                tracing::warn!(error = %e, "haptic engine would not start");
                return None;
            }
            Some(Motor {
                engine,
                player: None,
                amplitude: 0.0,
            })
        };
        let (low, high) = (motor(left), motor(right));
        if low.is_none() && high.is_none() {
            return None;
        }
        // Which actuators the platform will address by name: this is what
        // decides whether a pad can be driven per-grip and per-trigger, or
        // only as one lump. It differs by pad *and* by how it is attached.
        // SAFETY: property read on a live framework object.
        let localities = unsafe { haptics.supportedLocalities() }
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            low = low.is_some(),
            high = high.is_some(),
            localities = %localities,
            "controller motors opened"
        );
        Some(Self { low, high })
    }

    /// Play the host's amplitudes, 16-bit per motor. A zero pair stops.
    pub fn set(&mut self, low: u16, high: u16) {
        let scale = |v: u16| f32::from(v) / f32::from(u16::MAX);
        // A pad with one motor gets the stronger of the two rather than an
        // arbitrary half of the effect.
        match (&mut self.low, &mut self.high) {
            (Some(l), Some(h)) => {
                l.play(scale(low));
                h.play(scale(high));
            }
            (Some(only), None) | (None, Some(only)) => only.play(scale(low.max(high))),
            (None, None) => {}
        }
    }
}

impl Motor {
    fn play(&mut self, amplitude: f32) {
        if (amplitude - self.amplitude).abs() < MIN_AMPLITUDE {
            return;
        }
        self.amplitude = amplitude;

        if let Some(player) = self.player.take() {
            // SAFETY: stopping a player this type created and owns.
            let _ = unsafe { player.stopAtTime_error(0.0) };
        }
        if amplitude < MIN_AMPLITUDE {
            return;
        }
        tracing::debug!(amplitude, "playing rumble");
        if let Some(player) = self.build(amplitude) {
            // SAFETY: starting a player created from our own engine.
            match unsafe { player.startAtTime_error(0.0) } {
                Ok(()) => self.player = Some(player),
                // Loud: a rumble that never plays is otherwise indistinguishable
                // from a host that never asked for one.
                Err(e) => tracing::warn!(error = %e, "rumble would not start"),
            }
        }
    }

    /// A continuous effect at `amplitude`, bounded by [`MAX_EFFECT_SECS`].
    fn build(&self, amplitude: f32) -> Option<Retained<ProtocolObject<dyn CHHapticPatternPlayer>>> {
        // SAFETY: constructing framework value objects and asking our own
        // engine for a player; every argument is owned here.
        unsafe {
            let intensity = CHHapticEventParameter::initWithParameterID_value(
                CHHapticEventParameter::alloc(),
                CHHapticEventParameterIDHapticIntensity,
                amplitude,
            );
            // Sharpness picks the character of the effect; a body motor wants
            // the dull end rather than a click.
            let sharpness = CHHapticEventParameter::initWithParameterID_value(
                CHHapticEventParameter::alloc(),
                CHHapticEventParameterIDHapticSharpness,
                0.0,
            );
            let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
                CHHapticEvent::alloc(),
                CHHapticEventTypeHapticContinuous,
                &NSArray::from_retained_slice(&[intensity, sharpness]),
                0.0,
                MAX_EFFECT_SECS,
            );
            let pattern = CHHapticPattern::initWithEvents_parameters_error(
                CHHapticPattern::alloc(),
                &NSArray::from_retained_slice(&[event]),
                &NSArray::new(),
            )
            .inspect_err(|e| tracing::warn!(error = %e, "rumble pattern rejected"))
            .ok()?;
            self.engine
                .createPlayerWithPattern_error(&pattern)
                .inspect_err(|e| tracing::warn!(error = %e, "rumble player rejected"))
                .ok()
        }
    }
}
