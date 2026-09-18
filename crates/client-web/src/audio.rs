//! Opus packets to JavaScript, where WebCodecs decodes them.

use gsa_backend_moonlight::OpusSink;
use wasm_bindgen::prelude::*;

/// Calls `frame(bytes: Uint8Array)` and `lost(count: number)` on a
/// JavaScript object, in the order the stream has them.
#[derive(Debug)]
pub struct JsOpusSink {
    object: js_sys::Object,
    frame: Option<js_sys::Function>,
    lost: Option<js_sys::Function>,
}

impl JsOpusSink {
    /// Missing methods are tolerated: a page that plays no audio passes an
    /// empty object and the packets are dropped here.
    #[must_use]
    pub fn new(object: JsValue) -> Self {
        let object = object.dyn_into::<js_sys::Object>().unwrap_or_default();
        let method = |name: &str| {
            js_sys::Reflect::get(&object, &JsValue::from_str(name))
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        };
        Self {
            frame: method("frame"),
            lost: method("lost"),
            object,
        }
    }
}

impl OpusSink for JsOpusSink {
    fn frame(&mut self, opus: &[u8]) {
        if let Some(frame) = &self.frame {
            let bytes = js_sys::Uint8Array::from(opus);
            if let Err(e) = frame.call1(&self.object, &bytes) {
                tracing::debug!(error = ?e, "audio frame callback failed");
            }
        }
    }

    fn lost(&mut self, count: u16) {
        if let Some(lost) = &self.lost
            && let Err(e) = lost.call1(&self.object, &JsValue::from(count))
        {
            tracing::debug!(error = ?e, "audio loss callback failed");
        }
    }
}

/// Calls `heard(from: number, seq: number, bytes: Uint8Array)` on a
/// JavaScript object: one Opus frame of one person's voice, as the host
/// relayed it.
#[derive(Debug)]
pub struct JsVoiceSink {
    object: js_sys::Object,
    heard: Option<js_sys::Function>,
}

impl JsVoiceSink {
    /// A page that is not listening passes an empty object, and the room's
    /// voices are dropped here.
    #[must_use]
    pub fn new(object: JsValue) -> Self {
        let object = object.dyn_into::<js_sys::Object>().unwrap_or_default();
        let heard = js_sys::Reflect::get(&object, &JsValue::from_str("heard"))
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
        Self { object, heard }
    }
}

impl gsa_backend_moonlight::VoiceSink for JsVoiceSink {
    fn heard(&mut self, from: u16, seq: u16, opus: &[u8]) {
        if let Some(heard) = &self.heard {
            let bytes = js_sys::Uint8Array::from(opus);
            if let Err(e) = heard.call3(
                &self.object,
                &JsValue::from(from),
                &JsValue::from(seq),
                &bytes,
            ) {
                tracing::debug!(error = ?e, "voice callback failed");
            }
        }
    }
}
