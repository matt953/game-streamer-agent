//! Pairing handshake messages (spec 06). A SPAKE2 exchange over a pairing code
//! derives a shared key; each side then sends its identity pin authenticated by
//! an HMAC over that key, so a MitM (who lacks the code) can't substitute pins.
//! Sent over a QUIC bi stream via the usual postcard framing.

use serde::{Deserialize, Serialize};

use crate::grant::Scope;

/// Client → agent: the client's SPAKE2 message (round 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairHello {
    pub spake: Vec<u8>,
}

/// Agent → client: the agent's SPAKE2 message (round 1). After this both sides
/// hold the shared key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub spake: Vec<u8>,
}

/// Client → agent: the client's pin + requested scope, authenticated by `mac`
/// (HMAC over the shared key). The agent verifies before recording the peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairConfirm {
    pub pin: String,
    pub name: String,
    pub requested_scope: Scope,
    pub mac: Vec<u8>,
}

/// Agent → client: the outcome. On accept, the agent's pin + granted scope,
/// authenticated by `mac`; the client verifies before recording the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PairResult {
    Accepted {
        pin: String,
        scope: Scope,
        mac: Vec<u8>,
    },
    Rejected {
        reason: String,
    },
}
