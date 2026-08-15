//! Hex codec for the pairing query parameters.
//!
//! Hosts emit either case, so decoding must accept both; we emit uppercase.

use gsa_core::{Error, Result};

pub(crate) fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    out
}

pub(crate) fn decode(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err(Error::Session("hex value has an odd length".into()));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::Session(format!(
            "not a hex digit: {:?}",
            char::from(c)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn round_trips() {
        let bytes = [0x00, 0x0f, 0xa5, 0xff];
        assert_eq!(encode(&bytes), "000FA5FF");
        assert_eq!(decode("000FA5FF").unwrap(), bytes);
    }

    #[test]
    fn accepts_either_case_because_hosts_differ() {
        assert_eq!(decode("deadBEEF").unwrap(), decode("DEADbeef").unwrap());
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode("abc").is_err());
        assert!(decode("zz").is_err());
    }
}
