//! The pairing handshake.
//!
//! Both sides derive an AES key from a PIN that never crosses the wire, then
//! prove to each other that they hold it. Each side mixes its own certificate
//! signature into the hash it sends, so a man in the middle who swapped
//! certificates cannot produce a matching hash even if it learned the PIN.
//!
//! The client verifies the host's proof *before* sending its own secret
//! (see [`verify_host`]); skipping that is what let a tampered host collect
//! credentials, and it is the difference between detecting a wrong PIN here
//! and letting the host detect it a round trip later.

use crate::identity::{self, ClientIdentity};
use crate::{hex, http};
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use gsa_core::{Error, Result};
use rand_core::RngCore;
use rsa::signature::Verifier;
use sha2::Digest;

/// A completed pairing: what to persist so future connections are trusted.
#[derive(Debug, Clone)]
pub struct PairedHost {
    /// The host's certificate, PEM. Pin future TLS connections to this.
    pub host_cert_pem: String,
    /// The identifier we paired under; every later request must repeat it.
    pub client_id: String,
}

/// A PIN for the operator to type into the host.
///
/// Four digits is what host UIs expect. The entropy is low, which is exactly
/// why the value is never sent: it only ever exists as an input to the key
/// derivation on both sides.
#[must_use]
pub fn random_pin() -> String {
    let mut bytes = [0u8; 4];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| char::from(b'0' + b % 10)).collect()
}

pub(crate) fn random_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut out);
    out
}

/// `SHA256(salt || pin)`, truncated to an AES-128 key.
///
/// The salt is the raw random bytes, not their hex text, and the PIN is its
/// ASCII digits rather than a parsed number — both sides must agree exactly
/// or every subsequent ciphertext is noise.
fn pin_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(salt);
    hasher.update(pin.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// AES-128-ECB over whole blocks, no padding.
///
/// ECB is safe here only because every plaintext is a one-shot random value
/// or a hash; it must not be reused for anything with structure.
fn aes_ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(Error::Session(format!(
            "pairing payload is {} bytes, not whole AES blocks",
            data.len()
        )));
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = *GenericArray::from_slice(chunk);
        if encrypt {
            cipher.encrypt_block(&mut block);
        } else {
            cipher.decrypt_block(&mut block);
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

/// One field out of a host's XML reply, with the body quoted on failure so a
/// surprise is diagnosable instead of just absent.
fn field(body: &[u8], name: &str) -> Result<String> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Session("non-UTF-8 reply".into()))?;
    // A bare XML declaration with no root is how hosts answer a pairing
    // request they will not even begin — most often because pairing is
    // switched off host-side, or the host expects its own PIN to be issued
    // first. Say that, rather than reporting a parse error.
    let doc = roxmltree::Document::parse(text).map_err(|e| {
        if text.trim_start().starts_with("<?xml") && !text.contains("<root") {
            Error::Session(
                "host refused to start pairing (it returned an empty document): check that \
                 pairing is enabled host-side and that no other pairing is already pending"
                    .into(),
            )
        } else {
            Error::Session(format!("malformed pairing reply: {e}; body was {text:?}"))
        }
    })?;
    let root = doc.root_element();
    // Hosts disagree on whether a rejection is an HTTP error or a 200 with a
    // failure body, so the body is the authority.
    if root.attribute("status_code").is_some_and(|c| c != "200") {
        let message = root
            .attribute("status_message")
            .unwrap_or("no reason given");
        return Err(Error::Session(format!("host rejected pairing: {message}")));
    }
    if root
        .children()
        .find(|n| n.has_tag_name("paired"))
        .and_then(|n| n.text())
        .is_some_and(|v| v.trim() == "0")
    {
        return Err(Error::Session(
            "host rejected pairing (check the PIN was entered correctly)".into(),
        ));
    }
    root.children()
        .find(|n| n.has_tag_name(name))
        .and_then(|n| n.text())
        .map(str::to_owned)
        .ok_or_else(|| Error::Session(format!("pairing reply has no <{name}>: {text}")))
}

/// Percent-encode a value for a query string, keeping only the characters
/// that are unambiguous everywhere.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Run the handshake against a host.
///
/// `device_name` is how we introduce ourselves, and it is **required**: a host
/// that gets no name answers the first request with an empty document instead
/// of opening a pairing session, so the PIN never gets a chance to matter.
///
/// `pin` must be shown to the operator to enter host-side. The first request
/// **blocks until they do**, so this call can legitimately take minutes;
/// callers should let the user cancel rather than impose a short timeout.
pub async fn pair(
    addr: std::net::SocketAddr,
    client_id: &str,
    device_name: &str,
    pin: &str,
    identity: &ClientIdentity,
) -> Result<PairedHost> {
    let salt = random_16();
    let key = pin_key(&salt, pin);

    // Phase 1: offer our certificate and wait for the operator to approve.
    let body = http::get(
        addr,
        &format!(
            "/pair?uniqueid={client_id}&devicename={}&phrase=getservercert&salt={}&clientcert={}",
            query_escape(device_name),
            hex::encode(&salt),
            hex::encode(identity.cert_pem().as_bytes()),
        ),
    )
    .await?;
    let host_cert_pem = String::from_utf8(hex::decode(&field(&body, "plaincert")?)?)
        .map_err(|_| Error::Session("host certificate is not text".into()))?;

    // Phase 2: prove we hold the PIN, and collect the host's challenge.
    let client_challenge = random_16();
    let body = http::get(
        addr,
        &format!(
            "/pair?uniqueid={client_id}&clientchallenge={}",
            hex::encode(&aes_ecb(&key, &client_challenge, true)?)
        ),
    )
    .await?;
    let response = aes_ecb(
        &key,
        &hex::decode(&field(&body, "challengeresponse")?)?,
        false,
    )?;
    if response.len() != 48 {
        return Err(Error::Session(format!(
            "host challenge response is {} bytes, expected 48",
            response.len()
        )));
    }
    let (host_hash, host_challenge) = response.split_at(32);

    // Phase 3: answer the host's challenge, binding our own certificate in.
    let client_secret = random_16();
    let client_hash = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(host_challenge);
        hasher.update(identity.cert_signature());
        hasher.update(client_secret);
        hasher.finalize()
    };
    let body = http::get(
        addr,
        &format!(
            "/pair?uniqueid={client_id}&serverchallengeresp={}",
            hex::encode(&aes_ecb(&key, &client_hash, true)?)
        ),
    )
    .await?;
    let pairing_secret = hex::decode(&field(&body, "pairingsecret")?)?;
    verify_host(
        &pairing_secret,
        &host_cert_pem,
        &client_challenge,
        host_hash,
    )?;

    // Phase 4: hand over our secret, signed, now that the host has proven
    // itself. Only on accepting this does the host remember us.
    let mut proof = client_secret.to_vec();
    proof.extend_from_slice(&identity.sign(&client_secret));
    let body = http::get(
        addr,
        &format!(
            "/pair?uniqueid={client_id}&clientpairingsecret={}",
            hex::encode(&proof)
        ),
    )
    .await?;
    // `paired` is the whole answer here; `field` already fails on a rejection.
    let _ = field(&body, "paired")?;

    Ok(PairedHost {
        host_cert_pem,
        client_id: client_id.to_owned(),
    })
}

/// Check the host's half of the proof before trusting it with our secret.
///
/// Two independent things must hold: the host signed its secret with the key
/// in the certificate it gave us (so the certificate is really its own), and
/// the hash it sent earlier commits to that same certificate and secret (so
/// nobody swapped certificates in the middle).
fn verify_host(
    pairing_secret: &[u8],
    host_cert_pem: &str,
    client_challenge: &[u8; 16],
    host_hash: &[u8],
) -> Result<()> {
    if pairing_secret.len() <= 16 {
        return Err(Error::Session(format!(
            "host pairing secret is {} bytes, too short to carry a signature",
            pairing_secret.len()
        )));
    }
    let (host_secret, host_signature) = pairing_secret.split_at(16);

    let verifying = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(
        identity::public_key_from_cert_pem(host_cert_pem)?,
    );
    let signature = rsa::pkcs1v15::Signature::try_from(host_signature)
        .map_err(|e| Error::Session(format!("host signature is malformed: {e}")))?;
    verifying.verify(host_secret, &signature).map_err(|_| {
        Error::Session(
            "host did not sign its secret with the certificate it sent — refusing to pair".into(),
        )
    })?;

    let expected = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(client_challenge);
        hasher.update(identity::cert_signature_from_pem(host_cert_pem)?);
        hasher.update(host_secret);
        hasher.finalize()
    };
    if expected.as_slice() != host_hash {
        return Err(Error::Session(
            "host proof does not match its certificate — wrong PIN, or someone is in the middle"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{aes_ecb, pin_key, random_pin};

    #[test]
    fn key_derivation_is_pinned_to_exact_inputs() {
        let salt = [0u8; 16];
        // The PIN is hashed as text; a digit change must change the key, and
        // a wrong salt must too, or pairing would succeed against anything.
        assert_ne!(pin_key(&salt, "1234"), pin_key(&salt, "1235"));
        assert_ne!(pin_key(&salt, "1234"), pin_key(&[1u8; 16], "1234"));
        // Known answer computed independently (Python hashlib) for
        // SHA-256("\0"*16 || "1234")[..16]: the host derives this same key
        // from the PIN the operator types, so a drift here is unpairable.
        assert_eq!(
            crate::hex::encode(&pin_key(&salt, "1234")),
            "E4AE0C82639990744974CC3495A82432"
        );
    }

    #[test]
    fn ecb_round_trips_whole_blocks() {
        let key = [7u8; 16];
        let plain = [9u8; 48];
        let enc = aes_ecb(&key, &plain, true).unwrap();
        assert_eq!(enc.len(), 48);
        assert_ne!(enc, plain);
        assert_eq!(aes_ecb(&key, &enc, false).unwrap(), plain);
    }

    #[test]
    fn ecb_refuses_partial_blocks() {
        // Padding is never used on this wire, so a ragged length means we
        // built the payload wrong — fail rather than silently truncate.
        assert!(aes_ecb(&[0u8; 16], &[0u8; 20], true).is_err());
        assert!(aes_ecb(&[0u8; 16], &[], true).is_err());
    }

    #[test]
    fn pins_are_four_digits() {
        for _ in 0..32 {
            let pin = random_pin();
            assert_eq!(pin.len(), 4);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
    }
}
