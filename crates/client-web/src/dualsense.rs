//! The DualSense codec, exposed to the page.
//!
//! WebHID lives on the main thread, not in a worker, so the page reads the
//! pad's raw reports itself. It hands each one here; this wraps the shared
//! [`gsa_dualsense::Parser`] and returns the [`InputEvent`]s it produced as
//! plain JS objects, which the page forwards over its existing input channel
//! to the worker that holds the stream. The parsing is the same Rust a native
//! HID client would call — only the byte transport is the browser's.

use gsa_dualsense::{Connection, Parser};
use gsa_protocol::input::InputEvent;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
#[derive(Debug)]
pub struct DualSenseCodec {
    parser: Parser,
    clock: gsa_core::time::MediaClock,
    seat: u8,
}

#[wasm_bindgen]
impl DualSenseCodec {
    /// A codec for the pad on `seat`.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(seat: u8) -> Self {
        Self {
            parser: Parser::new(seat),
            clock: gsa_core::time::MediaClock::new(),
            seat,
        }
    }

    /// Parse one HID input report (its id and body without the id, as WebHID
    /// delivers them) into an array of `InputEvent`s — empty when nothing
    /// changed. A report id this codec does not read yields an empty array.
    #[wasm_bindgen]
    pub fn input_report(&mut self, report_id: u8, data: &[u8]) -> Result<JsValue, JsValue> {
        let mut out = Vec::new();
        if let Some(conn) = Connection::from_report_id(report_id) {
            self.parser.parse(conn, data, self.clock.now_us(), &mut out);
        }
        events_to_js(&out)
    }

    /// Give the codec this pad's own sensor calibration, as feature report
    /// `0x05` without its report id.
    ///
    /// Optional and best-effort: a pad whose calibration cannot be read still
    /// reports motion, on the scaling every DualSense shares.
    #[wasm_bindgen]
    pub fn calibration(&mut self, body: &[u8]) {
        self.parser
            .set_calibration(gsa_dualsense::Calibration::parse(body));
    }

    /// Start or stop motion samples, at the rate the host asked for.
    ///
    /// Nothing is sent until the host says it built a motion-capable pad, and
    /// the pad's own 250 Hz is thinned to this rate rather than flooding the
    /// control channel.
    #[wasm_bindgen]
    pub fn set_motion_rate(&mut self, hz: u16) {
        self.parser.set_motion_rate(hz);
    }

    /// The event marking this pad gone, for the page to forward on unplug.
    #[wasm_bindgen]
    pub fn disconnect(&self) -> Result<JsValue, JsValue> {
        let out = vec![InputEvent::GamepadDisconnect {
            seat: self.seat,
            ts_us: self.clock.now_us(),
        }];
        events_to_js(&out)
    }
}

fn events_to_js(events: &[InputEvent]) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(events).map_err(Into::into)
}
