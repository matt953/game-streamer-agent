//! Client-side pairing + persisted trust (spec 06). The dev client keeps a
//! persistent identity and a single paired-agent record under the data dir;
//! `--pair` runs the SPAKE2 exchange and writes that record, and streaming
//! reads it to pin the agent with mutual TLS.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gsa_client_core::{ClientIdentity, PairedAgent, ServerAuth};
use gsa_protocol::grant::Scope;
use serde::{Deserialize, Serialize};

/// Where the client keeps its identity + paired-agent record. Distinct from
/// the agent's dir so both can run on one machine during local testing.
fn client_dir() -> PathBuf {
    gsa_core::config::data_dir().join("client-dev")
}

fn record_path(dir: &Path) -> PathBuf {
    dir.join("paired-agent.json")
}

/// The agent this client has paired with (dev client tracks one).
#[derive(Debug, Serialize, Deserialize)]
struct PairedAgentRecord {
    agent_pin: String,
    scope: Scope,
}

impl PairedAgentRecord {
    fn load(dir: &Path) -> Result<Self> {
        let bytes = std::fs::read(record_path(dir))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(record_path(dir), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// How the client will authenticate the agent, owned so it can move into the
/// windowed client's network thread.
#[derive(Debug)]
pub enum Auth {
    Open,
    Pinned {
        agent_pin: String,
        identity: ClientIdentity,
    },
}

impl Auth {
    pub fn server_auth(&self) -> ServerAuth<'_> {
        match self {
            Auth::Open => ServerAuth::Open,
            Auth::Pinned {
                agent_pin,
                identity,
            } => ServerAuth::Pinned {
                agent_pin,
                identity,
            },
        }
    }
}

/// Resolve the streaming auth: dev-open if `GSA_DEV_OPEN` is set, else the
/// pinned identity + agent recorded by a prior `--pair`.
pub fn load_auth() -> Result<Auth> {
    if std::env::var_os("GSA_DEV_OPEN").is_some() {
        return Ok(Auth::Open);
    }
    let dir = client_dir();
    let record = PairedAgentRecord::load(&dir).context(
        "not paired with any agent — run `gsa-client-dev --connect <addr> --pair --code <CODE>` \
         first (or set GSA_DEV_OPEN for local dev)",
    )?;
    let identity = ClientIdentity::load_or_generate(&dir).context("load client identity")?;
    Ok(Auth::Pinned {
        agent_pin: record.agent_pin,
        identity,
    })
}

/// Run the pairing handshake and persist the result.
pub async fn run_pair(addr: std::net::SocketAddr, code: &str, name: &str) -> Result<()> {
    let dir = client_dir();
    let identity = ClientIdentity::load_or_generate(&dir).context("load client identity")?;
    println!(
        "Pairing with {addr} (this client's pin {})...",
        identity.fingerprint()
    );
    // Request the max; the agent caps the grant to what the owner authorized
    // when arming the window (`gsa pair --scope ...`).
    let PairedAgent { agent_pin, scope } =
        gsa_client_core::pair(addr, code, &identity, name, Scope::Manage).await?;
    PairedAgentRecord {
        agent_pin: agent_pin.clone(),
        scope,
    }
    .save(&dir)?;
    println!("✓ paired (granted scope {scope:?})");
    println!("  agent pin {agent_pin}");
    Ok(())
}
