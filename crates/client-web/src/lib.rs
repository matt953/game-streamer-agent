//! The browser client.
//!
//! The Moonlight backend, unchanged, with the browser supplying what a
//! native client gets from sockets: a WebTransport session as the tunnel
//! ([`transport`]), altc's API as the session authority ([`authority`]),
//! WebCodecs as the Opus decoder ([`audio`]). What JavaScript sees is one
//! [`WebStream`] handle: start it, pull encoded frames and host events from
//! it, push input into it, stop it.
//!
//! Only the wasm32 build has a body; natively this crate is empty so the
//! workspace builds and lints without a browser.
#![cfg(target_arch = "wasm32")]

mod audio;
mod authority;
mod dualsense;
mod stream;
mod transport;

pub use authority::JsAuthority;
pub use dualsense::DualSenseCodec;
pub use stream::{WebFrame, WebStream};
pub use transport::WebTunnel;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
fn init() {
    console_error_panic_hook::set_once();
}

/// The tunnel wire-format version this client speaks; the server rejects a
/// hello from any other.
#[wasm_bindgen]
#[must_use]
pub fn tunnel_version() -> u8 {
    gsa_tunnel::VERSION
}

/// A `JsValue` error as the crate's own, with the JavaScript message kept.
pub(crate) fn js_error(what: &str, err: &JsValue) -> gsa_core::Error {
    let message = err
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| err.as_string())
        .unwrap_or_else(|| format!("{err:?}"));
    gsa_core::Error::Transport(format!("{what}: {message}"))
}

/// The crate's own error as a `JsValue` for a rejected promise.
pub(crate) fn to_js(err: gsa_core::Error) -> JsValue {
    js_sys::Error::new(&err.to_string()).into()
}
