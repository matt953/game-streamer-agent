//! Agent-side pairing (spec 06): an armed pairing window plus the connection
//! handler that runs the SPAKE2 exchange and records the new peer.
//!
//! `gsa pair` arms a window (a code + the scope the owner authorizes) over the
//! admin socket; an anonymous QUIC connection then runs [`serve_pairing`],
//! which authenticates the client via SPAKE2, stores its pin, and reports the
//! outcome back for the CLI to poll.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gsa_core::{Error, Result};
use gsa_protocol::grant::Scope;
use gsa_protocol::pairing::{PairConfirm, PairHello, PairResult};
use gsa_transport::{AgentPairing, PeerStore, generate_code, recv_msg, send_msg};

use crate::state::AgentState;

/// How long an armed pairing window stays open for a client to complete it.
const WINDOW: Duration = Duration::from_secs(120);

/// Wrong-code guesses tolerated on one window before it's burned. Each SPAKE2
/// attempt is a single online guess against a 40-bit code, so a handful is
/// ample headroom for typos while keeping brute force hopeless (spec 06).
const MAX_ATTEMPTS: u32 = 5;

/// The result of a completed pairing, held until the CLI polls for it.
#[derive(Debug, Clone)]
pub struct PairOutcome {
    pub name: String,
    pub pin: String,
    pub scope: Scope,
}

/// What a `gsa pair` poll observes.
#[derive(Debug, Clone)]
pub enum PairingPoll {
    /// No window armed.
    Idle,
    /// Armed and waiting for a client.
    Waiting,
    /// A client paired; the window is now consumed.
    Completed(PairOutcome),
    /// The window elapsed with no client; now cleared.
    Expired,
}

struct Pending {
    code: String,
    scope: Scope,
    expires: Instant,
    outcome: Option<PairOutcome>,
    attempts: u32,
}

/// The agent's single active pairing window (at most one at a time).
#[derive(Default)]
pub struct PairingState {
    inner: Mutex<Option<Pending>>,
}

impl std::fmt::Debug for PairingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingState").finish_non_exhaustive()
    }
}

impl PairingState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm a fresh window authorizing up to `scope`; returns the pairing code.
    /// Replaces any existing window.
    pub fn begin(&self, scope: Scope) -> String {
        let code = generate_code();
        *self.inner.lock().expect("pairing state") = Some(Pending {
            code: code.clone(),
            scope,
            expires: Instant::now() + WINDOW,
            outcome: None,
            attempts: 0,
        });
        code
    }

    /// Record a wrong-code attempt; burns the window once [`MAX_ATTEMPTS`] is
    /// reached. Returns true if the window was burned.
    fn record_failure(&self) -> bool {
        let mut guard = self.inner.lock().expect("pairing state");
        if let Some(p) = guard.as_mut() {
            p.attempts += 1;
            if p.attempts >= MAX_ATTEMPTS {
                *guard = None;
                return true;
            }
        }
        false
    }

    /// The armed `(code, authorized scope)` if a window is open and unclaimed.
    fn armed(&self) -> Option<(String, Scope)> {
        let guard = self.inner.lock().expect("pairing state");
        let p = guard.as_ref()?;
        if p.outcome.is_some() || Instant::now() > p.expires {
            return None;
        }
        Some((p.code.clone(), p.scope))
    }

    /// Record a completed pairing on the open window.
    fn complete(&self, outcome: PairOutcome) {
        if let Some(p) = self.inner.lock().expect("pairing state").as_mut() {
            p.outcome = Some(outcome);
        }
    }

    /// Observe the window's state, consuming it once it resolves.
    pub fn poll(&self) -> PairingPoll {
        let mut guard = self.inner.lock().expect("pairing state");
        match guard.as_ref() {
            None => PairingPoll::Idle,
            Some(p) if p.outcome.is_some() => {
                let outcome = p.outcome.clone().expect("checked");
                *guard = None;
                PairingPoll::Completed(outcome)
            }
            Some(p) if Instant::now() > p.expires => {
                *guard = None;
                PairingPoll::Expired
            }
            Some(_) => PairingPoll::Waiting,
        }
    }
}

/// Drive one anonymous connection through the pairing handshake.
pub async fn serve_pairing(
    conn: quinn::Connection,
    state: Arc<AgentState>,
    peers: Arc<PeerStore>,
    pairing: Arc<PairingState>,
) {
    let peer = conn.remote_address().to_string();
    match pair_inner(&conn, &state, &peers, &pairing).await {
        Ok(Some(name)) => tracing::info!(peer, name, "paired new peer"),
        Ok(None) => tracing::info!(peer, "pairing attempt rejected"),
        Err(e) => tracing::info!(peer, error = %e, "pairing ended"),
    }
    // Hold the connection open until the client finishes reading the result
    // and closes it — dropping immediately would discard the buffered reply.
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
}

/// Returns `Ok(Some(name))` on a successful pairing, `Ok(None)` on a clean
/// rejection (no window / bad code).
async fn pair_inner(
    conn: &quinn::Connection,
    state: &Arc<AgentState>,
    peers: &Arc<PeerStore>,
    pairing: &Arc<PairingState>,
) -> Result<Option<String>> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| Error::Transport(format!("accept pairing stream: {e}")))?;

    let hello: PairHello = recv_msg(&mut recv).await?;
    let Some((code, authorized)) = pairing.armed() else {
        send_msg(
            &mut send,
            &PairResult::Rejected {
                reason: "no pairing in progress".into(),
            },
        )
        .await?;
        return Ok(None);
    };

    let (agent, response) = AgentPairing::respond(&code, &hello)?;
    send_msg(&mut send, &response).await?;

    let confirm: PairConfirm = recv_msg(&mut recv).await?;
    if !agent.verify(&confirm) {
        if pairing.record_failure() {
            tracing::warn!("pairing window closed after too many failed attempts");
        }
        send_msg(
            &mut send,
            &PairResult::Rejected {
                reason: "authentication failed (wrong code?)".into(),
            },
        )
        .await?;
        return Ok(None);
    }

    // Never grant more than the owner authorized when arming the window.
    let granted = confirm.requested_scope.min(authorized);
    peers.add(confirm.name.clone(), confirm.pin.clone(), granted)?;
    send_msg(&mut send, &agent.accept(state.fingerprint.clone(), granted)).await?;
    pairing.complete(PairOutcome {
        name: confirm.name.clone(),
        pin: confirm.pin,
        scope: granted,
    });
    Ok(Some(confirm.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_burns_after_max_failed_attempts() {
        let p = PairingState::new();
        p.begin(Scope::Interact);
        for _ in 0..MAX_ATTEMPTS - 1 {
            assert!(!p.record_failure(), "under the cap keeps the window open");
            assert!(p.armed().is_some());
        }
        assert!(p.record_failure(), "the last attempt burns the window");
        assert!(p.armed().is_none());
        assert!(matches!(p.poll(), PairingPoll::Idle));
    }
}
