//! Paired-peer store (spec 06): the pins + scopes established by pairing.
//! Persisted as JSON in the data dir; read by the pinned TLS verifier on every
//! handshake and by the admin API (`peers list|revoke`). Keyed by pin (the
//! peer's cert SHA-256 fingerprint) for O(1) verification lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use gsa_core::{Error, Result};
use gsa_protocol::grant::Scope;
use serde::{Deserialize, Serialize};

/// One paired peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Human label chosen at pairing (the client's name).
    pub name: String,
    /// Pin = the peer identity cert's SHA-256 fingerprint (hex).
    pub pin: String,
    /// What this peer is allowed to do.
    pub scope: Scope,
    /// Unix seconds when the pairing was recorded.
    pub paired_at: u64,
}

#[derive(Debug)]
pub struct PeerStore {
    path: PathBuf,
    peers: Mutex<HashMap<String, Peer>>,
}

impl PeerStore {
    /// Load the store from `dir/peers.json`, or start empty if absent.
    pub fn load_or_empty(dir: &Path) -> Result<Self> {
        let path = dir.join("peers.json");
        let peers = match std::fs::read(&path) {
            Ok(bytes) => {
                let list: Vec<Peer> = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Transport(format!("parse peer store: {e}")))?;
                list.into_iter().map(|p| (p.pin.clone(), p)).collect()
            }
            Err(_) => HashMap::new(),
        };
        Ok(Self {
            path,
            peers: Mutex::new(peers),
        })
    }

    /// Record a pairing (or update an existing peer's name/scope), then persist.
    pub fn add(&self, name: String, pin: String, scope: Scope) -> Result<()> {
        let peer = Peer {
            name,
            pin: pin.clone(),
            scope,
            paired_at: now_unix(),
        };
        {
            let mut peers = self.peers.lock().expect("peer store");
            peers.insert(pin, peer);
            Self::persist(&self.path, &peers)?;
        }
        Ok(())
    }

    /// Revoke a peer by pin; returns whether one was removed. Persists.
    pub fn remove(&self, pin: &str) -> Result<bool> {
        let mut peers = self.peers.lock().expect("peer store");
        let removed = peers.remove(pin).is_some();
        if removed {
            Self::persist(&self.path, &peers)?;
        }
        Ok(removed)
    }

    /// The peer for a pin, if paired (verifier + scope checks).
    #[must_use]
    pub fn get(&self, pin: &str) -> Option<Peer> {
        self.peers.lock().expect("peer store").get(pin).cloned()
    }

    /// All paired peers, name-sorted.
    #[must_use]
    pub fn list(&self) -> Vec<Peer> {
        let mut out: Vec<Peer> = self
            .peers
            .lock()
            .expect("peer store")
            .values()
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Atomic write (temp + rename) so a crash mid-write can't corrupt the store.
    fn persist(path: &Path, peers: &HashMap<String, Peer>) -> Result<()> {
        let list: Vec<&Peer> = peers.values().collect();
        let json = serde_json::to_vec_pretty(&list)
            .map_err(|e| Error::Transport(format!("serialize peers: {e}")))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Transport(format!("create {}: {e}", dir.display())))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| Error::Transport(format!("write peers: {e}")))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::Transport(format!("rename peers: {e}")))?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gsa-peers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn add_get_remove_and_persist() {
        let dir = tmp_dir("crud");
        let store = PeerStore::load_or_empty(&dir).unwrap();
        store
            .add("laptop".into(), "abc123".into(), Scope::Interact)
            .unwrap();
        assert_eq!(store.get("abc123").unwrap().scope, Scope::Interact);
        assert!(store.get("nope").is_none());

        // Reload from disk sees the peer.
        let reloaded = PeerStore::load_or_empty(&dir).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.get("abc123").unwrap().name, "laptop");

        assert!(reloaded.remove("abc123").unwrap());
        assert!(!reloaded.remove("abc123").unwrap());
        assert!(PeerStore::load_or_empty(&dir).unwrap().list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
