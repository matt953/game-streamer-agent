//! Media enums shared by capture, encode, protocol, and clients.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    /// The default preference order every client advertises, best first.
    ///
    /// One list, owned here, so the dev harness and the apps negotiate the
    /// same codec against the same host. HEVC leads: it saves bitrate over
    /// H.264 and is the most widely accelerated; AV1 saves more but is
    /// decoded by fewer devices — and, where it is, often with more latency.
    #[must_use]
    pub fn preference_order() -> [Self; 3] {
        [Self::Hevc, Self::Av1, Self::H264]
    }
}

/// H.264 profile, ordered by capability so the session negotiates with a `min`
/// (encoder ceiling vs client decode cap, spec 03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum H264Profile {
    ConstrainedBaseline,
    Main,
    High,
}

/// Pixel formats crossing the capture → encode boundary.
///
/// `Bgra444`/`P010` variants are reserved per spec 03 (4:4:4 option, HDR)
/// even though M0 only produces `Bgra8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 8-bit BGRA, tightly packed unless a stride is given.
    Bgra8,
    /// 4:2:0 biplanar (hardware encoder native input).
    Nv12,
    /// 10-bit 4:2:0 biplanar (HDR path, M4+).
    P010,
}

/// What an encoded frame is, on the wire and in the encoder contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FrameKind {
    Idr,
    P,
    IntraRefresh,
    /// FEC parity chunk (spec 04); reserved, unused at M0.
    FecParity,
}

impl FrameKind {
    #[must_use]
    pub fn to_wire(self) -> u8 {
        match self {
            FrameKind::Idr => 0,
            FrameKind::P => 1,
            FrameKind::IntraRefresh => 2,
            FrameKind::FecParity => 3,
        }
    }

    pub fn from_wire(b: u8) -> Result<Self, crate::error::ProtocolError> {
        Ok(match b {
            0 => FrameKind::Idr,
            1 => FrameKind::P,
            2 => FrameKind::IntraRefresh,
            3 => FrameKind::FecParity,
            other => return Err(crate::error::ProtocolError::UnknownFrameKind(other)),
        })
    }
}

/// A video mode (resolution + refresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_kind_round_trip() {
        for k in [
            FrameKind::Idr,
            FrameKind::P,
            FrameKind::IntraRefresh,
            FrameKind::FecParity,
        ] {
            assert_eq!(FrameKind::from_wire(k.to_wire()).unwrap(), k);
        }
        assert!(FrameKind::from_wire(200).is_err());
    }
}

/// Layout of a multistream surround feed, exactly as the host's SDP states
/// it: channel count, elementary streams, how many are coupled pairs, and
/// the output mapping. Parsed from the host rather than assumed, because the
/// host's encoder is the only authority on how it packed the channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurroundLayout {
    pub channels: u8,
    pub streams: u8,
    pub coupled: u8,
    pub mapping: Vec<u8>,
}
