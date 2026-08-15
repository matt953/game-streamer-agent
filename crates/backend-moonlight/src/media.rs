//! The media UDP ports.
//!
//! The port a host names during negotiation is where we *send* our ping, not
//! where it sends media from — the host learns our address from that ping and
//! streams back to its source port. So the ping must leave the very socket we
//! intend to receive on, and it must keep going until frames arrive: until
//! the host has heard from us it has nowhere to send.
//!
//! Two rules learned the hard way against a real host:
//!
//! - **Every media port must be pinged, including one we do not consume.**
//!   Pinging video alone let video flow, and then the host tore the whole
//!   session down after ten seconds — video and control both stopped.
//! - **Only one socket may use the session ping payload.** The host issues a
//!   single payload per session and binds streams by it, so a second socket
//!   sending the same value re-binds the first stream to the wrong port.
//!   Giving audio the same payload as video silently killed video entirely.
//!   The stream we actually read gets the payload; the other pings by
//!   address with the legacy form.

use gsa_core::{Error, Result};

/// The legacy ping: hosts that gave us no payload match us by address.
const PLAIN_PING: &[u8] = b"PING";

/// Opens a media port and keeps the host informed of where to send.
#[derive(Debug)]
pub struct MediaSocket {
    socket: std::net::UdpSocket,
    host: std::net::SocketAddr,
    /// Echoed so the host can bind this stream to our session rather than
    /// trusting the source address, which NAT may rewrite.
    payload: Option<[u8; 16]>,
    sequence: u32,
}

impl MediaSocket {
    /// `payload` binds this socket to the session; pass `None` to ping by
    /// address instead. See the module note — at most one socket per session
    /// may carry the payload.
    pub fn bind(host: std::net::SocketAddr, payload: Option<[u8; 16]>) -> Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| Error::Transport(format!("bind media socket: {e}")))?;
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .map_err(|e| Error::Transport(format!("media socket timeout: {e}")))?;
        Ok(Self {
            socket,
            host,
            payload,
            sequence: 0,
        })
    }

    /// Tell the host where to send. Repeat until media arrives.
    pub fn ping(&mut self) -> Result<()> {
        let datagram = match self.payload {
            Some(payload) => {
                let mut out = Vec::with_capacity(20);
                out.extend_from_slice(&payload);
                out.extend_from_slice(&self.sequence.to_le_bytes());
                self.sequence = self.sequence.wrapping_add(1);
                out
            }
            None => PLAIN_PING.to_vec(),
        };
        self.socket
            .send_to(&datagram, self.host)
            .map_err(|e| Error::Transport(format!("send media ping: {e}")))?;
        Ok(())
    }

    /// Receive one datagram, or `None` if the read timed out.
    pub fn recv(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        match self.socket.recv_from(buf) {
            Ok((n, _)) => Ok(Some(n)),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(Error::Transport(format!("receive media: {e}"))),
        }
    }

    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.socket
            .local_addr()
            .map_or(0, |a: std::net::SocketAddr| a.port())
    }
}
