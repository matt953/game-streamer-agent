//! The seam between a streaming **wire protocol** and the **shared client
//! core** (spec 16). Every backend — gsa over QUIC, Moonlight-protocol hosts,
//! consoles, cloud services — implements this crate's traits and nothing else.
//!
//! The division of responsibility is the contract:
//!
//! - **A backend owns the wire.** Discovery, pairing, transport, shard
//!   reassembly, FEC, retransmission, and the protocol's own feedback are
//!   wire-format-specific and stay inside the backend crate.
//! - **The core owns everything after a frame is whole.** De-jitter release,
//!   the reference-chain gate, freeze detection, and stream-health stats are
//!   protocol-independent and must not be reimplemented per backend.
//!
//! A backend therefore stops at "here is a complete access unit, and here is
//! when it truly arrived" ([`BackendFrame`]), plus the sinks the core calls
//! back into.
//!
//! Nothing is `dyn`-dispatched on the hot path: [`StreamBackend`] is used
//! generically (the embedder names the backend at the call site) and the
//! session it returns is plain data plus small synchronous sinks, with frames
//! arriving over a channel.

pub mod gamepad;

pub use gamepad::{GamepadFeedback, GamepadProfile, MotionSensor, PadCaps, PadKind, TriggerEffect};
use gsa_core::Result;
pub use gsa_protocol::input::InputEvent;

/// One complete encoded access unit (Annex-B) from a backend, with the timing
/// the core needs to gate and measure it.
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
    /// Capture-to-encode time measured *on the host* and carried per frame,
    /// where the backend's wire provides it. A duration on a single clock, so
    /// it needs no sync — it is the host's own share of the latency chain.
    pub host_latency_us: Option<u32>,
    /// Client-clock µs at **true reception**. Must be stamped on the receive
    /// path, before any pacing or release logic: arrival stamps feed
    /// delay-based bandwidth estimation, and a frame stamped at release time
    /// makes paced output indistinguishable from path congestion.
    pub arrival_us: u64,
}

/// What a backend's [`BackendFrame::capture_ts_us`] means. This decides
/// whether absolute latency exists at all for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureClock {
    /// The stamp is the host's capture instant, kept synchronised to the
    /// client clock by the backend. Latency is true glass-to-glass.
    HostSynced,
    /// The stamp is a stream presentation timestamp with no fixed relation to
    /// the capture instant. Differences are meaningful (cadence, jitter);
    /// the absolute value is not, and must never be reported as latency.
    StreamPts,
}

/// What a live session supports. The embedder's settings UI must gate controls
/// on these rather than assume: a control whose backend ignores it presents as
/// a broken stream.
#[derive(Debug, Clone, Copy)]
pub struct SessionCaps {
    /// [`SessionKnobs::set_bitrate`] takes effect mid-session.
    pub live_bitrate: bool,
    /// The host runs its own adaptive bitrate and [`SessionKnobs::set_abr`]
    /// arms it. When false there is no Auto mode to offer the user.
    pub server_abr: bool,
    /// The host can heal a broken reference chain without a full IDR
    /// ([`RecoverySink::request_recovery`]).
    pub reference_invalidation: bool,
    /// How to read [`BackendFrame::capture_ts_us`].
    pub capture_clock: CaptureClock,
    /// What of a controller this session carries, in both directions. The
    /// embedder enables a pad feature only where this and the pad's own
    /// [`GamepadProfile::caps`] agree — see [`gamepad`].
    pub pads: PadCaps,
}

/// Asks the host to repair a broken reference chain. The core calls this on a
/// frame gap, a decoder rejection, or unrecoverable loss; the backend
/// translates it to whatever its protocol offers.
///
/// Implementations must not block — the core calls them from the frame path —
/// and are fire-and-forget. The core rate-limits, so a backend may send every
/// request it receives.
pub trait RecoverySink: std::fmt::Debug + Send + Sync {
    /// Ask for a full keyframe. Always available and always sufficient.
    fn request_keyframe(&self);

    /// Ask the host to invalidate references past `last_good_frame_id` and
    /// continue without a full IDR, avoiding the bitrate spike a keyframe
    /// costs on an already-struggling link. Backends without the capability
    /// leave the default, which falls back to a keyframe.
    fn request_recovery(&self, last_good_frame_id: u32) {
        let _ = last_good_frame_id;
        self.request_keyframe();
    }
}

/// Live quality controls, to the extent the protocol supports them. Every
/// method is fire-and-forget; check [`SessionKnobs::caps`] before surfacing a
/// control to the user.
pub trait SessionKnobs: std::fmt::Debug + Send + Sync {
    fn caps(&self) -> SessionCaps;

    /// Request a new encode bitrate (bits/s); the host clamps it. No-op unless
    /// [`SessionCaps::live_bitrate`].
    fn set_bitrate(&self, bitrate_bps: u32) {
        let _ = bitrate_bps;
    }

    /// Arm or disarm host-side adaptive bitrate. No-op unless
    /// [`SessionCaps::server_abr`].
    fn set_abr(&self, enabled: bool) {
        let _ = enabled;
    }
}

/// Where the embedder's input goes. Must be non-blocking — it is called from
/// a UI event loop — and the backend owns ordering and delivery.
pub trait InputSink: std::fmt::Debug + Send + Sync {
    fn send(&self, events: Vec<InputEvent>);

    /// Tell the host what pad occupies `seat`, so it can present a matching
    /// virtual device and enable the features the pad actually has.
    ///
    /// Announce on connect and on any change. Backends whose protocol has no
    /// announcement keep the default and infer the pad from its input.
    fn announce_pad(&self, seat: u8, profile: GamepadProfile) {
        let _ = (seat, profile);
    }
}

/// A host-side event the embedder may surface. Backend-neutral: a variant here
/// must mean the same thing on every wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendEvent {
    /// The host confirmed its virtual pad for `seat` is live.
    GamepadConnected { seat: u8 },
    /// The host's virtual pad for `seat` went away.
    GamepadDisconnected { seat: u8 },
    /// The game asked a pad to do something — rumble, trigger resistance, an
    /// LED colour. The embedder renders what the pad supports and drops the
    /// rest ([`GamepadFeedback::requires`]).
    Feedback(GamepadFeedback),
    /// The host wants motion samples for `seat` at `rate_hz`, and not before:
    /// motion is opt-in on every protocol that carries it, because a client
    /// that streams gyro nobody consumes spends battery for nothing. Sampling
    /// starts on this and stops when the pad goes away.
    MotionRequested {
        seat: u8,
        sensor: MotionSensor,
        rate_hz: u16,
    },
    /// The control link's measured round-trip time — the wire's share of the
    /// latency chain, measured as a round trip on one clock so it needs no
    /// sync. Periodic; smoothed by the transport's own estimator.
    LinkRtt { rtt_us: u32 },
    /// Periodic host encoder telemetry (bits/s). A field the backend cannot
    /// know is 0, meaning unmeasured: display it as "—", not as zero.
    EncodeStats {
        target_bitrate_bps: u32,
        emitted_bitrate_bps: u32,
        ceiling_bitrate_bps: u32,
        estimate_bitrate_bps: u32,
        abr_enabled: bool,
    },
}

/// A running stream: plain data plus small sinks, driven identically by the
/// core for every backend.
///
/// Dropping it tears the session down.
#[derive(Debug)]
pub struct ActiveSession {
    /// Complete access units in decode order, stamped at true arrival. The
    /// core gates and releases these, so the backend must never pace this
    /// channel to a display cadence.
    pub frames: tokio::sync::mpsc::UnboundedReceiver<BackendFrame>,
    /// Decoded interleaved PCM, or `None` when the session carries no audio.
    /// Backends decode with the shared audio crate so every wire lands here in
    /// the same format.
    pub audio: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
    /// Host-side events for the embedder to surface.
    pub events: tokio::sync::mpsc::UnboundedReceiver<BackendEvent>,
    pub input: Box<dyn InputSink>,
    pub knobs: Box<dyn SessionKnobs>,
    pub recovery: std::sync::Arc<dyn RecoverySink>,
    /// Negotiated video codec, for the embedder's decoder setup.
    pub codec: gsa_core::media::Codec,
}

/// What kind of thing a catalog entry launches: enough for the embedder to
/// group and label a unified library without knowing the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CatalogKind {
    /// A specific game or application.
    Game,
    /// The host's whole desktop.
    Desktop,
    /// A launcher in its own right (Steam Big Picture, a console dashboard):
    /// the user browses their real library inside the stream.
    Shell,
}

/// One launchable thing on a host. Backends map their own vocabulary onto this
/// so the embedder can present one library across every backend.
///
/// Kept thin because a cloud catalog can run to thousands of entries; artwork
/// is fetched lazily per entry rather than carried here.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Backend-defined; goes back in [`SessionRequest::source_id`].
    pub id: u32,
    pub title: String,
    pub kind: CatalogKind,
    /// This entry is what the host is already running, so starting it
    /// resumes rather than launches.
    pub running: bool,
}

/// What the embedder asked for. A backend honours what its protocol supports
/// and reports what it actually got through [`SessionCaps`].
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// Which host-side source/app to stream. Backend-defined; matches
    /// [`CatalogEntry::id`].
    pub source_id: u32,
    /// Codecs the embedder can decode, richest first. Must include H.264.
    pub decode_codecs: Vec<gsa_core::media::Codec>,
    /// Requested bitrate (bits/s), 0 for the host default. With `abr` on this
    /// is a ceiling, matching the live control ("Auto, up to this").
    pub bitrate_bps: u32,
    /// Ask the host to run adaptive bitrate from the first frame.
    pub abr: bool,
}

/// How a session came to be.
///
/// Every protocol distinguishes starting something new from rejoining what the
/// host is already running, however it spells it. The distinction is surfaced
/// rather than hidden: the two have different costs, and the embedder needs it
/// to explain why a stream opens mid-game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOrigin {
    /// The host started the app for us.
    Launched,
    /// We rejoined a session the host was already running.
    Rejoined,
}

/// A streaming protocol the client can speak.
///
/// Used generically — the embedder names the backend at the call site — so
/// implementations may use plain `async fn` and pay no dynamic dispatch.
/// Runtime selection happens above, over the [`ActiveSession`] each backend
/// returns.
pub trait StreamBackend: std::fmt::Debug + Send {
    /// What this host can launch. The embedder merges the catalogs of every
    /// paired host into one library, so entries must be presentable side by
    /// side without the UI knowing which protocol produced them.
    ///
    /// A backend whose catalog is a single fixed destination returns one
    /// [`CatalogKind::Shell`] entry.
    fn catalog(&mut self) -> impl std::future::Future<Output = Result<Vec<CatalogEntry>>> + Send;

    /// Connect to an already-paired host and begin streaming.
    ///
    /// Pairing is not part of this trait: enrolment shapes differ too much to
    /// unify (a PIN typed on the host, a PIN shown by it, an OAuth round trip
    /// in a browser) and each backend persists its own credentials. A backend
    /// exposes its own pairing entry point and is constructed from the stored
    /// result.
    fn start(
        &mut self,
        request: SessionRequest,
    ) -> impl std::future::Future<Output = Result<ActiveSession>> + Send;
}
