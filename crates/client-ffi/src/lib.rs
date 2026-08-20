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

/// Read an embedder's ordered preference list.
///
/// Order is the preference — the backend takes the first entry its host also
/// has — so a list, not a mask: a mask says only *which* are allowed, and the
/// user's ordering would be lost. H.264 is appended whatever is passed, since
/// a session with nothing to negotiate is worse than one that falls back.
///
/// # Safety
/// `codecs` must point to `len` `GSA_CODEC_*` values, or be null with `len` 0.
pub(crate) unsafe fn codecs_from_list(codecs: *const u32, len: usize) -> Vec<Codec> {
    let mut out = Vec::with_capacity(len + 1);
    if !codecs.is_null() {
        // SAFETY: caller contract.
        for flag in unsafe { std::slice::from_raw_parts(codecs, len) } {
            let codec = match *flag {
                GSA_CODEC_HEVC => Codec::Hevc,
                GSA_CODEC_AV1 => Codec::Av1,
                _ => Codec::H264,
            };
            if !out.contains(&codec) {
                out.push(codec);
            }
        }
    }
    if !out.contains(&Codec::H264) {
        out.push(Codec::H264);
    }
    out
}

pub(crate) fn codecs_from_flags(flags: u32) -> Vec<Codec> {
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
/// The picture to ask the host for.
///
/// Pass the client's own display geometry. A host that can create a display to
/// match renders at exactly this — right aspect, right refresh, native
/// sharpness, and a game sees the real resolution rather than a cropped or
/// stretched one. A host that cannot scales its own desktop into these
/// dimensions instead, which still honours the size but letterboxes a shape it
/// does not have. Neither is detectable from the client, so the request is the
/// same either way.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GsaStreamMode {
    /// Zero for both dimensions falls back to 1080p: a session must start
    /// with something, and an embedder that cannot read its own display is
    /// better served by a common mode than by a refusal.
    pub width: u32,
    pub height: u32,
    /// Zero falls back to 60.
    pub fps: u32,
    /// Non-zero lets the host change its *physical* desktop resolution to
    /// match. Off by default: it rearranges the windows of whoever is using
    /// that machine, and it is unnecessary on a host that can make a display
    /// for the session.
    pub allow_host_mode_change: u32,
    /// Non-zero asks the host for HDR. A request, not a guarantee: a host
    /// whose display cannot do HDR answers in SDR without saying so, so what
    /// arrived is reported per session rather than assumed from this.
    pub hdr: u32,
    /// The latency-for-smoothness trade, one of the `GSA_PACING_*` values.
    /// The whole policy — how long a frame may be held, whether an unshown
    /// frame is dropped for a newer one, what rate to ask the host for — is
    /// resolved from this one value in the shared core, so a platform cannot
    /// quietly mean something different by the same setting. Out-of-range
    /// values fall back to Balanced, the recommended default.
    pub pacing: u32,
}

/// `GsaStreamMode::pacing` values, matching the reference client's modes.
pub const GSA_PACING_LOWEST_LATENCY: u32 = 0;
pub const GSA_PACING_BALANCED: u32 = 1;
pub const GSA_PACING_BALANCED_FPS_LIMIT: u32 = 2;
pub const GSA_PACING_SMOOTHEST: u32 = 3;

pub(crate) fn pacing_from_u32(value: u32) -> gsa_client_core::PacingMode {
    match value {
        GSA_PACING_LOWEST_LATENCY => gsa_client_core::PacingMode::LowestLatency,
        GSA_PACING_BALANCED_FPS_LIMIT => gsa_client_core::PacingMode::BalancedFpsLimit,
        GSA_PACING_SMOOTHEST => gsa_client_core::PacingMode::Smoothest,
        _ => gsa_client_core::PacingMode::Balanced,
    }
}

impl GsaStreamMode {
    /// Fill in what the embedder left at zero.
    fn resolve(self) -> (u32, u32, u32) {
        let (width, height) = if self.width == 0 || self.height == 0 {
            (1920, 1080)
        } else {
            (self.width, self.height)
        };
        (width, height, if self.fps == 0 { 60 } else { self.fps })
    }
}

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
    /// gamepad kinds). Fires on a dedicated thread. Ignore kinds you do not
    /// handle rather than treating them as an error.
    pub on_notification: Option<unsafe extern "C" fn(ctx: *mut c_void, kind: u32, arg: u32)>,
    /// The host asked a controller to do something: rumble, trigger motors, an
    /// LED colour, or to start sending motion. `kind` is a
    /// `GSA_PAD_FEEDBACK_*` value and `a`/`b`/`c` are kind-specific — see each
    /// constant. Fires on the session's control thread.
    ///
    /// Render only what the pad actually has: the session's capabilities
    /// (`gsa_session_pad_caps`) say what the wire carries, not what is in the
    /// user's hands. Ignore kinds you do not handle rather than treating them
    /// as an error.
    pub on_pad_feedback: Option<
        unsafe extern "C" fn(ctx: *mut c_void, kind: u32, seat: u32, a: u32, b: u32, c: u32),
    >,
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

/// `on_notification` kinds.
pub const GSA_NOTIFY_GAMEPAD_CONNECTED: u32 = 1;
pub const GSA_NOTIFY_GAMEPAD_DISCONNECTED: u32 = 2;

/// Kinds for [`GsaCallbacks::on_pad_feedback`].
///
/// Body rumble: `a` is the low-frequency motor, `b` the high-frequency one,
/// both 0..=65535. A zero pair is a **stop** and must be honoured — the host
/// ends an effect explicitly rather than giving it a duration.
pub const GSA_PAD_FEEDBACK_RUMBLE: u32 = 1;
/// Trigger motors, independent of body rumble: `a` left, `b` right.
pub const GSA_PAD_FEEDBACK_TRIGGER_RUMBLE: u32 = 2;
/// Lightbar colour: `a`, `b`, `c` are red, green and blue, 0..=255.
pub const GSA_PAD_FEEDBACK_LED: u32 = 3;
/// The host wants motion samples: `a` is the rate in Hz, `b` is 0 for the
/// gyroscope and 1 for the accelerometer. **Do not sample before this
/// arrives** — motion is opt-in on every protocol that carries it, and an
/// unread stream is battery spent for nothing.
pub const GSA_PAD_FEEDBACK_MOTION_REQUEST: u32 = 4;

/// Pad families for [`gsa_announce_gamepad`]. Announce what the user actually
/// holds: hosts build a matching virtual device, and the choice decides which
/// features exist for the whole session.
pub const GSA_PAD_KIND_GENERIC: u32 = 0;
pub const GSA_PAD_KIND_XBOX: u32 = 1;
pub const GSA_PAD_KIND_DUALSHOCK4: u32 = 2;
pub const GSA_PAD_KIND_DUALSENSE: u32 = 3;
pub const GSA_PAD_KIND_SWITCH_PRO: u32 = 4;

/// Contact phases for [`gsa_send_gamepad_touch`]. `CANCEL` is not `UP`: the
/// contact ended without the user lifting, and a game that treats it as a
/// release fires the action they aborted.
pub const GSA_TOUCH_DOWN: u32 = 0;
pub const GSA_TOUCH_MOVE: u32 = 1;
pub const GSA_TOUCH_UP: u32 = 2;
pub const GSA_TOUCH_CANCEL: u32 = 3;

/// Battery states for [`gsa_send_gamepad_battery`].
pub const GSA_BATTERY_UNKNOWN: u32 = 0;
pub const GSA_BATTERY_NOT_PRESENT: u32 = 1;
pub const GSA_BATTERY_DISCHARGING: u32 = 2;
pub const GSA_BATTERY_CHARGING: u32 = 3;
pub const GSA_BATTERY_FULL: u32 = 4;
/// Percentage meaning "there is a battery, but its level is unknown".
pub const GSA_BATTERY_PERCENT_UNKNOWN: u32 = 255;

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
    /// Live latency-chain summary for [`gsa_session_latency`], republished
    /// about once a second by the session loop. Behind a mutex because it is
    /// a struct, read at UI rate, written at 1 Hz — contention is nil.
    latency: std::sync::Arc<std::sync::Mutex<gsa_client_core::LatencySummary>>,
    /// Live pacing figures for [`gsa_session_pacing`]: the spread of transit
    /// drift the link delivered, and the spread after pacing. Shared rather
    /// than pushed, because they change every frame and an overlay wants
    /// whatever is current, not every value that ever was.
    pacing: std::sync::Arc<(std::sync::atomic::AtomicU32, std::sync::atomic::AtomicU32)>,
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
        /// Transit-drift spread as delivered, and as released after pacing.
        pacing: std::sync::Arc<(std::sync::atomic::AtomicU32, std::sync::atomic::AtomicU32)>,
        /// Per-stage latency percentiles, republished by the session loop.
        latency: std::sync::Arc<std::sync::Mutex<gsa_client_core::LatencySummary>>,
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
            latency,
            presented,
            decode_error,
            dejitter,
            pacing,
            codec,
            pad_caps,
        }) => Box::into_raw(Box::new(GsaSession {
            stop,
            thread: Some(thread),
            latency,
            input,
            knobs,
            presented,
            decode_error,
            dejitter,
            pacing,
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

/// How much the link's timing wobbled, and how much of that survived pacing.
///
/// Both figures are the spread (p90 − p10) of transit drift in microseconds:
/// `delivered_us` as frames arrived, `paced_us` as they were released. The
/// pair is the point — `paced_us` alone cannot distinguish good pacing from a
/// link that was never troubled, and `delivered_us` alone says nothing about
/// what was done with it.
///
/// Zero means not yet measured, which a session shows for its first second and
/// a backend without a capture stamp shows forever. Show "—" rather than "0".
///
/// # Safety
/// `session` must be a live handle, or NULL. `delivered_us` and `paced_us`
/// must be writable, or NULL to skip.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_pacing(
    session: *const GsaSession,
    delivered_us: *mut u32,
    paced_us: *mut u32,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees a live handle.
    let pacing = &unsafe { &*session }.pacing;
    use std::sync::atomic::Ordering::Relaxed;
    if !delivered_us.is_null() {
        // SAFETY: caller contract — writable or null, checked above.
        unsafe { *delivered_us = pacing.0.load(Relaxed) };
    }
    if !paced_us.is_null() {
        // SAFETY: as above.
        unsafe { *paced_us = pacing.1.load(Relaxed) };
    }
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

/// One latency stage's percentiles, µs. `valid == 0` means the stage was
/// never measured — show it as unknown, never as zero.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GsaLatencyStage {
    pub valid: u32,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
}

/// The latency chain as the reference client's overlay composes it: the wire
/// as a measured round trip, the host's own capture→encode duration, and the
/// client-side stages — plus the composed total (half the round trip + every
/// measured duration). Durations and round trips only, so no clock sync is
/// involved anywhere.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GsaLatencyChain {
    pub rtt: GsaLatencyStage,
    pub host: GsaLatencyStage,
    pub decode: GsaLatencyStage,
    pub hold: GsaLatencyStage,
    pub present: GsaLatencyStage,
    pub total: GsaLatencyStage,
}

fn stage_out(stage: Option<gsa_client_core::StagePercentiles>) -> GsaLatencyStage {
    stage.map_or_else(GsaLatencyStage::default, |s| GsaLatencyStage {
        valid: 1,
        p50_us: s.p50_us,
        p95_us: s.p95_us,
        p99_us: s.p99_us,
    })
}

/// Read the session's live latency chain.
///
/// # Safety
/// `session` must be a live handle; `out` must point to a writable
/// [`GsaLatencyChain`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_session_latency(
    session: *const GsaSession,
    out: *mut GsaLatencyChain,
) {
    if session.is_null() || out.is_null() {
        return;
    }
    // SAFETY: caller contract — a live session handle.
    let summary = *unsafe { &*session }
        .latency
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: caller contract — `out` is writable.
    unsafe {
        *out = GsaLatencyChain {
            rtt: stage_out(summary.rtt),
            host: stage_out(summary.host),
            decode: stage_out(summary.decode),
            hold: stage_out(summary.hold),
            present: stage_out(summary.present),
            total: stage_out(summary.total),
        };
    }
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

/// Identify a pad's family from what the platform can tell you about it.
///
/// **Use this rather than matching names in the app.** Names overlap in ways
/// that bite: a DualSense is often called "Wireless Controller" and an Xbox pad
/// "Xbox Wireless Controller", so a substring test for the former claims the
/// latter — and the host then builds the wrong virtual device for the whole
/// session. Sharing the rules here keeps every client identifying a pad the
/// same way, with the awkward cases covered by tests instead of by memory.
///
/// Pass `0` for an id the platform does not expose. Where a platform has
/// better evidence than a heuristic — a concrete device type rather than a
/// name — it should use that first and call this only as a fallback.
/// Unrecognised pads come back as [`GSA_PAD_KIND_XBOX`].
///
/// # Safety
/// `name` must be a valid NUL-terminated string for the call, or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_identify_pad(
    vendor_id: u32,
    product_id: u32,
    name: *const c_char,
) -> u32 {
    // SAFETY: caller contract; a null or non-UTF-8 name identifies by id alone.
    let name = unsafe { host::read_str(name) }.unwrap_or_default();
    pad_kind_to_flag(gsa_client_core::PadKind::identify(
        vendor_id, product_id, name,
    ))
}

/// What a pad of this family has, whatever the platform reports about it.
///
/// OR this with what the client can actually observe (sensors, a vibrator, a
/// battery): the hardware in the box is a fact about the model, while what an
/// OS exposes is a fact about the OS.
#[unsafe(no_mangle)]
pub extern "C" fn gsa_pad_family_caps(kind: u32) -> u32 {
    u32::from(pad_kind_from_flag(kind).implied_caps().bits())
}

fn pad_kind_to_flag(kind: gsa_client_core::PadKind) -> u32 {
    match kind {
        gsa_client_core::PadKind::Xbox => GSA_PAD_KIND_XBOX,
        gsa_client_core::PadKind::DualShock4 => GSA_PAD_KIND_DUALSHOCK4,
        gsa_client_core::PadKind::DualSense => GSA_PAD_KIND_DUALSENSE,
        gsa_client_core::PadKind::SwitchPro => GSA_PAD_KIND_SWITCH_PRO,
        _ => GSA_PAD_KIND_GENERIC,
    }
}

fn pad_kind_from_flag(kind: u32) -> gsa_client_core::PadKind {
    match kind {
        GSA_PAD_KIND_XBOX => gsa_client_core::PadKind::Xbox,
        GSA_PAD_KIND_DUALSHOCK4 => gsa_client_core::PadKind::DualShock4,
        GSA_PAD_KIND_DUALSENSE => gsa_client_core::PadKind::DualSense,
        GSA_PAD_KIND_SWITCH_PRO => gsa_client_core::PadKind::SwitchPro,
        _ => gsa_client_core::PadKind::Generic,
    }
}

/// Tell the host what controller occupies `seat`, before sending any state.
///
/// **Announce first, and announce again on every reconnect.** A host plugs a
/// *default* pad the moment state arrives for a seat, and then ignores a later
/// announcement for a seat it already has — so a snapshot that beats this call
/// leaves the seat as the wrong device for the whole session, with motion,
/// touch and battery silently dropped. This call clears the seat first, so
/// calling it late recovers rather than being ignored.
///
/// `kind` is a `GSA_PAD_KIND_*` value and `caps` is the OR of the `GSA_PAD_*`
/// flags the physical pad has. Report both honestly: hosts decide what virtual
/// device to build from them, and claiming a capability the pad lacks produces
/// feedback nothing can render.
///
/// # Safety
/// `session` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_announce_gamepad(
    session: *mut GsaSession,
    seat: u8,
    kind: u32,
    caps: u32,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: see `gsa_send_gamepad`.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        let kind = pad_kind_from_flag(kind);
        let caps = gsa_client_core::PadCaps::from_bits(caps as u16);
        input.announce_pad(seat, gsa_client_core::GamepadProfile::new(kind, caps));
    }
}

/// Send one motion sample for `seat`.
///
/// **Only after `GSA_PAD_FEEDBACK_MOTION_REQUEST`**, and at the rate it asked
/// for. Gyroscope values are degrees per second; acceleration is m/s²
/// **including gravity**, so a pad lying still reads about 9.81 on one axis
/// rather than zero. A reading of zero at rest means the sensors were never
/// switched on — on some platforms they must be enabled explicitly, and an
/// inactive sensor looks exactly like a perfectly still hand.
///
/// # Safety
/// `session` must be a live handle.
#[unsafe(no_mangle)]
#[allow(
    clippy::too_many_arguments,
    reason = "a C ABI takes scalars, not structs"
)]
pub unsafe extern "C" fn gsa_send_gamepad_motion(
    session: *mut GsaSession,
    seat: u8,
    gyro_x: f32,
    gyro_y: f32,
    gyro_z: f32,
    accel_x: f32,
    accel_y: f32,
    accel_z: f32,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: see `gsa_send_gamepad`.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        input.send(vec![InputEvent::GamepadMotion {
            seat,
            gyro: [gyro_x, gyro_y, gyro_z],
            accel: [accel_x, accel_y, accel_z],
            ts_us: 0,
        }]);
    }
}

/// Send one contact on the pad's own touch surface.
///
/// `phase` is a `GSA_TOUCH_*` value; `x`/`y` are normalised [0,1] from the
/// top-left of the surface, and `pressure` [0,1] (use 1.0 for a surface that
/// reports contact without pressure). `pointer` identifies the finger and must
/// stay stable for the life of that contact.
///
/// # Safety
/// `session` must be a live handle.
#[unsafe(no_mangle)]
#[allow(
    clippy::too_many_arguments,
    reason = "a C ABI takes scalars, not structs"
)]
pub unsafe extern "C" fn gsa_send_gamepad_touch(
    session: *mut GsaSession,
    seat: u8,
    pointer: u8,
    phase: u32,
    x: f32,
    y: f32,
    pressure: f32,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: see `gsa_send_gamepad`.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        let phase = match phase {
            GSA_TOUCH_DOWN => gsa_protocol::input::TouchPhase::Down,
            GSA_TOUCH_MOVE => gsa_protocol::input::TouchPhase::Move,
            GSA_TOUCH_UP => gsa_protocol::input::TouchPhase::Up,
            GSA_TOUCH_CANCEL => gsa_protocol::input::TouchPhase::Cancel,
            // An unknown phase is dropped rather than guessed: reporting the
            // wrong one strands a contact down or releases one still held.
            _ => return,
        };
        input.send(vec![InputEvent::GamepadTouch {
            seat,
            pointer,
            phase,
            x,
            y,
            pressure,
            ts_us: 0,
        }]);
    }
}

/// Report the pad's charge for `seat`.
///
/// `state` is a `GSA_BATTERY_*` value; `percent` is 0..=100, or
/// [`GSA_BATTERY_PERCENT_UNKNOWN`] when the pad reports a state but no level —
/// which is not the same as an empty battery. Send on change, not per frame.
///
/// # Safety
/// `session` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_send_gamepad_battery(
    session: *mut GsaSession,
    seat: u8,
    state: u32,
    percent: u32,
) {
    if session.is_null() {
        return;
    }
    // SAFETY: see `gsa_send_gamepad`.
    let session = unsafe { &*session };
    if let Some(input) = &session.input {
        let state = match state {
            GSA_BATTERY_NOT_PRESENT => gsa_protocol::input::BatteryState::NotPresent,
            GSA_BATTERY_DISCHARGING => gsa_protocol::input::BatteryState::Discharging,
            GSA_BATTERY_CHARGING => gsa_protocol::input::BatteryState::Charging,
            GSA_BATTERY_FULL => gsa_protocol::input::BatteryState::Full,
            _ => gsa_protocol::input::BatteryState::Unknown,
        };
        input.send(vec![InputEvent::GamepadBattery {
            seat,
            state,
            percent: (percent <= 100).then_some(percent as u8),
            ts_us: 0,
        }]);
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
    // The latency chain, republished about once a second for
    // `gsa_session_latency` — measured outright on this backend, since its
    // clocks are synced.
    let latency = std::sync::Arc::new(std::sync::Mutex::new(
        gsa_client_core::LatencySummary::default(),
    ));
    let mut last_latency_publish = std::time::Instant::now();

    let _ = ready_tx.send(SessionReady::Streaming {
        input,
        knobs,
        presented,
        decode_error,
        dejitter,
        pacing: std::sync::Arc::new((
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicU32::new(0),
        )),
        latency: latency.clone(),
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
            // Bound the backlog before delivering: a sink that blocks (a full
            // platform audio buffer) lets this channel grow during a stall,
            // and every queued packet plays that far behind the live video —
            // a desync that never heals, because the queue only drains by
            // underrun. Keeping only the newest few packets means a stall is
            // followed by a jump back to live rather than a permanent lag.
            const MAX_QUEUED_PACKETS: usize = 5; // ~50 ms at 10 ms a packet
            let mut queued = std::collections::VecDeque::from([pcm]);
            while let Ok(more) = audio_rx.try_recv() {
                queued.push_back(more);
                if queued.len() > MAX_QUEUED_PACKETS {
                    queued.pop_front();
                }
            }
            for pcm in queued {
                if let Some(cb) = on_audio {
                    // SAFETY: pointer+len describe this owned buffer for the
                    // call; the embedder copies what it keeps.
                    unsafe { cb(audio_ctx.0, pcm.as_ptr(), pcm.len()) };
                }
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
                    if last_latency_publish.elapsed() >= std::time::Duration::from_secs(1) {
                        last_latency_publish = std::time::Instant::now();
                        if let Ok(mut slot) = latency.lock() {
                            *slot = client.latency_chain();
                        }
                    }
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

/// Deliver one piece of controller feedback to the embedder, if it registered
/// a callback. Backend-neutral: the caller has already translated the wire
/// into [`gsa_client_core::BackendEvent`], so every protocol lands here in the
/// same shape.
pub(crate) fn fire_pad_feedback(cbs: &GsaCallbacks, event: &gsa_client_core::BackendEvent) {
    let Some(cb) = cbs.on_pad_feedback else {
        return;
    };
    use gsa_client_core::{BackendEvent, GamepadFeedback, MotionSensor};
    let (kind, seat, a, b, c) = match *event {
        BackendEvent::Feedback(GamepadFeedback::Rumble { seat, low, high }) => (
            GSA_PAD_FEEDBACK_RUMBLE,
            seat,
            u32::from(low),
            u32::from(high),
            0,
        ),
        BackendEvent::Feedback(GamepadFeedback::TriggerRumble { seat, left, right }) => (
            GSA_PAD_FEEDBACK_TRIGGER_RUMBLE,
            seat,
            u32::from(left),
            u32::from(right),
            0,
        ),
        BackendEvent::Feedback(GamepadFeedback::Led { seat, rgb }) => (
            GSA_PAD_FEEDBACK_LED,
            seat,
            u32::from(rgb[0]),
            u32::from(rgb[1]),
            u32::from(rgb[2]),
        ),
        BackendEvent::MotionRequested {
            seat,
            sensor,
            rate_hz,
        } => (
            GSA_PAD_FEEDBACK_MOTION_REQUEST,
            seat,
            u32::from(rate_hz),
            u32::from(sensor == MotionSensor::Accel),
            0,
        ),
        // Trigger effects are an opaque vendor blob and do not fit this
        // shape; they need their own entry point rather than a lossy
        // encoding here, and no host we support emits them yet.
        _ => return,
    };
    // SAFETY: `ctx` valid for the session per the embedder contract.
    unsafe { cb(cbs.ctx, kind, u32::from(seat), a, b, c) };
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
