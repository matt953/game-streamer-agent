//! Workspace error taxonomy (spec 01).
//!
//! Library crates return these typed errors; `anyhow` is permitted only in
//! binaries (`gsa-agent`, `gsa-client-dev`, `xtask`).

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("capture: {0}")]
    Capture(String),

    #[error("encode: {0}")]
    Encode(String),

    #[error("decode: {0}")]
    Decode(String),

    #[error("transport: {0}")]
    Transport(String),

    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("session: {0}")]
    Session(String),

    #[error("auth: {0}")]
    Auth(String),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors produced while encoding/decoding wire data. Split out because
/// these parse attacker-controlled bytes and are fuzzed (spec 06/13).
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    #[error("message too short: {got} bytes, need {need}")]
    TooShort { got: usize, need: usize },

    #[error("unknown datagram type {0}")]
    UnknownDatagramType(u8),

    #[error("unknown frame kind {0}")]
    UnknownFrameKind(u8),

    #[error("invalid chunk header: index {index} >= count {count}")]
    InvalidChunk { index: u16, count: u16 },

    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),

    #[error("serialization failed: {0}")]
    Serialize(String),

    #[error("deserialization failed: {0}")]
    Deserialize(String),
}
