//! Embeddable streaming client core (spec 01, decision D9): connection,
//! negotiation, datagram reassembly, decode orchestration, and latency
//! stats. **No UI, no rendering, no platform decode** — the embedding app
//! (or `client-dev`) supplies a [`VideoDecoder`] and owns presentation.
//! This boundary is what makes the M2 UniFFI factoring mechanical.

mod decode;
mod reassembly;
mod stats;

pub use decode::{DecodedFrame, PixelOrder, VideoDecoder};
pub use gsa_protocol::input::{InputEvent, MouseButton, MouseMove};
pub use reassembly::Reassembler;
pub use stats::{ClockSync, LatencyStats, StatsSummary};

use gsa_core::media::VideoMode;
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_protocol::PROTO_VERSION;
use gsa_protocol::control::{
    A2C, C2A, DecodeCaps, Hello, SessionParams, SessionRequest, SourceInfo,
};
use gsa_protocol::datagram::VideoDatagramHeader;
use gsa_transport::{client_connect, recv_msg, send_msg};

/// One decoded frame plus its measurements, handed to the embedder.
#[derive(Debug)]
pub struct FrameOutput {
    pub frame: DecodedFrame,
    pub frame_id: u32,
    /// Estimated glass-to-glass-so-far: agent capture → decoded on client.
    pub latency_us: Option<u32>,
    pub decode_us: u32,
}

/// Fire-and-forget input sink, decoupled from the frame-receive loop.
/// Sync `send` (safe to call from a UI event loop); a background task on the
/// client's runtime writes batches to the control stream in order.
#[derive(Debug, Clone)]
pub struct InputSender {
    tx: tokio::sync::mpsc::UnboundedSender<Vec<gsa_protocol::input::InputEvent>>,
}

impl InputSender {
    pub fn send(&self, events: Vec<gsa_protocol::input::InputEvent>) {
        if !events.is_empty() {
            let _ = self.tx.send(events);
        }
    }
}

pub struct Client {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    control_send: Option<quinn::SendStream>,
    control_recv: quinn::RecvStream,
    clock: MediaClock,
    clock_sync: ClockSync,
    reassembler: Reassembler,
    stats: LatencyStats,
    session: Option<SessionParams>,
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
    pub async fn connect(addr: std::net::SocketAddr, client_name: &str) -> Result<Self> {
        let (endpoint, conn) = client_connect(addr).await?;
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
                    codecs: vec![gsa_core::media::Codec::H264],
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
            control_recv,
            clock,
            clock_sync: ClockSync::default(),
            reassembler: Reassembler::new(),
            stats: LatencyStats::default(),
            session: None,
        };
        client.sync_clock(5).await?;
        Ok(client)
    }

    /// Ping/pong `rounds` times to estimate the agent-clock offset (spec 04).
    async fn sync_clock(&mut self, rounds: u32) -> Result<()> {
        for _ in 0..rounds {
            let sent = self.clock.now_us();
            send_msg(self.ctl()?, &C2A::Ping { client_ts_us: sent }).await?;
            match recv_msg::<A2C>(&mut self.control_recv).await? {
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

    /// Move the control send-stream into a background writer task and return
    /// a sync [`InputSender`] for a UI thread. Call after `start_session`;
    /// the client can no longer send control messages afterward (it only
    /// receives frames + control replies).
    pub fn take_input_sender(&mut self) -> Option<InputSender> {
        let mut stream = self.control_send.take()?;
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<gsa_protocol::input::InputEvent>>();
        tokio::spawn(async move {
            while let Some(events) = rx.recv().await {
                if send_msg(&mut stream, &C2A::InputBatch(events))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Some(InputSender { tx })
    }

    pub async fn list_sources(&mut self) -> Result<Vec<SourceInfo>> {
        send_msg(self.ctl()?, &C2A::ListSources).await?;
        match recv_msg::<A2C>(&mut self.control_recv).await? {
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
        match recv_msg::<A2C>(&mut self.control_recv).await? {
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

    /// Receive datagrams until the next complete frame decodes.
    /// Returns `None` when the connection closes.
    pub async fn recv_frame(
        &mut self,
        decoder: &mut dyn VideoDecoder,
    ) -> Result<Option<FrameOutput>> {
        loop {
            let datagram = match self.conn.read_datagram().await {
                Ok(d) => d,
                Err(quinn::ConnectionError::ApplicationClosed(_))
                | Err(quinn::ConnectionError::LocallyClosed) => return Ok(None),
                Err(e) => return Err(Error::Transport(format!("read datagram: {e}"))),
            };
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

            let decode_start = self.clock.now_us();
            match decoder.decode(&frame_data)? {
                Some(frame) => {
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
                None => continue, // decoder buffering (e.g. SPS/PPS only)
            }
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
