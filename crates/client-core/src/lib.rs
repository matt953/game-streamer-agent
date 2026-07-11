//! Embeddable streaming client core (spec 01, decision D9): connection,
//! negotiation, datagram reassembly, decode orchestration, and latency
//! stats. **No UI, no rendering, no platform decode** — the embedding app
//! (or `client-dev`) supplies a [`VideoDecoder`] and owns presentation.
//! This boundary is what makes the M2 UniFFI factoring mechanical.

mod audio;
mod decode;
mod reassembly;
mod stats;

pub use decode::{DecodedFrame, PixelOrder, VideoDecoder};
pub use gsa_protocol::control::{SourceInfo, SourceKind};
pub use gsa_protocol::input::{GamepadInput, InputEvent, MouseButton, MouseMove};
pub use reassembly::Reassembler;
pub use stats::{ClockSync, LatencyStats, StatsSummary};

use gsa_core::media::VideoMode;
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_protocol::PROTO_VERSION;
use gsa_protocol::control::{
    A2C, C2A, DecodeCaps, Hello, Notification, SessionParams, SessionRequest,
};
use gsa_protocol::datagram::VideoDatagramHeader;
use gsa_protocol::grant::Scope;
use gsa_protocol::pairing::{PairResponse, PairResult};
use gsa_transport::{
    ClientPairing, Identity, client_connect_anonymous, client_connect_pinned, recv_msg, send_msg,
};

pub use gsa_transport::Identity as ClientIdentity;

/// How the client authenticates the agent for a streaming connection.
#[derive(Debug)]
pub enum ServerAuth<'a> {
    /// Dev/e2e only: accept any agent cert and present no client cert.
    Open,
    /// Pinned mutual TLS: verify the agent against `agent_pin` and present
    /// `identity` (whose fingerprint the agent pinned at pairing).
    Pinned {
        agent_pin: &'a str,
        identity: &'a Identity,
    },
}

/// The outcome of [`pair`]: the agent's pin (to pin it on future connects)
/// and the scope it granted.
#[derive(Debug, Clone)]
pub struct PairedAgent {
    pub agent_pin: String,
    pub scope: Scope,
}

/// Pair with an agent: run the SPAKE2 exchange over an anonymous connection
/// (the pairing `code` is the shared secret) and return the agent's pin +
/// granted scope. `identity` is the client's persistent identity; its
/// fingerprint becomes this peer's pin in the agent's store.
pub async fn pair(
    addr: std::net::SocketAddr,
    code: &str,
    identity: &Identity,
    name: &str,
    requested_scope: Scope,
) -> Result<PairedAgent> {
    let (endpoint, conn) = client_connect_anonymous(addr).await?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Transport(format!("open pairing stream: {e}")))?;

    let (client, hello) = ClientPairing::start(
        code,
        identity.fingerprint(),
        name.to_string(),
        requested_scope,
    );
    send_msg(&mut send, &hello).await?;
    let response: PairResponse = recv_msg(&mut recv).await?;
    let (confirmed, confirm) = client.confirm(&response)?;
    send_msg(&mut send, &confirm).await?;
    let result: PairResult = recv_msg(&mut recv).await?;
    let (agent_pin, scope) = confirmed.finish(result)?;

    conn.close(0u32.into(), b"paired");
    endpoint.wait_idle().await;
    Ok(PairedAgent { agent_pin, scope })
}

/// One decoded frame plus its measurements, handed to the embedder.
#[derive(Debug)]
pub struct FrameOutput {
    pub frame: DecodedFrame,
    pub frame_id: u32,
    /// Estimated glass-to-glass-so-far: agent capture → decoded on client.
    pub latency_us: Option<u32>,
    pub decode_us: u32,
}

/// One complete encoded H.264 access unit (Annex-B) plus metadata, for
/// embedders that decode on the platform (VideoToolbox / MediaCodec) rather
/// than through a [`VideoDecoder`]. An IDR carries its own SPS/PPS.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub frame_id: u32,
    /// IDR (carries parameter sets); the embedder builds its format description.
    pub keyframe: bool,
    /// Agent-clock capture timestamp (wire, truncated to u32 µs).
    pub capture_ts_us: u32,
    /// Estimated capture→received latency (µs); decode happens app-side.
    pub latency_us: Option<u32>,
}

/// A user-facing event pushed by the agent over the control stream, for the
/// embedder to surface (a toast, etc.). Mirrors the wire [`Notification`] but is
/// the client-core-facing type, so embedders don't depend on the protocol crate.
/// Grow this alongside `Notification` as new notifications are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// The host confirmed its virtual pad for `seat` is plugged in (input live).
    GamepadConnected { seat: u8 },
    /// The host's virtual pad for `seat` was unplugged.
    GamepadDisconnected { seat: u8 },
}

/// Fire-and-forget input sink, decoupled from the frame-receive loop.
/// Sync `send` (safe to call from a UI event loop); a background task on the
/// client's runtime writes messages to the control stream in order.
#[derive(Debug, Clone)]
pub struct InputSender {
    tx: tokio::sync::mpsc::UnboundedSender<C2A>,
}

impl InputSender {
    pub fn send(&self, events: Vec<gsa_protocol::input::InputEvent>) {
        if !events.is_empty() {
            let _ = self.tx.send(C2A::InputBatch(events));
        }
    }
}

pub struct Client {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    control_send: Option<quinn::SendStream>,
    /// `None` once [`Client::take_control_events`] moves it into a reader task.
    control_recv: Option<quinn::RecvStream>,
    /// Present once the background control writer is running (windowed
    /// client); `recv_frame` sends keyframe requests through it.
    control_tx: Option<tokio::sync::mpsc::UnboundedSender<C2A>>,
    clock: MediaClock,
    clock_sync: ClockSync,
    reassembler: Reassembler,
    stats: LatencyStats,
    session: Option<SessionParams>,
    /// Frame id of the last frame handed to the decoder (gap detection).
    last_frame_id: Option<u32>,
    /// Client-clock µs of the last keyframe request (rate limiting).
    last_keyframe_request_us: u64,
    /// True while the P-frame reference chain is broken (lost/undecodable
    /// frame); we skip P-frames until a keyframe resyncs the decoder (spec 04).
    awaiting_idr: bool,
    /// Audio receive+decode, present once the embedder takes the audio output
    /// ([`Client::take_audio_output`]); `None` means audio datagrams are dropped.
    audio: Option<audio::AudioReceive>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("session", &self.session)
            .finish()
    }
}

impl Client {
    /// Connect, exchange hellos, and estimate the agent clock offset.
    /// `max_h264_profile` is the richest profile the embedder's decoder can
    /// handle — the host encodes at or below it (spec 03). `decode_codecs` are
    /// the codecs the embedder can actually decode (must be non-empty and
    /// include a fallback the host is sure to support, i.e. H.264); the agent
    /// picks the negotiated codec from these ([`Client::negotiated_codec`]).
    pub async fn connect(
        addr: std::net::SocketAddr,
        client_name: &str,
        max_h264_profile: gsa_core::media::H264Profile,
        decode_codecs: &[gsa_core::media::Codec],
        auth: ServerAuth<'_>,
    ) -> Result<Self> {
        let (endpoint, conn) = match auth {
            ServerAuth::Open => client_connect_anonymous(addr).await?,
            ServerAuth::Pinned {
                agent_pin,
                identity,
            } => client_connect_pinned(addr, agent_pin, identity).await?,
        };
        let (mut control_send, mut control_recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Transport(format!("open control stream: {e}")))?;

        send_msg(
            &mut control_send,
            &C2A::Hello(Hello {
                proto: PROTO_VERSION,
                client_name: client_name.to_string(),
                decode_caps: DecodeCaps {
                    codecs: decode_codecs.to_vec(),
                    max_h264_profile,
                },
            }),
        )
        .await?;
        match recv_msg::<A2C>(&mut control_recv).await? {
            A2C::HelloAck(ack) if ack.proto == PROTO_VERSION => {
                tracing::info!(agent = ack.agent_name, "connected");
            }
            A2C::HelloAck(ack) => {
                return Err(Error::Protocol(
                    gsa_core::error::ProtocolError::UnsupportedVersion(ack.proto),
                ));
            }
            A2C::Error(e) => return Err(Error::Session(e.message)),
            other => return Err(Error::Session(format!("unexpected reply: {other:?}"))),
        }

        let clock = MediaClock::new();
        let mut client = Self {
            endpoint,
            conn,
            control_send: Some(control_send),
            control_recv: Some(control_recv),
            control_tx: None,
            clock,
            clock_sync: ClockSync::default(),
            reassembler: Reassembler::new(),
            stats: LatencyStats::default(),
            session: None,
            last_frame_id: None,
            last_keyframe_request_us: 0,
            // Until the first keyframe, the decoder has no reference; skip any
            // P-frames that arrive ahead of it.
            awaiting_idr: true,
            audio: None,
        };
        client.sync_clock(5).await?;
        Ok(client)
    }

    /// Ping/pong `rounds` times to estimate the agent-clock offset (spec 04).
    async fn sync_clock(&mut self, rounds: u32) -> Result<()> {
        for _ in 0..rounds {
            let sent = self.clock.now_us();
            send_msg(self.ctl()?, &C2A::Ping { client_ts_us: sent }).await?;
            match recv_msg::<A2C>(self.ctl_recv()?).await? {
                A2C::Pong {
                    client_ts_us,
                    agent_ts_us,
                } if client_ts_us == sent => {
                    let now = self.clock.now_us();
                    self.clock_sync.record(sent, now, agent_ts_us);
                }
                A2C::Pong { .. } => continue, // stale pong; ignore
                other => return Err(Error::Session(format!("expected pong, got {other:?}"))),
            }
        }
        tracing::debug!(
            offset_us = self.clock_sync.offset_us(),
            "clock sync complete"
        );
        Ok(())
    }

    fn ctl(&mut self) -> Result<&mut quinn::SendStream> {
        self.control_send
            .as_mut()
            .ok_or_else(|| Error::Session("control stream moved to input sender".into()))
    }

    fn ctl_recv(&mut self) -> Result<&mut quinn::RecvStream> {
        self.control_recv
            .as_mut()
            .ok_or_else(|| Error::Session("control stream moved to event reader".into()))
    }

    /// Move the control recv-stream into a background reader task and return a
    /// channel of [`ControlEvent`]s (agent-pushed notifications) for the
    /// embedder to surface. Call after `start_session`; afterwards the client
    /// can no longer read control replies (it only receives frames). `None` if
    /// already taken. The receiver is tokio's so callers can `select!`/`try_recv`
    /// it on their own runtime; it closes when the connection ends.
    pub fn take_control_events(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<ControlEvent>> {
        let mut recv = self.control_recv.take()?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match recv_msg::<A2C>(&mut recv).await {
                    Ok(A2C::Notification(n)) => {
                        let event = match n {
                            Notification::GamepadConnected { seat } => {
                                ControlEvent::GamepadConnected { seat }
                            }
                            Notification::GamepadDisconnected { seat } => {
                                ControlEvent::GamepadDisconnected { seat }
                            }
                            // Unknown future notification: ignore, stay reading.
                            _ => continue,
                        };
                        if tx.send(event).is_err() {
                            break; // embedder dropped the receiver
                        }
                    }
                    // Other A2C during streaming (SessionEvent, stray replies):
                    // nothing acts on them yet, so drain and continue.
                    Ok(_) => continue,
                    Err(_) => break, // control stream closed → connection ending
                }
            }
        });
        Some(rx)
    }

    /// Move the control send-stream into a background writer task and return
    /// a sync [`InputSender`] for a UI thread. Call after `start_session`;
    /// the client can no longer send control messages afterward (it only
    /// receives frames + control replies).
    pub fn take_input_sender(&mut self) -> Option<InputSender> {
        let mut stream = self.control_send.take()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<C2A>();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if send_msg(&mut stream, &msg).await.is_err() {
                    break;
                }
            }
        });
        // Keep a clone so recv_frame can send keyframe requests too.
        self.control_tx = Some(tx.clone());
        Some(InputSender { tx })
    }

    /// Take the audio output channel — interleaved-i16 PCM frames for the
    /// embedder to play. Enables audio decode (until called, audio datagrams
    /// are dropped). Call once.
    pub fn take_audio_output(&mut self) -> Result<std::sync::mpsc::Receiver<Vec<i16>>> {
        if self.audio.is_some() {
            return Err(Error::Session("audio output already taken".into()));
        }
        let (recv, rx) = audio::AudioReceive::new()?;
        self.audio = Some(recv);
        Ok(rx)
    }

    pub async fn list_sources(&mut self) -> Result<Vec<SourceInfo>> {
        send_msg(self.ctl()?, &C2A::ListSources).await?;
        match recv_msg::<A2C>(self.ctl_recv()?).await? {
            A2C::Sources(s) => Ok(s),
            A2C::Error(e) => Err(Error::Session(e.message)),
            other => Err(Error::Session(format!("expected sources, got {other:?}"))),
        }
    }

    pub async fn start_session(
        &mut self,
        source: gsa_core::id::SourceId,
        mode: Option<VideoMode>,
    ) -> Result<SessionParams> {
        send_msg(
            self.ctl()?,
            &C2A::StartSession(SessionRequest {
                source,
                codec_prefs: vec![gsa_core::media::Codec::H264],
                mode,
            }),
        )
        .await?;
        match recv_msg::<A2C>(self.ctl_recv()?).await? {
            A2C::SessionStarted(params) => {
                self.session = Some(params.clone());
                Ok(params)
            }
            A2C::Error(e) => Err(Error::Session(e.message)),
            other => Err(Error::Session(format!(
                "expected session start, got {other:?}"
            ))),
        }
    }

    /// The codec the agent negotiated for the active session (from
    /// `SessionStarted`), or `None` before `start_session`. The embedder
    /// configures its decoder from this.
    #[must_use]
    pub fn negotiated_codec(&self) -> Option<gsa_core::media::Codec> {
        self.session.as_ref().map(|p| p.codec)
    }

    /// Receive datagrams until a video frame passes the loss-recovery gate,
    /// returning its header and reassembled access unit. Audio decodes+plays as
    /// a side effect. `None` when the connection closes. Shared by `recv_frame`
    /// (decode path) and `recv_encoded` (embedder passthrough path).
    async fn next_gated_frame(&mut self) -> Result<Option<(VideoDatagramHeader, Vec<u8>)>> {
        loop {
            let datagram = match self.conn.read_datagram().await {
                Ok(d) => d,
                Err(quinn::ConnectionError::ApplicationClosed(_))
                | Err(quinn::ConnectionError::LocallyClosed) => return Ok(None),
                Err(e) => return Err(Error::Transport(format!("read datagram: {e}"))),
            };
            // Route by datagram type: audio is decoded+played as a side effect;
            // this only returns on a completed video frame.
            match datagram
                .first()
                .copied()
                .map(gsa_protocol::DatagramType::from_wire)
            {
                Some(Ok(gsa_protocol::DatagramType::Audio)) => {
                    if let Some(a) = &mut self.audio {
                        a.handle(&datagram);
                    }
                    continue;
                }
                Some(Ok(gsa_protocol::DatagramType::Video)) => {}
                _ => {
                    tracing::warn!("unknown datagram dropped");
                    continue;
                }
            }
            let (header, payload) = match VideoDatagramHeader::parse(&datagram) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "bad datagram dropped");
                    continue;
                }
            };
            let Some(frame_data) = self.reassembler.push(header, payload) else {
                continue;
            };
            self.stats.on_frame_complete();

            // Loss recovery (spec 04): on a reference-chain break (lost frame),
            // hold the last good frame and skip P-frames until a keyframe
            // resyncs — decoding a P-frame against a stale reference corrupts
            // output. Request the keyframe immediately.
            let is_idr = header.kind == gsa_core::media::FrameKind::Idr;
            if is_idr {
                self.awaiting_idr = false;
            }
            if let Some(last) = self.last_frame_id {
                let delta = header.frame_id.wrapping_sub(last);
                if delta == 0 || delta > u32::MAX / 2 {
                    // Reassembler drops stale frames, so this should be
                    // unreachable; if it fires, ordering is broken upstream.
                    tracing::warn!(last, got = header.frame_id, "OUT-OF-ORDER frame gated");
                } else if delta != 1 && !is_idr && !self.awaiting_idr {
                    tracing::debug!(
                        gap_after = last,
                        got = header.frame_id,
                        "frame gap; freezing"
                    );
                    self.awaiting_idr = true;
                    self.request_keyframe_throttled().await?;
                }
            }

            // Frozen: skip P-frames (a broken reference is the corruption).
            // Advance the id so we don't re-flag the gap; keep asking for a key.
            if self.awaiting_idr && !is_idr {
                self.last_frame_id = Some(header.frame_id);
                self.request_keyframe_throttled().await?;
                continue;
            }

            // The frame is consumed regardless of what the caller does with it;
            // advance so the next frame isn't misread as another gap.
            self.last_frame_id = Some(header.frame_id);
            return Ok(Some((header, frame_data)));
        }
    }

    /// Receive datagrams until the next complete frame decodes.
    /// Returns `None` when the connection closes.
    pub async fn recv_frame(
        &mut self,
        decoder: &mut dyn VideoDecoder,
    ) -> Result<Option<FrameOutput>> {
        loop {
            let Some((header, frame_data)) = self.next_gated_frame().await? else {
                return Ok(None);
            };
            let decode_start = self.clock.now_us();
            match decoder.decode(&frame_data) {
                Ok(Some(frame)) => {
                    let now = self.clock.now_us();
                    let decode_us = (now - decode_start) as u32;
                    let latency_us = self.clock_sync.frame_latency_us(now, header.capture_ts_us);
                    self.stats.on_frame_decoded(latency_us, decode_us);
                    return Ok(Some(FrameOutput {
                        frame,
                        frame_id: header.frame_id,
                        latency_us,
                        decode_us,
                    }));
                }
                Ok(None) => {
                    // Decoder accepted the data but produced no frame
                    // (parameter sets / buffering).
                }
                Err(e) => {
                    // Undecodable (loss-damaged) frame: never fatal. Freeze and
                    // request a healing keyframe.
                    tracing::debug!(error = %e, "decode error; freezing until keyframe");
                    self.awaiting_idr = true;
                    self.request_keyframe_throttled().await?;
                }
            }
        }
    }

    /// Receive the next complete **encoded** access unit plus metadata, for
    /// embedders that decode on the platform (VideoToolbox / MediaCodec). Same
    /// loss-recovery gate as `recv_frame`; audio routes as a side effect.
    /// `None` when the connection closes.
    pub async fn recv_encoded(&mut self) -> Result<Option<EncodedFrame>> {
        let Some((header, frame_data)) = self.next_gated_frame().await? else {
            return Ok(None);
        };
        let now = self.clock.now_us();
        let latency_us = self.clock_sync.frame_latency_us(now, header.capture_ts_us);
        // Decode happens app-side; record it as zero in the stats window.
        self.stats.on_frame_decoded(latency_us, 0);
        Ok(Some(EncodedFrame {
            data: frame_data,
            frame_id: header.frame_id,
            keyframe: header.kind == gsa_core::media::FrameKind::Idr,
            capture_ts_us: header.capture_ts_us,
            latency_us,
        }))
    }

    /// Request a healing keyframe, rate-limited so a burst of gaps/errors
    /// doesn't spam the agent (one keyframe fixes them all).
    async fn request_keyframe_throttled(&mut self) -> Result<()> {
        const MIN_INTERVAL_US: u64 = 250_000;
        let now = self.clock.now_us();
        if now.saturating_sub(self.last_keyframe_request_us) < MIN_INTERVAL_US {
            return Ok(());
        }
        self.last_keyframe_request_us = now;
        self.send_keyframe_request().await
    }

    async fn send_keyframe_request(&mut self) -> Result<()> {
        if let Some(tx) = &self.control_tx {
            let _ = tx.send(C2A::RequestKeyframe);
            Ok(())
        } else if let Some(stream) = self.control_send.as_mut() {
            send_msg(stream, &C2A::RequestKeyframe).await
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub fn stats(&self) -> StatsSummary {
        self.stats.summary(self.reassembler.frames_dropped())
    }

    /// Graceful shutdown: close the connection and flush the endpoint.
    pub async fn close(self) {
        self.conn.close(0u32.into(), b"client done");
        self.endpoint.wait_idle().await;
    }
}
