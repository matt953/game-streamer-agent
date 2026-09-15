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
    encoder: gsa_dualsense::OutputEncoder,
    clock: gsa_core::time::MediaClock,
    seat: u8,
    /// How the pad is attached, learnt from the reports it sends. Feedback
    /// needs it: the two framings are different reports with different
    /// lengths, and over Bluetooth a signature the pad checks.
    conn: Option<Connection>,
}

#[wasm_bindgen]
impl DualSenseCodec {
    /// A codec for the pad on `seat`.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(seat: u8) -> Self {
        Self {
            parser: Parser::new(seat),
            encoder: gsa_dualsense::OutputEncoder::new(),
            clock: gsa_core::time::MediaClock::new(),
            seat,
            conn: None,
        }
    }

    /// Parse one HID input report (its id and body without the id, as WebHID
    /// delivers them) into an array of `InputEvent`s — empty when nothing
    /// changed. A report id this codec does not read yields an empty array.
    #[wasm_bindgen]
    pub fn input_report(&mut self, report_id: u8, data: &[u8]) -> Result<JsValue, JsValue> {
        let mut out = Vec::new();
        if let Some(conn) = Connection::from_report_id(report_id) {
            self.conn = Some(conn);
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

    /// Tell the codec this pad's firmware version, from feature report
    /// `0x20` without its report id, and get back the version it read.
    ///
    /// It decides which rumble the pad understands. Optional: a pad whose
    /// firmware cannot be read is driven the way a recent one wants, which is
    /// the same assumption every desktop client makes.
    #[wasm_bindgen]
    pub fn firmware(&mut self, body: &[u8]) -> Option<u16> {
        self.encoder.set_firmware(body)
    }

    /// The report that makes this pad rumble at `low` and `high`, or
    /// `undefined` before the pad has said how it is attached.
    ///
    /// Zero for both is how rumble stops — and how the pad gets its audio
    /// haptics back, which the motors borrow while they are running.
    #[wasm_bindgen]
    pub fn rumble_report(&mut self, low: u16, high: u16) -> Result<JsValue, JsValue> {
        let Some(conn) = self.conn else {
            return Ok(JsValue::UNDEFINED);
        };
        let report = self.encoder.report(
            conn,
            &gsa_dualsense::Effects {
                rumble: (low, high),
            },
        );
        serde_wasm_bindgen::to_value(&WebOutputReport {
            report_id: report.report_id,
            data: report.data,
        })
        .map_err(Into::into)
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

/// One HID output report as the page sends it: `sendReport(id, data)`.
#[derive(serde::Serialize)]
struct WebOutputReport {
    report_id: u8,
    data: Vec<u8>,
}

fn events_to_js(events: &[InputEvent]) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(events).map_err(Into::into)
}
