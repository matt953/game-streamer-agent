//! Moonlight-protocol client backend — talks to Sunshine, Apollo, and Wolf
//! hosts (spec 16, phase 1).
//!
//! # Provenance (read before editing)
//!
//! Clean-room implementation. The reference clients for this protocol
//! (moonlight-common-c and the moonlight-* apps) are GPL-3.0 and must not be
//! opened or copied from.
//!
//! Permitted references, consulted rather than copied:
//!
//! - Wolf's protocol documentation (MIT) — <https://games-on-whales.github.io/wolf/stable/protocols/>
//! - `moonshine` (BSD-2-Clause), a Rust implementation of the *server* side
//! - `fec-rs` (BSD-2-Clause) for the wire-compatible Reed-Solomon scheme
//! - Our own packet captures against the dev host
//!

mod audio;
mod backend;
mod control;
mod enet;
mod hex;
mod host;
mod http;
mod identity;
mod input;
mod media;
mod pair;
mod rtsp;
mod tls;
mod video;

pub use audio::AudioReceive;
pub use backend::{MoonlightStream, start};
pub use control::{Crypto, message, message_type, msg};
pub use enet::{Command, HostMessage, run as run_control};
pub use host::{LaunchedSession, PairedSession, StreamMode};
pub use identity::ClientIdentity;
pub use input::InputEncoder;
pub use media::MediaSocket;
pub use pair::{PairedHost, pair, random_pin};
pub use rtsp::{Negotiated, Rtsp, StreamRequest};
pub use video::{Depacketizer, FrameLoss, Received, ShardHeader, VideoFrame, parse_header};

/// The client certificate in the hex-encoded-PEM form the `/pair` endpoint
/// expects. Public so a pairing step can be reproduced by hand against a host.
#[must_use]
pub fn cert_pem_hex(identity: &ClientIdentity) -> String {
    hex::encode(identity.cert_pem().as_bytes())
}

use gsa_core::{Error, Result};

/// What a host reports over the cleartext `/serverinfo` endpoint.
///
/// Hosts withhold capability detail from unauthenticated callers:
/// `MaxLumaPixelsHEVC` reads 0 and the codec bitfield is minimal until the
/// caller presents a paired client certificate. Re-read capabilities over
/// mutual TLS before relying on them.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub hostname: String,
    /// Host application version. Selects the pairing hash generation, so it is
    /// load-bearing rather than cosmetic.
    pub app_version: String,
    /// The host's own identifier, stable across restarts.
    pub unique_id: String,
    /// Port for the mutual-TLS endpoints; everything past pairing lives here.
    pub https_port: u16,
    /// This host already trusts our client certificate.
    pub paired: bool,
    /// Free, or already streaming to someone.
    pub state: String,
    /// Codec capability bitfield, unreliable until paired (see above).
    pub codec_mode_support: u32,
    /// App id the host is currently running, 0 when idle.
    pub current_game: u32,
}

/// Ask a host to describe itself over the cleartext port. The only exchange
/// that works before pairing, and therefore also the reachability check: a
/// failure means the host is off, firewalled, or not a Moonlight host.
///
/// `client_id` must stay stable across calls — it is the identity the host
/// pairs with.
pub async fn probe(addr: std::net::SocketAddr, client_id: &str) -> Result<ServerInfo> {
    let body = http::get(addr, &format!("/serverinfo?uniqueid={client_id}")).await?;
    parse_server_info(&body)
}

pub(crate) fn parse_server_info(body: &[u8]) -> Result<ServerInfo> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Session("non-UTF-8 XML".into()))?;
    let doc = roxmltree::Document::parse(text)
        .map_err(|e| Error::Session(format!("malformed serverinfo XML: {e}")))?;
    let root = doc.root_element();
    let field = |name: &str| -> Option<String> {
        root.children()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .map(str::to_owned)
    };
    // Hostname and uniqueid are required: the pairing handshake keys off them,
    // so an empty string here would fail later and less clearly.
    let hostname =
        field("hostname").ok_or_else(|| Error::Session("serverinfo has no hostname".into()))?;
    let unique_id =
        field("uniqueid").ok_or_else(|| Error::Session("serverinfo has no uniqueid".into()))?;
    Ok(ServerInfo {
        hostname,
        app_version: field("appversion").unwrap_or_default(),
        unique_id,
        https_port: field("HttpsPort")
            .and_then(|p| p.parse().ok())
            .unwrap_or(47984),
        paired: field("PairStatus").is_some_and(|s| s.trim() == "1"),
        state: field("state").unwrap_or_default(),
        codec_mode_support: field("ServerCodecModeSupport")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        current_game: field("currentgame")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_server_info;

    /// Captured verbatim from the dev host (Apollo 7.1.431), unpaired.
    const UNPAIRED: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200"><hostname>DESKTOP-OREGC06</hostname><appversion>7.1.431.-1</appversion><GfeVersion>3.23.0.74</GfeVersion><uniqueid>DB75D553-67B9-382F-B405-ABF0FF7141FA</uniqueid><HttpsPort>47984</HttpsPort><ExternalPort>47989</ExternalPort><MaxLumaPixelsHEVC>0</MaxLumaPixelsHEVC><mac>00:00:00:00:00:00</mac><Permission>0</Permission><LocalIP>192.168.50.184</LocalIP><ServerCodecModeSupport>262145</ServerCodecModeSupport><PairStatus>0</PairStatus><currentgame>0</currentgame><currentgameuuid/><state>SUNSHINE_SERVER_FREE</state></root>"#;

    #[test]
    fn reads_a_real_unpaired_response() {
        let info = parse_server_info(UNPAIRED).unwrap();
        assert_eq!(info.hostname, "DESKTOP-OREGC06");
        assert_eq!(info.app_version, "7.1.431.-1");
        assert_eq!(info.https_port, 47984);
        assert_eq!(info.codec_mode_support, 262_145);
        assert!(!info.paired);
        assert_eq!(info.state, "SUNSHINE_SERVER_FREE");
    }

    #[test]
    fn treats_pair_status_1_as_paired() {
        let xml = br#"<root><hostname>h</hostname><uniqueid>u</uniqueid><PairStatus>1</PairStatus></root>"#;
        assert!(parse_server_info(xml).unwrap().paired);
    }

    #[test]
    fn rejects_a_response_without_an_identity() {
        let xml = br#"<root><appversion>1</appversion></root>"#;
        assert!(parse_server_info(xml).is_err());
    }
}
