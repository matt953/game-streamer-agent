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
    fn unrecognised_titles_stay_games() {
        assert_eq!(classify("Diablo IV"), CatalogKind::Game);
    }
}
