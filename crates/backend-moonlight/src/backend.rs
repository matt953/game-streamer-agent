//! Assembling a running session from the pieces.
//!
//! This is where the protocol stops. Everything below is Moonlight-specific —
//! launch, RTSP, ENet, shards, FEC — and everything the caller receives is
//! backend-neutral: complete access units stamped at true arrival, plus a way
//! to ask for repairs. The shared client core takes it from there.

use crate::host::{LaunchedSession, PairedSession, StreamMode};
use crate::{Command, Crypto, Depacketizer, MediaSocket, Received, Rtsp, StreamRequest};
use gsa_client_backend_api::{BackendFrame, RecoverySink};
use gsa_core::{Error, Result};

/// Asks the host to repair the reference chain, via the control channel.
///
/// The host advertises whether it can invalidate references without a full
/// keyframe; when it cannot, the default falls back to asking for an IDR.
#[derive(Debug)]
pub struct MoonlightRecovery {
    commands: std::sync::Mutex<std::sync::mpsc::Sender<Command>>,
    reference_invalidation: bool,
}

impl RecoverySink for MoonlightRecovery {
    fn request_keyframe(&self) {
        if let Ok(tx) = self.commands.lock() {
            let _ = tx.send(Command::RequestIdr);
        }
    }

    fn request_recovery(&self, last_good_frame_id: u32) {
        if !self.reference_invalidation {
            self.request_keyframe();
            return;
        }
        if let Ok(tx) = self.commands.lock() {
            let _ = tx.send(Command::InvalidateReferenceFrames {
                first: last_good_frame_id.wrapping_add(1),
                last: last_good_frame_id.wrapping_add(1),
            });
        }
    }
}

/// A live Moonlight stream, reduced to the neutral pieces.
#[derive(Debug)]
pub struct MoonlightStream {
    /// Complete access units, stamped when they truly arrived. Taken once,
    /// through [`MoonlightStream::take_frames`], so that claiming the frames
    /// cannot move the struct apart and drop the guard that keeps the
    /// receive threads alive.
    frames: Option<tokio::sync::mpsc::UnboundedReceiver<BackendFrame>>,
    pub recovery: std::sync::Arc<dyn RecoverySink>,
    /// Frames the wire could not deliver whole, for the shared health stats.
    pub dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity — loss that cost nothing visible.
    pub recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Video datagrams seen. Used to tell "the host is streaming" from "the
    /// handshake succeeded and nothing is coming", which look identical from
    /// every other signal.
    datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// What the host says over the control channel: rumble, termination, and
    /// features we do not act on yet. Drain it — a caller that ignores this
    /// still gets a stream, but loses the host's own account of what happened.
    pub events: std::sync::mpsc::Receiver<crate::HostMessage>,
    /// Dropping this tears the session down.
    _worker: Worker,
}

impl MoonlightStream {
    /// Wait until the host actually starts sending, or give up.
    ///
    /// Counted on datagrams rather than assembled frames so a host that is
    /// sending but losing shards still reads as alive.
    async fn wait_for_media(&self, within: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if self.datagrams.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// Claim the frame stream. Returns `None` if already taken.
    ///
    /// Keep the `MoonlightStream` itself alive for as long as you read from
    /// the receiver: dropping it stops the session.
    pub fn take_frames(&mut self) -> Option<tokio::sync::mpsc::UnboundedReceiver<BackendFrame>> {
        self.frames.take()
    }
}

/// Owns the receive threads and stops them on drop.
#[derive(Debug)]
pub struct Worker {
    commands: std::sync::mpsc::Sender<Command>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.commands.send(Command::Stop);
    }
}

/// Launch an app and bring every stream up.
///
/// Returns once media is negotiated; frames start arriving on the channel.
pub async fn start(
    session: &mut PairedSession,
    host_ip: std::net::IpAddr,
    app_id: u32,
    mode: StreamMode,
    bitrate_kbps: u32,
) -> Result<MoonlightStream> {
    // How long a healthy host takes to start sending. Generous: a slow host
    // starting an app is normal, a host that will never send is not.
    const FIRST_MEDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    for attempt in 0..2 {
        clear_running_session(session).await;
        let launched = session.launch(app_id, mode).await?;
        let stream = connect(&launched, host_ip, mode, bitrate_kbps).await?;
        if stream.wait_for_media(FIRST_MEDIA_TIMEOUT).await {
            return Ok(stream);
        }
        // Everything handshook and no media came: the host is holding a dead
        // session under our id. Tearing this one down and moving to a fresh
        // id is the only thing observed to clear it.
        drop(stream);
        let _ = session.cancel().await;
        if attempt == 0 {
            tracing::warn!("host accepted the session but sent no media; retrying with a fresh id");
            session.rotate_session_id();
        }
    }
    Err(Error::Session(
        "host accepted the session but never sent media, even after a retry".into(),
    ))
}

async fn connect(
    launched: &LaunchedSession,
    host_ip: std::net::IpAddr,
    mode: StreamMode,
    bitrate_kbps: u32,
) -> Result<MoonlightStream> {
    let mut rtsp = Rtsp::new(&launched.rtsp_url)?;
    let negotiated = rtsp
        .negotiate(StreamRequest {
            width: mode.width,
            height: mode.height,
            fps: mode.fps,
            // H.264 until platform decode is wired for HEVC.
            bitstream_format: 0,
            bitrate_kbps,
            packet_size: 1392,
            channels: mode.channels,
        })
        .await?;

    tracing::info!(
        video = negotiated.video_port,
        audio = negotiated.audio_port,
        control = negotiated.control_port,
        payload = negotiated.ping_payload.is_some(),
        connect_data = ?negotiated.connect_data,
        "negotiated"
    );
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let crypto = Crypto::new(launched.riaes_key, negotiated.control_v2());
    let control_addr = std::net::SocketAddr::new(host_ip, negotiated.control_port);
    let connect_data = negotiated.connect_data.unwrap_or(0);
    let control_commands = command_rx;
    std::thread::Builder::new()
        .name("moonlight-control".into())
        .spawn(move || {
            if let Err(e) = crate::run_control(
                control_addr,
                connect_data,
                crypto,
                control_commands,
                event_tx,
            ) {
                tracing::warn!(error = %e, "control channel ended");
            }
        })
        .map_err(|e| Error::Transport(format!("spawn control thread: {e}")))?;

    let (frames_tx, frames_rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let recovered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let datagrams = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let video_addr = std::net::SocketAddr::new(host_ip, negotiated.video_port);
    let audio_addr = std::net::SocketAddr::new(host_ip, negotiated.audio_port);
    let video = MediaSocket::bind(video_addr, negotiated.ping_payload)?;
    // Pinged but not consumed: a host that never hears from a media port
    // tears the whole session down. It pings by address — only one socket may
    // carry the session payload without stealing another stream's binding.
    let audio = MediaSocket::bind(audio_addr, None)?;

    let worker_stop = stop.clone();
    let worker_dropped = dropped.clone();
    let worker_recovered = recovered.clone();
    let worker_datagrams = datagrams.clone();
    std::thread::Builder::new()
        .name("moonlight-video".into())
        .spawn(move || {
            receive_video(
                video,
                audio,
                &frames_tx,
                &worker_stop,
                &worker_dropped,
                &worker_recovered,
                &worker_datagrams,
            );
        })
        .map_err(|e| Error::Transport(format!("spawn video thread: {e}")))?;

    Ok(MoonlightStream {
        frames: Some(frames_rx),
        events: event_rx,
        recovery: std::sync::Arc::new(MoonlightRecovery {
            commands: std::sync::Mutex::new(command_tx.clone()),
            reference_invalidation: negotiated.reference_invalidation,
        }),
        dropped,
        recovered,
        datagrams,
        _worker: Worker {
            commands: command_tx,
            stop,
        },
    })
}

/// Stop whatever the host is streaming, and wait until it says so.
///
/// Cancelling blind and launching immediately is a race: the host finishes
/// tearing down *after* our new session exists and takes it with it, which
/// presents as a session that handshakes perfectly and never sends a frame.
/// So only cancel when the host says it is busy, then wait for it to agree
/// that it is idle before launching.
async fn clear_running_session(session: &PairedSession) {
    const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    match session.server_info().await {
        // Nothing running: launching straight into a clean host is the
        // common case and must not pay for the recovery path.
        Ok(info) if !info.state.ends_with("_BUSY") => return,
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(error = %e, "could not read host state before launch");
            return;
        }
    }
    if let Err(e) = session.cancel().await {
        tracing::debug!(error = %e, "cancel before launch failed");
    }
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        match session.server_info().await {
            Ok(info) if !info.state.ends_with("_BUSY") => return,
            _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
    }
    tracing::warn!("host still reports a session running; launching anyway");
}

/// Read datagrams, reassemble, and publish frames at their arrival time.
///
/// Arrival is stamped here, on the receive side, and never on the way out:
/// a frame stamped when it is released would make a paced present look like
/// network delay to anything reasoning about the link.
fn receive_video(
    mut video: MediaSocket,
    mut audio: MediaSocket,
    frames: &tokio::sync::mpsc::UnboundedSender<BackendFrame>,
    stop: &std::sync::atomic::AtomicBool,
    dropped: &std::sync::atomic::AtomicU64,
    recovered: &std::sync::atomic::AtomicU64,
    datagrams: &std::sync::atomic::AtomicU64,
) {
    const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let clock = gsa_core::time::MediaClock::new();
    let mut depacketizer = Depacketizer::new();
    let mut buf = vec![0u8; 4096];
    let mut last_ping = std::time::Instant::now() - PING_INTERVAL;

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        if last_ping.elapsed() >= PING_INTERVAL {
            if let Err(e) = video.ping().and_then(|()| audio.ping()) {
                tracing::warn!(error = %e, "media ping failed");
                return;
            }
            last_ping = std::time::Instant::now();
        }
        let n = match video.recv(&mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "video receive stopped");
                return;
            }
        };
        let arrival_us = clock.now_us();
        datagrams.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        depacketizer.push(&buf[..n]);
        while let Some(event) = depacketizer.next_event() {
            match event {
                Received::Frame(frame) => {
                    if frame.recovered {
                        recovered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let out = BackendFrame {
                        data: frame.data,
                        frame_id: frame.frame_index,
                        keyframe: frame.keyframe,
                        // The host exposes no capture clock, so the frame is
                        // stamped with its arrival. Differences are real
                        // (cadence, jitter); the absolute value is not
                        // glass-to-glass and must not be reported as such.
                        capture_ts_us: arrival_us as u32,
                        arrival_us,
                    };
                    if frames.send(out).is_err() {
                        return; // consumer gone
                    }
                }
                Received::Lost(loss) => {
                    tracing::debug!(?loss, "frame lost");
                    dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Received::Nothing => {}
            }
        }
    }
}
