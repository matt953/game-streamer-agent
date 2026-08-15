//! A paired host: everything reachable once mutual TLS is established.

use crate::identity::ClientIdentity;
use crate::{ServerInfo, parse_server_info, tls};
use gsa_client_backend_api::{CatalogEntry, CatalogKind};
use gsa_core::{Error, Result};

/// A host we have paired with, ready to be queried or streamed from.
#[derive(Debug)]
pub struct PairedSession {
    tls_addr: std::net::SocketAddr,
    host_cert_pem: String,
    identity: ClientIdentity,
    client_id: String,
}

impl PairedSession {
    /// Bind a stored pairing to a live address.
    ///
    /// `tls_addr` is the host's TLS port — read it from [`ServerInfo`] rather
    /// than assuming the default, since hosts can be moved off it.
    #[must_use]
    pub fn new(
        tls_addr: std::net::SocketAddr,
        host_cert_pem: String,
        identity: ClientIdentity,
        client_id: String,
    ) -> Self {
        Self {
            tls_addr,
            host_cert_pem,
            identity,
            client_id,
        }
    }

    /// The host's own description of itself, authenticated this time — this
    /// is the copy whose capability fields can be believed.
    pub async fn server_info(&self) -> Result<ServerInfo> {
        let body = self
            .get(&format!("/serverinfo?uniqueid={}", self.client_id))
            .await?;
        parse_server_info(&body)
    }

    /// What this host can launch.
    ///
    /// The app list carries no "currently running" flag, so the running app
    /// is resolved from the host's own status instead of being guessed.
    pub async fn catalog(&self) -> Result<Vec<CatalogEntry>> {
        let running = self.server_info().await?.current_game;
        let body = self
            .get(&format!("/applist?uniqueid={}", self.client_id))
            .await?;
        parse_catalog(&body, running)
    }

    /// Ask the host to start streaming `app_id`, and get back the session's
    /// RTSP address plus the key the control channel will be encrypted with.
    ///
    /// `sops` lets the host change the desktop's resolution to match what we
    /// asked for. Defaulted off here: silently reconfiguring someone's
    /// monitor is a surprising thing for a client to do, and Apollo can be
    /// told to override the mode host-side anyway.
    pub async fn launch(&self, app_id: u32, mode: StreamMode) -> Result<LaunchedSession> {
        let (riaes_key, riaes_key_id) = new_stream_key();
        let body = self
            .get(&format!(
                "/launch?uniqueid={}&appid={app_id}&mode={}x{}x{}&additionalStates=1&sops={}\
                 &rikey={}&rikeyid={riaes_key_id}&localAudioPlayMode={}&surroundAudioInfo={}\
                 &hdrMode={}&gcmap=1",
                self.client_id,
                mode.width,
                mode.height,
                mode.fps,
                u8::from(mode.allow_host_mode_change),
                crate::hex::encode(&riaes_key),
                u8::from(mode.keep_host_audio),
                mode.surround_audio_info(),
                u8::from(mode.hdr),
            ))
            .await?;
        let rtsp_url = xml_field(&body, "sessionUrl0")?;
        Ok(LaunchedSession {
            rtsp_url,
            riaes_key,
            riaes_key_id,
        })
    }

    /// Rejoin the session the host is already holding for us.
    ///
    /// This is the call real clients make when the host still has a session
    /// for their certificate — it re-keys the streams and restarts delivery.
    /// Launching again instead silently inherits the old session, which
    /// handshakes perfectly and never sends a frame.
    pub async fn resume(&self, mode: StreamMode) -> Result<LaunchedSession> {
        let (riaes_key, riaes_key_id) = new_stream_key();
        let body = self
            .get(&format!(
                "/resume?uniqueid={}&rikey={}&rikeyid={riaes_key_id}&mode={}x{}x{}\
                 &surroundAudioInfo={}&hdrMode={}",
                self.client_id,
                crate::hex::encode(&riaes_key),
                mode.width,
                mode.height,
                mode.fps,
                mode.surround_audio_info(),
                u8::from(mode.hdr),
            ))
            .await?;
        let rtsp_url = xml_field(&body, "sessionUrl0")?;
        Ok(LaunchedSession {
            rtsp_url,
            riaes_key,
            riaes_key_id,
        })
    }

    /// Stop whatever the host is streaming. Safe to call when idle.
    pub async fn cancel(&self) -> Result<()> {
        let body = self
            .get(&format!("/cancel?uniqueid={}", self.client_id))
            .await?;
        xml_field(&body, "cancel").map(|_| ())
    }

    /// The id this session is currently using.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    async fn get(&self, path_and_query: &str) -> Result<Vec<u8>> {
        tls::get(
            self.tls_addr,
            &self.host_cert_pem,
            &self.identity,
            path_and_query,
        )
        .await
    }
}

/// A fresh stream key and its id.
///
/// The id doubles as a key epoch: hosts re-derive their ciphers when it
/// changes, which is what makes a resume actually restart the streams.
fn new_stream_key() -> ([u8; 16], i32) {
    let key = crate::pair::random_16();
    let id_bytes = crate::pair::random_16();
    // Hosts read this as a signed integer; keep it positive.
    let id = i32::from_be_bytes([id_bytes[0] & 0x7f, id_bytes[1], id_bytes[2], id_bytes[3]]);
    (key, id)
}

/// What we ask the host to encode.
#[derive(Debug, Clone, Copy)]
pub struct StreamMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Let the host change its desktop resolution to match. Off by default.
    pub allow_host_mode_change: bool,
    pub hdr: bool,
    /// Speaker count we can render. 2 is stereo.
    pub channels: u8,
    /// Leave the host's own speakers working while we stream.
    ///
    /// Hosts silence themselves by default so a stream does not play twice in
    /// one room. That is the wrong default for a machine somebody else is
    /// sitting at: it takes their audio away without asking.
    pub keep_host_audio: bool,
}

impl Default for StreamMode {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            allow_host_mode_change: false,
            hdr: false,
            channels: 2,
            keep_host_audio: true,
        }
    }
}

impl StreamMode {
    /// Channel count in the low half, channel mask in the high half — the
    /// packing the launch endpoint expects.
    fn surround_audio_info(self) -> u32 {
        let mask: u32 = match self.channels {
            6 => 0x3f,
            8 => 0x63f,
            _ => 0x3,
        };
        (mask << 16) | u32::from(self.channels.max(2))
    }
}

/// A stream the host has started for us.
#[derive(Debug, Clone)]
pub struct LaunchedSession {
    /// Where to run the RTSP handshake.
    pub rtsp_url: String,
    /// AES key for the control channel, chosen by us at launch.
    pub riaes_key: [u8; 16],
    /// Identifies that key; also feeds the control channel's nonces.
    pub riaes_key_id: i32,
}

/// One element's text from a host reply, erroring with the body when absent.
fn xml_field(body: &[u8], name: &str) -> Result<String> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Session("non-UTF-8 reply".into()))?;
    let doc = roxmltree::Document::parse(text)
        .map_err(|e| Error::Session(format!("malformed reply: {e}")))?;
    let root = doc.root_element();
    if root.attribute("status_code").is_some_and(|c| c != "200") {
        let message = root
            .attribute("status_message")
            .unwrap_or("no reason given");
        // Apollo grants only view/list rights to clients past the first, so a
        // refusal here is usually permissions rather than a protocol fault.
        return Err(Error::Session(format!(
            "host refused: {message} (if this is a permission error, grant this \
             client launch rights host-side)"
        )));
    }
    root.children()
        .find(|n| n.has_tag_name(name))
        .and_then(|n| n.text())
        .map(str::to_owned)
        .ok_or_else(|| Error::Session(format!("reply has no <{name}>: {text}")))
}

/// Classify an entry from its title.
///
/// The wire carries no notion of what an app *is*, so this is a presentation
/// hint rather than a fact: it exists so a unified library can group a host's
/// desktop and its game launcher sensibly. Anything unrecognised stays a
/// plain game, which is the harmless default.
fn classify(title: &str) -> CatalogKind {
    if title.eq_ignore_ascii_case("desktop") {
        CatalogKind::Desktop
    } else if title.contains("Big Picture") || title.eq_ignore_ascii_case("steam") {
        CatalogKind::Shell
    } else {
        CatalogKind::Game
    }
}

fn parse_catalog(body: &[u8], running_id: u32) -> Result<Vec<CatalogEntry>> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Session("non-UTF-8 applist".into()))?;
    let doc = roxmltree::Document::parse(text)
        .map_err(|e| Error::Session(format!("malformed applist: {e}")))?;
    let mut entries = Vec::new();
    for app in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name("App"))
    {
        let text_of = |name: &str| {
            app.children()
                .find(|n| n.has_tag_name(name))
                .and_then(|n| n.text())
                .map(str::trim)
        };
        // Hosts add their own elements here (Apollo carries a UUID and an
        // ordering index); unknown children are ignored rather than fatal.
        let (Some(title), Some(id)) = (text_of("AppTitle"), text_of("ID")) else {
            continue;
        };
        let Ok(id) = id.parse::<u32>() else { continue };
        entries.push(CatalogEntry {
            id,
            title: title.to_owned(),
            kind: classify(title),
            running: running_id != 0 && running_id == id,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{classify, parse_catalog};
    use gsa_client_backend_api::CatalogKind;

    /// Captured verbatim from the dev host (Apollo 0.4.6) over mutual TLS.
    const APPLIST: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200"><App><IsHdrSupported>0</IsHdrSupported><AppTitle>Desktop</AppTitle><UUID>D95532F8-2E17-4C09-9513-109B0F5BECF2</UUID><IDX>0</IDX><ID>881448767</ID></App><App><IsHdrSupported>0</IsHdrSupported><AppTitle>Steam Big Picture</AppTitle><UUID>4DE987D8-0F2A-6029-1002-4BEED1BD2C1A</UUID><IDX>1</IDX><ID>1093255277</ID></App><App><IsHdrSupported>0</IsHdrSupported><AppTitle>Virtual Display</AppTitle><UUID>8902CB19-674A-403D-A587-41B092E900BA</UUID><IDX>2</IDX><ID>382300562</ID></App></root>"#;

    #[test]
    fn reads_a_real_app_list() {
        let entries = parse_catalog(APPLIST, 0).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "Desktop");
        assert_eq!(entries[0].id, 881_448_767);
        assert_eq!(entries[0].kind, CatalogKind::Desktop);
        assert_eq!(entries[1].kind, CatalogKind::Shell);
        assert_eq!(entries[2].kind, CatalogKind::Game);
        assert!(entries.iter().all(|e| !e.running));
    }

    #[test]
    fn marks_the_running_app() {
        let entries = parse_catalog(APPLIST, 1_093_255_277).unwrap();
        assert!(entries[1].running);
        assert!(!entries[0].running);
    }

    #[test]
    fn zero_means_nothing_is_running() {
        // The host reports 0 when idle; treating that as an app id would
        // light up any entry that happened to have id 0.
        assert!(
            parse_catalog(APPLIST, 0)
                .unwrap()
                .iter()
                .all(|e| !e.running)
        );
    }

    #[test]
    fn surround_info_packs_mask_over_count() {
        use super::StreamMode;
        let stereo = StreamMode::default();
        assert_eq!(stereo.channels, 2);
        assert_eq!(stereo.surround_audio_info(), (0x3 << 16) | 2);
        let five_one = StreamMode {
            channels: 6,
            ..StreamMode::default()
        };
        assert_eq!(five_one.surround_audio_info(), (0x3f << 16) | 6);
        // A host must never be told we have fewer speakers than stereo; the
        // audio path has no mono mode to fall back to.
        let broken = StreamMode {
            channels: 0,
            ..StreamMode::default()
        };
        assert_eq!(broken.surround_audio_info() & 0xffff, 2);
    }

    #[test]
    fn a_refusal_names_the_reason_not_the_missing_field() {
        use super::xml_field;
        let xml = br#"<root status_code="503" status_message="Permission denied"/>"#;
        let err = xml_field(xml, "sessionUrl0").unwrap_err().to_string();
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn unrecognised_titles_stay_games() {
        assert_eq!(classify("Diablo IV"), CatalogKind::Game);
    }
}
