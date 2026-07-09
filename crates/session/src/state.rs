//! Shared agent state: what `gsa status` reports and sessions update.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use gsa_core::config::AgentConfig;
use gsa_core::media::VideoMode;
use gsa_core::time::MediaClock;
use quinn::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct AgentState {
    pub config: AgentConfig,
    pub clock: MediaClock,
    pub started: Instant,
    pub fingerprint: String,
    /// Live streaming connections keyed by peer pin, so revocation can drop
    /// them mid-session.
    pub conns: LiveConns,
    next_session: AtomicU64,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
}

/// Registry of live QUIC connections by peer pin. Registered when a paired
/// peer starts streaming, dropped when the connection ends, and force-closed
/// on revocation.
#[derive(Debug, Default)]
pub struct LiveConns {
    by_pin: Mutex<HashMap<String, Vec<Connection>>>,
}

impl LiveConns {
    pub fn register(&self, pin: String, conn: Connection) {
        self.by_pin
            .lock()
            .expect("live conns")
            .entry(pin)
            .or_default()
            .push(conn);
    }

    pub fn unregister(&self, pin: &str, stable_id: usize) {
        let mut map = self.by_pin.lock().expect("live conns");
        if let Some(v) = map.get_mut(pin) {
            v.retain(|c| c.stable_id() != stable_id);
            if v.is_empty() {
                map.remove(pin);
            }
        }
    }

    /// Close every live connection for `pin` (revocation). Returns the count.
    pub fn close_peer(&self, pin: &str) -> usize {
        let conns = self
            .by_pin
            .lock()
            .expect("live conns")
            .remove(pin)
            .unwrap_or_default();
        for c in &conns {
            c.close(1u32.into(), b"peer revoked");
        }
        conns.len()
    }
}

#[derive(Debug)]
pub struct SessionEntry {
    pub mode: VideoMode,
    pub peer: String,
    pub frames_sent: std::sync::Arc<AtomicU64>,
}

/// Wire/JSON form of one live session (admin API + `gsa status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: u64,
    pub peer: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames_sent: u64,
}

impl AgentState {
    #[must_use]
    pub fn new(config: AgentConfig, fingerprint: String) -> Self {
        Self {
            config,
            clock: MediaClock::new(),
            started: Instant::now(),
            fingerprint,
            conns: LiveConns::default(),
            next_session: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn allocate_session(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::Relaxed)
    }

    pub fn register_session(&self, id: u64, entry: SessionEntry) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(id, entry);
    }

    pub fn remove_session(&self, id: u64) {
        self.sessions.lock().expect("sessions lock").remove(&id);
    }

    #[must_use]
    pub fn session_summaries(&self) -> Vec<SessionSummary> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .iter()
            .map(|(id, e)| SessionSummary {
                id: *id,
                peer: e.peer.clone(),
                width: e.mode.width,
                height: e.mode.height,
                fps: e.mode.fps,
                frames_sent: e.frames_sent.load(Ordering::Relaxed),
            })
            .collect()
    }
}
