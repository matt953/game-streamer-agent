//! The client's long-lived cryptographic identity.
//!
//! A host remembers a paired client by its certificate, so this key and
//! certificate must be generated **once** and persisted for the life of the
//! install: regenerating them silently un-pairs every host, and on Apollo it
//! also loses the per-client settings and permissions the operator granted.
//!
//! RSA-2048 with a SHA-256 self-signature is not a free choice — hosts verify
//! the pairing signature as PKCS#1 v1.5 over an RSA key, so an ECDSA or
//! Ed25519 identity cannot pair at all.

use gsa_core::{Error, Result};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::signature::{SignatureEncoding, Signer};

/// Bits of RSA. Fixed by what hosts will verify, not a tuning knob.
const KEY_BITS: usize = 2048;

/// A client key pair plus its self-signed certificate.
pub struct ClientIdentity {
    key_pem: String,
    cert_pem: String,
    /// The certificate's outer `signatureValue` bytes — an input to the
    /// pairing hashes, so it is extracted once here rather than re-parsed.
    signature: Vec<u8>,
    signing_key: rsa::pkcs1v15::SigningKey<sha2::Sha256>,
}

impl std::fmt::Debug for ClientIdentity {
    /// Never render the private key, even by accident.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("cert_signature_len", &self.signature.len())
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// Mint a fresh identity. Slow (RSA key generation); do it once and store
    /// the PEMs from [`ClientIdentity::key_pem`] / [`ClientIdentity::cert_pem`].
    pub fn generate() -> Result<Self> {
        let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, KEY_BITS)
            .map_err(|e| Error::Session(format!("generate client key: {e}")))?;
        let key_pem = key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .map_err(|e| Error::Session(format!("encode client key: {e}")))?
            .to_string();
        Self::from_key_pem(&key_pem)
    }

    /// Rebuild an identity from a stored private key, re-deriving the
    /// certificate. Deterministic given the key, so a stored key alone is
    /// enough to stay paired.
    pub fn from_key_pem(key_pem: &str) -> Result<Self> {
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(key_pem)
            .map_err(|e| Error::Session(format!("read client key: {e}")))?;
        let rcgen_key = rcgen::KeyPair::from_pem_and_sign_algo(key_pem, &rcgen::PKCS_RSA_SHA256)
            .map_err(|e| Error::Session(format!("load key for certificate: {e}")))?;

        let mut params = rcgen::CertificateParams::default();
        // Hosts authenticate by certificate fingerprint and do not check
        // names, chains, or expiry — but a long validity keeps us honest with
        // anything stricter that might sit in the path later.
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2060, 1, 1);
        params.distinguished_name = {
            let mut dn = rcgen::DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, "gsa-client");
            dn
        };
        let cert = params
            .self_signed(&rcgen_key)
            .map_err(|e| Error::Session(format!("self-sign certificate: {e}")))?;
        let signature = cert_signature(cert.der())?;

        Ok(Self {
            key_pem: key_pem.to_owned(),
            cert_pem: cert.pem(),
            signature,
            signing_key: rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(key),
        })
    }

    /// PEM private key — persist this, and guard it like a password.
    #[must_use]
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// PEM certificate, as sent to hosts during pairing.
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The certificate's signature bytes, hashed into the pairing challenge.
    pub(crate) fn cert_signature(&self) -> &[u8] {
        &self.signature
    }

    /// PKCS#1 v1.5 SHA-256 signature, the form hosts verify.
    pub(crate) fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.signing_key.sign(message).to_vec()
    }
}

/// Pull the outer `signatureValue` out of a DER certificate — the signature
/// itself, without the algorithm identifier that precedes it.
pub(crate) fn cert_signature(der: &[u8]) -> Result<Vec<u8>> {
    let (_, parsed) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| Error::Session(format!("parse certificate: {e}")))?;
    Ok(parsed.signature_value.data.to_vec())
}

/// The RSA public key a host will be verified against, from its PEM cert.
pub(crate) fn public_key_from_cert_pem(pem: &str) -> Result<rsa::RsaPublicKey> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| Error::Session(format!("parse host certificate PEM: {e}")))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&block.contents)
        .map_err(|e| Error::Session(format!("parse host certificate: {e}")))?;
    let spki = cert.public_key();
    <rsa::RsaPublicKey as rsa::pkcs8::DecodePublicKey>::from_public_key_der(spki.raw)
        .map_err(|e| Error::Session(format!("host key is not RSA: {e}")))
}

/// The host certificate's signature bytes, hashed into the pairing challenge.
pub(crate) fn cert_signature_from_pem(pem: &str) -> Result<Vec<u8>> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| Error::Session(format!("parse host certificate PEM: {e}")))?;
    cert_signature(&block.contents)
}

#[cfg(test)]
mod tests {
    use super::ClientIdentity;

    /// Generating a key is slow, so one identity serves the whole module.
    fn identity() -> ClientIdentity {
        ClientIdentity::generate().expect("generate identity")
    }

    #[test]
    fn mints_a_usable_identity() {
        let id = identity();
        assert!(id.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(id.key_pem().contains("PRIVATE KEY"));
        // RSA-2048 signs 256-byte values; hosts reject anything shorter.
        assert_eq!(id.cert_signature().len(), 256);
        assert_eq!(id.sign(b"anything").len(), 256);
    }

    #[test]
    fn a_stored_key_reproduces_the_same_identity() {
        // The host remembers us by certificate, so a restart that reloads the
        // key must not present a different one.
        let first = identity();
        let second = ClientIdentity::from_key_pem(first.key_pem()).unwrap();
        assert_eq!(first.cert_pem(), second.cert_pem());
        assert_eq!(first.cert_signature(), second.cert_signature());
    }

    #[test]
    fn debug_never_leaks_the_private_key() {
        let id = identity();
        assert!(!format!("{id:?}").contains("PRIVATE"));
    }
}
