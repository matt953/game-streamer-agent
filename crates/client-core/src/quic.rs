//! The native protocol's QUIC client: pairing over SPAKE2, the control
//! stream, datagram media receive and reassembly. Everything here needs
//! sockets and a tokio runtime; the transport-neutral engine lives in the
//! crate root and builds without it.

use crate::{
    ClockSync, ControlEvent, EncodedFrame, FrameOutput, LatencySummary, PresentedSink, Reassembler,
    SourceInfo, StatsSummary, StreamSession, VideoDecoder, audio, stats,
};
use gsa_client_backend_api::{
    BackendFrame, CaptureClock, PadCaps, RecoverySink, SessionCaps, SessionKnobs,
};

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

impl gsa_client_backend_api::InputSink for InputSender {
    fn send(&self, events: Vec<gsa_protocol::input::InputEvent>) {
        InputSender::send(self, events);
    }
}

impl SessionKnobs for InputSender {
    fn caps(&self) -> SessionCaps {
        SessionCaps {
            live_bitrate: true,
            server_abr: true,
            reference_invalidation: true,
            // The agent stamps frames with its own capture clock and we keep
            // an offset estimate, so latency here is true glass-to-glass.
            capture_clock: CaptureClock::HostSynced,
            // Rumble rides the control stream and motion has its own input
            // event (spec 07). The richer pad features are not on the wire
            // yet, so they are not claimed here.
            pads: PadCaps::RUMBLE | PadCaps::MOTION,
        }
    }

    fn set_bitrate(&self, bitrate_bps: u32) {
        InputSender::set_bitrate(self, bitrate_bps);
    }

    fn set_abr(&self, enabled: bool) {
        InputSender::set_abr(self, enabled);
    }
}

/// The gsa backend's half of the recovery seam: reference invalidation over
/// the control stream, which the agent answers with a clean recovery point
/// rather than a full IDR when it can.
#[derive(Debug)]
struct GsaRecovery {
    control_tx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<C2A>>>>,
}

impl GsaRecovery {
    fn send(&self, msg: C2A) {
        if let Some(tx) = self.control_tx.lock().expect("control tx").as_ref() {
            let _ = tx.send(msg);
        }
    }
}

impl RecoverySink for GsaRecovery {
    fn request_keyframe(&self) {
        self.send(C2A::RequestKeyframe);
    }

    fn request_recovery(&self, last_good_frame_id: u32) {
        self.send(C2A::RequestRecovery { last_good_frame_id });
    }
}

/// Fire-and-forget presentation reporter, decoupled from the frame-receive
/// loop: the embedder calls [`PresentedSink::presented`] from its display
/// path each time a frame is handed to the screen; the client folds the
/// samples into its health stats at report time.
pub struct Client {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    control_send: Option<quinn::SendStream>,
    /// `None` once [`Client::take_control_events`] moves it into a reader task.
    control_recv: Option<quinn::RecvStream>,
    /// Set once the background control writer is running (windowed client);
    /// shared with the receive task, which sends NACKs and feedback through it.
    control_tx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<C2A>>>>,
    clock: MediaClock,
    /// Moves into the receive task when it spawns.
    reassembler: Option<Reassembler>,
    /// Handed to the receive task when it spawns; the matching receiver lives
    /// in [`StreamSession`].
    frames_tx: Option<tokio::sync::mpsc::UnboundedSender<BackendFrame>>,
    /// Everything that happens after a frame is whole — the gate, de-jitter,
    /// and health accounting shared with every other backend (spec 16).
    stream: StreamSession,
    /// Reassembler drop/recovery counters, mirrored out of the receive task.
    reassembly_dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reassembly_recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    session: Option<SessionParams>,
    /// Client-clock µs of the last `StatsReport` sent (ABR signal, ~2 Hz).
    last_stats_report_us: u64,
    /// Audio receive+decode, set by [`Client::take_audio_output`] and read by
    /// the receive task; `None` means audio datagrams are dropped.
    audio: std::sync::Arc<std::sync::Mutex<Option<audio::AudioReceive>>>,
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
        let control_tx = std::sync::Arc::new(std::sync::Mutex::new(None));
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let recovered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        // The channel is made here rather than when the receive task spawns,
        // so the embedder can take handles (present sink, de-jitter switch)
        // before the first frame arrives.
        let (frames_tx, frames_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut client = Self {
            endpoint,
            conn,
            control_send: Some(control_send),
            control_recv: Some(control_recv),
            stream: StreamSession::new(
                frames_rx,
                std::sync::Arc::new(GsaRecovery {
                    control_tx: control_tx.clone(),
                }),
                clock.clone(),
                ClockSync::default(),
                dropped.clone(),
                recovered.clone(),
            ),
            control_tx,
            clock,
            reassembler: Some(Reassembler::new()),
            frames_tx: Some(frames_tx),
            reassembly_dropped: dropped,
            reassembly_recovered: recovered,
            session: None,
            last_stats_report_us: 0,
            audio: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
                    self.stream.clock_sync_mut().record(sent, now, agent_ts_us);
                    // The same exchange is a measured round trip — the wire
                    // stage of the unified latency chain.
                    #[allow(clippy::cast_possible_truncation)]
                    self.stream.on_link_rtt((now - sent) as u32);
                }
                A2C::Pong { .. } => continue, // stale pong; ignore
                other => return Err(Error::Session(format!("expected pong, got {other:?}"))),
            }
        }
        tracing::debug!(
            offset_us = self.stream.clock_sync_mut().offset_us(),
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
        let recovery_point = self.stream.recovery_point();
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
        self.ensure_control_writer();
        let tx = self.control_tx.lock().expect("control tx").clone()?;
        Some(InputSender { tx })
    }

    /// Move the control send-stream into a background writer task, once.
    ///
    /// Everything that talks to the agent mid-stream — input, NACKs, packet
    /// feedback, stats reports, recovery requests — goes through this one
    /// writer, so those paths are plain synchronous sends. Called before the
    /// first frame as well as by [`Client::take_input_sender`]: the recovery
    /// seam must work whether or not the embedder wants an input sink.
    fn ensure_control_writer(&mut self) {
        if self.control_tx.lock().expect("control tx").is_some() {
            return;
        }
        let Some(mut stream) = self.control_send.take() else {
            return;
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<C2A>();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if send_msg(&mut stream, &msg).await.is_err() {
                    break;
                }
            }
        });
        *self.control_tx.lock().expect("control tx") = Some(tx);
    }

    /// Take the audio output channel — interleaved-i16 PCM frames for the
    /// embedder to play. Enables audio decode (until called, audio datagrams
    /// are dropped). Call once.
    pub fn take_audio_output(&mut self) -> Result<std::sync::mpsc::Receiver<Vec<i16>>> {
        let mut slot = self.audio.lock().expect("audio slot");
        if slot.is_some() {
            return Err(Error::Session("audio output already taken".into()));
        }
        let (recv, rx) = audio::AudioReceive::new()?;
        *slot = Some(recv);
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

    /// Spawn the datagram receive task on first use. Reception must never
    /// wait on the release side: arrival timestamps feed the agent's
    /// delay-based estimator, and a paced present must not read as path
    /// congestion. NACKs and feedback also fire at true arrival time.
    fn ensure_receiver(&mut self) {
        let Some(frames_tx) = self.frames_tx.take() else {
            return;
        };
        // The receive task and the gate both send on it; start it first so no
        // NACK or recovery request is dropped for want of a writer.
        self.ensure_control_writer();
        let task = ReceiveTask {
            conn: self.conn.clone(),
            clock: self.clock.clone(),
            reassembler: self.reassembler.take().expect("receiver spawned once"),
            control_tx: self.control_tx.clone(),
            audio: self.audio.clone(),
            frames_tx,
            dropped: self.reassembly_dropped.clone(),
            recovered: self.reassembly_recovered.clone(),
            feedback_batch: Vec::new(),
            last_feedback_us: 0,
            highest_seq: None,
            nacked: std::collections::VecDeque::new(),
            nack_window: (0, 0),
        };
        tokio::spawn(task.run());
    }

    /// Receive frames until the next one decodes. `None` when the connection
    /// closes. The gate, de-jitter and stats all live in the shared session.
    pub async fn recv_frame(
        &mut self,
        decoder: &mut dyn VideoDecoder,
    ) -> Result<Option<FrameOutput>> {
        self.ensure_receiver();
        let out = self.stream.recv_frame(decoder).await?;
        if out.is_some() {
            self.report_stats_if_due();
        }
        Ok(out)
    }

    /// Receive the next complete **encoded** access unit plus metadata, for
    /// embedders that decode on the platform (VideoToolbox / MediaCodec).
    /// `None` when the connection closes.
    pub async fn recv_encoded(&mut self) -> Result<Option<EncodedFrame>> {
        self.ensure_receiver();
        let out = self.stream.recv_encoded().await?;
        if out.is_some() {
            self.report_stats_if_due();
        }
        Ok(out)
    }

    /// Report client stats to the agent ~2 Hz — the ABR delay signal (spec 04).
    /// Fire-and-forget over the control writer; a no-op until it's running.
    fn report_stats_if_due(&mut self) {
        const INTERVAL_US: u64 = 500_000;
        let now = self.clock.now_us();
        if now.saturating_sub(self.last_stats_report_us) < INTERVAL_US {
            return;
        }
        let Some(tx) = self.control_tx.lock().expect("control tx").clone() else {
            return;
        };
        self.last_stats_report_us = now;
        let p = self.stream.present_stats();
        let s = self.stream.stats();
        let _ = tx.send(C2A::StatsReport(gsa_protocol::control::ClientStats {
            frames_received: s.frames_complete,
            frames_complete: s.frames_complete,
            frames_dropped_incomplete: s.frames_dropped_incomplete,
            frames_recovered: s.frames_recovered as u32,
            frames_decoded: s.frames_decoded,
            decode_us_p50: s.decode_ms_p50.map_or(0, |ms| (ms * 1000.0) as u32),
            jitter_us: self.stream.jitter_us(),
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

    /// Shared de-jitter switch for the embedder (default enabled).
    #[must_use]
    pub fn dejitter_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.stream.dejitter_flag()
    }

    /// Shared flag the embedder sets when its decoder rejected a frame; the
    /// gate treats it as a reference break and requests recovery.
    #[must_use]
    pub fn decode_error_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.stream.decode_error_flag()
    }

    /// Handle for the embedder's display path to report presented frames.
    #[must_use]
    pub fn presented_sink(&self) -> PresentedSink {
        self.stream.presented_sink()
    }

    /// Direct form of [`PresentedSink::presented`] for harnesses that own
    /// the client.
    pub fn frame_presented(&mut self, capture_ts_us: u32) {
        self.stream.frame_presented(capture_ts_us);
    }

    #[must_use]
    pub fn stats(&self) -> StatsSummary {
        self.stream.stats()
    }

    /// Per-stage latency percentiles and the total — measured outright here,
    /// since this backend's clocks are synced, rather than composed.
    #[must_use]
    pub fn latency_chain(&self) -> LatencySummary {
        self.stream.latency_chain()
    }

    /// Presentation-side health summary (fed by [`PresentedSink`]).
    pub fn present_stats(&mut self) -> stats::PresentSummary {
        self.stream.present_stats()
    }

    /// Graceful shutdown: close the connection and flush the endpoint.
    pub async fn close(self) {
        self.conn.close(0u32.into(), b"client done");
        self.endpoint.wait_idle().await;
    }
}

/// Owns the datagram read loop, decoupled from frame release: arrivals are
/// stamped, acknowledged, and reassembled the moment they land, regardless
/// of what the present side is doing. Exits when the connection closes or
/// the [`Client`] is dropped; audio decodes+plays here as a side effect.
struct ReceiveTask {
    conn: quinn::Connection,
    clock: MediaClock,
    reassembler: Reassembler,
    control_tx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<C2A>>>>,
    audio: std::sync::Arc<std::sync::Mutex<Option<audio::AudioReceive>>>,
    frames_tx: tokio::sync::mpsc::UnboundedSender<BackendFrame>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Pending per-packet arrival samples (seq, arrival µs) for the next
    /// `PacketFeedback` batch (~20 Hz).
    feedback_batch: Vec<(u32, u64)>,
    last_feedback_us: u64,
    /// Highest transport seq seen (NACK gap detection).
    highest_seq: Option<u32>,
    /// Recently NACKed seqs — each is requested exactly once.
    nacked: std::collections::VecDeque<u32>,
    /// NACK budget window: (window start µs, seqs requested this window).
    nack_window: (u64, usize),
}

impl ReceiveTask {
    async fn run(mut self) {
        loop {
            let datagram = match self.conn.read_datagram().await {
                Ok(d) => d,
                Err(quinn::ConnectionError::ApplicationClosed(_))
                | Err(quinn::ConnectionError::LocallyClosed) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "datagram receive stopped");
                    break;
                }
            };
            match datagram
                .first()
                .copied()
                .map(gsa_protocol::DatagramType::from_wire)
            {
                Some(Ok(gsa_protocol::DatagramType::Padding)) => {
                    self.record_arrival(&datagram);
                }
                Some(Ok(gsa_protocol::DatagramType::Audio)) => {
                    if let Some(a) = self.audio.lock().expect("audio slot").as_mut() {
                        a.handle(&datagram);
                    }
                }
                Some(Ok(gsa_protocol::DatagramType::Video)) => {
                    self.record_arrival(&datagram);
                    let (header, payload) = match VideoDatagramHeader::parse(&datagram) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(error = %e, "bad datagram dropped");
                            continue;
                        }
                    };
                    let arrival_us = self.clock.now_us();
                    for frame in self.reassembler.push(header, payload) {
                        let frame = BackendFrame {
                            data: frame.data,
                            frame_id: frame.frame_id,
                            keyframe: frame.kind == gsa_core::media::FrameKind::Idr,
                            capture_ts_us: frame.capture_ts_us,
                            // The gsa agent does not yet report its own
                            // capture-to-encode time per frame; its clock
                            // sync gives absolute latency instead.
                            host_latency_us: None,
                            arrival_us,
                        };
                        if self.frames_tx.send(frame).is_err() {
                            return; // client gone
                        }
                    }
                    self.dropped.store(
                        self.reassembler.frames_dropped(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    self.recovered.store(
                        self.reassembler.frames_recovered(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                _ => tracing::warn!("unknown datagram dropped"),
            }
        }
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
        // Mass loss is congestion, not sporadic drops: NACKing thousands of
        // seqs asks the sender to pile retransmits onto an already-collapsing
        // path. Beyond the budget, FEC and the recovery ladder take over.
        const NACK_BUDGET_PER_SEC: usize = 150;
        if let Some(high) = self.highest_seq {
            let ahead = seq.wrapping_sub(high);
            if ahead > 1 && ahead <= 64 {
                if now.saturating_sub(self.nack_window.0) >= 1_000_000 {
                    self.nack_window = (now, 0);
                }
                let budget = NACK_BUDGET_PER_SEC.saturating_sub(self.nack_window.1);
                let missing: Vec<u32> = (1..ahead)
                    .map(|i| high.wrapping_add(i))
                    .filter(|s| !self.nacked.contains(s))
                    .take(budget)
                    .collect();
                self.nack_window.1 += missing.len();
                if !missing.is_empty() {
                    tracing::debug!(count = missing.len(), first = missing[0], "nack sent");
                    for &m in &missing {
                        if self.nacked.len() == 512 {
                            self.nacked.pop_front();
                        }
                        self.nacked.push_back(m);
                    }
                    if let Some(tx) = self.control_tx.lock().expect("control tx").as_ref() {
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
        let Some(tx) = self.control_tx.lock().expect("control tx").clone() else {
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
}
