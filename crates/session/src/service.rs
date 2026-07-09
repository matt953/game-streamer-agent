//! Per-connection control protocol service (spec 05 session state machine,
//! M0 surface): Hello → ListSources/StartSession/StopSession/Ping.

use std::sync::Arc;

use gsa_capture_api::{RenderSource, SourceDescriptor};
use gsa_core::id::SourceId;
use gsa_core::media::VideoMode;
use gsa_core::{Error, Result};
use gsa_encode_api::Encoder;
use gsa_protocol::control::{A2C, C2A, HelloAck, ProtoErrorMsg, SessionParams, SourceKind};
use gsa_protocol::grant::Scope;
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

/// Produces an encoder compatible with a given source's frame format
/// (spec 03). E.g. a real display yields IOSurface/NV12 frames → hardware
/// encoder; the test pattern yields CPU/BGRA → software encoder.
pub trait EncoderFactory: Send + Sync {
    fn create(&self, source_kind: SourceKind) -> Result<Box<dyn Encoder>>;
}

/// Drive one client connection until it closes. The first bi stream the
/// client opens is the control stream.
pub async fn serve_connection(
    conn: quinn::Connection,
    state: Arc<AgentState>,
    sources: Arc<dyn SourceFactory>,
    encoders: Arc<dyn EncoderFactory>,
    peer_scope: Scope,
) {
    let peer = conn.remote_address().to_string();
    tracing::info!(peer, ?peer_scope, "client connected");
    if let Err(e) = serve_inner(&conn, &state, &sources, &encoders, &peer, peer_scope).await {
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
    peer_scope: Scope,
) -> Result<()> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| Error::Transport(format!("accept control stream: {e}")))?;

    let mut active: Option<ActiveSession> = None;
    let mut helloed = false;
    // Client's max decodable H.264 profile (from Hello), negotiated at session start.
    let mut client_h264_profile = gsa_core::media::H264Profile::ConstrainedBaseline;

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
                client_h264_profile = hello.decode_caps.max_h264_profile;
                tracing::info!(
                    peer,
                    client = hello.client_name,
                    ?client_h264_profile,
                    "hello"
                );
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
                match start_session(
                    conn,
                    state,
                    sources,
                    encoders,
                    peer,
                    &req,
                    client_h264_profile,
                ) {
                    Ok(started) => {
                        let (mode, bitrate) = (started.mode, started.bitrate);
                        let session_id = started.session.id;
                        active = Some(started.session);
                        send_msg(
                            &mut send,
                            &A2C::SessionStarted(SessionParams {
                                session: gsa_core::id::SessionId(session_id),
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
                if let Some(mut a) = active.take() {
                    let _ = a.pipeline.stop();
                    state.remove_session(a.id);
                    tracing::info!(peer, session = a.id, "session stopped by client");
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
            C2A::RequestKeyframe => {
                if let Some(a) = &active {
                    a.pipeline.request_keyframe();
                    tracing::debug!(peer, session = a.id, "keyframe requested by client");
                }
            }
            C2A::FrameAck { .. } => { /* full NACK/ref-invalidation ladder lands at M3 (spec 04) */
            }
            C2A::StatsReport(stats) => {
                tracing::debug!(peer, ?stats, "client stats");
            }
            C2A::InputBatch(events) => {
                // Injection requires the `interact` scope; a view-only peer's
                // input is silently dropped (spec 06).
                if peer_scope >= Scope::Interact
                    && let Some(a) = &mut active
                    && let Some(injector) = &mut a.injector
                {
                    for event in &events {
                        injector.inject(event);
                    }
                }
            }
            // C2A is non_exhaustive: newer clients may send messages this
            // agent version doesn't know; ignoring them is the compat rule.
            _ => {}
        }
    };

    if let Some(mut a) = active.take() {
        let _ = a.pipeline.stop();
        state.remove_session(a.id);
        tracing::info!(peer, session = a.id, "session cleaned up on disconnect");
    }
    result
}

/// Live session state held by the connection loop.
struct ActiveSession {
    id: u64,
    pipeline: pipeline::PipelineHandle,
    /// OS input injector for desktop/virtual-display sources (spec 07);
    /// `None` for emulator sources (which consume input in-process) or when
    /// no injector is available.
    injector: Option<Box<dyn gsa_input::Injector>>,
}

struct StartedSession {
    session: ActiveSession,
    mode: VideoMode,
    bitrate: u32,
}

fn start_session(
    conn: &quinn::Connection,
    state: &Arc<AgentState>,
    sources: &Arc<dyn SourceFactory>,
    encoders: &Arc<dyn EncoderFactory>,
    peer: &str,
    req: &control::SessionRequest,
    client_h264_profile: gsa_core::media::H264Profile,
) -> Result<StartedSession> {
    let source = sources.create(req.source)?;
    let descriptor = source.descriptor();
    let encoder = encoders.create(descriptor.kind())?;
    // Richest profile both sides support: encoder ceiling ∩ client decode cap.
    let h264_profile = encoder.caps().max_h264_profile.min(client_h264_profile);
    // Mode preference: client request > source native > agent config.
    let mode = req
        .mode
        .or_else(|| descriptor.modes.first().copied())
        .unwrap_or(state.config.video.mode);
    let bitrate = state.config.video.bitrate_bps;

    // Desktop / virtual displays inject at the OS level; emulators consume
    // input in-process and get no OS injector.
    let injector = match descriptor.kind() {
        SourceKind::Display | SourceKind::VirtualDisplay => {
            let inj = gsa_input::platform_injector();
            if inj.is_none() {
                tracing::warn!(peer, "no input injector (accessibility permission?)");
            }
            inj
        }
        _ => None,
    };

    let handle = pipeline::start(source, encoder, conn.clone(), mode, bitrate, h264_profile)?;
    let id = state.allocate_session();
    state.register_session(
        id,
        SessionEntry {
            mode,
            peer: peer.to_string(),
            frames_sent: handle.frames_sent.clone(),
        },
    );
    tracing::info!(
        peer,
        session = id,
        ?mode,
        bitrate,
        ?h264_profile,
        injecting = injector.is_some(),
        "session started"
    );
    Ok(StartedSession {
        session: ActiveSession {
            id,
            pipeline: handle,
            injector,
        },
        mode,
        bitrate,
    })
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "gsa-agent".into())
}
