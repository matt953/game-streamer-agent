//! The DualSense HID report codec.
//!
//! A DualSense speaks raw HID: a ~64-byte input report several hundred times a
//! second, and a 48-byte output report for its motors, triggers and lights.
//! This crate is the pure translation between those bytes and the client's
//! neutral vocabulary — [`gsa_protocol::input::InputEvent`] out,
//! [`gsa_client_backend_api::GamepadFeedback`] in — with no I/O of its own, so
//! the same code serves the browser (which moves the bytes over WebHID) and
//! any native client that reads a real pad over hidraw.
//!
//! The layout is the real controller's, cross-checked two ways: against the
//! `dualsense-tester` reference (the browser's own view of the pad) and against
//! altc's `altc-input` uhid emulation, which writes these exact input reports
//! and reads these exact output reports from the other side. This codec is the
//! inverse of that emulation, so a value set here must arrive there unchanged.

#![forbid(unsafe_code)]

mod input;
mod motion;
mod output;

pub use input::{Connection, Parser};
pub use motion::Calibration;
pub use output::{Effects, OutputEncoder, OutputReport, TRIGGER_BLOCK_LEN, TRIGGER_OFF};

/// Sony's USB vendor id.
pub const VENDOR_SONY: u16 = 0x054c;
/// The DualSense product id.
pub const PRODUCT_DUALSENSE: u16 = 0x0ce6;
/// The DualSense Edge product id: the same report layout.
pub const PRODUCT_DUALSENSE_EDGE: u16 = 0x0df2;

/// Whether a vendor/product pair is a DualSense this codec handles.
#[must_use]
pub fn is_dualsense(vendor: u16, product: u16) -> bool {
    vendor == VENDOR_SONY && matches!(product, PRODUCT_DUALSENSE | PRODUCT_DUALSENSE_EDGE)
}
