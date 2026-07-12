//! Per-connection control protocol service (spec 05 session state machine,
//! M0 surface): Hello → ListSources/StartSession/StopSession/Ping.

use std::sync::Arc;

use gsa_capture_api::{RenderSource, SourceDescriptor};
use gsa_core::id::SourceId;
use gsa_core::media::{Codec, VideoMode};
use gsa_core::{Error, Result};
use gsa_encode_api::Encoder;
use gsa_input::InputFeedback;
use gsa_protocol::control::{
    A2C, C2A, EncodeStats, HelloAck, Notification, ProtoErrorMsg, SessionParams, SourceKind,
};
use gsa_protocol::grant::Scope;
use gsa_protocol::{PROTO_VERSION, control};
use gsa_transport::{recv_msg, send_msg};

use crate::pipeline;
use crate::state::{AgentState, SessionEntry};

/// Bitrate clamp band for client `SetBitrate` requests (and, later, ABR): floor
/// keeps the picture alive on a bad link, ceiling bounds a runaway request.
const BITRATE_MIN_BPS: u32 = 200_000; // 0.2 Mbps
const BITRATE_MAX_BPS: u32 = 100_000_000; // 100 Mbps

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

    /// Codecs the agent can encode, advertised to clients in `HelloAck`
    /// (advisory — the authoritative choice is made per-session against the
    /// actual encoder's caps). Default: probe the display encoder's static caps
    /// (cheap — no session init) and ensure H.264 (the software fallback).
    fn supported_codecs(&self) -> Vec<Codec> {
        let mut codecs = self
            .create(SourceKind::Display)
            .map(|e| e.caps().codecs)
            .unwrap_or_default();
        if !codecs.contains(&Codec::H264) {
            codecs.push(Codec::H264);
        }
        codecs
    }
}

/// Pick the codec for a session: the encoder's most-preferred (its caps order)
/// that the client can also decode, falling back to H.264.
fn negotiate_codec(client_decodes: &[Codec], encoder_emits: &[Codec]) -> Codec {
    encoder_emits
        .iter()
        .copied()
        .find(|c| client_decodes.contains(c))
        .unwrap_or(Codec::H264)
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
    // ABR (spec 04): controller lives per-session (`None` between sessions);
    // `abr_enabled` gates whether it drives the bitrate. Off by default.
    let mut abr: Option<crate::abr::AbrController> = None;
    let mut abr_enabled = false;
    let mut helloed = false;
    // Client's max decodable H.264 profile (from Hello), negotiated at session start.
    let mut client_h264_profile = gsa_core::media::H264Profile::ConstrainedBaseline;
    // Codecs the client can decode (from Hello); the session codec is picked from
    // the intersection with the encoder's caps.
    let mut client_decode_codecs: Vec<Codec> = vec![Codec::H264];

    // Push the emitted-bitrate telemetry to the client ~1 Hz (spec 04).
    let mut stats_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = loop {
        let msg: C2A = tokio::select! {
            r = recv_msg(&mut recv) => match r {
                Ok(m) => m,
                Err(_) if conn.close_reason().is_some() => break Ok(()),
                Err(e) => break Err(e),
            },
            _ = stats_tick.tick() => {
                if let Some(a) = &active {
                    let stats = EncodeStats {
                        target_bitrate_bps: a.pipeline.bitrate(),
                        emitted_bitrate_bps: a.pipeline.emitted_bitrate_bps(),
                    };
                    if let Err(e) = send_msg(&mut send, &A2C::EncodeStats(stats)).await {
                        break Err(e);
                    }
                }
                continue;
            }
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
                client_decode_codecs = hello.decode_caps.codecs.clone();
                tracing::info!(
                    peer,
                    client = hello.client_name,
                    ?client_h264_profile,
                    ?client_decode_codecs,
                    "hello"
                );
                send_msg(
                    &mut send,
                    &A2C::HelloAck(HelloAck {
                        proto: PROTO_VERSION,
                        agent_name: hostname(),
                        encode_codecs: encoders.supported_codecs(),
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
                    &client_decode_codecs,
                ) {
                    Ok(started) => {
                        let (mode, bitrate, codec) = (started.mode, started.bitrate, started.codec);
                        let session_id = started.session.id;
                        // Seed ABR with this session's bitrate as its ceiling.
                        abr = Some(crate::abr::AbrController::new(
                            bitrate,
                            state.clock.now_us(),
                        ));
                        active = Some(started.session);
                        send_msg(
                            &mut send,
                            &A2C::SessionStarted(SessionParams {
                                session: gsa_core::id::SessionId(session_id),
                                codec,
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
                abr = None;
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
            C2A::SetBitrate { bitrate_bps } => {
                if let Some(a) = &active {
                    // Clamp to a sane band so a bad client can't drive the
                    // encoder to 0 or a runaway rate (spec 04). ABR uses the
                    // same pipeline actuator server-side.
                    let clamped = bitrate_bps.clamp(BITRATE_MIN_BPS, BITRATE_MAX_BPS);
                    a.pipeline.set_bitrate(clamped);
                    // The manual bitrate is ABR's ceiling.
                    if let Some(ctrl) = &mut abr {
                        ctrl.set_ceiling(clamped);
                    }
                    tracing::info!(
                        peer,
                        session = a.id,
                        bitrate = clamped,
                        "bitrate set by client"
                    );
                }
            }
            C2A::SetAbr { enabled } => {
                abr_enabled = enabled;
                // Start adapting from the current bitrate when turned on.
                if enabled && let (Some(a), Some(ctrl)) = (&active, &mut abr) {
                    ctrl.sync_target(a.pipeline.bitrate());
                }
                tracing::info!(peer, enabled, "abr toggled by client");
            }
            C2A::FrameAck { .. } => { /* full NACK/ref-invalidation ladder lands at M3 (spec 04) */
            }
            C2A::StatsReport(stats) => {
                if abr_enabled && let (Some(a), Some(ctrl)) = (&active, &mut abr) {
                    let target = ctrl.on_delay(stats.recent_delay_us, state.clock.now_us());
                    a.pipeline.set_bitrate(target);
                }
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
                        // Injection may report host state the client should hear
                        // about (a virtual pad plugging/unplugging) — forward it
                        // as a control-stream notification.
                        if let Some(feedback) = injector.inject(event) {
                            let notification = match feedback {
                                InputFeedback::GamepadConnected { seat } => {
                                    Notification::GamepadConnected { seat }
                                }
                                InputFeedback::GamepadDisconnected { seat } => {
                                    Notification::GamepadDisconnected { seat }
                                }
                            };
                            send_msg(&mut send, &A2C::Notification(notification)).await?;
                        }
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
    codec: Codec,
}

#[allow(clippy::too_many_arguments)]
fn start_session(
    conn: &quinn::Connection,
    state: &Arc<AgentState>,
    sources: &Arc<dyn SourceFactory>,
    encoders: &Arc<dyn EncoderFactory>,
    peer: &str,
    req: &control::SessionRequest,
    client_h264_profile: gsa_core::media::H264Profile,
    client_decode_codecs: &[Codec],
) -> Result<StartedSession> {
    let source = sources.create(req.source)?;
    let descriptor = source.descriptor();
    let encoder = encoders.create(descriptor.kind())?;
    // Codec: encoder's most-preferred that the client can decode.
    let codec = negotiate_codec(client_decode_codecs, &encoder.caps().codecs);
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

    let handle = pipeline::start(
        source,
        encoder,
        conn.clone(),
        mode,
        bitrate,
        codec,
        h264_profile,
    )?;
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
        ?codec,
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
        codec,
    })
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "gsa-agent".into())
}
