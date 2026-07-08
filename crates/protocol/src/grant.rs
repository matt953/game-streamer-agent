//! Access grants (spec 06/10): the owner-signed authorization artifact that
//! every enrollment flow converges on. Defined at v1 so invite links,
//! owner co-signs, and server-era account shares all use one verified type.
//! M0 defines the type and its signing payload; signature *verification*
//! lands with pairing at M2.

use serde::{Deserialize, Serialize};

use gsa_core::id::PeerId;

/// Authorization scopes (spec 06). Order matters: each level implies the
/// previous ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Scope {
    /// Receive video/audio only.
    View,
    /// + input injection.
    Interact,
    /// + start/stop sources, change modes, launch apps, mint invites.
    Manage,
}

/// `{peer key, agent id, scopes, expiry}` signed by the host owner's
/// identity key. The agent verifies the owner signature; a compromised
/// media server cannot forge grants (spec 06).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessGrant {
    pub peer: PeerId,
    pub agent: PeerId,
    pub scope: Scope,
    /// Unix seconds; 0 = no expiry.
    pub expires_unix: u64,
    /// Ed25519 signature by the owner identity key over
    /// [`AccessGrant::signing_payload`].
    #[serde(with = "sig64")]
    pub owner_signature: [u8; 64],
}

/// serde for `[u8; 64]` (serde's built-in array impls stop at 32).
/// Encodes as a byte string (compact under postcard, array under JSON).
mod sig64 {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = [u8; 64];

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("64 bytes")
            }

            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                v.try_into().map_err(|_| E::invalid_length(v.len(), &self))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = [0u8; 64];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }
        d.deserialize_bytes(V)
    }
}

impl AccessGrant {
    /// Canonical byte string the owner signs. Versioned domain separator
    /// prevents cross-protocol signature reuse.
    #[must_use]
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(96);
        p.extend_from_slice(b"gsa-access-grant-v0");
        p.extend_from_slice(&self.peer.0);
        p.extend_from_slice(&self.agent.0);
        p.push(match self.scope {
            Scope::View => 0,
            Scope::Interact => 1,
            Scope::Manage => 2,
        });
        p.extend_from_slice(&self.expires_unix.to_be_bytes());
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ordering_implies_privilege() {
        assert!(Scope::Manage > Scope::Interact && Scope::Interact > Scope::View);
    }

    #[test]
    fn signing_payload_is_domain_separated_and_stable() {
        let g = AccessGrant {
            peer: PeerId([1; 32]),
            agent: PeerId([2; 32]),
            scope: Scope::Interact,
            expires_unix: 42,
            owner_signature: [0; 64],
        };
        let p = g.signing_payload();
        assert!(p.starts_with(b"gsa-access-grant-v0"));
        assert_eq!(p, g.signing_payload());
    }
}
