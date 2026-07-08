//! Length-prefixed postcard control messages over QUIC streams (spec 05).
//! 4-byte big-endian length, capped at `MAX_CONTROL_MSG` (attacker-
//! controlled prefix; the cap is the defense).

use gsa_core::error::ProtocolError;
use gsa_core::{Error, Result};
use gsa_protocol::{MAX_CONTROL_MSG, decode_msg, encode_msg};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub async fn send_msg<T: Serialize>(stream: &mut quinn::SendStream, msg: &T) -> Result<()> {
    let bytes = encode_msg(msg)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| Error::Protocol(ProtocolError::Serialize("message > 4 GiB".into())))?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| Error::Transport(format!("send len: {e}")))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| Error::Transport(format!("send body: {e}")))?;
    Ok(())
}

pub async fn recv_msg<T: DeserializeOwned>(stream: &mut quinn::RecvStream) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Transport(format!("recv len: {e}")))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_CONTROL_MSG {
        return Err(Error::Protocol(ProtocolError::Deserialize(format!(
            "control message of {len} bytes exceeds cap {MAX_CONTROL_MSG}"
        ))));
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|e| Error::Transport(format!("recv body: {e}")))?;
    Ok(decode_msg(&body)?)
}
