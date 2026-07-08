//! Strongly-typed identifiers used across the workspace.

use serde::{Deserialize, Serialize};

/// Monotonic per-session-epoch frame counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FrameId(pub u64);

impl FrameId {
    pub const ZERO: FrameId = FrameId(0);

    #[must_use]
    pub fn next(self) -> FrameId {
        FrameId(self.0 + 1)
    }

    /// Wire representation is truncated to 32 bits (spec 04); frames wrap
    /// after ~2.2 years at 60 fps, and reassembly windows are tiny.
    #[must_use]
    pub fn wire(self) -> u32 {
        self.0 as u32
    }
}

/// Identifies one streaming session on an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// A peer's identity: SHA-256 fingerprint of its identity public key /
/// certificate. Pinned in the peer store (spec 06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// Hex rendering for logs and CLI output.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Identifies a render source (display, virtual display, emulator, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_id_wire_truncates() {
        assert_eq!(FrameId(0x1_0000_0002).wire(), 2);
    }

    #[test]
    fn peer_id_hex() {
        let mut b = [0u8; 32];
        b[0] = 0xab;
        assert!(PeerId(b).to_hex().starts_with("ab00"));
    }
}
