//! C ABI wrapper embedding `client-core` into the host apps (spec 01, D9).
//!
//! The apps link this into their existing Rust static lib and call it over C
//! (Swift bridging header / Android JNI), mirroring the app's `playback_ffi`
//! precedent — control + hot-path frames cross as plain bytes, never platform
//! types. This first function is a **spike**: prove the core links, connects,
//! and receives from inside the app before the real callback surface lands.

use std::ffi::{CStr, c_char};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use gsa_client_core::{Client, DecodedFrame, PixelOrder, ServerAuth, VideoDecoder};
use gsa_core::id::SourceId;
use gsa_core::media::H264Profile;

/// Counts complete access units without decoding — returns an empty frame so
/// `recv_frame` hands each reassembled frame back to the loop.
struct Counter;

impl VideoDecoder for Counter {
    fn decode(&mut self, _access_unit: &[u8]) -> gsa_core::Result<Option<DecodedFrame>> {
        Ok(Some(DecodedFrame {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            order: PixelOrder::Bgra,
        }))
    }
}

/// Spike: anonymously connect to the agent at `url` (host:port), stream its
/// first source, and count video frames received over `seconds`.
///
/// Returns the frame count (>= 0), or a negative error:
/// `-1` bad url, `-2` runtime init, `-3` connect, `-4` no sources,
/// `-5` start session. Blocking — call off the UI thread.
///
/// # Safety
/// `url` must be a valid NUL-terminated C string that stays valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_spike_connect(url: *const c_char, seconds: i32) -> i32 {
    if url.is_null() {
        return -1;
    }
    // SAFETY: the caller contract requires a valid NUL-terminated string.
    let Ok(url) = (unsafe { CStr::from_ptr(url) }).to_str() else {
        return -1;
    };
    let Ok(addr) = url.parse::<std::net::SocketAddr>() else {
        return -1;
    };
    let Ok(rt) = tokio::runtime::Runtime::new() else {
        return -2;
    };

    rt.block_on(async move {
        let mut client =
            match Client::connect(addr, "gsa-app-spike", H264Profile::High, ServerAuth::Open).await
            {
                Ok(c) => c,
                Err(_) => return -3,
            };
        let sources = match client.list_sources().await {
            Ok(s) if !s.is_empty() => s,
            _ => return -4,
        };
        let source_id: SourceId = sources[0].id;
        if client.start_session(source_id, None).await.is_err() {
            return -5;
        }

        let count = AtomicI32::new(0);
        let recv = async {
            let mut decoder = Counter;
            while let Ok(Some(_)) = client.recv_frame(&mut decoder).await {
                count.fetch_add(1, Ordering::Relaxed);
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(seconds.max(0) as u64), recv).await;
        let n = count.load(Ordering::Relaxed);
        client.close().await;
        n
    })
}
