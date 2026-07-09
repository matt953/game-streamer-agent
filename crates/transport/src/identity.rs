//! Ed25519 self-signed identity (persisted) + the anonymous cert verifier.
//!
//! `DevTrustVerifier` accepts any agent certificate and logs its fingerprint.
//! It is used where the agent's pin isn't known yet: during **pairing** (where
//! SPAKE2 does the authentication) and in **dev-open** mode. Streaming uses
//! `PinnedServerVerifier` instead (spec 06).

use std::path::Path;

use gsa_core::{Error, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// A TLS identity: Ed25519 self-signed certificate + private key. The cert's
/// SHA-256 fingerprint is the pin exchanged during pairing (spec 06).
#[derive(Debug)]
pub struct Identity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

/// Fresh Ed25519 self-signed cert + PKCS#8 key, as DER byte vectors.
fn fresh_ed25519() -> Result<(Vec<u8>, Vec<u8>)> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
        .map_err(|e| Error::Transport(format!("generate key: {e}")))?;
    let cert = rcgen::CertificateParams::new(vec!["gsa-agent".to_string()])
        .map_err(|e| Error::Transport(format!("cert params: {e}")))?
        .self_signed(&key)
        .map_err(|e| Error::Transport(format!("self-sign: {e}")))?;
    Ok((cert.der().to_vec(), key.serialize_der()))
}

impl Identity {
    /// Generate a fresh in-memory Ed25519 identity (ephemeral; tests and
    /// clients that don't persist).
    pub fn generate() -> Result<Self> {
        let (cert, key) = fresh_ed25519()?;
        Ok(Self {
            cert_der: CertificateDer::from(cert),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        })
    }

    /// Load the persisted identity from `dir`, or generate and persist a fresh
    /// one. Idempotent across runs → a stable pin (spec 06). The private key is
    /// written owner-only.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("identity.crt.der");
        let key_path = dir.join("identity.key.der");
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Self {
                cert_der: CertificateDer::from(cert),
                key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            });
        }
        let (cert, key) = fresh_ed25519()?;
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Transport(format!("create {}: {e}", dir.display())))?;
        write_private(&key_path, &key)?;
        std::fs::write(&cert_path, &cert)
            .map_err(|e| Error::Transport(format!("write cert: {e}")))?;
        Ok(Self {
            cert_der: CertificateDer::from(cert),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        })
    }

    /// SHA-256 fingerprint of the certificate (the peer-store pin format).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }
}

/// Write `bytes` to `path` owner-only (0600 on unix).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Transport(format!("open key file: {e}")))?;
        f.write_all(bytes)
            .map_err(|e| Error::Transport(format!("write key: {e}")))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).map_err(|e| Error::Transport(format!("write key: {e}")))
    }
}

/// Hex SHA-256 of a DER certificate.
#[must_use]
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let hash = Sha256::digest(cert.as_ref());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Anonymous verifier for pairing / dev-open: accept + log. See module docs.
#[derive(Debug)]
pub struct DevTrustVerifier {
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl DevTrustVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl Default for DevTrustVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl rustls::client::danger::ServerCertVerifier for DevTrustVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        tracing::info!(
            fingerprint = fingerprint(end_entity),
            "accepting agent cert unverified — pairing (SPAKE2 authenticates) or dev-open"
        );
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_identity_is_stable() {
        let dir = std::env::temp_dir().join(format!("gsa-id-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Identity::load_or_generate(&dir).unwrap();
        let b = Identity::load_or_generate(&dir).unwrap();
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "reload yields the same pin"
        );
        assert_eq!(a.fingerprint().len(), 64, "sha-256 hex");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_identities_are_distinct() {
        assert_ne!(
            Identity::generate().unwrap().fingerprint(),
            Identity::generate().unwrap().fingerprint()
        );
    }
}
