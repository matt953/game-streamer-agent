//! Embeddable streaming client core (spec 01, decision D9): connection,
//! negotiation, datagram reassembly, decode orchestration, and latency
//! stats. **No UI, no rendering, no platform decode** — the embedding app
//! (or `client-dev`) supplies a [`VideoDecoder`] and owns presentation.
//! This boundary is what makes the M2 UniFFI factoring mechanical.

mod audio;
mod decode;
mod reassembly;
pub mod stats;

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
    /// Agent-clock capture stamp (µs, wrapping) — echo to `frame_presented`.
    pub capture_ts_us: u32,
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
    /// Periodic encoder telemetry from the agent (target + emitted bitrate +
    /// manual ceiling, bits/s).
    EncodeStats {
        target_bitrate_bps: u32,
        emitted_bitrate_bps: u32,
        ceiling_bitrate_bps: u32,
        estimate_bitrate_bps: u32,
        abr_enabled: bool,
    },
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

    /// Ask the agent to change the live encode bitrate (bps). The manual quality
    /// knob; fire-and-forget over the control stream. The agent clamps the value.
    pub fn set_bitrate(&self, bitrate_bps: u32) {
        let _ = self.tx.send(C2A::SetBitrate { bitrate_bps });
    }

    /// Enable/disable server-side ABR for the session.
    pub fn set_abr(&self, enabled: bool) {
        let _ = self.tx.send(C2A::SetAbr { enabled });
    }
}

/// Fire-and-forget presentation reporter, decoupled from the frame-receive
/// loop: the embedder calls [`PresentedSink::presented`] from its display
/// path each time a frame is handed to the screen; the client folds the
/// samples into its health stats at report time.
#[derive(Debug, Clone)]
pub struct PresentedSink {
    tx: tokio::sync::mpsc::UnboundedSender<(u32, std::time::Instant)>,
}

impl PresentedSink {
    /// `capture_ts_us` is the frame's agent-clock capture stamp, echoed from
    /// the video callback. Timestamped here so queueing costs nothing.
    pub fn presented(&self, capture_ts_us: u32) {
        let _ = self.tx.send((capture_ts_us, std::time::Instant::now()));
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
    /// Frames released by the reassembler, awaiting the gate (drained one
    /// per `next_gated_frame` call).
    completed: std::collections::VecDeque<reassembly::CompletedFrame>,
    stats: LatencyStats,
    present: stats::PresentStats,
    presented_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, std::time::Instant)>,
    presented_tx: tokio::sync::mpsc::UnboundedSender<(u32, std::time::Instant)>,
    session: Option<SessionParams>,
    /// Frame id of the last frame handed to the decoder (gap detection).
    last_frame_id: Option<u32>,
    /// Last frame actually DELIVERED for decoding — recovery requests must
    /// cite a frame the decoder truly has; frames skipped while awaiting
    /// resync were never decoded and are unusable as references.
    last_delivered_id: Option<u32>,
    /// Client-clock µs of the last keyframe request (rate limiting).
    last_keyframe_request_us: u64,
    /// Client-clock µs of the last `StatsReport` sent (ABR signal, ~2 Hz).
    last_stats_report_us: u64,
    /// Pending per-packet arrival samples (seq, arrival µs) for the next
    /// `PacketFeedback` batch (~20 Hz).
    feedback_batch: Vec<(u32, u64)>,
    last_feedback_us: u64,
    /// Highest transport seq seen (NACK gap detection).
    highest_seq: Option<u32>,
    /// Recently NACKed seqs — each is requested exactly once.
    nacked: std::collections::VecDeque<u32>,
    /// True while the P-frame reference chain is broken (lost/undecodable
    /// frame); we skip P-frames until a keyframe — or an agent-announced
    /// recovery point — resyncs the decoder (spec 04).
    awaiting_idr: bool,
    /// Agent-announced first-safe frame id + 1 (0 = none), written by the
    /// control reader task on [`SessionEvent::RecoveryPoint`].
    recovery_point: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set by the embedder when its decoder rejected a delivered frame: the
    /// reference chain is broken in ways delivery tracking cannot see.
    decode_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Adaptive de-jitter (default on): early frames align to the source
    /// cadence only while measured jitter is high; late frames never wait.
    dejitter: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Recent capture-ts deltas (µs) — the source cadence.
    cadence_win: std::collections::VecDeque<u32>,
    /// Recent capture→arrival latencies (µs) — their spread is the jitter.
    jitter_win: std::collections::VecDeque<u32>,
    last_release_us: u64,
    cadence_prev_ts: Option<u32>,
    dejitter_active: bool,
    first_gate_us: Option<u64>,
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
        let presented_channel = tokio::sync::mpsc::unbounded_channel();
        let mut client = Self {
            endpoint,
            conn,
            control_send: Some(control_send),
            control_recv: Some(control_recv),
            control_tx: None,
            clock,
            clock_sync: ClockSync::default(),
            reassembler: Reassembler::new(),
            completed: std::collections::VecDeque::new(),
            stats: LatencyStats::default(),
            present: stats::PresentStats::default(),
            presented_rx: presented_channel.1,
            presented_tx: presented_channel.0,
            session: None,
            last_frame_id: None,
            last_delivered_id: None,
            last_keyframe_request_us: 0,
            last_stats_report_us: 0,
            feedback_batch: Vec::new(),
            highest_seq: None,
            nacked: std::collections::VecDeque::new(),
            last_feedback_us: 0,
            // Until the first keyframe, the decoder has no reference; skip any
            // P-frames that arrive ahead of it.
            awaiting_idr: true,
            recovery_point: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            decode_error: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dejitter: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            cadence_win: std::collections::VecDeque::new(),
            jitter_win: std::collections::VecDeque::new(),
            last_release_us: 0,
            cadence_prev_ts: None,
            dejitter_active: false,
            first_gate_us: None,
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
        let recovery_point = self.recovery_point.clone();
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
                    Ok(A2C::EncodeStats(s)) => {
                        if tx
                            .send(ControlEvent::EncodeStats {
                                target_bitrate_bps: s.target_bitrate_bps,
                                emitted_bitrate_bps: s.emitted_bitrate_bps,
                                ceiling_bitrate_bps: s.ceiling_bitrate_bps,
                                estimate_bitrate_bps: s.estimate_bitrate_bps,
                                abr_enabled: s.abr_enabled,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(A2C::SessionEvent(gsa_protocol::control::SessionEvent::RecoveryPoint {
                        first_safe_frame_id,
                    })) => {
                        recovery_point.store(
                            u64::from(first_safe_frame_id) + 1,
                            std::sync::atomic::Ordering::Release,
                        );
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
        bitrate_bps: Option<u32>,
        abr: bool,
    ) -> Result<SessionParams> {
        send_msg(
            self.ctl()?,
            &C2A::StartSession(SessionRequest {
                source,
                codec_prefs: vec![gsa_core::media::Codec::H264],
                mode,
                bitrate_bps,
                abr,
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
            if let Some(f) = self.completed.pop_front() {
                if let Some(out) = self.gate(f).await? {
                    return Ok(Some(out));
                }
                continue;
            }
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
                Some(Ok(gsa_protocol::DatagramType::Padding)) => {
                    self.record_arrival(&datagram);
                    continue;
                }
                Some(Ok(gsa_protocol::DatagramType::Audio)) => {
                    if let Some(a) = &mut self.audio {
                        a.handle(&datagram);
                    }
                    continue;
                }
                Some(Ok(gsa_protocol::DatagramType::Video)) => {
                    self.record_arrival(&datagram);
                }
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
            self.completed
                .extend(self.reassembler.push(header, payload));
        }
    }

    /// Reference-chain gate for one released frame (spec 04): on a break
    /// (lost frame), hold the last good picture and skip P-frames until a
    /// keyframe resyncs — decoding a P-frame against a stale reference
    /// corrupts output. Requests the keyframe immediately.
    async fn gate(
        &mut self,
        f: reassembly::CompletedFrame,
    ) -> Result<Option<(VideoDatagramHeader, Vec<u8>)>> {
        self.stats
            .on_frame_complete(f.data.len(), self.clock.now_us());
        // A decoder-rejected frame breaks the chain even though delivery
        // looked clean: freeze and request recovery like any gap.
        if self
            .decode_error
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            && !self.awaiting_idr
        {
            tracing::debug!("embedder reported decode error; freezing");
            self.awaiting_idr = true;
            self.request_keyframe_throttled().await?;
        }
        let is_idr = f.kind == gsa_core::media::FrameKind::Idr;
        if is_idr {
            self.awaiting_idr = false;
        }
        if let Some(last) = self.last_frame_id {
            let delta = f.frame_id.wrapping_sub(last);
            if delta == 0 || delta > u32::MAX / 2 {
                // Reassembler releases in order, so this should be
                // unreachable; if it fires, ordering is broken upstream.
                tracing::warn!(last, got = f.frame_id, "OUT-OF-ORDER frame gated");
            } else if delta != 1 && !is_idr && !self.awaiting_idr {
                tracing::debug!(gap_after = last, got = f.frame_id, "frame gap; freezing");
                self.awaiting_idr = true;
                self.request_keyframe_throttled().await?;
            }
        }

        // Frozen: an agent-announced recovery point resumes decoding without
        // a keyframe — frames from it on reference nothing we're missing.
        if self.awaiting_idr && !is_idr {
            let rp = self
                .recovery_point
                .load(std::sync::atomic::Ordering::Acquire);
            let safe = rp > 0 && {
                let first_safe = (rp - 1) as u32;
                f.frame_id.wrapping_sub(first_safe) < u32::MAX / 2
            };
            if safe {
                self.awaiting_idr = false;
            } else {
                // Skip P-frames (a broken reference is the corruption);
                // advance the id so the gap isn't re-flagged, keep asking.
                self.last_frame_id = Some(f.frame_id);
                self.request_keyframe_throttled().await?;
                return Ok(None);
            }
        }

        // The frame is consumed regardless of what the caller does with it;
        // advance so the next frame isn't misread as another gap.
        self.last_frame_id = Some(f.frame_id);
        self.last_delivered_id = Some(f.frame_id);
        let backlog = !self.completed.is_empty();
        self.dejitter_release(f.capture_ts_us, backlog).await;
        let header = VideoDatagramHeader {
            seq: 0,
            session_epoch: 0,
            frame_id: f.frame_id,
            kind: f.kind,
            chunk_index: 0,
            chunk_count: 1,
            parity_count: 0,
            frame_len: 0,
            capture_ts_us: f.capture_ts_us,
        };
        Ok(Some((header, f.data)))
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
                    self.report_stats_if_due();
                    return Ok(Some(FrameOutput {
                        frame,
                        frame_id: header.frame_id,
                        capture_ts_us: header.capture_ts_us,
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
        self.report_stats_if_due();
        Ok(Some(EncodedFrame {
            data: frame_data,
            frame_id: header.frame_id,
            keyframe: header.kind == gsa_core::media::FrameKind::Idr,
            capture_ts_us: header.capture_ts_us,
            latency_us,
        }))
    }

    /// Report client stats to the agent ~2 Hz — the ABR delay signal (spec 04).
    /// Fire-and-forget over the control writer; a no-op until it's running.
    fn report_stats_if_due(&mut self) {
        const INTERVAL_US: u64 = 500_000;
        let now = self.clock.now_us();
        if now.saturating_sub(self.last_stats_report_us) < INTERVAL_US {
            return;
        }
        if self.control_tx.is_none() {
            return;
        }
        self.last_stats_report_us = now;
        self.drain_presented();
        let p = self.present.summary();
        let s = self.stats.summary(
            self.reassembler.frames_dropped(),
            self.reassembler.frames_recovered(),
        );
        let Some(tx) = &self.control_tx else { return };
        let _ = tx.send(C2A::StatsReport(gsa_protocol::control::ClientStats {
            frames_received: s.frames_complete,
            frames_complete: s.frames_complete,
            frames_dropped_incomplete: s.frames_dropped_incomplete,
            frames_recovered: s.frames_recovered as u32,
            frames_decoded: s.frames_decoded,
            decode_us_p50: s.decode_ms_p50.map_or(0, |ms| (ms * 1000.0) as u32),
            jitter_us: 0,
            frames_presented: p.presented,
            present_fps_x100: p.fps_x100,
            low1_fps_x100: p.low1_fps_x100,
            latency_p50_us: p.latency_p50_us,
            latency_p95_us: p.latency_p95_us,
            latency_p99_us: p.latency_p99_us,
            stutters: p.stutters as u32,
            src_stutters: p.src_stutters as u32,
            freezes: p.freezes as u32,
            freeze_ms_total: p.freeze_ms_total,
            episodes: p.episodes,
            worst_episode_ms: p.worst_episode_ms,
        }));
    }

    /// Record one sequenced datagram's arrival and flush the feedback batch
    /// at ~20 Hz (or when full). Fire-and-forget like the stats report.
    fn record_arrival(&mut self, datagram: &[u8]) {
        const FEEDBACK_INTERVAL_US: u64 = 50_000;
        const MAX_BATCH: usize = 512;
        let Ok(seq) = gsa_protocol::datagram::read_seq(datagram) else {
            return;
        };
        let now = self.clock.now_us();
        self.feedback_batch.push((seq, now));
        // Gap in send order = loss (or reordering): re-request immediately.
        // One RTT beats any parity ratio, and a spurious NACK for a merely
        // reordered datagram costs one duplicate the reassembler ignores.
        if let Some(high) = self.highest_seq {
            let ahead = seq.wrapping_sub(high);
            if ahead > 1 && ahead <= 64 {
                let missing: Vec<u32> = (1..ahead)
                    .map(|i| high.wrapping_add(i))
                    .filter(|s| !self.nacked.contains(s))
                    .collect();
                if !missing.is_empty() {
                    tracing::debug!(count = missing.len(), first = missing[0], "nack sent");
                    for &m in &missing {
                        if self.nacked.len() == 512 {
                            self.nacked.pop_front();
                        }
                        self.nacked.push_back(m);
                    }
                    if let Some(tx) = &self.control_tx {
                        let _ = tx.send(C2A::Nack { seqs: missing });
                    }
                }
            }
            if (1..u32::MAX / 2).contains(&ahead) {
                self.highest_seq = Some(seq);
            }
        } else {
            self.highest_seq = Some(seq);
        }
        if self.feedback_batch.len() < MAX_BATCH
            && now.saturating_sub(self.last_feedback_us) < FEEDBACK_INTERVAL_US
        {
            return;
        }
        self.last_feedback_us = now;
        let Some(tx) = &self.control_tx else {
            self.feedback_batch.clear();
            return;
        };
        let base = self.feedback_batch.first().map_or(now, |&(_, t)| t);
        let samples = self
            .feedback_batch
            .drain(..)
            .map(|(seq, t)| (seq, t.saturating_sub(base) as u32))
            .collect();
        let _ = tx.send(C2A::PacketFeedback(gsa_protocol::control::PacketFeedback {
            base_arrival_us: base,
            samples,
        }));
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
        // With a known-good frame the agent can clean references instead of
        // resetting the world with an IDR (spec 04 rung 2).
        let msg = match self.last_delivered_id {
            Some(last_good_frame_id) => C2A::RequestRecovery { last_good_frame_id },
            None => C2A::RequestKeyframe,
        };
        if let Some(tx) = &self.control_tx {
            let _ = tx.send(msg);
            Ok(())
        } else if let Some(stream) = self.control_send.as_mut() {
            send_msg(stream, &msg).await
        } else {
            Ok(())
        }
    }

    /// Shared de-jitter switch for the embedder (default enabled).
    #[must_use]
    pub fn dejitter_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.dejitter.clone()
    }

    /// Cadence-align an early frame while jitter is high. Never delays a
    /// late frame, holds at most [`DEJITTER_MAX_US`], and is a no-op on a
    /// clean link — latency is only spent where jitter already ruined it.
    async fn dejitter_release(&mut self, capture_ts_us: u32, backlog: bool) {
        const WIN: usize = 32;
        const JITTER_ON_US: u32 = 12_000;
        const DEJITTER_MAX_US: u64 = 33_000;
        /// Startup transient (clock sync settling, burst catch-up) must not
        /// read as jitter.
        const WARMUP_US: u64 = 2_000_000;
        let now = self.clock.now_us();
        self.first_gate_us.get_or_insert(now);
        if let Some(prev) = self.cadence_prev_ts {
            let d = capture_ts_us.wrapping_sub(prev);
            if d > 0 && d < 250_000 {
                if self.cadence_win.len() == WIN {
                    self.cadence_win.pop_front();
                }
                self.cadence_win.push_back(d);
            }
        }
        self.cadence_prev_ts = Some(capture_ts_us);
        if let Some(lat) = self.clock_sync.frame_latency_us(now, capture_ts_us) {
            if self.jitter_win.len() == WIN {
                self.jitter_win.pop_front();
            }
            self.jitter_win.push_back(lat);
        }
        let ordered = |w: &std::collections::VecDeque<u32>| {
            let mut v: Vec<u32> = w.iter().copied().collect();
            v.sort_unstable();
            v
        };
        let release = 'release: {
            // Pacing is only sound at the queue head with nothing waiting:
            // holding a frame while more are already queued builds a standing
            // backlog that can never drain — the opposite of smoothing.
            if backlog
                || !self.dejitter.load(std::sync::atomic::Ordering::Relaxed)
                || self.jitter_win.len() < WIN / 2
            {
                break 'release now;
            }
            let age = now.saturating_sub(self.first_gate_us.unwrap_or(now));
            if age < WARMUP_US {
                break 'release now;
            }
            let lat = ordered(&self.jitter_win);
            let jitter = lat[lat.len() * 9 / 10] - lat[lat.len() / 10];
            let high = jitter >= JITTER_ON_US;
            if high != self.dejitter_active {
                self.dejitter_active = high;
                tracing::debug!(jitter_us = jitter, active = high, "dejitter mode");
            }
            if !high {
                break 'release now;
            }
            let cadence = if self.cadence_win.is_empty() {
                16_667
            } else {
                u64::from(ordered(&self.cadence_win)[self.cadence_win.len() / 2])
            };
            (self.last_release_us + cadence).min(now + DEJITTER_MAX_US)
        };
        if release > now {
            tokio::time::sleep(std::time::Duration::from_micros(release - now)).await;
        }
        self.last_release_us = self.clock.now_us();
    }

    /// Shared flag the embedder sets when its decoder rejects a frame; the
    /// gate treats it as a reference break and requests recovery.
    #[must_use]
    pub fn decode_error_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.decode_error.clone()
    }

    /// Handle for the embedder's display path to report presented frames.
    #[must_use]
    pub fn presented_sink(&self) -> PresentedSink {
        PresentedSink {
            tx: self.presented_tx.clone(),
        }
    }

    /// Direct form of [`PresentedSink::presented`] for harnesses that own
    /// the client.
    pub fn frame_presented(&mut self, capture_ts_us: u32) {
        let now = self.clock.now_us();
        let latency = self.clock_sync.frame_latency_us(now, capture_ts_us);
        self.present.on_presented(latency, capture_ts_us, now);
    }

    /// Fold queued presentation reports (stamped on the display thread) into
    /// the health stats.
    fn drain_presented(&mut self) {
        while let Ok((capture_ts, at)) = self.presented_rx.try_recv() {
            let now = self
                .clock
                .now_us()
                .saturating_sub(at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            let latency = self.clock_sync.frame_latency_us(now, capture_ts);
            self.present.on_presented(latency, capture_ts, now);
        }
    }

    #[must_use]
    pub fn stats(&self) -> StatsSummary {
        self.stats.summary(
            self.reassembler.frames_dropped(),
            self.reassembler.frames_recovered(),
        )
    }

    /// Presentation-side health summary (fed by [`PresentedSink`]).
    #[must_use]
    pub fn present_stats(&mut self) -> stats::PresentSummary {
        self.drain_presented();
        self.present.summary()
    }

    /// Graceful shutdown: close the connection and flush the endpoint.
    pub async fn close(self) {
        self.conn.close(0u32.into(), b"client done");
        self.endpoint.wait_idle().await;
    }
}
