//! Loading the Moonlight dev identity (spec 16).
//!
//! Pairing is done once by `gsa-backend-moonlight`'s `pair` example, which
//! stores the client key and the host certificate. This reads them back so
//! the harness can stream without re-pairing.

use anyhow::{Context, Result};
use gsa_backend_moonlight::{ClientIdentity, PairedSession};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// Rebuild a paired session for `addr` from the stored dev credentials.
pub(crate) async fn paired_session(addr: std::net::SocketAddr) -> Result<PairedSession> {
    let key = std::fs::read_to_string(store("gsa-moonlight-dev-key.pem"))
        .context("no stored Moonlight identity — pair first with the backend's `pair` example")?;
    let host_cert = std::fs::read_to_string(store("gsa-moonlight-host-cert.pem"))
        .context("no stored host certificate — pair first")?;
    let identity = ClientIdentity::from_key_pem(&key).context("load Moonlight identity")?;

    // The TLS port comes from the host rather than the default: it is
    // configurable, and guessing it fails in a way that looks like a network
    // fault rather than a misconfiguration.
    let client_id = "0123456789ABCDEF";
    let info = gsa_backend_moonlight::probe(addr, client_id)
        .await
        .context("probe Moonlight host")?;
    Ok(PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.to_owned(),
    ))
}
