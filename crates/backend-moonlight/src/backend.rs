//! Assembling a running session from the pieces.
//!
//! The backend boundary. Everything below is Moonlight-specific — launch,
//! RTSP, ENet, shards, FEC — and everything handed to the caller is
//! backend-neutral: complete access units stamped at arrival, plus a sink for
//! repair requests. The shared client core takes it from there.

use crate::codec;
use crate::host::{LaunchedSession, PairedSession, StreamMode};
use crate::{Command, Crypto, Depacketizer, MediaSocket, Received, Rtsp, StreamRequest};
use gsa_client_backend_api::{BackendFrame, InputSink, RecoverySink, SessionOrigin};
use gsa_core::media::Codec;
use gsa_core::{Error, Result};

/// Asks the host to repair the reference chain, via the control channel.
///
/// **Always asks for a keyframe**, even where the host advertises the cheaper
/// reference-invalidation path. Invalidation requires the host to name the
/// frame that is safe to resume from; this protocol has no such message, so
/// the picture stays frozen until a keyframe happens to arrive. At 8% packet
/// loss: invalidation decoded 65 of 393 frames, keyframe requests 336 of 363.
///
/// The native protocol can take the cheap path because its agent announces a
/// recovery point — the frame from which references are clean again.
#[derive(Debug)]
pub struct MoonlightRecovery {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    reference_invalidation: bool,
    /// Repair requests by kind. Counted separately because a host may answer
    /// an invalidation with a full keyframe anyway, which makes the cheap
    /// path no cheaper.
    invalidations: std::sync::Arc<std::sync::atomic::AtomicU64>,
    keyframes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MoonlightRecovery {
    /// Repair requests sent: (reference invalidations, full keyframes).
    #[must_use]
    pub fn requests(&self) -> (u64, u64) {
        (
            self.invalidations
                .load(std::sync::atomic::Ordering::Relaxed),
            self.keyframes.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

impl RecoverySink for MoonlightRecovery {
    fn request_keyframe(&self) {
        self.keyframes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.commands.send(Command::RequestIdr);
    }

    fn request_recovery(&self, last_good_frame_id: u32) {
        let _ = last_good_frame_id;
        // A keyframe by default; see the type's documentation. The opt-in
        // below exists to re-measure if a host ever gains a resume-point
        // signal.
        if self.reference_invalidation {
            self.invalidations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = self.commands.send(Command::InvalidateReferenceFrames {
                first: last_good_frame_id.wrapping_add(1),
                last: last_good_frame_id.wrapping_add(1),
            });
            return;
        }
        self.request_keyframe();
    }
}

/// Sends the embedder's input over the control channel.
///
/// The encoder is stateful — held modifiers, which pads are plugged in — so
/// it is retained here rather than rebuilt per event.
#[derive(Debug)]
pub struct MoonlightInput {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    encoder: std::sync::Mutex<crate::InputEncoder>,
}

impl InputSink for MoonlightInput {
    fn send(&self, events: Vec<gsa_client_backend_api::InputEvent>) {
        let Ok(mut encoder) = self.encoder.lock() else {
            return;
        };
        for event in &events {
            // Motion carries two sensors in one event but the wire has one
            // message per sensor, so it is expanded here rather than losing
            // half of it.
            if let gsa_client_backend_api::InputEvent::GamepadMotion {
                seat, gyro, accel, ..
            } = event
            {
                for (sensor, values) in [
                    (gsa_client_backend_api::MotionSensor::Gyro, gyro),
                    (gsa_client_backend_api::MotionSensor::Accel, accel),
                ] {
                    let message = encoder.motion_message(*seat, sensor, *values);
                    let _ = self.commands.send(Command::Input {
                        bytes: message.bytes,
                        delivery: message.delivery,
                        channel: message.channel,
                    });
                }
                continue;
            }
            if let Some(message) = encoder.encode(event) {
                // Fire-and-forget: a full queue means the session is ending,
                // and this runs on the embedder's UI thread.
                let _ = self.commands.send(Command::Input {
                    bytes: message.bytes,
                    delivery: message.delivery,
                    channel: message.channel,
                });
            }
        }
    }

    fn announce_pad(&self, seat: u8, profile: gsa_client_backend_api::GamepadProfile) {
        let Ok(mut encoder) = self.encoder.lock() else {
            return;
        };
        tracing::info!(seat, ?profile, "announcing controller to host");
        // Unplug the seat first. The host plugs a *default* pad the moment any
        // state arrives for a slot, and then ignores an arrival for a slot it
        // already has — so a snapshot that beat this call (a pad connected
        // mid-session, a second capture path) would otherwise leave the seat
        // stuck as the wrong device for the rest of the session, with motion,
        // touch and battery silently dropped. Clearing it first makes
        // announcing idempotent and recoverable rather than order-dependent.
        if let Some(clear) = encoder
            .encode(&gsa_client_backend_api::InputEvent::GamepadDisconnect { seat, ts_us: 0 })
        {
            let _ = self.commands.send(Command::Input {
                bytes: clear.bytes,
                delivery: clear.delivery,
                channel: clear.channel,
            });
        }
        let message = encoder.arrival_message(seat, profile);
        let _ = self.commands.send(Command::Input {
            bytes: message.bytes,
            delivery: message.delivery,
            channel: message.channel,
        });
    }
}

/// A live Moonlight stream, reduced to the neutral pieces.
#[derive(Debug)]
pub struct MoonlightStream {
    /// Complete access units, stamped at arrival. Claimed through
    /// [`MoonlightStream::take_frames`] rather than by move, so taking the
    /// frames cannot drop the worker guard that keeps the receive threads
    /// alive.
    frames: Option<tokio::sync::mpsc::UnboundedReceiver<BackendFrame>>,
    pub recovery: std::sync::Arc<dyn RecoverySink>,
    /// The same object, typed, for reading the repair counters.
    pub repairs: std::sync::Arc<MoonlightRecovery>,
    /// Where to send keyboard, mouse and controller input.
    pub input: std::sync::Arc<dyn InputSink>,
    /// Frames the wire could not deliver whole, for the shared health stats.
    pub dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity: loss with no visible cost.
    pub recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Video datagrams seen. The only signal that separates a streaming host
    /// from one whose handshake succeeded and which sends nothing.
    datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Whether this started the app or rejoined one already running.
    pub origin: SessionOrigin,
    /// The codec the host agreed to encode. The decoder must be built for
    /// this, not for what was asked for: a host may answer with less.
    pub codec: Codec,
    /// Decoded interleaved PCM, in the same shape every backend produces.
    pub audio: std::sync::mpsc::Receiver<Vec<i16>>,
    /// Host control-channel messages: rumble, termination, and features not
    /// acted on yet. Must be drained; ignoring it leaves the stream running
    /// but discards the host's own account of what happened.
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
    /// The field is replaced with a disconnected channel, so a second caller
    /// gets silence rather than a panic or a stolen stream.
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

    /// What of a controller this session carries.
    ///
    /// This is the *protocol's* reach, not a promise about the host at the
    /// other end: a host that emulates a lesser pad ignores what it cannot
    /// use. Motion is the exception that proves the design — it is listed
    /// here but must not be sampled until the host asks
    /// ([`gsa_client_backend_api::BackendEvent::MotionRequested`]), which is
    /// how a client avoids streaming gyro nobody consumes.
    #[must_use]
    pub fn pad_caps(&self) -> gsa_client_backend_api::PadCaps {
        use gsa_client_backend_api::PadCaps;
        PadCaps::RUMBLE
            | PadCaps::TRIGGER_RUMBLE
            | PadCaps::MOTION
            | PadCaps::TOUCHPAD
            | PadCaps::ADAPTIVE_TRIGGERS
            | PadCaps::LED
            | PadCaps::BATTERY
    }
}

/// Owns the receive threads and stops them on drop.
#[derive(Debug)]
pub struct Worker {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
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
    decode_codecs: &[Codec],
) -> Result<MoonlightStream> {
    // Upper bound on how long a healthy host takes to start sending; a slow
    // app launch is normal.
    const FIRST_MEDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    // Seconds to wait before each attempt. A host that just lost a client
    // needs a variable moment before it will serve the next one.
    const SETTLE: [u64; 3] = [0, 2, 4];

    // Read over mutual TLS: the cleartext probe understates what a host can
    // encode, so negotiating from it would settle for H.264 against a host
    // that offers better.
    let (host_codecs, running, app_version) = match session.server_info().await {
        Ok(info) => (
            codec::HostCodecs {
                modes: info.codec_mode_support,
                max_luma_hevc: info.max_luma_pixels_hevc,
            },
            info.current_game,
            info.app_version,
        ),
        Err(e) => {
            tracing::debug!(error = %e, "could not read host capabilities; assuming H.264 and idle");
            (codec::HostCodecs::default(), 0, String::new())
        }
    };
    // Which generation's session-start pair this host expects.
    let modern_start = app_version_at_least(&app_version, 7, 1, 431);
    let chosen = codec::choose(host_codecs, decode_codecs);

    for (attempt, settle) in SETTLE.iter().enumerate() {
        if *settle > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(*settle)).await;
        }
        // Only the first attempt rejoins a session the host is already
        // running: a session that failed to deliver reproduces the failure on
        // rejoin, so later attempts start a new one.
        let (launched, origin) = if attempt == 0 {
            begin(session, app_id, mode, running).await?
        } else {
            let _ = session.cancel().await;
            (session.launch(app_id, mode).await?, SessionOrigin::Launched)
        };
        let mut stream = connect(
            &launched,
            host_ip,
            mode,
            bitrate_kbps,
            chosen,
            host_codecs,
            modern_start,
        )
        .await?;
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
/// The host's state must be read first: launching over a session the host
/// still holds yields one that handshakes and never streams, and resuming
/// with nothing running has nothing to resume.
async fn begin(
    session: &PairedSession,
    app_id: u32,
    mode: StreamMode,
    running: u32,
) -> Result<(LaunchedSession, SessionOrigin)> {
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
    codec: Codec,
    host_codecs: codec::HostCodecs,
    modern_start: bool,
) -> Result<MoonlightStream> {
    // An HDR session is a request for a 10-bit profile, which is advertised
    // separately from the codec itself. The request still goes out — hosts
    // answer in SDR rather than refusing, and withholding it on a bit we may
    // have misread would be worse — but a session that comes back SDR should
    // say why here rather than look like a client fault.
    if mode.hdr && !host_codecs.supports_ten_bit(codec) {
        tracing::warn!(
            ?codec,
            host_modes = format!("{:#x}", host_codecs.modes),
            "HDR asked for, but this host advertises no 10-bit profile for the \
             negotiated codec; expect it to answer in SDR"
        );
    }
    // Both sides even, whatever the caller asked for. H.264 and HEVC carry
    // colour at half resolution, so an odd side has no whole number of chroma
    // samples. Measured against a real host, an odd height negotiated,
    // connected, played audio, and delivered no video at all — a failure with
    // no error anywhere to explain it, so the constraint is enforced here
    // rather than trusted to every caller.
    let (width, height) = (mode.width & !1, mode.height & !1);
    if (width, height) != (mode.width, mode.height) {
        tracing::warn!(
            asked = format!("{}x{}", mode.width, mode.height),
            using = format!("{width}x{height}"),
            "an odd frame size cannot be encoded; rounded down"
        );
    }

    let mut rtsp = Rtsp::new(&launched.rtsp_url)?;
    let negotiated = rtsp
        .negotiate(
            StreamRequest {
                width,
                height,
                fps: mode.fps,
                bitstream_format: codec::bitstream_format(codec),
                hdr: mode.hdr,
                bitrate_kbps,
                packet_size: 1392,
                channels: mode.channels,
            },
            modern_start,
        )
        .await?;

    tracing::info!(
        video = negotiated.video_port,
        audio = negotiated.audio_port,
        control = negotiated.control_port,
        payload = negotiated.ping_payload.is_some(),
        connect_data = ?negotiated.connect_data,
        "negotiated"
    );
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    // Say which scheme this session sealed with, and on whose word. Picking
    // wrong is invisible from here — the host drops what it cannot verify
    // without answering — so a session where nothing we send has any effect
    // looks identical to a host ignoring us. A host that supports the modern
    // scheme but does not advertise it reads here as an ancient one.
    tracing::info!(
        scheme = if negotiated.control_v2() {
            "v2"
        } else {
            "legacy"
        },
        encryption_supported = format!("{:#x}", negotiated.encryption_supported),
        "control encryption chosen from the host's advertisement"
    );
    let crypto = Crypto::new(launched.riaes_key, negotiated.control_v2());
    let control_addr = std::net::SocketAddr::new(host_ip, negotiated.control_port);
    let connect_data = negotiated.connect_data.unwrap_or(0);
    // The protocol runs here, transport-neutral; ENet is the link under it.
    let link = crate::EnetLink::connect(control_addr, connect_data)?;
    let control = crate::ControlSession::new(crypto, modern_start);
    gsa_core::runtime::spawn(async move {
        if let Err(e) = crate::drive(link, control, command_rx, event_tx).await {
            tracing::warn!(error = %e, "control channel ended");
        }
    });

    let (frames_tx, frames_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Some(layout) = &negotiated.surround {
        tracing::info!(?layout, "surround audio negotiated");
    }
    let (audio_rx, audio_pcm) = crate::AudioReceive::new(negotiated.surround.as_ref())?;
    let recovery = std::sync::Arc::new(MoonlightRecovery {
        commands: command_tx.clone(),
        // Off unless explicitly requested: it makes recovery from loss worse
        // on every host measured. The host's advertised capability is
        // recorded during negotiation, not acted on here.
        reference_invalidation: negotiated.reference_invalidation
            && std::env::var("GSA_MOONLIGHT_INVALIDATE").is_ok(),
        invalidations: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        keyframes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    });
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let recovered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let datagrams = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // One socket for every media stream, and it must stay one. A host binds a
    // stream to whichever address pinged its port, so a second socket in the
    // same session takes the first stream's binding: video then arrives on
    // the audio socket and is indistinguishable from a host sending nothing.
    // Both ports are still pinged, from this single socket, because a stream
    // the host cannot deliver tears the session down after ten seconds.
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
        codec,
        events: event_rx,
        recovery: recovery.clone(),
        repairs: recovery,
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

/// Deterministic packet-loss injection for chaos runs.
///
/// Drops a share of received datagrams before anything inspects them, which
/// exercises FEC recovery, the reference gate and the repair path against a
/// real host without degrading the network. Deterministic so a failure can be
/// re-run; off unless `GSA_MOONLIGHT_LOSS` is set.
#[derive(Debug)]
struct LossInjector {
    /// Drop probability in parts per thousand.
    per_mille: u32,
    state: u32,
    dropped: u64,
}

impl LossInjector {
    fn from_env() -> Option<Self> {
        let per_mille: u32 = std::env::var("GSA_MOONLIGHT_LOSS").ok()?.parse().ok()?;
        (per_mille > 0).then(|| {
            tracing::warn!(per_mille, "injecting packet loss for a chaos run");
            Self {
                per_mille: per_mille.min(1000),
                state: 0x2545_f491,
                dropped: 0,
            }
        })
    }

    /// True when this datagram should be discarded.
    fn drops(&mut self) -> bool {
        // xorshift from a fixed seed: cheap, and repeatable, so a chaos run
        // that finds a bug replays exactly.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        let drop = self.state % 1000 < self.per_mille;
        if drop {
            self.dropped += 1;
        }
        drop
    }
}

/// Convert the host's 90 kHz stream clock to microseconds.
///
/// Wraps with the field, which the shared clock handling already expects.
fn stream_clock_us(ticks: u32) -> u32 {
    // 90 kHz → µs is ×100/9; done in 64-bit so the multiply cannot overflow
    // before the division brings it back into range.
    ((u64::from(ticks) * 100 / 9) & u64::from(u32::MAX)) as u32
}

/// Video datagrams carry packet type 0; audio uses its own types.
fn is_video(datagram: &[u8]) -> bool {
    datagram.len() > 1 && datagram[1] == 0
}

/// Counters the receive loop keeps for the shared health stats.
struct Counters {
    /// Frames the wire could not deliver whole.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity: loss with no visible cost.
    recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Any media datagram, used to tell streaming from silence.
    datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Counters {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Read datagrams, reassemble, and publish frames stamped with their arrival.
///
/// Arrival is stamped here on the receive side, never on release: a frame
/// stamped when it is released would make paced presentation look like
/// network delay to anything reasoning about the link.
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
    let mut loss = LossInjector::from_env();
    let mut depacketizer = Depacketizer::new();
    let mut buf = vec![0u8; 4096];
    let mut last_ping = std::time::Instant::now() - PING_INTERVAL;

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        if last_ping.elapsed() >= PING_INTERVAL {
            // Both ports, one socket. The audio port cannot be skipped: a
            // host holding a media stream it cannot deliver tears the whole
            // session down after ten seconds.
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
        // Drop before anything inspects the packet, so the rest of the path
        // cannot tell an injected loss from a real one.
        if let Some(injector) = loss.as_mut()
            && injector.drops()
        {
            continue;
        }
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
                        // The host's 90 kHz stream clock, in µs. Its origin
                        // is unknown, so absolute latency from it is
                        // meaningless; the *gaps* between frames are real
                        // host-side timing, which is what the de-jitter
                        // window measures. Stamping arrival here instead
                        // would make every frame look perfectly timed and the
                        // window would never engage.
                        capture_ts_us: stream_clock_us(frame.timestamp),
                        host_latency_us: frame.host_latency_us,
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

/// Whether a host's `appversion` quad is at least `major.minor.build`.
///
/// The reference client gates its session-start pair and its control-stream
/// encryption on exactly this comparison, so the same reading has to happen
/// here: a host one build older expects a different opening exchange.
/// Unparseable versions read as older, which is the safe direction — the older
/// pair is what every host understood before the newer one existed.
fn app_version_at_least(version: &str, major: u32, minor: u32, build: u32) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let quad = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    quad >= (major, minor, build)
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn the_start_pair_follows_the_hosts_generation() {
        // The build this host reports is the exact boundary the reference
        // client uses, so it is the case worth pinning.
        assert!(super::app_version_at_least("7.1.431.-1", 7, 1, 431));
        assert!(super::app_version_at_least("7.1.432.0", 7, 1, 431));
        assert!(!super::app_version_at_least("7.1.430.0", 7, 1, 431));
        assert!(!super::app_version_at_least("7.0.999.0", 7, 1, 431));
        // A host that says nothing readable is treated as older.
        assert!(!super::app_version_at_least("", 7, 1, 431));
    }
}
