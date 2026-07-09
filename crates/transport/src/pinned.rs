//! Pinned TLS verifiers (spec 06) — the pairing-derived replacement for
//! `DevTrustVerifier`. The client pins the agent's exact identity; the agent
//! accepts only clients whose cert pin is in the peer store (mutual TLS). Not
//! wired into the transport config until pairing exists to populate the pins.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};

use crate::identity::fingerprint;
use crate::peers::PeerStore;

fn supported() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// Client-side: accept only the agent whose cert fingerprint equals the pin
/// established at pairing.
#[derive(Debug)]
pub struct PinnedServerVerifier {
    expected_pin: String,
    supported: WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    #[must_use]
    pub fn new(expected_pin: String) -> Self {
        Self {
            expected_pin,
            supported: supported(),
        }
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if fingerprint(end_entity) == self.expected_pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General("agent cert pin mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Agent-side: accept only clients whose cert pin is a paired peer.
#[derive(Debug)]
pub struct PinnedClientVerifier {
    store: Arc<PeerStore>,
    supported: WebPkiSupportedAlgorithms,
}

impl PinnedClientVerifier {
    #[must_use]
    pub fn new(store: Arc<PeerStore>) -> Self {
        Self {
            store,
            supported: supported(),
        }
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        if self.store.get(&fingerprint(end_entity)).is_some() {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(TlsError::General("client is not a paired peer".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use gsa_protocol::grant::Scope;

    #[test]
    fn server_verifier_matches_only_the_pinned_cert() {
        let agent = Identity::generate().unwrap();
        let other = Identity::generate().unwrap();
        let v = PinnedServerVerifier::new(agent.fingerprint());
        let name = ServerName::try_from("gsa-agent").unwrap();
        assert!(
            v.verify_server_cert(&agent.cert_der, &[], &name, &[], UnixTime::now())
                .is_ok()
        );
        assert!(
            v.verify_server_cert(&other.cert_der, &[], &name, &[], UnixTime::now())
                .is_err()
        );
    }

    #[test]
    fn client_verifier_accepts_only_paired_peers() {
        let dir = std::env::temp_dir().join(format!("gsa-pin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(PeerStore::load_or_empty(&dir).unwrap());
        let paired = Identity::generate().unwrap();
        let stranger = Identity::generate().unwrap();
        store
            .add("laptop".into(), paired.fingerprint(), Scope::Interact)
            .unwrap();

        let v = PinnedClientVerifier::new(store);
        assert!(
            v.verify_client_cert(&paired.cert_der, &[], UnixTime::now())
                .is_ok()
        );
        assert!(
            v.verify_client_cert(&stranger.cert_der, &[], UnixTime::now())
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
