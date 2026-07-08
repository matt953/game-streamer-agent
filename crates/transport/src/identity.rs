//! Per-run self-signed identity + the M0 dev-trust verifier.
//!
//! ⚠ `DevTrustVerifier` accepts any certificate and merely logs its
//! fingerprint (trust-on-first-use, development only). Pairing-derived
//! pinned verification replaces it at M2 (spec 06); this type is the seam
//! where the pinned verifier slots in.

use gsa_core::{Error, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// A TLS identity: self-signed certificate + private key.
#[derive(Debug)]
pub struct Identity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

impl Identity {
    /// Generate a fresh self-signed identity (per-run at M0; persisted in
    /// the OS keystore from M2).
    pub fn generate() -> Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["gsa-agent".into()])
            .map_err(|e| Error::Transport(format!("generate identity: {e}")))?;
        let cert_der = cert.cert.der().clone();
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        Ok(Self { cert_der, key_der })
    }

    /// SHA-256 fingerprint of the certificate (the peer-store pin format).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }
}

/// Hex SHA-256 of a DER certificate.
#[must_use]
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let hash = Sha256::digest(cert.as_ref());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Dev-only verifier: accept + log. See module docs.
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
            "DEV TRUST: accepting unverified agent certificate (M0 only)"
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
