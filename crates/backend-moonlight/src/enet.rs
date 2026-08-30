//! Driving the control channel's ENet connection.
//!
//! Hosts run a fork of ENet. Its wire structures are byte-identical to
//! upstream and its other changes are local behaviour — timers, IPv6
//! addressing, a lower default MTU (negotiated down to whatever the host
//! proposes; do not assume 1392).
//!
//! One fork change is *not* local, and a stock ENet does not interoperate
//! without matching it: the fork drops upstream's check that a datagram's
//! source address equals the peer's, identifying the peer by the header's ids
//! instead. See [`PeerAddr`] for why that matters and what it costs.
//!
//! ENet is poll-driven rather than async, so this runs on its own thread and
//! talks to the rest of the client over channels.

use crate::control::{Crypto, message, message_type, msg};
use crate::input::channel;
use gsa_core::{Error, Result};
use rusty_enet as enet;

/// Application-level keepalive period. Hosts drop a session after several
/// seconds without one; ENet's own keepalives do not count.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Poll interval. ENet needs servicing regularly to retransmit and to
/// surface received packets.
/// The longest the control loop waits with nothing arriving. Arrivals wake it
/// immediately (see the socket peek below); this only bounds how stale queued
/// *outgoing* work can get.
const SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);
/// How often the measured link round-trip is republished to the embedder.
const RTT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Commands sent per service tick before ENet is serviced again. High enough
/// that ordinary input never queues, bounded so a runaway producer cannot
/// starve the connection's own upkeep (acks, pings, retransmits).
const MAX_COMMANDS_PER_TICK: usize = 64;

/// Messages held while the control channel connects. Enough for a client that
/// announces a controller and starts streaming its state immediately; beyond
/// that the oldest input is stale anyway.
const MAX_PENDING_BEFORE_START: usize = 256;

/// Channel count to request. Reference clients open this many and place some
/// controller streams on the higher channels.
const CHANNELS: usize = 48;

/// The host's address, compared the way this protocol needs rather than the
/// way ENet compares addresses by default.
///
/// **A datagram is matched to the peer by the ENet header's peer and session
/// ids, not by where it came from.** Upstream ENet also requires the source
/// address to equal the one dialled; reference clients bundle a fork with that
/// comparison commented out, so a host answering from a *different* local
/// address than the one it was reached on is ordinary here. A machine with two
/// interfaces on one subnet, a NAT that remaps, a VPN — the reply arrives from
/// an address the client never dialled, upstream ENet discards it, and the
/// channel simply never connects while video and audio flow normally. That
/// failure is silent and looks like the host ignoring us.
///
/// Accepting any source costs little: the peer and session ids must still
/// match, the payloads are authenticated by the session key, and the host is
/// free to move — ENet re-points the peer at whatever address answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerAddr(std::net::SocketAddr);

impl enet::Address for PeerAddr {
    fn same_host(&self, _other: &Self) -> bool {
        true
    }

    fn same(&self, _other: &Self) -> bool {
        true
    }

    fn is_broadcast(&self) -> bool {
        false
    }
}

/// The control channel's socket, wrapping the standard one so the address type
/// above is the one ENet compares. Behaviour is otherwise the crate's own.
struct ControlSocket {
    socket: std::net::UdpSocket,
    /// Where the host was dialled, so an answer from anywhere else is worth
    /// one line: it is legal, and it is also the first thing to know when a
    /// host behaves unexpectedly.
    dialled: std::net::SocketAddr,
    reported_elsewhere: bool,
}

impl enet::Socket for ControlSocket {
    type Address = PeerAddr;
    type Error = std::io::Error;

    fn init(&mut self, _options: enet::SocketOptions) -> std::io::Result<()> {
        self.socket.set_nonblocking(true)?;
        self.socket.set_broadcast(true)?;
        Ok(())
    }

    fn send(&mut self, address: PeerAddr, buffer: &[u8]) -> std::io::Result<usize> {
        match self.socket.send_to(buffer, address.0) {
            Ok(sent) => Ok(sent),
            // A full send buffer is back-pressure, not a failure: ENet
            // retransmits what it must.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> std::io::Result<Option<(PeerAddr, enet::PacketReceived)>> {
        match self.socket.recv_from(buffer) {
            Ok((len, from)) => {
                if from != self.dialled && !self.reported_elsewhere {
                    self.reported_elsewhere = true;
                    tracing::info!(
                        dialled = %self.dialled,
                        answered = %from,
                        "host answers the control channel from another address"
                    );
                }
                Ok(Some((PeerAddr(from), enet::PacketReceived::Complete(len))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

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
    /// Trigger motors, separate from body rumble.
    RumbleTriggers {
        controller: u16,
        left: u16,
        right: u16,
    },
    /// The host built a motion-capable pad and wants samples at `rate_hz`.
    /// Until this arrives, motion must not be sent.
    MotionRequest {
        controller: u16,
        rate_hz: u16,
        /// 0x01 acceleration, 0x02 gyroscope.
        motion_type: u8,
    },
    /// Lightbar colour.
    SetLed { controller: u16, rgb: [u8; 3] },
    /// Trigger effects, as the game's own opaque blobs — one per trigger,
    /// forwarded verbatim to a pad that understands them.
    AdaptiveTriggers {
        controller: u16,
        /// Which triggers this applies to: 0x04 right, 0x08 left.
        affected: u8,
        left_effect: u8,
        right_effect: u8,
        left_params: [u8; 10],
        right_params: [u8; 10],
    },
    /// The control peer's round-trip time, republished about once a second.
    /// ENet measures it on its own acknowledgements — a same-clock round trip,
    /// which is what makes it a real wire figure with no clock sync anywhere.
    LinkRtt { rtt_us: u32 },
    /// A message type not acted on here, surfaced so callers can log it rather
    /// than discard it silently.
    ///
    /// The payload rides along because this is how an unimplemented feature
    /// gets implemented: the host's own bytes are the specification we are
    /// allowed to read, so a message we cannot yet parse is evidence, not
    /// noise.
    Other { kind: u16, payload: Vec<u8> },
}

impl HostMessage {
    /// A key identifying which feedback slot this message overwrites, or
    /// `None` for everything that is not repeated-state feedback.
    ///
    /// Some hosts mirror the virtual pad's whole output state continuously —
    /// measured at ~90 messages/second of identical rumble and LED values —
    /// and every one forwarded is a callback crossing on the embedder, which
    /// has been seen to knock a controller's haptics engine offline. Only a
    /// *change* is information; a repeat of the current state is not.
    fn feedback_slot(&self) -> Option<(u8, u16)> {
        match self {
            Self::Rumble { controller, .. } => Some((0, *controller)),
            Self::RumbleTriggers { controller, .. } => Some((1, *controller)),
            Self::SetLed { controller, .. } => Some((2, *controller)),
            Self::AdaptiveTriggers { controller, .. } => Some((3, *controller)),
            _ => None,
        }
    }
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
                    seat: seat(controller),
                    low: low_frequency,
                    high: high_frequency,
                },
            )),
            Self::RumbleTriggers {
                controller,
                left,
                right,
            } => Some(gsa_client_backend_api::BackendEvent::Feedback(
                gsa_client_backend_api::GamepadFeedback::TriggerRumble {
                    seat: seat(controller),
                    left,
                    right,
                },
            )),
            Self::LinkRtt { rtt_us } => {
                Some(gsa_client_backend_api::BackendEvent::LinkRtt { rtt_us })
            }
            Self::SetLed { controller, rgb } => {
                Some(gsa_client_backend_api::BackendEvent::Feedback(
                    gsa_client_backend_api::GamepadFeedback::Led {
                        seat: seat(controller),
                        rgb,
                    },
                ))
            }
            Self::AdaptiveTriggers {
                controller,
                affected,
                left_effect,
                right_effect,
                left_params,
                right_params,
            } => {
                // Only the triggers the flags name are being changed; the
                // other keeps whatever effect it already has, so sending it
                // `Off` would cancel an effect the game still wants.
                const RIGHT: u8 = 0x04;
                const LEFT: u8 = 0x08;
                let effect = |set: bool, effect: u8, params: [u8; 10]| {
                    if set {
                        gsa_client_backend_api::TriggerEffect::Raw { effect, params }
                    } else {
                        gsa_client_backend_api::TriggerEffect::Unchanged
                    }
                };
                Some(gsa_client_backend_api::BackendEvent::Feedback(
                    gsa_client_backend_api::GamepadFeedback::AdaptiveTriggers {
                        seat: seat(controller),
                        left: effect(affected & LEFT != 0, left_effect, left_params),
                        right: effect(affected & RIGHT != 0, right_effect, right_params),
                    },
                ))
            }
            Self::MotionRequest {
                controller,
                rate_hz,
                motion_type,
            } => Some(gsa_client_backend_api::BackendEvent::MotionRequested {
                seat: seat(controller),
                sensor: match motion_type {
                    0x01 => gsa_client_backend_api::MotionSensor::Accel,
                    0x02 => gsa_client_backend_api::MotionSensor::Gyro,
                    // A sensor we do not know is not a sensor we can sample.
                    _ => return None,
                },
                rate_hz,
            }),
            _ => None,
        }
    }
}

/// Controller numbers are `u16` on this wire and a `u8` seat index everywhere
/// else. There is no seat above 255, so the clamp cannot lose a real one.
fn seat(controller: u16) -> u8 {
    controller.min(u16::from(u8::MAX)) as u8
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
    Input {
        bytes: Vec<u8>,
        delivery: Delivery,
        /// Which control channel this class of input belongs on. See
        /// [`crate::input::channel`]: a host may route on this alone.
        channel: u8,
    },
    Stop,
}

/// How a message rides the control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Retransmitted until acknowledged, and delivered in order. Correct for
    /// anything stateful: a lost button-up leaves the key held.
    Reliable,
    /// Dropped rather than retransmitted. Correct only for a stream that
    /// supersedes itself — motion samples at 100 Hz, where a lost sample is
    /// replaced 10 ms later and a retransmitted one would delay every real
    /// input queued behind it.
    Unreliable,
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
    // A second handle onto the same socket, used only to *wait* for
    // readability between service passes. Sleeping a fixed interval instead
    // leaves every acknowledgement sitting unread for up to that interval —
    // which inflates the measured round trip by the polling loop rather than
    // the wire, and delays every host message the same way. Peeking does not
    // consume: the ENet host still reads the datagram itself.
    let waker = socket.try_clone().ok();
    if let Some(w) = &waker {
        let _ = w.set_read_timeout(Some(SERVICE_INTERVAL));
    }
    // `Host::new` initialises the socket (non-blocking, broadcast) itself.
    let mut host = enet::Host::new(
        ControlSocket {
            socket,
            dialled: addr,
            reported_elsewhere: false,
        },
        enet::HostSettings {
            peer_limit: 1,
            channel_limit: CHANNELS,
            ..enet::HostSettings::default()
        },
    )
    .map_err(|e| Error::Transport(format!("create control host: {e}")))?;

    // The connect data binds this ENet peer to the RTSP session; the host does
    // not identify the peer by source address.
    host.connect(PeerAddr(addr), CHANNELS, connect_data)
        .map_err(|_| Error::Transport("no ENet peer slot".into()))?;

    let mut connected = false;
    let mut started = false;
    // Messages held while the channel comes up.
    let mut pending: Vec<(Vec<u8>, Delivery, u8)> = Vec::new();
    // Kinds already reported, so an unparsed message is logged once loudly
    // rather than every time it arrives.
    let mut seen_kinds = std::collections::HashSet::new();
    // The current value of each feedback slot, for dropping repeats.
    let mut last_feedback: std::collections::HashMap<(u8, u16), HostMessage> =
        std::collections::HashMap::new();
    let mut last_ping = std::time::Instant::now();
    let mut last_rtt = std::time::Instant::now();
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
                                if let HostMessage::Other { kind, payload } = &m {
                                    // First sighting of a kind is news; after
                                    // that it is a repeat, and a host that
                                    // sends one every frame would drown the log.
                                    if seen_kinds.insert(*kind) {
                                        tracing::info!(
                                            kind = format!("0x{kind:04x}"),
                                            body = crate::hex::encode(payload),
                                            "host message not parsed yet"
                                        );
                                    } else {
                                        tracing::debug!(
                                            kind = format!("0x{kind:04x}"),
                                            body = crate::hex::encode(payload),
                                            "host message not parsed yet"
                                        );
                                    }
                                }
                                // Feedback is state, not an event stream:
                                // drop a message that repeats what the same
                                // slot already holds.
                                if let Some(slot) = m.feedback_slot() {
                                    if last_feedback.get(&slot) == Some(&m) {
                                        continue;
                                    }
                                    last_feedback.insert(slot, m.clone());
                                }
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
            send(
                &mut host,
                &mut crypto,
                &message(msg::START_A, &[]),
                Delivery::Reliable,
                channel::GENERIC,
            )?;
            send(
                &mut host,
                &mut crypto,
                &message(msg::START_B, &[1, 0, 0]),
                Delivery::Reliable,
                channel::GENERIC,
            )?;
            started = true;
        }

        if !connected && std::time::Instant::now() > deadline {
            return Err(Error::Transport(
                "control channel did not connect within 10s".into(),
            ));
        }

        // Anything queued before the channel was up is sent first, in order:
        // a send with no peer is dropped silently, so input from a client that
        // announces its controller the moment the session starts would
        // otherwise vanish with nothing to show for it.
        if started {
            for (plaintext, delivery, channel) in std::mem::take(&mut pending) {
                send(&mut host, &mut crypto, &plaintext, delivery, channel)?;
            }
        }

        // Drain the queue rather than taking one command per service tick: at
        // one per `SERVICE_INTERVAL` the channel tops out around 250 messages
        // a second, and motion alone can exceed that — the backlog would show
        // as input lag that grows for as long as the user keeps moving.
        // Bounded so a flood cannot starve ENet's own servicing.
        for _ in 0..MAX_COMMANDS_PER_TICK {
            match commands.try_recv() {
                Ok(Command::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    for peer in host.connected_peers_mut() {
                        peer.disconnect(0);
                    }
                    let _ = host.service();
                    return Ok(());
                }
                Ok(command) => {
                    let (plaintext, delivery, channel) = match command {
                        // Recovery goes on the urgent channel so it is not
                        // queued behind the input the user is still producing.
                        Command::RequestIdr => (
                            message(msg::IDR_FRAME, &[0, 0]),
                            Delivery::Reliable,
                            channel::URGENT,
                        ),
                        Command::InvalidateReferenceFrames { first, last } => {
                            let mut payload = Vec::with_capacity(24);
                            payload.extend_from_slice(&first.to_le_bytes());
                            payload.extend_from_slice(&0u32.to_le_bytes());
                            payload.extend_from_slice(&last.to_le_bytes());
                            payload.extend_from_slice(&[0u8; 12]);
                            (
                                message(msg::INVALIDATE_REF_FRAMES, &payload),
                                Delivery::Reliable,
                                channel::URGENT,
                            )
                        }
                        Command::Input {
                            bytes,
                            delivery,
                            channel,
                        } => (bytes, delivery, channel),
                        Command::Stop => unreachable!("handled above"),
                    };
                    if started {
                        send(&mut host, &mut crypto, &plaintext, delivery, channel)?;
                    } else {
                        // Bounded: a host that never completes the handshake
                        // fails on the deadline below, and until then a
                        // runaway producer must not grow this without limit.
                        if pending.len() < MAX_PENDING_BEFORE_START {
                            pending.push((plaintext, delivery, channel));
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // Publish the link's round trip about once a second. ENet keeps its
        // own smoothed estimate from acknowledgements; reading it costs
        // nothing and the receiver treats it as telemetry, not control.
        if connected && last_rtt.elapsed() >= RTT_INTERVAL {
            last_rtt = std::time::Instant::now();
            if let Some(peer) = host.connected_peers_mut().next() {
                #[allow(clippy::cast_possible_truncation)]
                let rtt_us = peer.round_trip_time().as_micros().min(u128::from(u32::MAX)) as u32;
                let _ = events.send(HostMessage::LinkRtt { rtt_us });
            }
        }

        if connected && last_ping.elapsed() >= PING_INTERVAL {
            send(
                &mut host,
                &mut crypto,
                &message(msg::PERIODIC_PING, &[4, 0, 0, 0, 0, 0, 0, 0]),
                Delivery::Reliable,
                channel::GENERIC,
            )?;
            last_ping = std::time::Instant::now();
        }

        match &waker {
            // Wake the moment a datagram lands (or after the interval, for
            // outgoing work), instead of letting it wait out a sleep.
            Some(w) => {
                let mut probe = [0u8; 1];
                let _ = w.peek(&mut probe);
            }
            None => std::thread::sleep(SERVICE_INTERVAL),
        }
    }
}

fn send<S: enet::Socket>(
    host: &mut enet::Host<S>,
    crypto: &mut Crypto,
    plaintext: &[u8],
    delivery: Delivery,
    channel: u8,
) -> Result<()> {
    let frame = crypto.seal(plaintext)?;
    // The sequence number rides in the nonce, so a dropped unreliable message
    // leaves a gap the host can decrypt across — it is not a cipher chain.
    let packet = match delivery {
        Delivery::Reliable => enet::Packet::reliable(frame.as_slice()),
        Delivery::Unreliable => enet::Packet::unreliable(frame.as_slice()),
    };
    let mut sent = false;
    for peer in host.connected_peers_mut() {
        match peer.send(channel, &packet) {
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
    // Every host→client body is read at fixed offsets with a minimum length,
    // never an exact one: one host's structs pick up C alignment padding, so
    // its messages arrive one or two bytes longer than the fields they carry.
    // Rejecting on an exact length would drop them all.
    let le16 = |at: usize| -> Option<u16> {
        payload
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    if kind == msg::RUMBLE && payload.len() >= 10 {
        // Four bytes of padding precede the fields in this one, unlike its
        // trigger counterpart below.
        return Some(HostMessage::Rumble {
            controller: le16(4)?,
            low_frequency: le16(6)?,
            high_frequency: le16(8)?,
        });
    }
    if kind == msg::RUMBLE_TRIGGERS && payload.len() >= 6 {
        return Some(HostMessage::RumbleTriggers {
            controller: le16(0)?,
            left: le16(2)?,
            right: le16(4)?,
        });
    }
    if kind == msg::MOTION_EVENT && payload.len() >= 5 {
        return Some(HostMessage::MotionRequest {
            controller: le16(0)?,
            rate_hz: le16(2)?,
            motion_type: payload[4],
        });
    }
    if kind == msg::RGB_LED && payload.len() >= 5 {
        return Some(HostMessage::SetLed {
            controller: le16(0)?,
            rgb: [payload[2], payload[3], payload[4]],
        });
    }
    if kind == msg::ADAPTIVE_TRIGGER && payload.len() >= 25 {
        let mut left_params = [0u8; 10];
        let mut right_params = [0u8; 10];
        left_params.copy_from_slice(&payload[5..15]);
        right_params.copy_from_slice(&payload[15..25]);
        return Some(HostMessage::AdaptiveTriggers {
            controller: le16(0)?,
            affected: payload[2],
            left_effect: payload[3],
            right_effect: payload[4],
            left_params,
            right_params,
        });
    }
    Some(HostMessage::Other {
        kind,
        payload: payload.to_vec(),
    })
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
        // Reported by kind *and* body: the bytes of a message we do not parse
        // yet are how the next feature gets built.
        assert_eq!(
            interpret(&message(0x0999, &[1, 2, 3, 4, 5, 6])),
            Some(HostMessage::Other {
                kind: 0x0999,
                payload: vec![1, 2, 3, 4, 5, 6]
            })
        );
        assert_eq!(interpret(&[0]), None);
    }

    /// One host's host→client structs pick up C alignment padding, so its
    /// messages arrive a byte or two longer than their fields. Reading at
    /// fixed offsets with a minimum length is what makes both hosts work;
    /// an exact-length check would drop every message from that one.
    #[test]
    fn trailing_padding_does_not_stop_a_message_parsing() {
        let mut padded = vec![1, 0, 100, 0, 0x02];
        padded.push(0);
        assert_eq!(
            interpret(&message(msg::MOTION_EVENT, &padded)),
            Some(HostMessage::MotionRequest {
                controller: 1,
                rate_hz: 100,
                motion_type: 0x02,
            })
        );
    }

    #[test]
    fn a_motion_request_is_what_turns_sampling_on() {
        let m = interpret(&message(msg::MOTION_EVENT, &[0, 0, 100, 0, 0x01]));
        assert_eq!(
            m,
            Some(HostMessage::MotionRequest {
                controller: 0,
                rate_hz: 100,
                motion_type: 0x01,
            })
        );
        // It reaches the embedder as a request to start sampling, naming the
        // sensor: a client that samples the wrong one sends numbers the host
        // will misread as the other.
        assert_eq!(
            m.unwrap().neutral(),
            Some(gsa_client_backend_api::BackendEvent::MotionRequested {
                seat: 0,
                sensor: gsa_client_backend_api::MotionSensor::Accel,
                rate_hz: 100,
            })
        );
    }

    #[test]
    fn trigger_rumble_and_body_rumble_have_different_layouts() {
        // Body rumble carries four leading pad bytes; trigger rumble does not.
        assert_eq!(
            interpret(&message(
                msg::RUMBLE,
                &[0, 0, 0, 0, 1, 0, 0x34, 0x12, 0x78, 0x56]
            )),
            Some(HostMessage::Rumble {
                controller: 1,
                low_frequency: 0x1234,
                high_frequency: 0x5678,
            })
        );
        assert_eq!(
            interpret(&message(
                msg::RUMBLE_TRIGGERS,
                &[1, 0, 0x34, 0x12, 0x78, 0x56]
            )),
            Some(HostMessage::RumbleTriggers {
                controller: 1,
                left: 0x1234,
                right: 0x5678,
            })
        );
    }

    /// The flags say which triggers changed. The other trigger keeps its
    /// current effect — reporting it as `Off` would cancel something the game
    /// still wants.
    #[test]
    fn an_unaffected_trigger_is_unchanged_rather_than_off() {
        let mut payload = vec![0, 0, 0x08, 0x25, 0x05];
        payload.extend_from_slice(&[7u8; 10]);
        payload.extend_from_slice(&[9u8; 10]);
        let neutral = interpret(&message(msg::ADAPTIVE_TRIGGER, &payload))
            .expect("parses")
            .neutral()
            .expect("is neutral feedback");
        let gsa_client_backend_api::BackendEvent::Feedback(
            gsa_client_backend_api::GamepadFeedback::AdaptiveTriggers { left, right, .. },
        ) = neutral
        else {
            panic!("wrong event");
        };
        assert_eq!(
            left,
            gsa_client_backend_api::TriggerEffect::Raw {
                effect: 0x25,
                params: [7; 10]
            }
        );
        assert_eq!(right, gsa_client_backend_api::TriggerEffect::Unchanged);
    }

    #[test]
    fn a_truncated_message_is_dropped_rather_than_read_past() {
        // One byte short of the fields it claims.
        assert!(matches!(
            interpret(&message(msg::RGB_LED, &[0, 0, 1, 2])),
            Some(HostMessage::Other { .. })
        ));
        assert_eq!(
            interpret(&message(msg::RGB_LED, &[0, 0, 1, 2, 3])),
            Some(HostMessage::SetLed {
                controller: 0,
                rgb: [1, 2, 3]
            })
        );
    }
}
