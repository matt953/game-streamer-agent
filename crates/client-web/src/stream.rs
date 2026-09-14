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
/// report would send those effects into a void. Motion is absent for the same
/// reason — nothing parses it yet. Widen this as each one lands, and the host,
/// the game and the interface all follow from here.
const WEB_DUALSENSE_CAPS: gsa_client_backend_api::PadCaps =
    gsa_client_backend_api::PadCaps::from_bits(
        gsa_client_backend_api::PadCaps::TOUCHPAD.bits()
            | gsa_client_backend_api::PadCaps::BATTERY.bits(),
    );

/// One seated pad, as the page renders it.
#[derive(serde::Serialize)]
struct WebPad {
    seat: u8,
    kind: &'static str,
    caps: u16,
    features: Vec<&'static str>,
    /// The button that opens the overlay on this pad, named for it.
    system_button: &'static str,
    /// The host's own word: `None` where the host does not report pad state,
    /// so the interface shows nothing rather than inventing a worry.
    confirmed: Option<bool>,
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
            }),
        })
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
    #[wasm_bindgen]
    pub fn next_event(&self) -> Result<JsValue, JsValue> {
        let stream = self.inner.stream.borrow();
        let Some(stream) = stream.as_ref() else {
            return Ok(JsValue::UNDEFINED);
        };
        match stream.events.try_recv() {
            Ok(message) => serde_wasm_bindgen::to_value(&message).map_err(|e| e.into()),
            Err(_) => Ok(JsValue::UNDEFINED),
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
    /// "Xbox Controller" rather than "Controller". Capabilities stay empty:
    /// the Gamepad API carries buttons, sticks and triggers and nothing else,
    /// whatever the pad in the user's hands can do.
    #[wasm_bindgen]
    pub fn announce_gamepad(&self, seat: u8, id: &str) {
        use gsa_client_backend_api::{GamepadProfile, PadCaps, PadKind};
        if let Some(stream) = self.inner.stream.borrow().as_ref() {
            let kind = PadKind::from_browser_id(id);
            stream
                .input
                .announce_pad(seat, GamepadProfile::new(kind, PadCaps::NONE));
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
                        system_button: pad.profile.kind.system_button(),
                        confirmed: pad.confirmed,
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
