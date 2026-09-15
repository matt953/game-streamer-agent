//! The stream handle JavaScript drives.

use crate::audio::JsOpusSink;
use crate::authority::JsAuthority;
use crate::to_js;
use crate::transport::WebTunnel;
use gsa_backend_moonlight::tunnel::TunnelLinks;
use gsa_backend_moonlight::{MoonlightStream, StreamMode, start_with};
use gsa_client_backend_api::BackendFrame;
use gsa_core::Error;
use gsa_core::error::ProtocolError;
use gsa_core::media::Codec;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

/// One running stream: the Moonlight session over the tunnel.
///
/// Frames come out of [`WebStream::next_frame`] as fast as the host sends
/// them; the page hands each to a WebCodecs `VideoDecoder`. Host events —
/// rumble, LED colours, termination — come out of [`WebStream::next_event`].
#[wasm_bindgen]
#[derive(Debug)]
pub struct WebStream {
    inner: Rc<Inner>,
}

#[derive(Debug)]
struct Inner {
    stream: RefCell<Option<MoonlightStream>>,
    frames: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<BackendFrame>>,
    codec: Codec,
    /// Effects waiting for the page to render, already filtered to what the
    /// pad on that seat can do.
    feedback: RefCell<std::collections::VecDeque<WebFeedback>>,
}

/// One effect for one pad, as the page renders it. Flat rather than tagged:
/// a page switches on `kind` and reads the fields that kind carries.
#[derive(Debug, serde::Serialize)]
struct WebFeedback {
    kind: &'static str,
    seat: u8,
    /// Rumble and trigger rumble: the two motor levels, full-scale `u16`.
    low: u16,
    high: u16,
    /// A light's colour, for `led`.
    rgb: [u8; 3],
    /// Trigger effect blocks for `adaptive_triggers`, each the game's own
    /// eleven bytes, or empty for a trigger this message does not address.
    left: Vec<u8>,
    right: Vec<u8>,
}

/// One encoded access unit for the decoder.
#[wasm_bindgen]
#[derive(Debug)]
pub struct WebFrame {
    data: Vec<u8>,
    frame_id: u32,
    keyframe: bool,
    capture_ts_us: u32,
}

#[wasm_bindgen]
impl WebFrame {
    /// The access unit; a keyframe carries its own parameter sets.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn data(&self) -> js_sys::Uint8Array {
        js_sys::Uint8Array::from(&self.data[..])
    }

    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn frame_id(&self) -> u32 {
        self.frame_id
    }

    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn keyframe(&self) -> bool {
        self.keyframe
    }

    /// The host's capture stamp, µs on the host's clock, wrapping.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn capture_ts_us(&self) -> u32 {
        self.capture_ts_us
    }
}

fn codec_from_name(name: &str) -> Result<Codec, Error> {
    match name {
        "h264" => Ok(Codec::H264),
        "hevc" => Ok(Codec::Hevc),
        "av1" => Ok(Codec::Av1),
        other => Err(Error::Protocol(ProtocolError::Deserialize(format!(
            "unknown codec {other:?}"
        )))),
    }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
        _ => "unknown",
    }
}

/// What a DualSense read over WebHID can actually deliver in a browser today.
///
/// Deliberately not `PadKind::DualSense.implied_caps()`: announcing a feature
/// makes the host enable it and the game send it, so claiming rumble, trigger
/// rumble, adaptive triggers or the lights while this client has no output
/// report would send those effects into a void. Widen this as each one lands,
/// and the host, the game and the interface all follow from here.
const WEB_DUALSENSE_CAPS: gsa_client_backend_api::PadCaps =
    gsa_client_backend_api::PadCaps::from_bits(
        gsa_client_backend_api::PadCaps::TOUCHPAD.bits()
            | gsa_client_backend_api::PadCaps::BATTERY.bits()
            | gsa_client_backend_api::PadCaps::MOTION.bits()
            | gsa_client_backend_api::PadCaps::RUMBLE.bits()
            | gsa_client_backend_api::PadCaps::ADAPTIVE_TRIGGERS.bits(),
    );

/// What a browser's haptic actuator says it can play, as capabilities.
///
/// The names are the Gamepad API's own effect types. A browser that names
/// none — or has no actuator at all — gets nothing, and the host is told this
/// pad has no motors rather than being asked to send effects nobody can play.
fn browser_haptics(effects: &[String]) -> gsa_client_backend_api::PadCaps {
    use gsa_client_backend_api::PadCaps;
    let mut caps = PadCaps::NONE;
    for effect in effects {
        match effect.as_str() {
            "dual-rumble" => caps = caps | PadCaps::RUMBLE,
            "trigger-rumble" => caps = caps | PadCaps::TRIGGER_RUMBLE,
            _ => {}
        }
    }
    caps
}

/// A trigger effect as the pad's own eleven-byte block, or nothing for a
/// trigger to be left as it is.
///
/// Only the game's own bytes are passed on. A described effect — constant
/// resistance from here, a break there — would have to be turned into the
/// pad's bit-packed parameters, and no host on this path sends one; getting
/// that translation wrong gives a trigger that fights the player, so until a
/// host does, a described effect leaves the trigger alone.
fn trigger_block(effect: gsa_client_backend_api::TriggerEffect) -> Vec<u8> {
    use gsa_client_backend_api::TriggerEffect;
    match effect {
        TriggerEffect::Raw { effect, params } => {
            let mut block = vec![effect];
            block.extend_from_slice(&params);
            block
        }
        TriggerEffect::Off => gsa_dualsense::TRIGGER_OFF.to_vec(),
        _ => Vec::new(),
    }
}

/// One seated pad, as the page renders it.
#[derive(serde::Serialize)]
struct WebPad {
    seat: u8,
    kind: &'static str,
    caps: u16,
    features: Vec<&'static str>,
    /// The host's own word: `None` where the host does not report pad state,
    /// so the interface shows nothing rather than inventing a worry.
    confirmed: Option<bool>,
    /// The pad's charge, where it reports one at all.
    battery: Option<WebBattery>,
}

/// A pad's charge as the page renders it: the state by name, and the level
/// where the pad gives one.
#[derive(serde::Serialize)]
struct WebBattery {
    state: &'static str,
    percent: Option<u8>,
}

/// The charge state's name, in the vocabulary the page writes.
fn battery_state_name(state: gsa_protocol::input::BatteryState) -> &'static str {
    use gsa_protocol::input::BatteryState;
    match state {
        BatteryState::NotPresent => "absent",
        BatteryState::Discharging => "discharging",
        BatteryState::Charging => "charging",
        BatteryState::Full => "full",
        _ => "unknown",
    }
}

#[wasm_bindgen]
impl WebStream {
    /// Launch `app_id` through `authority` and bring the stream up over
    /// `tunnel`. Resolves once the host is sending media.
    ///
    /// `mode` is a `StreamMode` object, `codecs` the decoder's codecs by
    /// name (`"h264"`, `"hevc"`, `"av1"`) best first, `audio` an object with
    /// `frame(bytes)` and `lost(count)` for the Opus packets.
    #[wasm_bindgen]
    pub async fn start(
        tunnel: &WebTunnel,
        authority: JsValue,
        app_id: u32,
        mode: JsValue,
        bitrate_kbps: u32,
        codecs: Vec<String>,
        audio: JsValue,
    ) -> Result<WebStream, JsValue> {
        let mode: StreamMode = serde_wasm_bindgen::from_value(mode).map_err(|e| {
            to_js(Error::Protocol(ProtocolError::Deserialize(format!(
                "mode: {e}"
            ))))
        })?;
        let codecs = codecs
            .iter()
            .map(|name| codec_from_name(name))
            .collect::<Result<Vec<_>, _>>()
            .map_err(to_js)?;
        let mut authority = JsAuthority::new(authority).map_err(to_js)?;
        let mut links = TunnelLinks::new(tunnel.clone(), Box::new(JsOpusSink::new(audio)));
        let mut stream = start_with(
            &mut authority,
            &mut links,
            app_id,
            mode,
            bitrate_kbps,
            &codecs,
        )
        .await
        .map_err(to_js)?;
        let frames = stream
            .take_frames()
            .ok_or_else(|| to_js(Error::Session("frames already claimed".into())))?;
        Ok(Self {
            inner: Rc::new(Inner {
                codec: stream.codec,
                stream: RefCell::new(Some(stream)),
                frames: tokio::sync::Mutex::new(frames),
                feedback: RefCell::new(std::collections::VecDeque::new()),
            }),
        })
    }

    /// Put aside anything in `message` that a pad of ours should render.
    ///
    /// The capability check is the contract the client core states: an
    /// embedder without the feature drops the effect rather than
    /// approximating it on another motor. Here that also keeps a page from
    /// being told to rumble a controller the browser gave us no way to drive.
    fn keep_feedback(
        &self,
        stream: &MoonlightStream,
        message: &gsa_backend_moonlight::HostMessage,
    ) {
        use gsa_client_backend_api::{BackendEvent, GamepadFeedback};
        let Some(BackendEvent::Feedback(feedback)) = message.neutral() else {
            return;
        };
        let seat = feedback.seat();
        let Some(pad) = stream.input.pads().into_iter().find(|pad| pad.seat == seat) else {
            return;
        };
        if !pad.profile.caps.contains(feedback.requires()) {
            return;
        }
        let kept = match feedback {
            GamepadFeedback::Rumble { low, high, .. } => WebFeedback {
                kind: "rumble",
                seat,
                low,
                high,
                rgb: [0; 3],
                left: Vec::new(),
                right: Vec::new(),
            },
            GamepadFeedback::TriggerRumble { left, right, .. } => WebFeedback {
                kind: "trigger_rumble",
                seat,
                low: left,
                high: right,
                rgb: [0; 3],
                left: Vec::new(),
                right: Vec::new(),
            },
            GamepadFeedback::Led { rgb, .. } => WebFeedback {
                kind: "led",
                seat,
                low: 0,
                high: 0,
                rgb,
                left: Vec::new(),
                right: Vec::new(),
            },
            GamepadFeedback::AdaptiveTriggers { left, right, .. } => WebFeedback {
                kind: "adaptive_triggers",
                seat,
                low: 0,
                high: 0,
                rgb: [0; 3],
                left: trigger_block(left),
                right: trigger_block(right),
            },
            // Anything the client's vocabulary grows later.
            _ => return,
        };
        self.inner.feedback.borrow_mut().push_back(kept);
    }

    /// The codec the host is sending.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn codec(&self) -> String {
        codec_name(self.inner.codec).to_owned()
    }

    /// The next encoded frame, or `undefined` once the stream has ended.
    #[wasm_bindgen]
    pub fn next_frame(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let mut frames = inner.frames.lock().await;
            Ok(match frames.recv().await {
                Some(frame) => JsValue::from(WebFrame {
                    data: frame.data,
                    frame_id: frame.frame_id,
                    keyframe: frame.keyframe,
                    capture_ts_us: frame.capture_ts_us,
                }),
                None => JsValue::UNDEFINED,
            })
        })
    }

    /// The next host message, or `undefined` when none is waiting. Never
    /// blocks: poll it from the frame loop.
    ///
    /// Anything the host says that is an effect for one of this client's pads
    /// is also put aside here, in the neutral form and only where the pad can
    /// render it, for [`WebStream::next_feedback`].
    #[wasm_bindgen]
    pub fn next_event(&self) -> Result<JsValue, JsValue> {
        let stream = self.inner.stream.borrow();
        let Some(stream) = stream.as_ref() else {
            return Ok(JsValue::UNDEFINED);
        };
        match stream.events.try_recv() {
            Ok(message) => {
                self.keep_feedback(stream, &message);
                serde_wasm_bindgen::to_value(&message).map_err(|e| e.into())
            }
            Err(_) => Ok(JsValue::UNDEFINED),
        }
    }

    /// The next effect one of this client's pads should render, or
    /// `undefined` when none is waiting.
    ///
    /// Already decided: the host's own message turned into the client's
    /// neutral vocabulary, addressed to a seat this client holds, and only
    /// when that pad can actually do it. A page renders what it is given
    /// rather than working out whether a controller has motors.
    #[wasm_bindgen]
    pub fn next_feedback(&self) -> Result<JsValue, JsValue> {
        match self.inner.feedback.borrow_mut().pop_front() {
            Some(feedback) => serde_wasm_bindgen::to_value(&feedback).map_err(Into::into),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// Send input events, an array of `InputEvent` objects, to the host.
    #[wasm_bindgen]
    pub fn send_input(&self, events: JsValue) -> Result<(), JsValue> {
        let events: Vec<gsa_protocol::input::InputEvent> = serde_wasm_bindgen::from_value(events)?;
        if let Some(stream) = self.inner.stream.borrow().as_ref() {
            stream.input.send(events);
        }
        Ok(())
    }

    /// Tell the host a DualSense occupies `seat`, so it builds a matching
    /// virtual pad and turns on its features. Call once when the pad is
    /// claimed over WebHID, before its first input.
    #[wasm_bindgen]
    pub fn announce_dualsense(&self, seat: u8) {
        use gsa_client_backend_api::{GamepadProfile, PadKind};
        if let Some(stream) = self.inner.stream.borrow().as_ref() {
            stream.input.announce_pad(
                seat,
                GamepadProfile::new(PadKind::DualSense, WEB_DUALSENSE_CAPS),
            );
        }
    }

    /// Announce a pad the browser reported through the Gamepad API, named by
    /// its `Gamepad.id`.
    ///
    /// The core decides what the pad is, from the same rules every client
    /// uses, so the host builds a matching device and the interface can say
    /// "Xbox Controller" rather than "Controller". Capabilities come from
    /// what this browser can actually drive, not from what the controller in
    /// the user's hands can do: the Gamepad API carries buttons, sticks,
    /// triggers and whichever rumble effects its actuator names.
    #[wasm_bindgen]
    pub fn announce_gamepad(&self, seat: u8, id: &str, effects: Vec<String>) {
        use gsa_client_backend_api::{GamepadProfile, PadKind};
        if let Some(stream) = self.inner.stream.borrow().as_ref() {
            let kind = PadKind::from_browser_id(id);
            let caps = browser_haptics(&effects);
            stream
                .input
                .announce_pad(seat, GamepadProfile::new(kind, caps));
        }
    }

    /// Every pad the core currently has seated, as the interface should list
    /// them: the seat, what the pad is, and what it carries.
    #[wasm_bindgen]
    pub fn pads(&self) -> Result<JsValue, JsValue> {
        let pads: Vec<WebPad> = self
            .inner
            .stream
            .borrow()
            .as_ref()
            .map(|stream| {
                stream
                    .input
                    .pads()
                    .into_iter()
                    .map(|pad| WebPad {
                        seat: pad.seat,
                        kind: pad.profile.kind.label(),
                        caps: pad.profile.caps.bits(),
                        features: pad.profile.caps.names(),
                        confirmed: pad.confirmed,
                        battery: pad.battery.map(|battery| WebBattery {
                            state: battery_state_name(battery.state),
                            percent: battery.percent,
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default();
        serde_wasm_bindgen::to_value(&pads).map_err(Into::into)
    }

    /// Every seat the host has a device on, as a bitmask, whoever put it
    /// there — including a pad announced by another browser resuming this same
    /// session. A client must place its own pad clear of these.
    #[wasm_bindgen]
    pub fn occupied_seats(&self) -> u16 {
        self.inner
            .stream
            .borrow()
            .as_ref()
            .map_or(0, |stream| stream.input.occupied_seats())
    }

    /// Changes whenever [`WebStream::pads`] would answer differently, so the
    /// page watches one number instead of rebuilding the list every frame.
    #[wasm_bindgen]
    pub fn pads_generation(&self) -> f64 {
        self.inner
            .stream
            .borrow()
            .as_ref()
            .map_or(0, |stream| stream.input.pads_generation()) as f64
    }

    /// Ask the host for a keyframe: the decoder lost its reference chain.
    #[wasm_bindgen]
    pub fn request_keyframe(&self) {
        if let Some(stream) = self.inner.stream.borrow().as_ref() {
            stream.recovery.request_keyframe();
        }
    }

    /// Frames lost and recovered so far, as `[dropped, recovered]`.
    #[wasm_bindgen]
    #[must_use]
    pub fn loss(&self) -> Vec<u64> {
        match self.inner.stream.borrow().as_ref() {
            Some(stream) => vec![
                stream.dropped.load(std::sync::atomic::Ordering::Relaxed),
                stream.recovered.load(std::sync::atomic::Ordering::Relaxed),
            ],
            None => vec![0, 0],
        }
    }

    /// End the session: tells the host to stop and closes the control
    /// channel. The tunnel itself stays open for the page to close.
    #[wasm_bindgen]
    pub fn stop(&self) {
        // Dropping the stream sends Stop and releases the links; the
        // frame channel then ends and `next_frame` resolves undefined.
        drop(self.inner.stream.borrow_mut().take());
    }
}
