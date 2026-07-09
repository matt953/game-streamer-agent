//! Wire protocol (spec 05): control messages (postcard-encoded serde types)
//! and hand-rolled datagram framing for the hot path (spec 04).
//!
//! This crate is shared verbatim by the agent, clients, and the future
//! media server. It contains **no I/O** — just types and encode/decode.

pub mod control;
pub mod datagram;
pub mod grant;
pub mod input;

pub use control::{A2C, C2A};
pub use datagram::{DatagramType, VideoDatagramHeader};

use gsa_core::error::ProtocolError;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Protocol version. Bumped on any incompatible wire change; the
/// `Hello`/`HelloAck` exchange rejects mismatches with a typed error.
pub const PROTO_VERSION: u16 = 0;

/// Maximum accepted size for one control message (defense-in-depth cap for
/// attacker-controlled length prefixes).
pub const MAX_CONTROL_MSG: usize = 1 << 20;

/// Encode a control message with postcard.
pub fn encode_msg<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(msg).map_err(|e| ProtocolError::Serialize(e.to_string()))
}

/// Decode a control message with postcard.
pub fn decode_msg<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtocolError> {
    postcard::from_bytes(bytes).map_err(|e| ProtocolError::Deserialize(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use control::*;

    #[test]
    fn hello_round_trip() {
        let hello = C2A::Hello(Hello {
            proto: PROTO_VERSION,
            client_name: "test".into(),
            decode_caps: DecodeCaps {
                codecs: vec![gsa_core::media::Codec::H264],
                max_h264_profile: gsa_core::media::H264Profile::High,
            },
        });
        let bytes = encode_msg(&hello).unwrap();
        let back: C2A = decode_msg(&bytes).unwrap();
        assert_eq!(format!("{hello:?}"), format!("{back:?}"));
    }

    #[test]
    fn decode_garbage_is_error_not_panic() {
        let garbage = [0xffu8; 64];
        assert!(decode_msg::<C2A>(&garbage).is_err());
    }
}
