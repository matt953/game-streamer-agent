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
    StatsReport(ClientStats),
    Ping {
        client_ts_us: u64,
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
    Pong { client_ts_us: u64, agent_ts_us: u64 },
    /// A user-facing async notification (host → client), for the client to
    /// surface however it likes (a toast, etc.). Distinct from [`SessionEvent`],
    /// which the client's *pipeline* reacts to; this is purely informational and
    /// is the reusable channel for such messages.
    Notification(Notification),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParams {
    pub session: SessionId,
    pub codec: Codec,
    pub mode: VideoMode,
    pub bitrate_bps: u32,
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
    SourceEnded { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoErrorMsg {
    pub message: String,
}

/// Per-interval client feedback that drives ABR (spec 04).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStats {
    pub frames_received: u64,
    pub frames_complete: u64,
    pub frames_dropped_incomplete: u64,
    pub frames_decoded: u64,
    pub decode_us_p50: u32,
    pub jitter_us: u32,
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
            position(&A2C::Notification(Notification::GamepadConnected { seat: 0 })),
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
