//! Control-stream messages (spec 05). All types are serde so the same
//! definitions serve postcard (QUIC/local socket) and JSON (CLI/HTTP
//! gateway later) — one schema, several encodings.

use gsa_core::id::{FrameId, SessionId, SourceId};
use gsa_core::media::{Codec, VideoMode};
use serde::{Deserialize, Serialize};

use crate::input::InputEvent;

/// Client → Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum C2A {
    Hello(Hello),
    ListSources,
    StartSession(SessionRequest),
    StopSession,
    InputBatch(Vec<InputEvent>),
    FrameAck {
        latest_decoded: FrameId,
        lost: Vec<FrameId>,
    },
    /// Ask the agent to emit a keyframe (IDR) — the client's decoder lost
    /// its reference chain (missed/corrupt frame) and needs to resync
    /// (spec 04 loss-recovery ladder).
    RequestKeyframe,
    /// Per-packet arrival feedback batch (see [`PacketFeedback`]).
    PacketFeedback(PacketFeedback),
    /// Set the encode target bitrate (bps). When ABR is off this is the live
    /// target; when ABR is on it's the ceiling ABR adapts below (spec 04). The
    /// agent clamps to a sane range.
    SetBitrate {
        bitrate_bps: u32,
    },
    /// Enable/disable server-side ABR for this session (spec 04).
    SetAbr {
        enabled: bool,
    },
    StatsReport(ClientStats),
    Ping {
        client_ts_us: u64,
    },
    /// Loss recovery (spec 04 rung 2): the client's reference chain broke;
    /// `last_good_frame_id` is the newest frame it decoded. The agent
    /// invalidates newer references (or falls back to an IDR) and answers
    /// with [`SessionEvent::RecoveryPoint`].
    RequestRecovery {
        last_good_frame_id: u32,
    },
    /// Retransmit request for lost datagrams (transport seqs). The agent
    /// resends what its retention ring still holds and ignores the rest;
    /// below one RTT of loss this beats any parity ratio.
    Nack {
        seqs: Vec<u32>,
    },
}

/// Agent → Client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum A2C {
    HelloAck(HelloAck),
    Sources(Vec<SourceInfo>),
    SessionStarted(SessionParams),
    SessionEvent(SessionEvent),
    Error(ProtoErrorMsg),
    Pong {
        client_ts_us: u64,
        agent_ts_us: u64,
    },
    /// A user-facing async notification (host → client), for the client to
    /// surface however it likes (a toast, etc.). Distinct from [`SessionEvent`],
    /// which the client's *pipeline* reacts to; this is purely informational and
    /// is the reusable channel for such messages.
    Notification(Notification),
    /// Periodic encoder telemetry from the agent (~1 Hz).
    EncodeStats(EncodeStats),
}

/// Agent-measured encoder telemetry (spec 04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeStats {
    /// Live target bitrate (bits/s) the encoder is aiming for.
    pub target_bitrate_bps: u32,
    /// Rolling emitted video bitrate (bits/s) over ~1 s — the encoder's output.
    pub emitted_bitrate_bps: u32,
    /// The manual bitrate cap (bits/s) ABR adapts under; equals the target
    /// when ABR is off.
    pub ceiling_bitrate_bps: u32,
    /// ABR's dynamic network cap (bits/s): the headroom-scaled transport
    /// delivered-rate estimate. 0 when unmeasured or ABR is off.
    pub estimate_bitrate_bps: u32,
    /// Whether ABR is driving the bitrate, as the agent has it.
    pub abr_enabled: bool,
}

/// User-facing notifications pushed by the agent over the control stream. Add
/// variants here (and mirror them in the client's control-event surface) to
/// grow the set — the transport/dispatch plumbing is shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Notification {
    /// The host plugged its virtual pad for `seat`: input is now live on the
    /// host, confirmed rather than merely attempted.
    GamepadConnected { seat: u8 },
    /// The host unplugged `seat`'s virtual pad.
    GamepadDisconnected { seat: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub client_name: String,
    pub decode_caps: DecodeCaps,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub proto: u16,
    pub agent_name: String,
    pub encode_codecs: Vec<Codec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeCaps {
    pub codecs: Vec<Codec>,
    /// Highest H.264 profile the client can decode; the host encodes at or below it.
    pub max_h264_profile: gsa_core::media::H264Profile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    pub source: SourceId,
    pub codec_prefs: Vec<Codec>,
    pub mode: Option<VideoMode>,
    /// Starting bitrate (bits/s); `None` uses the agent's default. With `abr`
    /// it's the ramp start (ceiling stays the agent's max); without it, the rate.
    pub bitrate_bps: Option<u32>,
    /// Whether ABR drives the bitrate from the first frame.
    pub abr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParams {
    pub session: SessionId,
    pub codec: Codec,
    pub mode: VideoMode,
    pub bitrate_bps: u32,
    /// Dev log collector (`host:port`) the agent pushes its logs to; debug
    /// clients push theirs to the same place. `None` outside dev.
    pub log_sink: Option<String>,
}

/// Wire description of a launchable/streamable source (spec 09's
/// `SourceDescriptor`, wire form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SourceKind {
    Display,
    VirtualDisplay,
    Emulator,
    TestPattern,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SessionEvent {
    EncoderReset,
    ModeChanged(VideoMode),
    SourceEnded {
        reason: String,
    },
    /// Frames from `first_safe_frame_id` on reference nothing the client is
    /// missing: decoding may resume there without a keyframe.
    RecoveryPoint {
        first_safe_frame_id: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoErrorMsg {
    pub message: String,
}

/// Per-packet arrival feedback (the TWCC equivalent): the receiver reports
/// every datagram's transport sequence and arrival time, batched on the
/// control stream (~20 Hz). Drives the delay-gradient bandwidth estimator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PacketFeedback {
    /// Receiver clock (µs) the batch's deltas are relative to.
    pub base_arrival_us: u64,
    /// `(seq, arrival − base_arrival_us)` per received datagram, send-order
    /// gaps meaning loss. Capped per batch; overflow rolls to the next batch.
    pub samples: Vec<(u32, u32)>,
}

/// Periodic client-side stream health for HUDs and logs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStats {
    pub frames_received: u64,
    pub frames_complete: u64,
    pub frames_dropped_incomplete: u64,
    /// Frames completed only thanks to FEC parity reconstruction.
    pub frames_recovered: u32,
    pub frames_decoded: u64,
    pub decode_us_p50: u32,
    pub jitter_us: u32,
    /// Frames handed to the display, as reported by the embedder.
    pub frames_presented: u64,
    /// Presented fps ×100, average / worst-1%-of-gaps over a rolling window.
    pub present_fps_x100: u32,
    pub low1_fps_x100: u32,
    /// Capture→present latency percentiles (µs); 0 when unmeasured.
    pub latency_p50_us: u32,
    pub latency_p95_us: u32,
    pub latency_p99_us: u32,
    /// Cadence breaks: gaps well past the rolling median / hard freezes.
    pub stutters: u32,
    /// Cadence breaks already present in the source (game hitches).
    pub src_stutters: u32,
    pub freezes: u32,
    pub freeze_ms_total: u32,
    /// Clustered cadence breaks (visible degradation events) and the
    /// longest one's duration.
    pub episodes: u32,
    pub worst_episode_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::{A2C, Notification};

    /// postcard writes the enum variant's position as the leading byte. Pin the
    /// appended variants so a mid-enum insertion fails here, not on the wire
    /// (the append-only rule — see 05-protocol.md).
    #[test]
    fn a2c_wire_positions_are_stable() {
        let position = |m: &A2C| crate::encode_msg(m).unwrap()[0];
        assert_eq!(
            position(&A2C::Pong {
                client_ts_us: 0,
                agent_ts_us: 0,
            }),
            5
        );
        assert_eq!(
            position(&A2C::Notification(Notification::GamepadConnected {
                seat: 0
            })),
            6
        );
    }

    #[test]
    fn notification_round_trips() {
        let msg = A2C::Notification(Notification::GamepadDisconnected { seat: 2 });
        let bytes = crate::encode_msg(&msg).unwrap();
        let back: A2C = crate::decode_msg(&bytes).unwrap();
        assert!(matches!(
            back,
            A2C::Notification(Notification::GamepadDisconnected { seat: 2 })
        ));
    }
}
