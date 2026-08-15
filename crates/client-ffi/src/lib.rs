//! C ABI wrapper embedding `client-core` into the host apps (spec 01, D9).
//!
//! The apps link this into their existing Rust static lib and call it over C
//! (Swift bridging header / Android JNI), mirroring the app's `playback_ffi`
//! precedent — control + hot-path frames cross as plain bytes, never platform
//! types. This first function is a **spike**: prove the core links, connects,
//! and receives from inside the app before the real callback surface lands.

pub(crate) mod devlog;
mod host;
mod moonlight;

// The C entry points are exported by the linker regardless, but Rust callers
// (the app's `shared` crate re-exports them so they survive stripping) need a
// path to them.
pub use host::{GSA_BACKEND_GSA, GSA_BACKEND_MOONLIGHT, gsa_catalog, gsa_host_session_start};
pub use moonlight::{gsa_moonlight_identity, gsa_moonlight_pair};

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use gsa_client_core::{
    Client, ControlEvent, DecodedFrame, GamepadInput, InputEvent, PixelOrder, PresentedSink,
    ServerAuth, SourceKind, VideoDecoder,
};
use gsa_core::id::SourceId;
use gsa_core::media::{Codec, H264Profile};
use tokio::sync::Notify;

/// Codec bit flags for `gsa_session_start`'s `decode_codecs` (a set the embedder
/// can decode) and the single value `gsa_session_codec` returns (the negotiated
/// one). H.264 must always be included in `decode_codecs` as the fallback.
pub const GSA_CODEC_H264: u32 = 1 << 0;
pub const GSA_CODEC_HEVC: u32 = 1 << 1;
pub const GSA_CODEC_AV1: u32 = 1 << 2;

/// Controller capability flags, from `gsa_session_pad_caps` (spec 07, 16).
///
/// One vocabulary for every backend. The embedder enables a pad feature only
/// where the session carries it *and* the pad has it: capture nothing
/// transmits costs battery for nothing, and an effect the pad cannot render
/// is silence the user reads as a fault.
pub const GSA_PAD_RUMBLE: u32 = 1 << 0;
pub const GSA_PAD_TRIGGER_RUMBLE: u32 = 1 << 1;
pub const GSA_PAD_GYRO: u32 = 1 << 2;
pub const GSA_PAD_ACCEL: u32 = 1 << 3;
pub const GSA_PAD_TOUCHPAD: u32 = 1 << 4;
pub const GSA_PAD_ADAPTIVE_TRIGGERS: u32 = 1 << 5;
pub const GSA_PAD_LED: u32 = 1 << 6;
pub const GSA_PAD_BATTERY: u32 = 1 << 7;

fn codecs_from_flags(flags: u32) -> Vec<Codec> {
    let mut codecs = Vec::new();
    if flags & GSA_CODEC_HEVC != 0 {
        codecs.push(Codec::Hevc);
    }
    if flags & GSA_CODEC_AV1 != 0 {
        codecs.push(Codec::Av1);
    }
    // H.264 always present as the guaranteed fallback.
    codecs.push(Codec::H264);
    codecs
}

fn codec_to_flag(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => GSA_CODEC_H264,
        Codec::Hevc => GSA_CODEC_HEVC,
        Codec::Av1 => GSA_CODEC_AV1,
        // `Codec` is non_exhaustive; an unknown codec maps to no flag.
        _ => 0,
    }
}

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
        let mut client = match Client::connect(
            addr,
            "gsa-app-spike",
            H264Profile::High,
            &[Codec::H264],
            ServerAuth::Open,
        )
        .await
        {
            Ok(c) => c,
            Err(_) => return -3,
        };
        let sources = match client.list_sources().await {
            Ok(s) if !s.is_empty() => s,
            _ => return -4,
        };
        let source_id: SourceId = sources[0].id;
        if client
            .start_session(source_id, None, None, false)
            .await
            .is_err()
        {
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

/// Callbacks the embedder registers to receive a live session's media. Both are
/// invoked **present-on-arrival** — decode/render happens app-side (spec 01, D9:
/// encoded passthrough; PCM is decoded here). `ctx` is passed back verbatim.
///
/// Threading: `on_video` fires on the session's receive thread; `on_audio` on a
/// separate audio thread. Both may run concurrently, so the embedder must
/// synchronize any shared state behind `ctx`. Neither pointer's data outlives
/// the call — copy what you need to keep. Callbacks must not call back into the
/// session (no `gsa_session_stop` from inside a callback).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GsaCallbacks {
    /// Opaque embedder handle, passed to every callback. Not touched by Rust.
    pub ctx: *mut c_void,
    /// One complete H.264 Annex-B access unit. `keyframe` marks an IDR (carries
    /// SPS/PPS). `capture_ts_us` is the agent-clock capture time (µs, wrapping).
    /// `latency_us` is the estimated capture→received latency (0 if unknown).
    pub on_video: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            data: *const u8,
            len: usize,
            keyframe: bool,
            capture_ts_us: u32,
            latency_us: u32,
        ),
    >,
    /// Interleaved-i16 PCM, 48 kHz stereo. `samples` counts i16 values (frames
    /// × 2), not bytes.
    pub on_audio: Option<unsafe extern "C" fn(ctx: *mut c_void, pcm: *const i16, samples: usize)>,
    /// A user-facing notification pushed by the host (a toast, etc.). `kind` is
    /// a `GSA_NOTIFY_*` value; `arg` is kind-specific (the gamepad seat for the
    /// gamepad kinds). Fires on a dedicated thread. Unknown kinds should be
    /// ignored so new ones stay backward-compatible.
    pub on_notification: Option<unsafe extern "C" fn(ctx: *mut c_void, kind: u32, arg: u32)>,
    /// Periodic encoder/network telemetry (~1 Hz): agent target + emitted
    /// bitrate + manual ceiling + ABR's network estimate (0 = unmeasured),
    /// client received goodput (all bits/s), frames dropped incomplete
    /// (cumulative), and the agent's ABR state.
    pub on_stats: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            target_bps: u32,
            emitted_bps: u32,
            ceiling_bps: u32,
            estimate_bps: u32,
            recv_bps: u32,
            dropped_frames: u32,
            abr_enabled: bool,
        ),
    >,
}

/// `on_notification` kinds. Stable across the ABI; append new values.
pub const GSA_NOTIFY_GAMEPAD_CONNECTED: u32 = 1;
/// The host asked a controller to rumble; `arg` is the seat.
///
/// Magnitudes are not carried yet: this callback shape passes a single `u32`,
/// and packing two 16-bit levels into it would be a trap for the next reader.
/// A richer feedback callback is the right fix (see spec 16, task 52).
pub const GSA_NOTIFY_RUMBLE: u32 = 3;
pub const GSA_NOTIFY_GAMEPAD_DISCONNECTED: u32 = 2;

/// Raw `ctx` isn't `Send`; the embedder owns its thread-safety, so we carry the
/// callback set across the receive-thread boundary explicitly.
pub(crate) struct SendCallbacks(pub(crate) GsaCallbacks);
// SAFETY: the embedder guarantees `ctx` is safe to use from the receive/audio
// threads (documented on `GsaCallbacks`); Rust only passes it back opaquely.
unsafe impl Send for SendCallbacks {}

/// Opaque live session handle. Owns the receive thread (which owns the tokio
/// runtime + connection). Free exactly once with [`gsa_session_stop`].
#[derive(Debug)]
pub struct GsaSession {
    stop: Arc<Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Where input goes. Backend-neutral so one session handle serves every
    /// protocol; `None` if input could not be enabled.
    input: Option<Arc<dyn gsa_client_core::InputSink>>,
    /// Live quality controls, when the backend has any. A Moonlight host
    /// fixes bitrate at negotiation, so it has none — and a control the host
    /// would ignore is not offered.
    knobs: Option<Arc<dyn gsa_client_core::SessionKnobs>>,
    /// Presentation reporter for [`gsa_frame_presented`].
    presented: PresentedSink,
    /// Decoder-failure latch for [`gsa_frame_undecodable`].
    decode_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Adaptive de-jitter switch for [`gsa_set_dejitter`].
    dejitter: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The negotiated codec (a `GSA_CODEC_*` flag), for `gsa_session_codec`.
    codec: u32,
    /// What of a controller this session carries (`GSA_PAD_*` flags), for
    /// `gsa_session_pad_caps`.
    pad_caps: u32,
}

/// Handed back from `session_loop` once it knows the outcome: whether the
/// session reached the streaming state, plus its input sink and negotiated codec.
pub(crate) enum SessionReady {
    /// The session never reached streaming. The string is for the user, so it
    /// must say what actually went wrong — "could not connect" for a failure
    /// the host explained is the kind of message that costs an hour.
    Failed(String),
    Streaming {
        input: Option<Arc<dyn gsa_client_core::InputSink>>,
        knobs: Option<Arc<dyn gsa_client_core::SessionKnobs>>,
        presented: PresentedSink,
        decode_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
        dejitter: std::sync::Arc<std::sync::atomic::AtomicBool>,
        codec: u32,
        pad_caps: u32,
    },
}

/// Connect anonymously to the agent at `url` (host:port), start the source
/// `source_id` (from [`gsa_list_sources`]), and stream media to `callbacks`
/// until [`gsa_session_stop`]. Blocks until the session is streaming (or fails).
///
/// `decode_codecs` is the OR of the `GSA_CODEC_*` flags the embedder can decode;
/// H.264 is always included as the fallback regardless. Query the codec the
/// agent actually chose with [`gsa_session_codec`].
///
/// `bitrate_bps` is the initial bitrate ceiling (0 = the agent's configured
/// default); `abr` turns adaptive bitrate on from the first frame. Both remain
/// adjustable live via [`gsa_set_bitrate`] / [`gsa_set_abr`].
///
/// Returns an owned session handle, or NULL on failure (bad url, runtime init,
/// connect, or start-session). Call `gsa_session_stop` to release.
///
/// # Safety
/// `url` must be a valid NUL-terminated C string for the duration of the call.
/// The function pointers and `ctx` in `callbacks` must stay valid until
/// `gsa_session_stop` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_start(
    url: *const c_char,
    source_id: u32,
    decode_codecs: u32,
    bitrate_bps: u32,
    abr: bool,
    callbacks: GsaCallbacks,
) -> *mut GsaSession {
    devlog::init();
    if url.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: caller contract requires a valid NUL-terminated string.
    let Ok(url) = (unsafe { CStr::from_ptr(url) }).to_str() else {
        return std::ptr::null_mut();
    };
    let Ok(addr) = url.parse::<std::net::SocketAddr>() else {
        return std::ptr::null_mut();
    };

    let stop = Arc::new(Notify::new());
    let cbs = SendCallbacks(callbacks);
    let thread_stop = stop.clone();
    // Signals whether the session reached the streaming state (and its input
    // sink) before we hand a handle back; keeps failures synchronous rather
    // than a silently-dead thread.
    let (ready_tx, ready_rx) = channel::<SessionReady>();

    let thread = std::thread::spawn(move || {
        let cbs = cbs; // move the whole callback set onto this thread
        // This thread drives the receive loop and the `on_video` hand-off; its
        // tokio workers carry the network I/O. Boost all of them so the OS
        // schedules the real-time path promptly under contention.
        boost_thread_qos();
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(boost_thread_qos)
            .build()
        {
            Ok(rt) => rt,
            Err(_) => {
                let _ = ready_tx.send(SessionReady::Failed(String::new()));
                return;
            }
        };
        rt.block_on(session_loop(
            addr,
            SessionOpts {
                source_id,
                decode_codecs: codecs_from_flags(decode_codecs),
                bitrate_bps: (bitrate_bps > 0).then_some(bitrate_bps),
                abr,
            },
            cbs,
            thread_stop,
            ready_tx,
        ));
    });

    match ready_rx.recv() {
        Ok(SessionReady::Streaming {
            input,
            knobs,
            presented,
            decode_error,
            dejitter,
            codec,
            pad_caps,
        }) => Box::into_raw(Box::new(GsaSession {
            stop,
            thread: Some(thread),
            input,
            knobs,
            presented,
            decode_error,
            dejitter,
            codec,
            pad_caps,
        })),
        _ => {
            let _ = thread.join();
            std::ptr::null_mut()
        }
    }
}

/// The codec the session negotiated with the agent, as a single `GSA_CODEC_*`
/// flag — the embedder configures its decoder from this. NULL returns 0.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_codec(session: *const GsaSession) -> u32 {
    if session.is_null() {
        return 0;
    }
    // SAFETY: caller contract guarantees a live handle.
    unsafe { &*session }.codec
}

/// What of a controller this session carries, as `GSA_PAD_*` flags. NULL
/// returns 0.
///
/// Gate every pad feature on this **and** on what the physical pad has: motion
/// capture nothing transmits drains battery, and an effect the pad cannot
/// render reads as a fault. Backends differ widely here — a console protocol
/// carries a pad whole, a PC host takes rumble — so nothing may be assumed
/// from the fact that a session started.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] or
/// [`gsa_host_session_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_pad_caps(session: *const GsaSession) -> u32 {
    if session.is_null() {
        return 0;
    }
    // SAFETY: caller contract guarantees a live handle.
    unsafe { &*session }.pad_caps
}

/// Stop a session started by [`gsa_session_start`], join its threads, and free
/// the handle. After this returns, no further callbacks fire. NULL is a no-op.
///
/// # Safety
/// `session` must be a handle from `gsa_session_start` not already stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_stop(session: *mut GsaSession) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live, once-only handle.
    let mut session = unsafe { Box::from_raw(session) };
    session.stop.notify_one();
    if let Some(t) = session.thread.take() {
        let _ = t.join();
    }
}

/// Set the encode target bitrate (bps). With ABR on this is the ceiling ABR
/// adapts below; with ABR off it's the live target. Fire-and-forget; NULL is a
/// no-op. The agent clamps to a sane range.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_set_bitrate(session: *const GsaSession, bitrate_bps: u32) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    if let Some(knobs) = &unsafe { &*session }.knobs {
        knobs.set_bitrate(bitrate_bps);
    }
}

/// Report a frame handed to the display. `capture_ts_us` is the frame's
/// capture stamp from the video callback; call from the presentation path
/// (cheap, lock-free). Feeds presented-fps/stutter/latency health stats.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_frame_presented(session: *const GsaSession, capture_ts_us: u32) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    unsafe { &*session }.presented.presented(capture_ts_us);
}

/// Report that the embedder's decoder rejected a delivered frame (bad
/// reference, corrupt bitstream): the session freezes P-frames and asks the
/// agent for recovery, exactly as for a lost frame.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_frame_undecodable(session: *const GsaSession) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    unsafe { &*session }
        .decode_error
        .store(true, std::sync::atomic::Ordering::Release);
}

/// Enable/disable adaptive presentation de-jitter (default on). It aligns
/// early frames to the source cadence only while measured jitter is high;
/// late frames are never delayed.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_set_dejitter(session: *const GsaSession, enabled: bool) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    unsafe { &*session }
        .dejitter
        .store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// The protocol's bitrate ceiling (bps) — the top of every bitrate control.
#[unsafe(no_mangle)]
pub extern "C" fn gsa_bitrate_max_bps() -> u32 {
    gsa_protocol::BITRATE_MAX_BPS
}

/// Enable/disable server-side ABR for the session. Fire-and-forget; NULL is a
/// no-op.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_set_abr(session: *const GsaSession, enabled: bool) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    if let Some(knobs) = &unsafe { &*session }.knobs {
        knobs.set_abr(enabled);
    }
}

/// Send a full gamepad state snapshot for `seat`. Fire-and-forget; the first
/// snapshot plugs the host's virtual pad (spec 07). `buttons` is XInput's
/// `wButtons` layout in the low 16 bits; sticks are full-range i16 with +Y up,
/// triggers are `0..=i16::MAX`. Cheap + thread-safe — call from the input
/// thread on every change.
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_send_gamepad(
    session: *mut GsaSession,
    seat: u8,
    buttons: u32,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    lt: i16,
    rt: i16,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle; `input` is set at
    // creation and never mutated, so a shared read is sound.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        input.send(vec![InputEvent::Gamepad(GamepadInput {
            seat,
            buttons,
            axes: [lx, ly, rx, ry, lt, rt, 0, 0],
            ts_us: now_us(),
        })]);
    }
}

/// Tell the host the controller for `seat` went away — unplug its virtual pad
/// rather than leave it frozen at neutral (spec 07).
///
/// # Safety
/// `session` must be a live handle from [`gsa_session_start`] (not yet stopped).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_send_gamepad_disconnect(session: *mut GsaSession, seat: u8) {
    if session.is_null() {
        return;
    }
    // SAFETY: see `gsa_send_gamepad`.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        input.send(vec![InputEvent::GamepadDisconnect {
            seat,
            ts_us: now_us(),
        }]);
    }
}

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Raise the calling thread to `USER_INITIATED` QoS on Apple platforms, so the
/// OS schedules the real-time receive/decode/audio path promptly under
/// contention (`USER_INTERACTIVE` is reserved for UI). These threads block on
/// I/O rather than spin, so this only affects *when* they wake, not fairness.
/// No-op on other platforms (Android/desktop use their own mechanisms).
#[cfg(target_vendor = "apple")]
pub(crate) fn boost_thread_qos() {
    // SAFETY: sets only the calling thread's QoS class; always safe to call.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(not(target_vendor = "apple"))]
pub(crate) fn boost_thread_qos() {}

/// Kind of a source, as reported to [`gsa_list_sources`]. Values are stable
/// across the ABI; `Unknown` covers future variants. Non-`TestPattern` display
/// sources carry loopback audio.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GsaSourceKind {
    Display = 0,
    VirtualDisplay = 1,
    Emulator = 2,
    TestPattern = 3,
    Unknown = 255,
}

impl From<SourceKind> for GsaSourceKind {
    fn from(kind: SourceKind) -> Self {
        match kind {
            SourceKind::Display => Self::Display,
            SourceKind::VirtualDisplay => Self::VirtualDisplay,
            SourceKind::Emulator => Self::Emulator,
            SourceKind::TestPattern => Self::TestPattern,
            _ => Self::Unknown,
        }
    }
}

/// Connect anonymously to the agent at `url` (host:port), enumerate its capture
/// sources, and invoke `on_source(ctx, id, kind, name)` once per source (the
/// `name` C string is valid only for that call). Then disconnect.
///
/// Returns the source count (>= 0), or a negative error: `-1` bad url,
/// `-2` runtime init, `-3` connect, `-4` list request. Blocks — call off the
/// UI thread. The chosen `id` is passed to [`gsa_session_start`].
///
/// # Safety
/// `url` must be a valid NUL-terminated C string for the duration of the call.
/// `ctx` must remain valid until this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_list_sources(
    url: *const c_char,
    on_source: Option<
        unsafe extern "C" fn(ctx: *mut c_void, id: u32, kind: GsaSourceKind, name: *const c_char),
    >,
    ctx: *mut c_void,
) -> i32 {
    if url.is_null() {
        return -1;
    }
    // SAFETY: caller contract requires a valid NUL-terminated string.
    let Ok(url) = (unsafe { CStr::from_ptr(url) }).to_str() else {
        return -1;
    };
    let Ok(addr) = url.parse::<std::net::SocketAddr>() else {
        return -1;
    };
    // Current-thread runtime: this one-shot connect/list/close runs entirely on
    // the (QoS-elevated) calling thread, with no worker threads to invert on.
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return -2;
    };

    rt.block_on(async move {
        let mut client = match Client::connect(
            addr,
            "gsa-app",
            H264Profile::High,
            &[Codec::H264],
            ServerAuth::Open,
        )
        .await
        {
            Ok(c) => c,
            Err(_) => return -3,
        };
        let sources = match client.list_sources().await {
            Ok(s) => s,
            Err(_) => {
                client.close().await;
                return -4;
            }
        };
        if let Some(cb) = on_source {
            for source in &sources {
                // Interior NUL can't occur in a source name; skip if it somehow does.
                let Ok(name) = std::ffi::CString::new(source.name.as_str()) else {
                    continue;
                };
                // SAFETY: `name` outlives the call; `ctx` valid per contract.
                unsafe { cb(ctx, source.id.0, source.kind.into(), name.as_ptr()) };
            }
        }
        client.close().await;
        sources.len() as i32
    })
}

/// The session body: connect, take audio, start `source_id`, then pump encoded
/// video to `on_video` and PCM to `on_audio` until `stop` fires or the
/// connection closes. Reports readiness through `ready_tx`.
/// What to stream and how to start it (the `gsa_session_start` args).
struct SessionOpts {
    source_id: u32,
    decode_codecs: Vec<Codec>,
    bitrate_bps: Option<u32>,
    abr: bool,
}

async fn session_loop(
    addr: std::net::SocketAddr,
    opts: SessionOpts,
    cbs: SendCallbacks,
    stop: Arc<Notify>,
    ready_tx: std::sync::mpsc::Sender<SessionReady>,
) {
    let SessionOpts {
        source_id,
        decode_codecs,
        bitrate_bps,
        abr,
    } = opts;
    let cbs = cbs.0;
    let mut client = match Client::connect(
        addr,
        "gsa-app",
        H264Profile::High,
        &decode_codecs,
        ServerAuth::Open,
    )
    .await
    {
        Ok(c) => c,
        Err(_) => {
            let _ = ready_tx.send(SessionReady::Failed(String::new()));
            return;
        }
    };
    // Enable audio decode before the first recv so no datagrams are dropped.
    let audio_rx = match client.take_audio_output() {
        Ok(rx) => rx,
        Err(_) => {
            let _ = ready_tx.send(SessionReady::Failed(String::new()));
            return;
        }
    };
    let params = match client
        .start_session(SourceId(source_id), None, bitrate_bps, abr)
        .await
    {
        Ok(p) => p,
        Err(_) => {
            let _ = ready_tx.send(SessionReady::Failed(String::new()));
            return;
        }
    };
    // Agent running with a dev log collector: push our logs there too
    // (debug builds only — no-op in release).
    if let Some(sink) = &params.log_sink {
        devlog::activate(sink);
    }
    // Host-pushed notifications (e.g. gamepad plugged) arrive on the control
    // stream; handle them in the select loop below so they fire on this thread
    // (which `gsa_session_stop` joins — no callback outlives the session).
    let mut control_rx = client.take_control_events();

    // Hand the sync input sink back with the ready signal; it also routes the
    // recv loop's keyframe requests through its background writer task.
    let input = client.take_input_sender().map(Arc::new);
    let knobs: Option<Arc<dyn gsa_client_core::SessionKnobs>> = input
        .clone()
        .map(|i| i as Arc<dyn gsa_client_core::SessionKnobs>);
    let input: Option<Arc<dyn gsa_client_core::InputSink>> =
        input.map(|i| i as Arc<dyn gsa_client_core::InputSink>);
    let presented = client.presented_sink();
    let decode_error = client.decode_error_flag();
    let dejitter = client.dejitter_flag();
    let codec = client
        .negotiated_codec()
        .map_or(GSA_CODEC_H264, codec_to_flag);
    let knob_caps = knobs.as_ref().map(|k| k.caps());
    let _ = ready_tx.send(SessionReady::Streaming {
        input,
        knobs,
        presented,
        decode_error,
        dejitter,
        codec,
        // The agent's own pad support, straight from the backend seam rather
        // than restated here, so the two cannot drift.
        pad_caps: knob_caps.map_or(0, |caps| u32::from(caps.pads.bits())),
    });

    // Audio drains on its own thread: PCM must flow steadily even while the
    // receive loop is parked awaiting the next video frame. The channel closes
    // when `client` (holding the Sender) drops, ending this thread.
    let audio_ctx = SendPtr(cbs.ctx);
    let on_audio = cbs.on_audio;
    let audio_thread = std::thread::spawn(move || {
        boost_thread_qos(); // audio must not be starved by background work
        let audio_ctx = audio_ctx;
        while let Ok(pcm) = audio_rx.recv() {
            if let Some(cb) = on_audio {
                // SAFETY: pointer+len describe this owned buffer for the call;
                // the embedder copies what it keeps.
                unsafe { cb(audio_ctx.0, pcm.as_ptr(), pcm.len()) };
            }
        }
    });

    // Latest client received goodput (bps) and cumulative incomplete-frame
    // drops, refreshed off the frame path and reported alongside the agent's
    // telemetry on each `EncodeStats`.
    let mut recv_bps: u32 = 0;
    let mut dropped_frames: u32 = 0;
    let mut frames: u64 = 0;
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            // Agent notification (gamepad plugged) / telemetry: forward to the embedder.
            event = async {
                match &mut control_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<ControlEvent>>().await,
                }
            } => {
                if let Some(event) = event {
                    match event {
                        ControlEvent::GamepadConnected { seat } => {
                            fire_notification(&cbs, GSA_NOTIFY_GAMEPAD_CONNECTED, seat as u32);
                        }
                        ControlEvent::GamepadDisconnected { seat } => {
                            fire_notification(&cbs, GSA_NOTIFY_GAMEPAD_DISCONNECTED, seat as u32);
                        }
                        ControlEvent::EncodeStats {
                            target_bitrate_bps,
                            emitted_bitrate_bps,
                            ceiling_bitrate_bps,
                            estimate_bitrate_bps,
                            abr_enabled,
                        } => {
                            if let Some(cb) = cbs.on_stats {
                                // SAFETY: `ctx` valid for the session per the contract.
                                unsafe {
                                    cb(cbs.ctx, target_bitrate_bps, emitted_bitrate_bps, ceiling_bitrate_bps, estimate_bitrate_bps, recv_bps, dropped_frames, abr_enabled);
                                }
                            }
                        }
                    }
                }
            }
            frame = client.recv_encoded() => match frame {
                Ok(Some(f)) => {
                    if let Some(cb) = cbs.on_video {
                        // SAFETY: pointer+len describe f.data for the call only.
                        unsafe {
                            cb(
                                cbs.ctx,
                                f.data.as_ptr(),
                                f.data.len(),
                                f.keyframe,
                                f.capture_ts_us,
                                f.latency_us.unwrap_or(0),
                            )
                        };
                    }
                    // Refresh received goodput a few times a second (stats() sorts
                    // its windows, so don't do it every frame).
                    frames += 1;
                    if frames.is_multiple_of(15) {
                        let s = client.stats();
                        recv_bps = (s.recv_mbps.unwrap_or(0.0) * 1_000_000.0) as u32;
                        dropped_frames = s.frames_dropped_incomplete as u32;
                    }
                }
                _ => break, // closed or errored
            },
        }
    }

    // `close` consumes the client, dropping the audio Sender and so closing the
    // channel; the drain thread then ends. Wait for it.
    client.close().await;
    let _ = audio_thread.join();
}

/// Deliver a `GSA_NOTIFY_*` notification to the embedder, if it registered one.
pub(crate) fn fire_notification(cbs: &GsaCallbacks, kind: u32, arg: u32) {
    if let Some(cb) = cbs.on_notification {
        // SAFETY: `ctx` valid for the session per the embedder contract.
        unsafe { cb(cbs.ctx, kind, arg) };
    }
}

/// Carries a raw `ctx` onto the audio thread. Same embedder contract as
/// [`SendCallbacks`].
pub(crate) struct SendPtr(pub(crate) *mut c_void);
// SAFETY: see `GsaCallbacks` threading contract.
unsafe impl Send for SendPtr {}

#[cfg(test)]
mod tests {
    use gsa_client_core::PadCaps;

    /// The `GSA_PAD_*` flags are the C ABI's copy of [`PadCaps`]. They are
    /// declared twice (here and in the app's header) and shipped, so a
    /// renumbered bit would silently enable the wrong feature on a device.
    #[test]
    fn pad_flags_match_the_seam_bit_for_bit() {
        let pairs = [
            (super::GSA_PAD_RUMBLE, PadCaps::RUMBLE),
            (super::GSA_PAD_TRIGGER_RUMBLE, PadCaps::TRIGGER_RUMBLE),
            (super::GSA_PAD_GYRO, PadCaps::GYRO),
            (super::GSA_PAD_ACCEL, PadCaps::ACCEL),
            (super::GSA_PAD_TOUCHPAD, PadCaps::TOUCHPAD),
            (super::GSA_PAD_ADAPTIVE_TRIGGERS, PadCaps::ADAPTIVE_TRIGGERS),
            (super::GSA_PAD_LED, PadCaps::LED),
            (super::GSA_PAD_BATTERY, PadCaps::BATTERY),
        ];
        for (flag, cap) in pairs {
            assert_eq!(flag, u32::from(cap.bits()));
        }
    }
}
