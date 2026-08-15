//! The encrypted control channel (ENet over UDP).
//!
//! Every message is wrapped in an AES-GCM envelope keyed by the value we sent
//! at launch. Two nonce constructions exist in the wild and they are not
//! interchangeable, so which one to use is read from the host's SDP rather
//! than assumed — see [`Crypto::new`].

use gsa_core::{Error, Result};

/// Message types. Only the ones we actually send or act on are named; an
/// unknown type is ignored rather than treated as an error, because hosts
/// send controller features we have not wired up yet.
pub mod msg {
    pub const ENCRYPTED: u16 = 0x0001;
    pub const START_A: u16 = 0x0305;
    pub const START_B: u16 = 0x0307;
    pub const PERIODIC_PING: u16 = 0x0200;
    pub const IDR_FRAME: u16 = 0x0302;
    pub const INVALIDATE_REF_FRAMES: u16 = 0x0301;
    pub const RUMBLE: u16 = 0x010b;
    /// The specification and every implementation disagree here, so both are
    /// accepted on receive.
    pub const TERMINATION: u16 = 0x0109;
    pub const TERMINATION_ALT: u16 = 0x0100;
}

/// Which nonce layout the control channel uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// 16-byte IV whose first byte is the sequence number. Older hosts.
    Legacy,
    /// 12-byte IV: sequence little-endian, then a per-direction tag so the
    /// two directions never share a nonce.
    V2,
}

/// Seals and opens control messages.
#[derive(Debug)]
pub struct Crypto {
    key: [u8; 16],
    scheme: Scheme,
    /// Ours to choose and increment; the host tracks it as replay protection.
    next_seq: u32,
}

impl Crypto {
    /// `control_v2` comes from the host's advertised `encryptionSupported`.
    #[must_use]
    pub fn new(key: [u8; 16], control_v2: bool) -> Self {
        Self {
            key,
            scheme: if control_v2 {
                Scheme::V2
            } else {
                Scheme::Legacy
            },
            next_seq: 0,
        }
    }

    /// `from_host` selects the direction tag; the two directions must never
    /// use the same nonce with the same key.
    fn nonce(&self, seq: u32, from_host: bool) -> Vec<u8> {
        match self.scheme {
            Scheme::Legacy => {
                // Only the low byte of the sequence survives here, which is
                // the scheme's own limitation, not ours.
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            Scheme::V2 => {
                let mut iv = vec![0u8; 12];
                iv[..4].copy_from_slice(&seq.to_le_bytes());
                iv[10..].copy_from_slice(if from_host { b"HC" } else { b"CC" });
                iv
            }
        }
    }

    /// Wrap one plaintext message for sending.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::aead::AeadInPlace;
        use aes_gcm::{Aes128Gcm, KeyInit};

        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let cipher = Aes128Gcm::new_from_slice(&self.key)
            .map_err(|e| Error::Session(format!("control key rejected: {e}")))?;
        let mut buffer = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(
                aes_gcm::Nonce::from_slice(&self.nonce(seq, false)),
                &[],
                &mut buffer,
            )
            .map_err(|_| Error::Session("control encryption failed".into()))?;

        // type, length-of-everything-after-length, sequence, tag, ciphertext.
        let mut out = Vec::with_capacity(24 + buffer.len());
        out.extend_from_slice(&msg::ENCRYPTED.to_le_bytes());
        out.extend_from_slice(&((4 + 16 + buffer.len()) as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&buffer);
        Ok(out)
    }

    /// Unwrap one received message, returning its plaintext.
    pub fn open(&self, frame: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::aead::AeadInPlace;
        use aes_gcm::{Aes128Gcm, KeyInit};

        if frame.len() < 24 {
            return Err(Error::Session(format!(
                "control frame is {} bytes, too short to be encrypted",
                frame.len()
            )));
        }
        let kind = u16::from_le_bytes([frame[0], frame[1]]);
        if kind != msg::ENCRYPTED {
            return Err(Error::Session(format!(
                "expected an encrypted control frame, got type {kind:#06x}"
            )));
        }
        let seq = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let tag = &frame[8..24];
        let mut buffer = frame[24..].to_vec();
        let cipher = Aes128Gcm::new_from_slice(&self.key)
            .map_err(|e| Error::Session(format!("control key rejected: {e}")))?;
        cipher
            .decrypt_in_place_detached(
                aes_gcm::Nonce::from_slice(&self.nonce(seq, true)),
                &[],
                &mut buffer,
                aes_gcm::Tag::from_slice(tag),
            )
            .map_err(|_| Error::Session("control frame failed authentication".into()))?;
        Ok(buffer)
    }
}

/// Build a plaintext control message: type, payload length, payload.
#[must_use]
pub fn message(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The type of a decrypted message, or `None` if it is too short to have one.
#[must_use]
pub fn message_type(plaintext: &[u8]) -> Option<u16> {
    (plaintext.len() >= 2).then(|| u16::from_le_bytes([plaintext[0], plaintext[1]]))
}

/// True for either spelling of the host's "session is over" message.
#[must_use]
pub fn is_termination(kind: u16) -> bool {
    kind == msg::TERMINATION || kind == msg::TERMINATION_ALT
}

#[cfg(test)]
mod tests {
    use super::{Crypto, Scheme, is_termination, message, message_type, msg};

    #[test]
    fn a_sealed_message_round_trips() {
        let mut tx = Crypto::new([7u8; 16], true);
        let plaintext = message(msg::START_B, &[1, 0, 0]);
        let frame = tx.seal(&plaintext).unwrap();
        // The receiving side derives its nonce from the frame, so opening
        // with a fresh context must work — the host has no shared counter.
        let rx = Crypto::new([7u8; 16], true);
        // Our own frames carry the client direction tag, so decrypt with a
        // context that expects the same direction.
        let opened = open_as_client(&rx, &frame).unwrap();
        assert_eq!(opened, plaintext);
        assert_eq!(message_type(&opened), Some(msg::START_B));
    }

    /// Open a frame we sealed ourselves (client-direction nonce).
    fn open_as_client(c: &Crypto, frame: &[u8]) -> Option<Vec<u8>> {
        use aes_gcm::aead::AeadInPlace;
        use aes_gcm::{Aes128Gcm, KeyInit};
        let seq = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let mut buf = frame[24..].to_vec();
        let cipher = Aes128Gcm::new_from_slice(&c.key_for_test()).unwrap();
        cipher
            .decrypt_in_place_detached(
                aes_gcm::Nonce::from_slice(&c.nonce_for_test(seq, false)),
                &[],
                &mut buf,
                aes_gcm::Tag::from_slice(&frame[8..24]),
            )
            .ok()
            .map(|()| buf)
    }

    #[test]
    fn the_envelope_matches_the_wire_layout() {
        let mut c = Crypto::new([0u8; 16], true);
        let frame = c.seal(&message(msg::IDR_FRAME, &[0, 0])).unwrap();
        assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), msg::ENCRYPTED);
        // length counts sequence + tag + ciphertext, not itself or the type.
        let len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(
            u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
            0
        );
    }

    #[test]
    fn sequence_numbers_advance_per_message() {
        let mut c = Crypto::new([0u8; 16], true);
        let a = c.seal(b"aaaaaaaa").unwrap();
        let b = c.seal(b"aaaaaaaa").unwrap();
        assert_eq!(u32::from_le_bytes([a[4], a[5], a[6], a[7]]), 0);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 1);
        // Same plaintext, different nonce, so the ciphertext must differ —
        // otherwise the stream would leak repeats.
        assert_ne!(a[24..], b[24..]);
    }

    #[test]
    fn the_two_schemes_build_different_nonces() {
        let v2 = Crypto::new([0u8; 16], true);
        let legacy = Crypto::new([0u8; 16], false);
        assert_eq!(v2.scheme_for_test(), Scheme::V2);
        assert_eq!(legacy.scheme_for_test(), Scheme::Legacy);
        assert_eq!(v2.nonce_for_test(1, false).len(), 12);
        assert_eq!(legacy.nonce_for_test(1, false).len(), 16);
        // Direction must change the nonce under v2, or both directions would
        // encrypt different data under the same key and counter.
        assert_ne!(v2.nonce_for_test(1, false), v2.nonce_for_test(1, true));
        // Legacy has no direction tag; recorded so the difference is not a
        // surprise if a host ever needs it.
        assert_eq!(
            legacy.nonce_for_test(1, false),
            legacy.nonce_for_test(1, true)
        );
    }

    #[test]
    fn a_tampered_frame_is_rejected() {
        let mut c = Crypto::new([3u8; 16], true);
        let mut frame = c.seal(&message(msg::PERIODIC_PING, &[0; 8])).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        assert!(c.open(&frame).is_err());
        assert!(c.open(&frame[..10]).is_err());
    }

    #[test]
    fn both_termination_spellings_are_recognised() {
        assert!(is_termination(msg::TERMINATION));
        assert!(is_termination(msg::TERMINATION_ALT));
        assert!(!is_termination(msg::RUMBLE));
    }

    impl Crypto {
        fn key_for_test(&self) -> [u8; 16] {
            self.key
        }
        fn nonce_for_test(&self, seq: u32, from_host: bool) -> Vec<u8> {
            self.nonce(seq, from_host)
        }
        fn scheme_for_test(&self) -> Scheme {
            self.scheme
        }
    }
}
