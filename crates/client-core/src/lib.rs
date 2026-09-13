//! Embeddable streaming client core (spec 01, decision D9): connection,
//! negotiation, datagram reassembly, decode orchestration, and latency
//! stats. **No UI, no rendering, no platform decode** — the embedding app
//! (or `client-dev`) supplies a [`VideoDecoder`] and owns presentation.
//! This boundary is what makes the M2 UniFFI factoring mechanical.

/// Audio receive for the native protocol; the browser build decodes with WebCodecs.
#[cfg(feature = "quic")]
pub(crate) mod audio;
mod decode;
pub mod hdr;
pub mod pacing;
/// The native protocol's QUIC client: pairing, session control, media
/// receive. Behind `quic` so the transport-neutral engine builds for wasm32.
#[cfg(feature = "quic")]
pub mod quic;
mod reassembly;
mod session;
pub mod stats;

pub use decode::{DecodedFrame, HdrPayloadState, HdrStatus, PixelOrder, VideoDecoder, VideoFormat};
pub use gsa_client_backend_api::{
    ActiveSession, BackendEvent, BackendFrame, CaptureClock, CatalogEntry, CatalogKind,
    DecodedTriggerEffect, GamepadFeedback, GamepadProfile, InputSink, MotionSensor, PadCaps,
    PadKind, RecoverySink, SessionCaps, SessionKnobs, SessionOrigin, StreamBackend, TriggerEffect,
};
pub use gsa_protocol::control::{SourceInfo, SourceKind};
pub use gsa_protocol::input::{GamepadInput, InputEvent, MouseButton, MouseMove};
pub use pacing::PacingMode;
#[cfg(feature = "quic")]
pub use quic::{Client, ClientIdentity, InputSender, PairedAgent, ServerAuth, pair};
pub use reassembly::Reassembler;
pub use session::StreamSession;
pub use stats::{ClockSync, LatencyStats, LatencySummary, StagePercentiles, StatsSummary};

/// One decoded frame plus its measurements, handed to the embedder.
#[derive(Debug)]
pub struct FrameOutput {
    pub frame: DecodedFrame,
    pub frame_id: u32,
    /// Agent-clock capture stamp (µs, wrapping) — echo to `frame_presented`.
    pub capture_ts_us: u32,
    /// Estimated glass-to-glass-so-far: agent capture → decoded on client.
    pub latency_us: Option<u32>,
    pub decode_us: u32,
}

/// One complete encoded H.264 access unit (Annex-B) plus metadata, for
/// embedders that decode on the platform (VideoToolbox / MediaCodec) rather
/// than through a [`VideoDecoder`]. An IDR carries its own SPS/PPS.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub frame_id: u32,
    /// IDR (carries parameter sets); the embedder builds its format description.
    pub keyframe: bool,
    /// Agent-clock capture timestamp (wire, truncated to u32 µs).
    pub capture_ts_us: u32,
    /// Estimated capture→received latency (µs); decode happens app-side.
    pub latency_us: Option<u32>,
}

/// A user-facing event pushed by the agent over the control stream, for the
/// embedder to surface (a toast, etc.). Mirrors the wire [`Notification`] but is
/// the client-core-facing type, so embedders don't depend on the protocol crate.
/// Grow this alongside `Notification` as new notifications are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// The host confirmed its virtual pad for `seat` is plugged in (input live).
    GamepadConnected { seat: u8 },
    /// The host's virtual pad for `seat` was unplugged.
    GamepadDisconnected { seat: u8 },
    /// Periodic encoder telemetry from the agent (target + emitted bitrate +
    /// manual ceiling, bits/s).
    EncodeStats {
        target_bitrate_bps: u32,
        emitted_bitrate_bps: u32,
        ceiling_bitrate_bps: u32,
        estimate_bitrate_bps: u32,
        abr_enabled: bool,
    },
}

#[derive(Debug, Clone)]
pub struct PresentedSink {
    tx: tokio::sync::mpsc::UnboundedSender<(u32, gsa_core::time::Instant)>,
}

impl PresentedSink {
    /// `capture_ts_us` is the frame's agent-clock capture stamp, echoed from
    /// the video callback. Timestamped here so queueing costs nothing.
    pub fn presented(&self, capture_ts_us: u32) {
        let _ = self
            .tx
            .send((capture_ts_us, gsa_core::time::Instant::now()));
    }
}
