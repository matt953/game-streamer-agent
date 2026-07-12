//! C ABI wrapper embedding `client-core` into the host apps (spec 01, D9).
//!
//! The apps link this into their existing Rust static lib and call it over C
//! (Swift bridging header / Android JNI), mirroring the app's `playback_ffi`
//! precedent — control + hot-path frames cross as plain bytes, never platform
//! types. This first function is a **spike**: prove the core links, connects,
//! and receives from inside the app before the real callback surface lands.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use gsa_client_core::{
    Client, ControlEvent, DecodedFrame, GamepadInput, InputEvent, InputSender, PixelOrder,
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
}

/// `on_notification` kinds. Stable across the ABI; append new values.
pub const GSA_NOTIFY_GAMEPAD_CONNECTED: u32 = 1;
pub const GSA_NOTIFY_GAMEPAD_DISCONNECTED: u32 = 2;

/// Raw `ctx` isn't `Send`; the embedder owns its thread-safety, so we carry the
/// callback set across the receive-thread boundary explicitly.
struct SendCallbacks(GsaCallbacks);
// SAFETY: the embedder guarantees `ctx` is safe to use from the receive/audio
// threads (documented on `GsaCallbacks`); Rust only passes it back opaquely.
unsafe impl Send for SendCallbacks {}

/// Opaque live session handle. Owns the receive thread (which owns the tokio
/// runtime + connection). Free exactly once with [`gsa_session_stop`].
#[derive(Debug)]
pub struct GsaSession {
    stop: Arc<Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Sync input sink (input events → reliable control stream). Present once
    /// the session is streaming; `None` if input couldn't be enabled.
    input: Option<InputSender>,
    /// The negotiated codec (a `GSA_CODEC_*` flag), for `gsa_session_codec`.
    codec: u32,
}

/// Handed back from `session_loop` once it knows the outcome: whether the
/// session reached the streaming state, plus its input sink and negotiated codec.
enum SessionReady {
    Failed,
    Streaming {
        input: Option<InputSender>,
        codec: u32,
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
    callbacks: GsaCallbacks,
) -> *mut GsaSession {
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
                let _ = ready_tx.send(SessionReady::Failed);
                return;
            }
        };
        rt.block_on(session_loop(
            addr,
            source_id,
            codecs_from_flags(decode_codecs),
            cbs,
            thread_stop,
            ready_tx,
        ));
    });

    match ready_rx.recv() {
        Ok(SessionReady::Streaming { input, codec }) => Box::into_raw(Box::new(GsaSession {
            stop,
            thread: Some(thread),
            input,
            codec,
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
    if let Some(input) = &unsafe { &*session }.input {
        input.set_bitrate(bitrate_bps);
    }
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
    if let Some(input) = &unsafe { &*session }.input {
        input.set_abr(enabled);
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
fn boost_thread_qos() {
    // SAFETY: sets only the calling thread's QoS class; always safe to call.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(not(target_vendor = "apple"))]
fn boost_thread_qos() {}

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
async fn session_loop(
    addr: std::net::SocketAddr,
    source_id: u32,
    decode_codecs: Vec<Codec>,
    cbs: SendCallbacks,
    stop: Arc<Notify>,
    ready_tx: std::sync::mpsc::Sender<SessionReady>,
) {
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
            let _ = ready_tx.send(SessionReady::Failed);
            return;
        }
    };
    // Enable audio decode before the first recv so no datagrams are dropped.
    let audio_rx = match client.take_audio_output() {
        Ok(rx) => rx,
        Err(_) => {
            let _ = ready_tx.send(SessionReady::Failed);
            return;
        }
    };
    if client
        .start_session(SourceId(source_id), None)
        .await
        .is_err()
    {
        let _ = ready_tx.send(SessionReady::Failed);
        return;
    }
    // Host-pushed notifications (e.g. gamepad plugged) arrive on the control
    // stream; handle them in the select loop below so they fire on this thread
    // (which `gsa_session_stop` joins — no callback outlives the session).
    let mut control_rx = client.take_control_events();

    // Hand the sync input sink back with the ready signal; it also routes the
    // recv loop's keyframe requests through its background writer task.
    let input = client.take_input_sender();
    let codec = client
        .negotiated_codec()
        .map_or(GSA_CODEC_H264, codec_to_flag);
    let _ = ready_tx.send(SessionReady::Streaming { input, codec });

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

    loop {
        tokio::select! {
            _ = stop.notified() => break,
            // Agent notification (gamepad plugged, etc.): forward to the embedder.
            event = async {
                match &mut control_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<ControlEvent>>().await,
                }
            } => {
                if let Some(event) = event {
                    let notify = match event {
                        ControlEvent::GamepadConnected { seat } => {
                            Some((GSA_NOTIFY_GAMEPAD_CONNECTED, seat as u32))
                        }
                        ControlEvent::GamepadDisconnected { seat } => {
                            Some((GSA_NOTIFY_GAMEPAD_DISCONNECTED, seat as u32))
                        }
                        // Encoder telemetry isn't a user notification; no mobile
                        // surface for it yet, so drop it here.
                        ControlEvent::EncodeStats { .. } => None,
                    };
                    if let (Some((kind, arg)), Some(cb)) = (notify, cbs.on_notification) {
                        // SAFETY: `ctx` valid for the session per the embedder contract.
                        unsafe { cb(cbs.ctx, kind, arg) };
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

/// Carries a raw `ctx` onto the audio thread. Same embedder contract as
/// [`SendCallbacks`].
struct SendPtr(*mut c_void);
// SAFETY: see `GsaCallbacks` threading contract.
unsafe impl Send for SendPtr {}
