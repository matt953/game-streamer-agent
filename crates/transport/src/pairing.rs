//! SPAKE2 pairing state machine (spec 06). A pairing code is the PAKE password;
//! both sides derive a shared key from it, then authenticate their identity
//! pins with an HMAC over that key. Wrong code → different keys → MAC fails, so
//! a MitM or eavesdropper who lacks the code learns nothing and can't pair.

use gsa_core::{Error, Result};
use gsa_protocol::grant::Scope;
use gsa_protocol::pairing::{PairConfirm, PairHello, PairResponse, PairResult};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

type HmacSha256 = Hmac<Sha256>;

// SPAKE2 identities — same values, same order, on both sides.
const ID_CLIENT: &[u8] = b"gsa-pair-client";
const ID_AGENT: &[u8] = b"gsa-pair-agent";
// Domain-separation labels so the client and agent confirmations can't be
// replayed against each other.
const MAC_CLIENT: &[u8] = b"gsa-pair-mac-client";
const MAC_AGENT: &[u8] = b"gsa-pair-mac-agent";

/// A high-entropy pairing code (40 bits, Crockford base32, grouped for reading,
/// e.g. `4KQ7-9WXB`). PAKE makes low-entropy codes safe against offline attack;
/// online guessing is rate-limited by the agent (spec 06).
#[must_use]
pub fn generate_code() -> String {
    let mut bytes = [0u8; 5];
    getrandom::getrandom(&mut bytes).expect("os rng");
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford: no I L O U
    let mut n = u64::from(bytes[0]) << 32
        | u64::from(bytes[1]) << 24
        | u64::from(bytes[2]) << 16
        | u64::from(bytes[3]) << 8
        | u64::from(bytes[4]);
    let mut chars = [0u8; 8];
    for c in chars.iter_mut().rev() {
        *c = ALPHABET[(n & 0x1f) as usize];
        n >>= 5;
    }
    let s = std::str::from_utf8(&chars).expect("ascii");
    format!("{}-{}", &s[..4], &s[4..])
}

/// HMAC-SHA256 over `label || json(pin, name, scope)` keyed by the shared key.
/// JSON of a tuple is a deterministic array, so both sides produce identical
/// bytes.
fn tag(key: &[u8], label: &[u8], pin: &str, name: &str, scope: Scope) -> HmacSha256 {
    let body = serde_json::to_vec(&(pin, name, scope)).expect("encode mac body");
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    m.update(label);
    m.update(&body);
    m
}

fn mac(key: &[u8], label: &[u8], pin: &str, name: &str, scope: Scope) -> Vec<u8> {
    tag(key, label, pin, name, scope)
        .finalize()
        .into_bytes()
        .to_vec()
}

/// Constant-time verify.
fn mac_ok(key: &[u8], label: &[u8], pin: &str, name: &str, scope: Scope, expect: &[u8]) -> bool {
    tag(key, label, pin, name, scope)
        .verify_slice(expect)
        .is_ok()
}

// ── Client side ────────────────────────────────────────────────────────────

pub struct ClientPairing {
    spake: Spake2<Ed25519Group>,
    my_pin: String,
    name: String,
    scope: Scope,
}

// Manual Debug on the pairing state types: never print the SPAKE2 state or the
// derived key.
impl std::fmt::Debug for ClientPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientPairing").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ClientConfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfirmed").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for AgentPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPairing").finish_non_exhaustive()
    }
}

impl ClientPairing {
    /// Round 1: produce the client's SPAKE2 message.
    #[must_use]
    pub fn start(code: &str, my_pin: String, name: String, scope: Scope) -> (Self, PairHello) {
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_CLIENT),
            &Identity::new(ID_AGENT),
        );
        (
            Self {
                spake,
                my_pin,
                name,
                scope,
            },
            PairHello { spake: msg },
        )
    }

    /// Round 2: derive the key from the agent's message; produce the confirm.
    pub fn confirm(self, resp: &PairResponse) -> Result<(ClientConfirmed, PairConfirm)> {
        let key = self
            .spake
            .finish(&resp.spake)
            .map_err(|e| Error::Session(format!("pairing key exchange failed: {e:?}")))?;
        let m = mac(&key, MAC_CLIENT, &self.my_pin, &self.name, self.scope);
        let confirm = PairConfirm {
            pin: self.my_pin,
            name: self.name,
            requested_scope: self.scope,
            mac: m,
        };
        Ok((ClientConfirmed { key }, confirm))
    }
}

pub struct ClientConfirmed {
    key: Vec<u8>,
}

impl ClientConfirmed {
    /// Verify the agent's result; on accept return the agent's pin + granted
    /// scope to record.
    pub fn finish(self, result: PairResult) -> Result<(String, Scope)> {
        match result {
            PairResult::Accepted { pin, scope, mac } => {
                if mac_ok(&self.key, MAC_AGENT, &pin, "", scope, &mac) {
                    Ok((pin, scope))
                } else {
                    Err(Error::Session(
                        "agent authentication failed (wrong code?)".into(),
                    ))
                }
            }
            PairResult::Rejected { reason } => {
                Err(Error::Session(format!("pairing rejected: {reason}")))
            }
        }
    }
}

// ── Agent side ─────────────────────────────────────────────────────────────

pub struct AgentPairing {
    key: Vec<u8>,
}

impl AgentPairing {
    /// Round 1: derive the key from the client's hello; produce the response.
    pub fn respond(code: &str, hello: &PairHello) -> Result<(Self, PairResponse)> {
        let (spake, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_CLIENT),
            &Identity::new(ID_AGENT),
        );
        let key = spake
            .finish(&hello.spake)
            .map_err(|e| Error::Session(format!("pairing key exchange failed: {e:?}")))?;
        Ok((Self { key }, PairResponse { spake: msg }))
    }

    /// Round 2: verify the client's confirm (proves it knew the code + binds its
    /// pin). The caller then chooses the granted scope and calls [`Self::accept`].
    #[must_use]
    pub fn verify(&self, confirm: &PairConfirm) -> bool {
        mac_ok(
            &self.key,
            MAC_CLIENT,
            &confirm.pin,
            &confirm.name,
            confirm.requested_scope,
            &confirm.mac,
        )
    }

    /// Build the accept message binding the agent's pin + granted scope.
    #[must_use]
    pub fn accept(&self, my_pin: String, granted: Scope) -> PairResult {
        let m = mac(&self.key, MAC_AGENT, &my_pin, "", granted);
        PairResult::Accepted {
            pin: my_pin,
            scope: granted,
            mac: m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive both halves in memory and return what each side records.
    fn run(client_code: &str, agent_code: &str) -> Result<((String, Scope), (String, Scope))> {
        // Client round 1.
        let (client, hello) = ClientPairing::start(
            client_code,
            "client-pin".into(),
            "laptop".into(),
            Scope::Interact,
        );
        // Agent round 1.
        let (agent, response) = AgentPairing::respond(agent_code, &hello)?;
        // Client round 2.
        let (client, confirm) = client.confirm(&response)?;
        // Agent round 2: verify + accept (grant exactly what was asked here).
        if !agent.verify(&confirm) {
            return Err(Error::Session("client mac rejected".into()));
        }
        let agent_recorded = (confirm.name.clone(), confirm.requested_scope);
        let result = agent.accept("agent-pin".into(), confirm.requested_scope);
        // Client finishes.
        let (agent_pin, scope) = client.finish(result)?;
        Ok(((agent_pin, scope), agent_recorded))
    }

    #[test]
    fn matching_code_pairs_mutually() {
        let ((agent_pin, scope), (client_name, client_scope)) =
            run("ABCD-1234", "ABCD-1234").unwrap();
        assert_eq!(agent_pin, "agent-pin");
        assert_eq!(scope, Scope::Interact);
        assert_eq!(client_name, "laptop");
        assert_eq!(client_scope, Scope::Interact);
    }

    #[test]
    fn wrong_code_fails_authentication() {
        assert!(run("ABCD-1234", "ZZZZ-9999").is_err());
    }

    #[test]
    fn generated_codes_are_distinct_and_formatted() {
        let a = generate_code();
        assert_eq!(a.len(), 9); // XXXX-XXXX
        assert_eq!(a.as_bytes()[4], b'-');
        assert_ne!(a, generate_code());
    }
}
