//! The control channel's protocol, independent of what carries it.
//!
//! One [`ControlSession`] holds everything the reference clients do on the
//! control channel that is not transport: the AES-GCM envelope, the
//! generation-specific session-start pair, the input queue held until the
//! channel is up, the periodic ping, feedback de-duplication, and turning host
//! bytes into [`HostMessage`]s. It is driven by exactly one of two links —
//! ENet over UDP natively, the altc tunnel in a browser — through
//! [`ControlLink`] and [`drive`], so the two never disagree about the protocol.

use crate::control::{Crypto, message, message_type, msg};
use crate::input::channel;
use gsa_core::Result;
use gsa_core::runtime::MaybeSend;
use gsa_core::time::Instant;

/// Application-level keepalive period. Hosts drop a session after several
/// seconds without one; the link's own keepalives do not count.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Messages held while the control channel connects. Enough for a client that
/// announces a controller and starts streaming its state immediately; beyond
/// that the oldest input is stale anyway.
const MAX_PENDING_BEFORE_START: usize = 256;

/// How long the link may take to connect before the session is given up.
pub const CONNECT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Something the host told us over the control channel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    /// The host's own word on its virtual pad for `seat`: live, or gone. Only
    /// hosts that announce [`HostMessage::PadsReported`] send these.
    PadState { seat: u8, live: bool },
    /// What the game wrote to the virtual pad's player-light row: a five-bit
    /// mask plus the pad's own "no fade" bit, forwarded untouched.
    PlayerLights { seat: u8, mask: u8 },
    /// What the game wrote to the virtual pad's mic-mute light: 0 off,
    /// 1 solid, 2 pulsing.
    MuteLight { seat: u8, mode: u8 },
    /// This host reports pad state. Sent once when the control link comes up,
    /// so the absence of a confirmation means "not yet" on a host that speaks
    /// this, and means nothing at all on a host that does not.
    PadsReported,
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
            Self::PlayerLights { seat, .. } => Some((4, u16::from(*seat))),
            Self::MuteLight { seat, .. } => Some((5, u16::from(*seat))),
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
            Self::PadState { seat, live: true } => {
                Some(gsa_client_backend_api::BackendEvent::GamepadConnected { seat })
            }
            Self::PadState { seat, live: false } => {
                Some(gsa_client_backend_api::BackendEvent::GamepadDisconnected { seat })
            }
            // A capability announcement is for the core, not the embedder.
            Self::PadsReported => None,
            Self::PlayerLights { seat, mask } => {
                Some(gsa_client_backend_api::BackendEvent::Feedback(
                    gsa_client_backend_api::GamepadFeedback::PlayerLights { seat, mask },
                ))
            }
            Self::MuteLight { seat, mode } => Some(gsa_client_backend_api::BackendEvent::Feedback(
                gsa_client_backend_api::GamepadFeedback::MuteLight { seat, mode },
            )),
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

/// One sealed message ready for the link, with how it must travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The encrypted envelope, exactly the bytes to put on the wire.
    pub frame: Vec<u8>,
    pub delivery: Delivery,
    /// See [`crate::input::channel`]: a host may route on this alone.
    pub channel: u8,
}

/// What a link reports to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// The link is up and the host accepted the session binding.
    Connected,
    /// One frame from the host, still sealed.
    Frame(Vec<u8>),
    /// The link's own round-trip measurement, when it has one.
    Rtt { rtt_us: u32 },
    /// The link went away without a host reason.
    Disconnected,
}

/// A connected control link: sends sealed frames with the requested delivery
/// and channel, and yields what the host sends back.
///
/// Implemented natively over ENet and in the browser over the altc tunnel.
/// The link owns connection establishment; the first event it yields is
/// [`LinkEvent::Connected`].
pub trait ControlLink {
    /// Put one sealed frame on the wire.
    fn send(&mut self, out: &Outgoing)
    -> impl std::future::Future<Output = Result<()>> + MaybeSend;
    /// The next thing the link has to say; `None` once it is closed for good.
    fn recv(&mut self) -> impl std::future::Future<Output = Option<LinkEvent>> + MaybeSend;
    /// Ask the peer to close, then let the link go.
    fn close(&mut self) -> impl std::future::Future<Output = ()> + MaybeSend;
}

/// The protocol state of one control channel.
#[derive(Debug)]
pub struct ControlSession {
    crypto: Crypto,
    /// True for hosts on the encrypted-control generation (app version
    /// 7.1.431 or newer), which open a session differently; see `start_pair`.
    modern_start: bool,
    connected: bool,
    started: bool,
    /// Messages queued before the channel was up, sent first and in order.
    pending: Vec<(Vec<u8>, Delivery, u8)>,
    /// Kinds already reported, so an unparsed message is logged once loudly
    /// rather than every time it arrives.
    seen_kinds: std::collections::HashSet<u16>,
    /// The current value of each feedback slot, for dropping repeats.
    last_feedback: std::collections::HashMap<(u8, u16), HostMessage>,
    last_ping: Option<Instant>,
    /// Kinds sent at least once, so the log says what actually left.
    sent_kinds: std::collections::HashSet<u16>,
}

impl ControlSession {
    #[must_use]
    pub fn new(crypto: Crypto, modern_start: bool) -> Self {
        Self {
            crypto,
            modern_start,
            connected: false,
            started: false,
            pending: Vec::new(),
            seen_kinds: std::collections::HashSet::new(),
            last_feedback: std::collections::HashMap::new(),
            last_ping: None,
            sent_kinds: std::collections::HashSet::new(),
        }
    }

    /// Whether the link has reported itself connected.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// The link came up: the session-start pair, then whatever queued
    /// before it, in order.
    ///
    /// Media does not start until both start messages are sent; pinging the
    /// media ports alone leaves some hosts silent. The pair is
    /// generation-specific. On the encrypted-control generation the first
    /// message is a keyframe request (0x0302 with a two-byte body) — that
    /// generation has no Start A at all — and Start B carries a single zero.
    /// Older hosts take Start A (0x0305) with an empty body and a three-byte
    /// Start B. A host built to the current reference has no reason to
    /// recognise the older pair, and rejecting it costs the whole session's
    /// input while video still flows.
    pub fn on_connected(&mut self, now: Instant) -> Result<Vec<Outgoing>> {
        self.connected = true;
        let (first, first_body, start_b_body): (u16, &[u8], &[u8]) = if self.modern_start {
            (msg::IDR_FRAME, &[0, 0], &[0])
        } else {
            (msg::START_A, &[], &[1, 0, 0])
        };
        let mut out = vec![
            self.seal(
                &message(first, first_body),
                Delivery::Reliable,
                channel::GENERIC,
            )?,
            self.seal(
                &message(msg::START_B, start_b_body),
                Delivery::Reliable,
                channel::GENERIC,
            )?,
        ];
        self.started = true;
        self.last_ping = Some(now);
        // A send with no peer is dropped silently, so input from a client
        // that announces its controller the moment the session starts would
        // otherwise vanish with nothing to show for it.
        for (plaintext, delivery, ch) in std::mem::take(&mut self.pending) {
            out.push(self.seal(&plaintext, delivery, ch)?);
        }
        Ok(out)
    }

    /// One frame from the host: authenticate, decode, drop repeated feedback.
    /// `None` for frames that fail authentication (not fatal: one bad packet
    /// must not end a working session) and for repeats.
    pub fn on_frame(&mut self, frame: &[u8]) -> Option<HostMessage> {
        let plaintext = match self.crypto.open(frame) {
            Ok(plaintext) => plaintext,
            Err(e) => {
                tracing::debug!(error = %e, "control frame dropped");
                return None;
            }
        };
        let m = interpret(&plaintext)?;
        if let HostMessage::Other { kind, payload } = &m {
            // First sighting of a kind is news; after that it is a repeat,
            // and a host that sends one every frame would drown the log.
            if self.seen_kinds.insert(*kind) {
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
        // Feedback is state, not an event stream: drop a message that
        // repeats what the same slot already holds.
        if let Some(slot) = m.feedback_slot() {
            if self.last_feedback.get(&slot) == Some(&m) {
                return None;
            }
            self.last_feedback.insert(slot, m.clone());
        }
        Some(m)
    }

    /// Something the embedder wants sent. Queued until the channel is up;
    /// `Ok(None)` means queued (or dropped, once the queue is full).
    pub fn on_command(&mut self, command: Command) -> Result<Option<Outgoing>> {
        let (plaintext, delivery, ch) = match command {
            // Recovery goes on the urgent channel so it is not queued behind
            // the input the user is still producing.
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
            Command::Stop => return Ok(None),
        };
        if self.started {
            return self.seal(&plaintext, delivery, ch).map(Some);
        }
        // Bounded: a host that never completes the handshake fails on the
        // connect deadline, and until then a runaway producer must not grow
        // this without limit.
        if self.pending.len() < MAX_PENDING_BEFORE_START {
            self.pending.push((plaintext, delivery, ch));
        }
        Ok(None)
    }

    /// The periodic ping, when one is due.
    pub fn on_tick(&mut self, now: Instant) -> Result<Option<Outgoing>> {
        if !self.started {
            return Ok(None);
        }
        let due = self
            .last_ping
            .is_none_or(|last| now.duration_since(last) >= PING_INTERVAL);
        if !due {
            return Ok(None);
        }
        self.last_ping = Some(now);
        self.seal(
            &message(msg::PERIODIC_PING, &[4, 0, 0, 0, 0, 0, 0, 0]),
            Delivery::Reliable,
            channel::GENERIC,
        )
        .map(Some)
    }

    fn seal(&mut self, plaintext: &[u8], delivery: Delivery, ch: u8) -> Result<Outgoing> {
        // Say what actually leaves, once per message type. Without it a
        // session where the host ignores us is indistinguishable from one
        // where the client never sent anything.
        if let Some(kind) = message_type(plaintext)
            && self.sent_kinds.insert(kind)
        {
            tracing::info!(
                kind = format!("{kind:#06x}"),
                channel = ch,
                bytes = plaintext.len(),
                "control message sent for the first time"
            );
        }
        // The sequence number rides in the nonce, so a dropped unreliable
        // message leaves a gap the host can decrypt across.
        Ok(Outgoing {
            frame: self.crypto.seal(plaintext)?,
            delivery,
            channel: ch,
        })
    }
}

/// Run a control session over `link` until the host ends it, the embedder
/// stops it, or the link dies. Host messages go to `events`; the embedder's
/// requests arrive on `commands`.
pub async fn drive<L: ControlLink>(
    mut link: L,
    mut session: ControlSession,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: std::sync::mpsc::Sender<HostMessage>,
    pads: std::sync::Arc<dyn gsa_client_backend_api::InputSink>,
) -> Result<()> {
    let opened = Instant::now();
    loop {
        // Ping cadence bounds the wait: with nothing arriving and nothing to
        // send, the loop still wakes to keep the host's keepalive fed.
        let tick = gsa_core::runtime::sleep(std::time::Duration::from_millis(50));
        tokio::select! {
            event = link.recv() => match event {
                None | Some(LinkEvent::Disconnected) => {
                    let _ = events.send(HostMessage::Disconnected);
                    return Ok(());
                }
                Some(LinkEvent::Connected) => {
                    let _ = events.send(HostMessage::Connected);
                    for out in session.on_connected(Instant::now())? {
                        link.send(&out).await?;
                    }
                }
                Some(LinkEvent::Rtt { rtt_us }) => {
                    let _ = events.send(HostMessage::LinkRtt { rtt_us });
                }
                Some(LinkEvent::Frame(frame)) => {
                    if let Some(m) = session.on_frame(&frame) {
                        let terminated = matches!(m, HostMessage::Terminated { .. });
                        // The host's word on its pads belongs in the core's
                        // registry, so an interface reads one source rather
                        // than stitching this together itself.
                        match m {
                            HostMessage::PadsReported => pads.note_pads_reported(),
                            HostMessage::PadState { seat, live } => pads.confirm_pad(seat, live),
                            _ => {}
                        }
                        let _ = events.send(m);
                        if terminated {
                            link.close().await;
                            return Ok(());
                        }
                    }
                }
            },
            command = commands.recv() => match command {
                None | Some(Command::Stop) => {
                    link.close().await;
                    return Ok(());
                }
                Some(command) => {
                    if let Some(out) = session.on_command(command)? {
                        link.send(&out).await?;
                    }
                }
            },
            () = tick => {}
        }
        if !session.connected() && opened.elapsed() > CONNECT_DEADLINE {
            return Err(gsa_core::Error::Transport(
                "control channel did not connect within 10s".into(),
            ));
        }
        if let Some(out) = session.on_tick(Instant::now())? {
            link.send(&out).await?;
        }
    }
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
    if kind == msg::PAD_STATE && payload.len() >= 7 {
        // Magic and version first: this type is ours, and a body that does not
        // introduce itself is not one we are willing to read.
        let magic = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if magic != msg::PAD_STATE_MAGIC || payload[4] != msg::PAD_STATE_VERSION {
            return None;
        }
        return match payload[6] {
            0 => Some(HostMessage::PadState {
                seat: payload[5],
                live: false,
            }),
            1 => Some(HostMessage::PadState {
                seat: payload[5],
                live: true,
            }),
            2 => Some(HostMessage::PadsReported),
            _ => None,
        };
    }
    if kind == msg::PAD_LIGHTS && payload.len() >= 8 {
        let magic = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if magic != msg::PAD_STATE_MAGIC || payload[4] != msg::PAD_STATE_VERSION {
            return None;
        }
        return match payload[6] {
            0 => Some(HostMessage::PlayerLights {
                seat: payload[5],
                mask: payload[7],
            }),
            1 => Some(HostMessage::MuteLight {
                seat: payload[5],
                mode: payload[7],
            }),
            _ => None,
        };
    }
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
mod session_tests {
    use super::{
        Command, ControlLink, ControlSession, Delivery, HostMessage, LinkEvent, Outgoing, drive,
    };
    use crate::control::{Crypto, message, message_type, msg};
    use crate::input::channel;
    use gsa_core::time::Instant;

    const KEY: [u8; 16] = [7; 16];

    fn opened(frame: &[u8]) -> Vec<u8> {
        // A host-side view: the same key, opening what the client sealed
        // (the client seals with the client→host direction tag).
        Crypto::new(KEY, true)
            .open_as_host(frame)
            .expect("authenticates")
    }

    #[test]
    fn the_start_pair_follows_the_hosts_generation() {
        let mut modern = ControlSession::new(Crypto::new(KEY, true), true);
        let out = modern.on_connected(Instant::now()).unwrap();
        let kinds: Vec<u16> = out
            .iter()
            .map(|o| message_type(&opened(&o.frame)).unwrap())
            .collect();
        assert_eq!(kinds, vec![msg::IDR_FRAME, msg::START_B]);

        let mut legacy = ControlSession::new(Crypto::new(KEY, false), false);
        let out = legacy.on_connected(Instant::now()).unwrap();
        let kinds: Vec<u16> = out
            .iter()
            .map(|o| {
                message_type(&Crypto::new(KEY, false).open_as_host(&o.frame).unwrap()).unwrap()
            })
            .collect();
        assert_eq!(kinds, vec![msg::START_A, msg::START_B]);
    }

    #[test]
    fn input_before_the_link_is_up_is_queued_then_sent_in_order() {
        let mut session = ControlSession::new(Crypto::new(KEY, true), true);
        for n in 0..3u8 {
            let queued = session
                .on_command(Command::Input {
                    bytes: message(msg::INPUT_DATA, &[n]),
                    delivery: Delivery::Reliable,
                    channel: channel::KEYBOARD,
                })
                .unwrap();
            assert!(queued.is_none(), "nothing leaves before the start pair");
        }
        let out = session.on_connected(Instant::now()).unwrap();
        // Start pair first, then the three inputs in the order they came.
        assert_eq!(out.len(), 5);
        let bodies: Vec<u8> = out[2..].iter().map(|o| opened(&o.frame)[4]).collect();
        assert_eq!(bodies, vec![0, 1, 2]);
        assert!(out[2..].iter().all(|o| o.channel == channel::KEYBOARD));
        // Once up, input leaves immediately.
        let now = session
            .on_command(Command::Input {
                bytes: message(msg::INPUT_DATA, &[9]),
                delivery: Delivery::Unreliable,
                channel: channel::MOUSE,
            })
            .unwrap()
            .expect("sent");
        assert_eq!(now.delivery, Delivery::Unreliable);
        assert_eq!(now.channel, channel::MOUSE);
    }

    #[test]
    fn recovery_requests_take_the_urgent_channel() {
        let mut session = ControlSession::new(Crypto::new(KEY, true), true);
        session.on_connected(Instant::now()).unwrap();
        let idr = session.on_command(Command::RequestIdr).unwrap().unwrap();
        assert_eq!(idr.channel, channel::URGENT);
        assert_eq!(message_type(&opened(&idr.frame)), Some(msg::IDR_FRAME));
        let inv = session
            .on_command(Command::InvalidateReferenceFrames { first: 5, last: 9 })
            .unwrap()
            .unwrap();
        let plain = opened(&inv.frame);
        assert_eq!(message_type(&plain), Some(msg::INVALIDATE_REF_FRAMES));
        assert_eq!(&plain[4..8], &5u32.to_le_bytes());
        assert_eq!(&plain[12..16], &9u32.to_le_bytes());
    }

    #[test]
    fn the_ping_keeps_its_cadence_and_only_once_started() {
        let mut session = ControlSession::new(Crypto::new(KEY, true), true);
        let t0 = Instant::now();
        assert!(
            session.on_tick(t0).unwrap().is_none(),
            "no ping before start"
        );
        session.on_connected(t0).unwrap();
        assert!(
            session.on_tick(t0).unwrap().is_none(),
            "just pinged at start"
        );
        let later = t0 + std::time::Duration::from_millis(250);
        let ping = session.on_tick(later).unwrap().expect("a ping is due");
        assert_eq!(message_type(&opened(&ping.frame)), Some(msg::PERIODIC_PING));
        assert!(
            session.on_tick(later).unwrap().is_none(),
            "not twice in one period"
        );
    }

    #[test]
    fn repeated_feedback_is_dropped_and_termination_is_reported() {
        let mut session = ControlSession::new(Crypto::new(KEY, true), true);
        let mut host = Crypto::new(KEY, true);
        let mut rumble = vec![0u8; 4];
        rumble.extend_from_slice(&0u16.to_le_bytes());
        rumble.extend_from_slice(&100u16.to_le_bytes());
        rumble.extend_from_slice(&200u16.to_le_bytes());
        let frame = host.seal_as_host(&message(msg::RUMBLE, &rumble)).unwrap();
        assert!(matches!(
            session.on_frame(&frame),
            Some(HostMessage::Rumble { .. })
        ));
        let again = host.seal_as_host(&message(msg::RUMBLE, &rumble)).unwrap();
        assert!(
            session.on_frame(&again).is_none(),
            "same state twice is not news"
        );
        let end = host
            .seal_as_host(&message(msg::TERMINATION, &0x8003_0023u32.to_be_bytes()))
            .unwrap();
        assert_eq!(
            session.on_frame(&end),
            Some(HostMessage::Terminated {
                reason: 0x8003_0023
            })
        );
        assert!(
            session.on_frame(b"garbage").is_none(),
            "a bad frame is not fatal"
        );
    }

    /// An in-memory link: connects at once, records what was sent, feeds
    /// scripted host frames.
    struct MockLink {
        inbound: std::collections::VecDeque<LinkEvent>,
        sent: std::sync::Arc<std::sync::Mutex<Vec<Outgoing>>>,
        closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl ControlLink for MockLink {
        async fn send(&mut self, out: &Outgoing) -> gsa_core::Result<()> {
            self.sent.lock().unwrap().push(out.clone());
            Ok(())
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            match self.inbound.pop_front() {
                Some(event) => Some(event),
                // Nothing scripted: park forever, like a quiet host.
                None => std::future::pending().await,
            }
        }
        async fn close(&mut self) {
            self.closed
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// A sink for tests that drive the control loop without a pad registry.
    #[derive(Debug)]
    struct NoPads;
    impl gsa_client_backend_api::InputSink for NoPads {
        fn send(&self, _events: Vec<gsa_client_backend_api::InputEvent>) {}
    }

    #[tokio::test]
    async fn the_driver_starts_the_session_relays_the_host_and_stops_on_command() {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut host = Crypto::new(KEY, true);
        let led = host
            .seal_as_host(&message(msg::RGB_LED, &[0, 0, 1, 2, 3]))
            .unwrap();
        let link = MockLink {
            inbound: [
                LinkEvent::Connected,
                LinkEvent::Rtt { rtt_us: 1500 },
                LinkEvent::Frame(led),
            ]
            .into_iter()
            .collect(),
            sent: sent.clone(),
            closed: closed.clone(),
        };
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        cmd_tx.send(Command::RequestIdr).unwrap();
        let driver = tokio::spawn(drive(
            link,
            ControlSession::new(Crypto::new(KEY, true), true),
            cmd_rx,
            evt_tx,
            std::sync::Arc::new(NoPads),
        ));
        // Let the scripted events and the queued command flow, then stop.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        cmd_tx.send(Command::Stop).unwrap();
        driver.await.unwrap().unwrap();

        let events: Vec<HostMessage> = evt_rx.try_iter().collect();
        assert!(matches!(events[0], HostMessage::Connected));
        assert!(events.contains(&HostMessage::LinkRtt { rtt_us: 1500 }));
        assert!(events.contains(&HostMessage::SetLed {
            controller: 0,
            rgb: [1, 2, 3]
        }));
        let kinds: Vec<u16> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|o| message_type(&opened(&o.frame)).unwrap())
            .collect();
        // Start pair, then the keyframe request that was queued before the
        // link came up.
        assert_eq!(&kinds[..3], &[msg::IDR_FRAME, msg::START_B, msg::IDR_FRAME]);
        assert!(closed.load(std::sync::atomic::Ordering::Acquire));
    }
}

#[cfg(test)]
mod tests {
    use super::{HostMessage, interpret};
    use crate::control::{message, msg};

    /// The host's word on its pads is ours alone, so a body that does not
    /// introduce itself with our magic and version must be refused rather than
    /// read: that is what keeps a future upstream message on this type from
    /// being silently misparsed as a pad state.
    #[test]
    fn pad_state_is_read_only_when_it_identifies_itself() {
        let body = |magic: u32, version: u8, seat: u8, state: u8| {
            let mut b = magic.to_be_bytes().to_vec();
            b.extend_from_slice(&[version, seat, state]);
            message(msg::PAD_STATE, &b)
        };
        assert_eq!(
            interpret(&body(msg::PAD_STATE_MAGIC, msg::PAD_STATE_VERSION, 1, 1)),
            Some(HostMessage::PadState {
                seat: 1,
                live: true
            })
        );
        assert_eq!(
            interpret(&body(msg::PAD_STATE_MAGIC, msg::PAD_STATE_VERSION, 1, 0)),
            Some(HostMessage::PadState {
                seat: 1,
                live: false
            })
        );
        assert_eq!(
            interpret(&body(msg::PAD_STATE_MAGIC, msg::PAD_STATE_VERSION, 0xff, 2)),
            Some(HostMessage::PadsReported)
        );
        // Someone else's message that happens to land on this type.
        assert_eq!(interpret(&body(0xdead_beef, 1, 1, 1)), None);
        // A body layout from a later version we have not been taught.
        assert_eq!(interpret(&body(msg::PAD_STATE_MAGIC, 9, 1, 1)), None);
        // A state we do not know is not a state we will guess at.
        assert_eq!(
            interpret(&body(msg::PAD_STATE_MAGIC, msg::PAD_STATE_VERSION, 1, 7)),
            None
        );
    }

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
