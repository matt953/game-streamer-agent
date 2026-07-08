//! Per-connection control protocol service (spec 05 session state machine,
//! M0 surface): Hello → ListSources/StartSession/StopSession/Ping.

use std::sync::Arc;

use gsa_capture_api::{RenderSource, SourceDescriptor};
use gsa_core::id::SourceId;
use gsa_core::media::VideoMode;
use gsa_core::{Error, Result};
use gsa_encode_api::Encoder;
use gsa_protocol::control::{A2C, C2A, HelloAck, ProtoErrorMsg, SessionParams};
use gsa_protocol::{PROTO_VERSION, control};
use gsa_transport::{recv_msg, send_msg};

use crate::pipeline;
use crate::state::{AgentState, SessionEntry};

/// Produces sources on demand (agent wires TestPattern at M0, platform
/// capture from M1).
pub trait SourceFactory: Send + Sync {
    fn list(&self) -> Vec<SourceDescriptor>;
    fn create(&self, id: SourceId) -> Result<Box<dyn RenderSource>>;
}

/// Produces an encoder per session (probing/selection per spec 03 lands
/// with hardware backends).
pub trait EncoderFactory: Send + Sync {
    fn create(&self) -> Result<Box<dyn Encoder>>;
}

/// Drive one client connection until it closes. The first bi stream the
/// client opens is the control stream.
pub async fn serve_connection(
    conn: quinn::Connection,
    state: Arc<AgentState>,
    sources: Arc<dyn SourceFactory>,
    encoders: Arc<dyn EncoderFactory>,
) {
    let peer = conn.remote_address().to_string();
    tracing::info!(peer, "client connected");
    if let Err(e) = serve_inner(&conn, &state, &sources, &encoders, &peer).await {
        tracing::info!(peer, error = %e, "connection ended");
    } else {
        tracing::info!(peer, "client disconnected");
    }
}

async fn serve_inner(
    conn: &quinn::Connection,
    state: &Arc<AgentState>,
    sources: &Arc<dyn SourceFactory>,
    encoders: &Arc<dyn EncoderFactory>,
    peer: &str,
) -> Result<()> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| Error::Transport(format!("accept control stream: {e}")))?;

    let mut active: Option<(u64, pipeline::PipelineHandle)> = None;
    let mut helloed = false;

    let result = loop {
        let msg: C2A = match recv_msg(&mut recv).await {
            Ok(m) => m,
            Err(_) if conn.close_reason().is_some() => break Ok(()),
            Err(e) => break Err(e),
        };

        match msg {
            C2A::Hello(hello) => {
                if hello.proto != PROTO_VERSION {
                    let err = A2C::Error(ProtoErrorMsg {
                        message: format!(
                            "protocol {} unsupported (agent speaks {PROTO_VERSION})",
                            hello.proto
                        ),
                    });
                    send_msg(&mut send, &err).await?;
                    break Err(Error::Protocol(
                        gsa_core::error::ProtocolError::UnsupportedVersion(hello.proto),
                    ));
                }
                helloed = true;
                tracing::info!(peer, client = hello.client_name, "hello");
                send_msg(
                    &mut send,
                    &A2C::HelloAck(HelloAck {
                        proto: PROTO_VERSION,
                        agent_name: hostname(),
                        encode_codecs: vec![gsa_core::media::Codec::H264],
                    }),
                )
                .await?;
            }
            _ if !helloed => {
                send_msg(
                    &mut send,
                    &A2C::Error(ProtoErrorMsg {
                        message: "hello first".into(),
                    }),
                )
                .await?;
            }
            C2A::ListSources => {
                let infos = sources.list().into_iter().map(|d| d.info).collect();
                send_msg(&mut send, &A2C::Sources(infos)).await?;
            }
            C2A::StartSession(req) => {
                if active.is_some() {
                    send_msg(
                        &mut send,
                        &A2C::Error(ProtoErrorMsg {
                            message: "session already active".into(),
                        }),
                    )
                    .await?;
                    continue;
                }
                match start_session(conn, state, sources, encoders, peer, &req) {
                    Ok((id, handle, mode, bitrate)) => {
                        active = Some((id, handle));
                        send_msg(
                            &mut send,
                            &A2C::SessionStarted(SessionParams {
                                session: gsa_core::id::SessionId(id),
                                codec: gsa_core::media::Codec::H264,
                                mode,
                                bitrate_bps: bitrate,
                            }),
                        )
                        .await?;
                    }
                    Err(e) => {
                        tracing::warn!(peer, error = %e, "session start failed");
                        send_msg(
                            &mut send,
                            &A2C::Error(ProtoErrorMsg {
                                message: e.to_string(),
                            }),
                        )
                        .await?;
                    }
                }
            }
            C2A::StopSession => {
                if let Some((id, mut handle)) = active.take() {
                    let _ = handle.stop();
                    state.remove_session(id);
                    tracing::info!(peer, session = id, "session stopped by client");
                }
            }
            C2A::Ping { client_ts_us } => {
                send_msg(
                    &mut send,
                    &A2C::Pong {
                        client_ts_us,
                        agent_ts_us: state.clock.now_us(),
                    },
                )
                .await?;
            }
            C2A::FrameAck { .. } => { /* NACK/ref-invalidation ladder lands at M3 (spec 04) */ }
            C2A::StatsReport(stats) => {
                tracing::debug!(peer, ?stats, "client stats");
            }
            C2A::InputBatch(_) => { /* input injection lands at M1 (spec 07) */ }
            // C2A is non_exhaustive: newer clients may send messages this
            // agent version doesn't know; ignoring them is the compat rule.
            _ => {}
        }
    };

    if let Some((id, mut handle)) = active.take() {
        let _ = handle.stop();
        state.remove_session(id);
        tracing::info!(peer, session = id, "session cleaned up on disconnect");
    }
    result
}

fn start_session(
    conn: &quinn::Connection,
    state: &Arc<AgentState>,
    sources: &Arc<dyn SourceFactory>,
    encoders: &Arc<dyn EncoderFactory>,
    peer: &str,
    req: &control::SessionRequest,
) -> Result<(u64, pipeline::PipelineHandle, VideoMode, u32)> {
    let source = sources.create(req.source)?;
    let encoder = encoders.create()?;
    let mode = req.mode.unwrap_or(state.config.video.mode);
    let bitrate = state.config.video.bitrate_bps;

    let handle = pipeline::start(source, encoder, conn.clone(), mode, bitrate)?;
    let id = state.allocate_session();
    state.register_session(
        id,
        SessionEntry {
            mode,
            peer: peer.to_string(),
            frames_sent: handle.frames_sent.clone(),
        },
    );
    tracing::info!(peer, session = id, ?mode, bitrate, "session started");
    Ok((id, handle, mode, bitrate))
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "gsa-agent".into())
}
