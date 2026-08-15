//! Driving the control channel's ENet connection.
//!
//! Hosts run a fork of ENet whose changes are local behaviour only — timers,
//! IPv6 addressing, a lower default MTU. Its wire structures are byte-identical
//! to upstream, so a stock Rust ENet interoperates. The MTU is negotiated down
//! to whatever the host proposes; do not assume 1392.
//!
//! ENet is poll-driven rather than async, so this runs on its own thread and
//! talks to the rest of the client over channels.

use crate::control::{Crypto, message, message_type, msg};
use gsa_core::{Error, Result};
use rusty_enet as enet;

/// Application-level keepalive period. Hosts drop a session after several
/// seconds without one; ENet's own keepalives do not count.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Poll interval. ENet needs servicing regularly to retransmit and to
/// surface received packets.
const SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);

/// Channel count to request. Reference clients open this many and place some
/// controller streams on the higher channels.
const CHANNELS: usize = 48;

/// Something the host told us over the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// The control channel is up and the host accepted the session binding.
    /// Reported explicitly: a failure to connect is otherwise indistinguishable
    /// from a host that has nothing to say.
    Connected,
    /// The host ended the session and supplied a reason code.
    Terminated { reason: u32 },
    /// The ENet peer went away without a reason — a timeout or a reset.
    /// Distinct from `Terminated`, which carries a host reason code.
    Disconnected,
    Rumble {
        controller: u16,
        low_frequency: u16,
        high_frequency: u16,
    },
    /// A message type not acted on here, surfaced so callers can log it rather
    /// than discard it silently.
    Other { kind: u16 },
}

impl HostMessage {
    /// The backend-neutral form, for embedders that do not know which protocol
    /// produced it. `None` for messages with no meaning outside this backend
    /// (connection lifecycle, unimplemented features).
    #[must_use]
    pub fn neutral(&self) -> Option<gsa_client_backend_api::BackendEvent> {
        match *self {
            Self::Rumble {
                controller,
                low_frequency,
                high_frequency,
            } => Some(gsa_client_backend_api::BackendEvent::Feedback(
                gsa_client_backend_api::GamepadFeedback::Rumble {
                    // Controller numbers are u16 on this wire and a u8 seat
                    // index everywhere else; there is no seat above 255.
                    seat: controller.min(u16::from(u8::MAX)) as u8,
                    low: low_frequency,
                    high: high_frequency,
                },
            )),
            _ => None,
        }
    }
}

/// What the caller can ask the control channel to send.
#[derive(Debug, Clone)]
pub enum Command {
    /// Request a fresh keyframe. Full-cost recovery.
    RequestIdr,
    /// Invalidate references over a frame range instead; far cheaper in
    /// bitrate than a keyframe on a constrained link.
    InvalidateReferenceFrames {
        first: u32,
        last: u32,
    },
    Input(Vec<u8>),
    Stop,
}

/// Connect, run the control channel, and stream host messages back.
///
/// Blocking: run it on its own thread.
pub fn run(
    addr: std::net::SocketAddr,
    connect_data: u32,
    mut crypto: Crypto,
    commands: std::sync::mpsc::Receiver<Command>,
    events: std::sync::mpsc::Sender<HostMessage>,
) -> Result<()> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| Error::Transport(format!("bind control socket: {e}")))?;
    // `Host::new` initialises the socket (non-blocking, broadcast) itself.
    let mut host = enet::Host::new(
        socket,
        enet::HostSettings {
            peer_limit: 1,
            channel_limit: CHANNELS,
            ..enet::HostSettings::default()
        },
    )
    .map_err(|e| Error::Transport(format!("create control host: {e}")))?;

    // The connect data binds this ENet peer to the RTSP session; the host does
    // not identify the peer by source address.
    host.connect(addr, CHANNELS, connect_data)
        .map_err(|_| Error::Transport("no ENet peer slot".into()))?;

    let mut connected = false;
    let mut started = false;
    let mut last_ping = std::time::Instant::now();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    loop {
        while let Some(event) = host
            .service()
            .map_err(|e| Error::Transport(format!("control service: {e}")))?
        {
            match event {
                enet::Event::Connect { .. } => {
                    tracing::info!("control channel connected");
                    connected = true;
                    let _ = events.send(HostMessage::Connected);
                }
                enet::Event::Disconnect { .. } => {
                    let _ = events.send(HostMessage::Disconnected);
                    return Ok(());
                }
                enet::Event::Receive { packet, .. } => {
                    match crypto.open(packet.data()) {
                        Ok(plaintext) => {
                            if let Some(m) = interpret(&plaintext) {
                                let terminated = matches!(m, HostMessage::Terminated { .. });
                                let _ = events.send(m);
                                if terminated {
                                    return Ok(());
                                }
                            }
                        }
                        // A frame that fails authentication is not fatal; one
                        // bad packet must not end a working session.
                        Err(e) => tracing::debug!(error = %e, "control frame dropped"),
                    }
                }
            }
        }

        if connected && !started {
            // Media does not start until both of these are sent; pinging the
            // media ports alone leaves some hosts silent.
            send(&mut host, &mut crypto, &message(msg::START_A, &[]))?;
            send(&mut host, &mut crypto, &message(msg::START_B, &[1, 0, 0]))?;
            started = true;
        }

        if !connected && std::time::Instant::now() > deadline {
            return Err(Error::Transport(
                "control channel did not connect within 10s".into(),
            ));
        }

        match commands.try_recv() {
            Ok(Command::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                for peer in host.connected_peers_mut() {
                    peer.disconnect(0);
                }
                let _ = host.service();
                return Ok(());
            }
            Ok(command) => {
                let plaintext = match command {
                    Command::RequestIdr => message(msg::IDR_FRAME, &[0, 0]),
                    Command::InvalidateReferenceFrames { first, last } => {
                        let mut payload = Vec::with_capacity(24);
                        payload.extend_from_slice(&first.to_le_bytes());
                        payload.extend_from_slice(&0u32.to_le_bytes());
                        payload.extend_from_slice(&last.to_le_bytes());
                        payload.extend_from_slice(&[0u8; 12]);
                        message(msg::INVALIDATE_REF_FRAMES, &payload)
                    }
                    Command::Input(bytes) => bytes,
                    Command::Stop => unreachable!("handled above"),
                };
                send(&mut host, &mut crypto, &plaintext)?;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        if connected && last_ping.elapsed() >= PING_INTERVAL {
            send(
                &mut host,
                &mut crypto,
                &message(msg::PERIODIC_PING, &[4, 0, 0, 0, 0, 0, 0, 0]),
            )?;
            last_ping = std::time::Instant::now();
        }

        std::thread::sleep(SERVICE_INTERVAL);
    }
}

fn send<S: enet::Socket>(
    host: &mut enet::Host<S>,
    crypto: &mut Crypto,
    plaintext: &[u8],
) -> Result<()> {
    let frame = crypto.seal(plaintext)?;
    let packet = enet::Packet::reliable(frame.as_slice());
    let mut sent = false;
    for peer in host.connected_peers_mut() {
        match peer.send(0, &packet) {
            Ok(()) => sent = true,
            // Logged, not dropped: a dead control channel is otherwise
            // indistinguishable from an idle one.
            Err(e) => tracing::warn!(error = ?e, "control send failed"),
        }
    }
    if !sent {
        tracing::warn!("control message dropped: no connected peer");
    }
    Ok(())
}

/// Turn a decrypted control message into something the caller can act on.
fn interpret(plaintext: &[u8]) -> Option<HostMessage> {
    let kind = message_type(plaintext)?;
    let payload = plaintext.get(4..).unwrap_or(&[]);
    if crate::control::is_termination(kind) {
        // The reason code is big-endian even though the envelope around it is
        // little-endian.
        let reason = payload
            .get(..4)
            .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        return Some(HostMessage::Terminated { reason });
    }
    if kind == msg::RUMBLE && payload.len() >= 10 {
        return Some(HostMessage::Rumble {
            controller: u16::from_le_bytes([payload[4], payload[5]]),
            low_frequency: u16::from_le_bytes([payload[6], payload[7]]),
            high_frequency: u16::from_le_bytes([payload[8], payload[9]]),
        });
    }
    Some(HostMessage::Other { kind })
}

#[cfg(test)]
mod tests {
    use super::{HostMessage, interpret};
    use crate::control::{message, msg};

    #[test]
    fn reads_a_termination_reason_as_big_endian() {
        // 0x80030023 is the graceful-exit code; the field is big-endian.
        let m = message(msg::TERMINATION, &0x8003_0023u32.to_be_bytes());
        assert_eq!(
            interpret(&m),
            Some(HostMessage::Terminated {
                reason: 0x8003_0023
            })
        );
    }

    #[test]
    fn accepts_the_other_termination_spelling() {
        let m = message(msg::TERMINATION_ALT, &0u32.to_be_bytes());
        assert!(matches!(
            interpret(&m),
            Some(HostMessage::Terminated { .. })
        ));
    }

    #[test]
    fn decodes_rumble() {
        let mut payload = vec![0u8; 4];
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0x1234u16.to_le_bytes());
        payload.extend_from_slice(&0x5678u16.to_le_bytes());
        assert_eq!(
            interpret(&message(msg::RUMBLE, &payload)),
            Some(HostMessage::Rumble {
                controller: 1,
                low_frequency: 0x1234,
                high_frequency: 0x5678,
            })
        );
    }

    #[test]
    fn unknown_messages_are_surfaced_not_swallowed() {
        // Unimplemented types are reported by kind rather than dropped.
        assert_eq!(
            interpret(&message(0x5502, &[0; 6])),
            Some(HostMessage::Other { kind: 0x5502 })
        );
        assert_eq!(interpret(&[0]), None);
    }
}
