//! Assembling a running session from the pieces.
//!
//! This is where the protocol stops. Everything below is Moonlight-specific —
//! launch, RTSP, ENet, shards, FEC — and everything the caller receives is
//! backend-neutral: complete access units stamped at true arrival, plus a way
//! to ask for repairs. The shared client core takes it from there.

use crate::host::{LaunchedSession, PairedSession, StreamMode};
use crate::{Command, Crypto, Depacketizer, MediaSocket, Received, Rtsp, StreamRequest};
use gsa_client_backend_api::{BackendFrame, InputSink, RecoverySink, SessionOrigin};
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

/// Sends the embedder's input over the control channel.
///
/// The encoder is stateful — it remembers held modifiers and which pads are
/// plugged in — so it lives here rather than being rebuilt per event.
#[derive(Debug)]
pub struct MoonlightInput {
    commands: std::sync::mpsc::Sender<Command>,
    encoder: std::sync::Mutex<crate::InputEncoder>,
}

impl InputSink for MoonlightInput {
    fn send(&self, events: Vec<gsa_client_backend_api::InputEvent>) {
        let Ok(mut encoder) = self.encoder.lock() else {
            return;
        };
        for event in &events {
            if let Some(message) = encoder.encode(event) {
                // Fire-and-forget: a full queue means the session is going
                // away, and blocking a UI thread on it would be worse.
                let _ = self.commands.send(Command::Input(message));
            }
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
    /// Where to send keyboard, mouse and controller input.
    pub input: std::sync::Arc<dyn InputSink>,
    /// Frames the wire could not deliver whole, for the shared health stats.
    pub dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity — loss that cost nothing visible.
    pub recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Video datagrams seen. Used to tell "the host is streaming" from "the
    /// handshake succeeded and nothing is coming", which look identical from
    /// every other signal.
    datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Whether this started the app or rejoined one already running.
    pub origin: SessionOrigin,
    /// Decoded interleaved PCM, in the same shape every backend produces.
    pub audio: std::sync::mpsc::Receiver<Vec<i16>>,
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

    /// Take the decoded PCM channel.
    ///
    /// Replaced with a disconnected channel, so a second caller gets silence
    /// rather than a panic or a stolen stream.
    pub fn audio_channel(&mut self) -> std::sync::mpsc::Receiver<Vec<i16>> {
        let (_, empty) = std::sync::mpsc::channel();
        std::mem::replace(&mut self.audio, empty)
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
    // A host that just lost a client needs a moment before it will serve the
    // next one, and how long varies; back off rather than hammering it.
    const SETTLE: [u64; 3] = [0, 2, 4];

    for (attempt, settle) in SETTLE.iter().enumerate() {
        if *settle > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(*settle)).await;
        }
        // Only the first attempt honours a session the host is already
        // running. Once one has failed to deliver, rejoining it again just
        // reproduces the failure — start something new instead.
        let (launched, origin) = if attempt == 0 {
            begin(session, app_id, mode).await?
        } else {
            let _ = session.cancel().await;
            (session.launch(app_id, mode).await?, SessionOrigin::Launched)
        };
        let mut stream = connect(&launched, host_ip, mode, bitrate_kbps).await?;
        stream.origin = origin;
        if stream.wait_for_media(FIRST_MEDIA_TIMEOUT).await {
            if attempt > 0 {
                tracing::info!(attempt, "media flowing after retry");
            }
            return Ok(stream);
        }
        drop(stream);
        let _ = session.cancel().await;
        tracing::warn!(attempt, "host accepted the session but sent no media");
    }
    Err(Error::Session(
        "host accepted the session but never sent media, after three attempts".into(),
    ))
}

/// Start the app, or rejoin the one the host is already running.
///
/// Asking the host what it is doing first is the whole point: launching over
/// a session the host still holds gets a session that handshakes and never
/// streams, and resuming when nothing is running has nothing to resume.
async fn begin(
    session: &PairedSession,
    app_id: u32,
    mode: StreamMode,
) -> Result<(LaunchedSession, SessionOrigin)> {
    let running = match session.server_info().await {
        Ok(info) => info.current_game,
        Err(e) => {
            tracing::debug!(error = %e, "could not read host state; assuming idle");
            0
        }
    };
    if running == app_id {
        return Ok((session.resume(mode).await?, SessionOrigin::Rejoined));
    }
    if running != 0 {
        // Something else is running; it must stop before ours can start.
        let _ = session.cancel().await;
    }
    Ok((session.launch(app_id, mode).await?, SessionOrigin::Launched))
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
    let (audio_rx, audio_pcm) = crate::AudioReceive::new()?;
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let recovered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let datagrams = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // One socket for every media stream. A host binds a stream to whichever
    // address pinged its port, so a second socket sharing the session steals
    // the first stream's binding: video then arrives on the audio socket and
    // reads exactly like "the host sent nothing". With a single socket there
    // is nothing to steal, and both ports still get the ping they need to
    // keep the session alive.
    let video_addr = std::net::SocketAddr::new(host_ip, negotiated.video_port);
    let media = MediaSocket::bind(video_addr, negotiated.ping_payload)?;
    let audio_port = negotiated.audio_port;

    let worker_stop = stop.clone();
    let worker_counters = Counters {
        dropped: dropped.clone(),
        recovered: recovered.clone(),
        datagrams: datagrams.clone(),
    };
    std::thread::Builder::new()
        .name("moonlight-video".into())
        .spawn(move || {
            receive_media(
                media,
                audio_port,
                audio_rx,
                &frames_tx,
                &worker_stop,
                &worker_counters,
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
        input: std::sync::Arc::new(MoonlightInput {
            commands: command_tx.clone(),
            encoder: std::sync::Mutex::new(crate::InputEncoder::new()),
        }),
        dropped,
        recovered,
        datagrams,
        // Overwritten by `start`, which knows how the session began.
        origin: SessionOrigin::Launched,
        audio: audio_pcm,
        _worker: Worker {
            commands: command_tx,
            stop,
        },
    })
}

/// Video datagrams carry packet type 0; audio uses its own types.
fn is_video(datagram: &[u8]) -> bool {
    datagram.len() > 1 && datagram[1] == 0
}

/// Read datagrams, reassemble, and publish frames at their arrival time.
///
/// Arrival is stamped here, on the receive side, and never on the way out:
/// a frame stamped when it is released would make a paced present look like
/// network delay to anything reasoning about the link.
/// Counters the receive loop keeps for the shared health stats.
struct Counters {
    /// Frames the wire could not deliver whole.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity — loss that cost nothing visible.
    recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Any media datagram, used to tell streaming from silence.
    datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Counters {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn receive_media(
    mut media: MediaSocket,
    audio_port: u16,
    mut audio: crate::AudioReceive,
    frames: &tokio::sync::mpsc::UnboundedSender<BackendFrame>,
    stop: &std::sync::atomic::AtomicBool,
    counters: &Counters,
) {
    const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let clock = gsa_core::time::MediaClock::new();
    let mut depacketizer = Depacketizer::new();
    let mut buf = vec![0u8; 4096];
    let mut last_ping = std::time::Instant::now() - PING_INTERVAL;

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        if last_ping.elapsed() >= PING_INTERVAL {
            // Both ports, one socket. Skipping the audio port is not an
            // option: a host with a media stream it cannot deliver tears the
            // whole session down after ten seconds.
            if let Err(e) = media.ping().and_then(|()| media.ping_port(audio_port)) {
                tracing::warn!(error = %e, "media ping failed");
                return;
            }
            last_ping = std::time::Instant::now();
        }
        let n = match media.recv(&mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "video receive stopped");
                return;
            }
        };
        let arrival_us = clock.now_us();
        Counters::bump(&counters.datagrams);
        // Both streams share this socket, so each is routed by packet type.
        if crate::AudioReceive::owns(&buf[..n]) {
            audio.handle(&buf[..n]);
            continue;
        }
        if !is_video(&buf[..n]) {
            continue;
        }
        depacketizer.push(&buf[..n]);
        while let Some(event) = depacketizer.next_event() {
            match event {
                Received::Frame(frame) => {
                    if frame.recovered {
                        Counters::bump(&counters.recovered);
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
                    Counters::bump(&counters.dropped);
                }
                Received::Nothing => {}
            }
        }
    }
}
