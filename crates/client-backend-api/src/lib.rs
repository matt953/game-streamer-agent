//! The seam between a streaming **wire protocol** and the **shared client
//! core** (spec 16). One app streams from several backends — our own gsa
//! agent over QUIC, Moonlight-protocol hosts, consoles — and everything
//! below the wire is the same work: hold early frames to a jitter target,
//! keep the reference chain honest, measure what actually reached the glass.
//!
//! The split this crate encodes:
//!
//! - **A backend owns the wire.** Discovery, pairing, transport, shard
//!   reassembly, FEC, retransmission, and the protocol's own feedback are
//!   wire-format-specific and stay inside the backend crate.
//! - **The core owns everything after a frame is whole.** De-jitter release,
//!   the reference-chain gate, freeze detection, and stream-health stats are
//!   not protocol-specific and must not be reimplemented per backend — they
//!   are where the field tuning lives.
//!
//! A backend therefore stops at *"here is a complete access unit, and here is
//! when it truly arrived"* ([`BackendFrame`]), plus a few sinks the core
//! calls back into.
//!
//! Nothing here is `dyn`-dispatched on the hot path: [`StreamBackend`] is used
//! generically (the embedder picks a backend at the call site), and the
//! session it returns is plain data plus small synchronous sinks. Frames move
//! over a channel, so adding a backend costs no indirection per frame.

use gsa_core::Result;
pub use gsa_protocol::input::InputEvent;

/// One complete encoded access unit (Annex-B) from a backend, with the timing
/// the shared core needs to gate and measure it.
///
/// Every target protocol delivers exactly this — H.264/HEVC access units plus
/// a presentation stamp — which is why the core above this type is shared.
#[derive(Debug, Clone)]
pub struct BackendFrame {
    /// The access unit. A keyframe carries its own parameter sets.
    pub data: Vec<u8>,
    /// Monotonic per-session frame counter. Backends whose wire format has no
    /// frame id synthesise one; the core uses it only for gap detection, so
    /// it must increment by exactly 1 per delivered frame.
    pub frame_id: u32,
    /// This frame resets the reference chain (IDR).
    pub keyframe: bool,
    /// Host-side stamp, interpreted per [`SessionCaps::capture_clock`]. µs,
    /// wrapping — the core's clock sync handles the wrap.
    pub capture_ts_us: u32,
    /// Client-clock µs at **true reception**, stamped by the backend's
    /// receive path before any pacing or release logic.
    ///
    /// This is load-bearing, not bookkeeping: arrival stamps feed delay-based
    /// bandwidth estimation, and a frame stamped at *release* time makes a
    /// paced present look like path congestion. Backends must stamp on the
    /// receive side and never on the way out.
    pub arrival_us: u64,
}

/// What a host's capture stamp actually means — so latency figures can say
/// what they are instead of implying a precision the wire never carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureClock {
    /// The stamp is the host's capture instant, and the backend keeps it
    /// synchronised to the client clock. Latency is true glass-to-glass.
    HostSynced,
    /// The stamp is a stream presentation timestamp with no fixed relation to
    /// the capture instant. Differences are meaningful (jitter, cadence);
    /// absolute latency is not, and must not be reported as glass-to-glass.
    StreamPts,
}

/// What a live session can actually do. The embedder's settings UI reads
/// these instead of assuming: a control the host will ignore is worse than an
/// absent one, and a bitrate slider that silently does nothing reads as a bug
/// in the stream.
#[derive(Debug, Clone, Copy)]
pub struct SessionCaps {
    /// [`SessionKnobs::set_bitrate`] takes effect mid-session.
    pub live_bitrate: bool,
    /// The host runs its own adaptive bitrate and [`SessionKnobs::set_abr`]
    /// arms it. When false there is no Auto mode to offer.
    pub server_abr: bool,
    /// The host can heal a broken reference chain without a full IDR
    /// ([`RecoverySink::request_recovery`]).
    pub reference_invalidation: bool,
    /// How to read [`BackendFrame::capture_ts_us`].
    pub capture_clock: CaptureClock,
}

/// Asks the host to repair a broken reference chain. The core calls this when
/// it detects a gap, a decoder rejection, or an unrecoverable loss; the
/// backend translates it to whatever its protocol offers.
///
/// Implementations are fire-and-forget and must not block: the core calls
/// them from the frame path. Rate limiting is the core's job, so a backend
/// may send every request it receives.
pub trait RecoverySink: std::fmt::Debug + Send + Sync {
    /// Ask for a full keyframe — always available, always sufficient.
    fn request_keyframe(&self);

    /// Ask the host to invalidate references past `last_good_frame_id` and
    /// continue without a full IDR — cheaper, and far less of a bitrate spike
    /// on a link that is already struggling. Backends without the capability
    /// leave the default, which falls back to a keyframe.
    fn request_recovery(&self, last_good_frame_id: u32) {
        let _ = last_good_frame_id;
        self.request_keyframe();
    }
}

/// Live quality controls, as far as the protocol supports them. Every method
/// is fire-and-forget; check [`SessionKnobs::caps`] before surfacing a
/// control to the user.
pub trait SessionKnobs: std::fmt::Debug + Send + Sync {
    fn caps(&self) -> SessionCaps;

    /// Request a new encode bitrate (bits/s). No-op unless
    /// [`SessionCaps::live_bitrate`]. The host clamps.
    fn set_bitrate(&self, bitrate_bps: u32) {
        let _ = bitrate_bps;
    }

    /// Arm or disarm host-side adaptive bitrate. No-op unless
    /// [`SessionCaps::server_abr`].
    fn set_abr(&self, enabled: bool) {
        let _ = enabled;
    }
}

/// Where the embedder's input goes. Fire-and-forget and safe to call from a
/// UI event loop; the backend owns ordering and delivery.
pub trait InputSink: std::fmt::Debug + Send + Sync {
    fn send(&self, events: Vec<InputEvent>);
}

/// Something that happened host-side and the embedder may want to surface.
/// Backend-neutral: a variant here must mean the same thing on every wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendEvent {
    /// The host confirmed its virtual pad for `seat` is live.
    GamepadConnected { seat: u8 },
    /// The host's virtual pad for `seat` went away.
    GamepadDisconnected { seat: u8 },
    /// Rumble for `seat`, 16-bit amplitudes.
    Rumble { seat: u8, low: u16, high: u16 },
    /// Periodic host encoder telemetry (bits/s). Fields a backend cannot
    /// know are zero — a HUD must show "—", never a fabricated number.
    EncodeStats {
        target_bitrate_bps: u32,
        emitted_bitrate_bps: u32,
        ceiling_bitrate_bps: u32,
        estimate_bitrate_bps: u32,
        abr_enabled: bool,
    },
}

/// A running stream. Plain data plus small sinks: the shared core drives this
/// identically for every backend.
///
/// Dropping it tears the session down.
#[derive(Debug)]
pub struct ActiveSession {
    /// Complete access units in decode order, stamped at true arrival. The
    /// core gates and releases these; the backend must never pace this
    /// channel to a display cadence.
    pub frames: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
    /// Decoded interleaved PCM, or `None` when the session carries no audio.
    /// Backends decode Opus with the shared audio crate so every wire lands
    /// in the same format here.
    pub audio: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
    /// Host-side events for the embedder to surface.
    pub events: tokio::sync::mpsc::UnboundedReceiver<BackendEvent>,
    pub input: Box<dyn InputSink>,
    pub knobs: Box<dyn SessionKnobs>,
    pub recovery: std::sync::Arc<dyn RecoverySink>,
    /// Negotiated video codec, for the embedder's decoder setup.
    pub codec: gsa_core::media::Codec,
}

/// What the embedder asked for. A backend honours what its protocol supports
/// and reports the truth back through [`SessionCaps`].
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// Which host-side source/app to stream; backend-defined (a gsa source
    /// id, a Moonlight app id).
    pub source_id: u32,
    /// Codecs the embedder can decode, richest first. Must include H.264.
    pub decode_codecs: Vec<gsa_core::media::Codec>,
    /// Requested bitrate (bits/s), 0 for the host default. With `abr` on this
    /// is a ceiling, matching the live control ("Auto, up to this").
    pub bitrate_bps: u32,
    /// Ask the host to run adaptive bitrate from the first frame.
    pub abr: bool,
}

/// A streaming protocol the client can speak.
///
/// Used generically — the embedder names the backend at the call site — so
/// implementations may use plain `async fn` and pay no dynamic dispatch.
/// Runtime selection happens above, over the [`ActiveSession`] each backend
/// returns.
pub trait StreamBackend: std::fmt::Debug + Send {
    /// Connect to an already-paired host and begin streaming.
    ///
    /// Pairing is deliberately **not** on this trait: enrolment differs too
    /// much between protocols to share one shape (a PIN typed on the host, a
    /// PIN shown by it, an OAuth round trip in a browser), and each backend
    /// persists its own credentials. A backend exposes its own pairing entry
    /// point and is constructed from the stored result.
    fn start(
        &mut self,
        request: SessionRequest,
    ) -> impl std::future::Future<Output = Result<ActiveSession>> + Send;
}
