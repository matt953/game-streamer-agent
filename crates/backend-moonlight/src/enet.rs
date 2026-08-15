//! Driving the control channel's ENet connection.
//!
//! Hosts in this family run a fork of ENet, but the fork's changes are all
//! local behaviour — timers, IPv6 addressing, a lower default MTU — and its
//! wire structures are byte-identical to upstream, so a stock Rust ENet
//! interoperates. The MTU is negotiated down to whatever the host proposes,
//! so nothing here should assume 1392.
//!
//! ENet is a poll-driven library rather than an async one, so this runs on
//! its own thread and talks to the rest of the client over channels.

use crate::control::{Crypto, message, message_type, msg};
use gsa_core::{Error, Result};
use rusty_enet as enet;

/// How often to tell the host we are still here. Hosts drop a session after
/// several seconds without one, and ENet's own keepalives do not count.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Poll interval. ENet needs servicing regularly to retransmit and to
/// surface received packets.
const SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);

/// Channel count to request. Reference clients open many and place some
/// controller streams on the higher ones.
const CHANNELS: usize = 48;

/// Something the host told us over the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// The control channel is up and the host accepted our session binding.
    /// Reported rather than inferred: everything else depends on it, and a
    /// silent failure to connect looks exactly like a quiet host.
    Connected,
    /// The host said the session is over, with its own reason code.
    Terminated { reason: u32 },
    /// The ENet peer went away without the host saying why — a timeout or a
    /// reset. Distinct from `Terminated` because the causes and the fixes are
    /// completely different.
    Disconnected,
    Rumble {
        controller: u16,
        low_frequency: u16,
        high_frequency: u16,
    },
    /// Anything we do not act on yet, kept so callers can log it rather than
    /// silently discarding host behaviour we have not implemented.
    Other { kind: u16 },
}

impl HostMessage {
    /// The backend-neutral form, for embedders that should not care which
    /// protocol produced it. `None` for messages that carry no meaning
    /// outside this backend (connection lifecycle, unimplemented features).
    #[must_use]
    pub fn neutral(&self) -> Option<gsa_client_backend_api::BackendEvent> {
        match *self {
            Self::Rumble {
                controller,
                low_frequency,
                high_frequency,
            } => Some(gsa_client_backend_api::BackendEvent::Rumble {
                // Controller numbers are u16 on this wire and a seat index
                // everywhere else; pads past 255 do not exist.
                seat: controller.min(u16::from(u8::MAX)) as u8,
                low: low_frequency,
                high: high_frequency,
            }),
            _ => None,
        }
    }
}

/// What the caller can ask the control channel to send.
#[derive(Debug, Clone)]
pub enum Command {
    /// Ask for a fresh keyframe — the blunt recovery instrument.
    RequestIdr,
    /// Ask the host to invalidate references in a frame range instead, which
    /// costs far less bitrate on a link that is already struggling.
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

    // The connect data binds this ENet peer to our RTSP session, so the host
    // does not have to identify us by source address.
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
                        // A frame we cannot authenticate is not fatal on its
                        // own — log it and keep the session rather than
                        // dropping a working stream over one bad packet.
                        Err(e) => tracing::debug!(error = %e, "control frame dropped"),
                    }
                }
            }
        }

        if connected && !started {
            // These two are what actually make a host begin sending media;
            // pinging the media ports alone leaves some hosts silent.
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
            // Silently dropping these would turn a dead control channel into
            // a mystery: the session simply stops responding.
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
        // The reason is big-endian here even though the envelope around it
        // is little-endian.
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
        // 0x80030023 is the graceful-exit code; parsing it little-endian
        // would report a nonsense reason to the user.
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
        // Hosts send controller features we have not wired up; reporting the
        // type keeps them visible instead of silently dropped.
        assert_eq!(
            interpret(&message(0x5502, &[0; 6])),
            Some(HostMessage::Other { kind: 0x5502 })
        );
        assert_eq!(interpret(&[0]), None);
    }
}
