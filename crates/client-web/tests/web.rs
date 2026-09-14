//! Browser tests: what can be proven without an altc server behind the page.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn the_page_speaks_the_tunnel_version_the_crate_does() {
    assert_eq!(gsa_client_web::tunnel_version(), gsa_tunnel::VERSION);
}

#[wasm_bindgen_test]
async fn a_tunnel_nobody_listens_on_fails_to_connect() {
    // Port 1 on loopback: refused at once, never a hang.
    let result = gsa_client_web::WebTunnel::connect("https://127.0.0.1:1/tunnel", None).await;
    assert!(result.is_err(), "a closed port must not yield a session");
}

#[wasm_bindgen_test]
async fn the_authority_calls_javascript_and_reads_the_backend_types_back() {
    use gsa_backend_moonlight::{SessionAuthority, StreamMode};
    let object = js_sys::Function::new_no_args(
        r#"return {
            calls: [],
            serverInfo() { this.calls.push("serverInfo"); return Promise.resolve({
                hostname: "altc", app_version: "7.1.431.0", unique_id: "u", https_port: 47984,
                paired: true, state: "SUNSHINE_SERVER_FREE", codec_mode_support: 0x3f,
                max_luma_pixels_hevc: 0, current_game: 0 }); },
            launch(appId, mode) { this.calls.push(["launch", appId, mode.width]); return {
                rtsp_url: "rtsp://altc:48010", riaes_key: Array(16).fill(7), riaes_key_id: 3,
                tunnel_token: [1, 2, 3] }; },
            resume(mode) { return Promise.reject(new Error("nothing to resume")); },
            cancel() { this.calls.push("cancel"); },
        }"#,
    )
    .call0(&JsValue::NULL)
    .unwrap();
    let authority = gsa_client_web::JsAuthority::new(object.clone()).unwrap();
    let info = authority.server_info().await.unwrap();
    assert_eq!(info.hostname, "altc");
    assert_eq!(info.codec_mode_support, 0x3f);
    let mode = StreamMode {
        width: 1920,
        height: 1080,
        fps: 60,
        allow_host_mode_change: false,
        hdr: false,
        channels: 2,
        keep_host_audio: false,
    };
    let launched = authority.launch(42, mode).await.unwrap();
    assert_eq!(launched.rtsp_url, "rtsp://altc:48010");
    assert_eq!(launched.riaes_key, [7; 16]);
    assert_eq!(launched.tunnel_token.as_deref(), Some(&[1u8, 2, 3][..]));
    let err = authority.resume(mode).await.unwrap_err().to_string();
    assert!(err.contains("nothing to resume"), "{err}");
    authority.cancel().await.unwrap();
    let calls = js_sys::Reflect::get(&object, &"calls".into()).unwrap();
    assert_eq!(js_sys::Array::from(&calls).length(), 3);
}

/// The page builds gamepad events as plain JS objects and posts them to the
/// worker, which deserializes them exactly as `WebStream::send_input` does.
/// If that deserialization drops the gamepad variant, no pad reaches the
/// host however well the browser detects it — so this pins the wire.
#[wasm_bindgen_test]
fn a_page_built_gamepad_event_deserializes() {
    use gsa_protocol::input::InputEvent;
    let js = js_sys::eval(
        r#"([
            { Gamepad: { seat: 0, buttons: 4096, axes: [100, -100, 0, 0, 32000, 0, 0, 0], ts_us: 123 } },
            { GamepadDisconnect: { seat: 0, ts_us: 124 } }
        ])"#,
    )
    .unwrap();
    let events: Vec<InputEvent> =
        serde_wasm_bindgen::from_value(js).expect("gamepad events must deserialize");
    assert_eq!(events.len(), 2);
    match &events[0] {
        InputEvent::Gamepad(pad) => {
            assert_eq!(pad.buttons, 4096);
            assert_eq!(pad.axes[0], 100);
            assert_eq!(pad.axes[4], 32000);
        }
        other => panic!("expected a gamepad, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        InputEvent::GamepadDisconnect { seat: 0, .. }
    ));
}
