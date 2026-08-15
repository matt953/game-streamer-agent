//! The media UDP ports.
//!
//! The port a host names during negotiation is where the ping is *sent*, not
//! where media comes from: the host learns the client address from that ping
//! and streams back from its own source port. The ping must therefore leave
//! the same socket that will receive, and must repeat until frames arrive —
//! until the host has heard from us it has nowhere to send.
//!
//! Two protocol rules:
//!
//! - **Every media port must be pinged, including ones whose stream is not
//!   consumed.** A host holding a media stream it cannot deliver tears the
//!   whole session down after ten seconds, video and control alike.
//! - **Only one socket per session may send the session ping payload.** The
//!   host issues one payload per session and binds streams by it, so a second
//!   socket sending the same value re-binds the first stream to the wrong
//!   port. The stream that is actually read gets the payload; other ports are
//!   pinged by address with the legacy form.

use gsa_core::{Error, Result};

/// Legacy ping form: hosts that issue no payload match the client by address.
const PLAIN_PING: &[u8] = b"PING";

/// Opens a media port and keeps the host informed of where to send.
#[derive(Debug)]
pub struct MediaSocket {
    socket: std::net::UdpSocket,
    host: std::net::SocketAddr,
    /// Echoed so the host binds this stream to the session rather than to the
    /// source address, which NAT may rewrite.
    payload: Option<[u8; 16]>,
    sequence: u32,
}

impl MediaSocket {
    /// `payload` binds this socket to the session; `None` pings by address
    /// instead. At most one socket per session may carry the payload — see
    /// the module documentation.
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
        let datagram = self.ping_datagram();
        self.socket
            .send_to(&datagram, self.host)
            .map_err(|e| Error::Transport(format!("send media ping: {e}")))?;
        Ok(())
    }

    /// Ping a different port on the same host from this socket.
    ///
    /// Claims every media stream for one socket. A host binds a stream to
    /// whichever address pinged it, so pinging from two sockets lets the
    /// later one take the earlier one's stream; pinging both ports from a
    /// single socket leaves nothing to take. The streams are then told apart
    /// by packet type on arrival.
    pub fn ping_port(&mut self, port: u16) -> Result<()> {
        let target = std::net::SocketAddr::new(self.host.ip(), port);
        let datagram = self.ping_datagram();
        self.socket
            .send_to(&datagram, target)
            .map_err(|e| Error::Transport(format!("send media ping to {target}: {e}")))?;
        Ok(())
    }

    /// Ping another port with the address-matched legacy form.
    ///
    /// Some hosts bind a stream from the payload and others from the source
    /// address. The wire does not say which form a given port expects, so the
    /// caller chooses.
    pub fn ping_port_plain(&self, port: u16) -> Result<()> {
        let target = std::net::SocketAddr::new(self.host.ip(), port);
        self.socket
            .send_to(PLAIN_PING, target)
            .map_err(|e| Error::Transport(format!("send media ping to {target}: {e}")))?;
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

    fn ping_datagram(&mut self) -> Vec<u8> {
        match self.payload {
            Some(payload) => {
                let mut out = Vec::with_capacity(20);
                out.extend_from_slice(&payload);
                out.extend_from_slice(&self.sequence.to_le_bytes());
                self.sequence = self.sequence.wrapping_add(1);
                out
            }
            None => PLAIN_PING.to_vec(),
        }
    }

    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.socket
            .local_addr()
            .map_or(0, |a: std::net::SocketAddr| a.port())
    }
}
