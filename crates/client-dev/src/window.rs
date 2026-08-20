//! Windowed presentation: winit + wgpu. The network/decode loop runs on its
//! own thread (with a private tokio runtime) and posts decoded frames to
//! the event loop; presentation uploads the frame as a texture and draws an
//! aspect-fit quad (GPU scaling — HiDPI handled by physical-pixel surface).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use gsa_client_core::{Client, ControlEvent, DecodedFrame, PixelOrder};
use gsa_core::id::SourceId;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::decoder::{DisplayMapping, make_decoder};
use crate::gamepad_capture::GamepadCapture;

/// Gamepad poll period. Controllers are read on the event-loop thread — gilrs
/// wants the thread with the platform run loop — so the loop wakes on a timer
/// rather than only when a frame or a key arrives. 250 Hz keeps stick motion
/// well under the frame interval without spinning.
const GAMEPAD_POLL: std::time::Duration = std::time::Duration::from_millis(4);

#[derive(Debug)]
enum AppEvent {
    /// Session is streaming: where input goes, the quality controls the
    /// backend actually supports (`None` when it has none), and the starting
    /// bitrate (bps).
    Ready(
        std::sync::Arc<dyn gsa_client_core::InputSink>,
        Option<std::sync::Arc<dyn gsa_client_core::SessionKnobs>>,
        u32,
    ),
    Frame(Box<DecodedFrame>),
    /// Rolling received video goodput (Mb/s), for the title HUD.
    RecvMbps(Option<f64>),
    /// What the decoder is actually producing, once it has configured itself.
    VideoFormat(gsa_client_core::VideoFormat),
    /// Agent-pushed notification (e.g. host confirmed the virtual pad plugged).
    Notification(ControlEvent),
    /// The host asked for motion samples at this rate. Sampling starts here
    /// and not before — an unasked motion stream is spent battery.
    MotionRequested {
        rate_hz: u16,
    },
    /// The host asked the pad to rumble; a zero pair is a stop.
    Rumble {
        low: u16,
        high: u16,
    },
    StreamEnded(String),
}

/// A bottom-of-screen toast that slides in, holds, and slides out. The
/// client-dev take on the reusable notification surface — a colored bar
/// (green = connected, grey = disconnected); the window title carries the text.
struct Toast {
    connected: bool,
    at: Instant,
    text: String,
}

impl Toast {
    const IN: f32 = 0.18;
    const HOLD: f32 = 2.0;
    const OUT: f32 = 0.30;
    const TOTAL: f32 = Self::IN + Self::HOLD + Self::OUT;

    /// 0 = fully hidden (below the screen), 1 = fully shown.
    fn slide(&self) -> f32 {
        let t = self.at.elapsed().as_secs_f32();
        if t < Self::IN {
            (t / Self::IN).clamp(0.0, 1.0)
        } else if t < Self::IN + Self::HOLD {
            1.0
        } else {
            (1.0 - (t - Self::IN - Self::HOLD) / Self::OUT).clamp(0.0, 1.0)
        }
    }

    fn expired(&self) -> bool {
        self.at.elapsed().as_secs_f32() > Self::TOTAL
    }

    fn color(&self) -> [f32; 4] {
        if self.connected {
            [0.16, 0.55, 0.24, 0.92]
        } else {
            [0.35, 0.35, 0.38, 0.92]
        }
    }
}

pub fn run(
    addr: std::net::SocketAddr,
    source: Option<String>,
    force_sw: bool,
    auth: crate::pairing::Auth,
) -> Result<()> {
    let event_loop = EventLoop::<AppEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    std::thread::Builder::new()
        .name("gsa-client-net".into())
        .spawn(move || network_loop(addr, source, force_sw, auth, &proxy))?;

    let mut app = App::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Drive a synthetic controller so protocol work does not depend on hardware.
///
/// Sends a pad snapshot at a plausible rate with one button cycling, which is
/// enough to make the host plug a virtual pad and reveal what it sends back.
/// The returned guard stops the thread when the session ends.
fn spawn_synthetic_pad(input: std::sync::Arc<dyn gsa_client_core::InputSink>) -> SyntheticPad {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = stop.clone();
    // Motion starts only when the host asks, so the sampling rate is the
    // host's choice and an unasked stream is never sent.
    let motion_hz = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let thread_motion = motion_hz.clone();
    let handle = std::thread::spawn(move || {
        // Announce before sending state: a host that has not been told what
        // the pad is builds a plain one and then silently drops everything
        // richer than buttons.
        input.announce_pad(
            0,
            gsa_client_core::GamepadProfile::new(
                gsa_client_core::PadKind::DualSense,
                gsa_client_core::PadCaps::RUMBLE
                    | gsa_client_core::PadCaps::TRIGGER_RUMBLE
                    | gsa_client_core::PadCaps::MOTION
                    | gsa_client_core::PadCaps::TOUCHPAD
                    | gsa_client_core::PadCaps::LED
                    | gsa_client_core::PadCaps::BATTERY,
            ),
        );
        let mut tick = 0u32;
        while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
            // A button held for half a second, then released: a game (and the
            // host's own logs) sees a real pad rather than a frozen one.
            let buttons = if (tick / 30).is_multiple_of(2) {
                gsa_protocol::input::gamepad::A
            } else {
                0
            };
            input.send(vec![gsa_client_core::InputEvent::Gamepad(
                gsa_protocol::input::GamepadInput {
                    seat: 0,
                    buttons,
                    axes: [0; 8],
                    ts_us: 0,
                },
            )]);

            // Motion at the rate the host asked for, sampled between pad
            // snapshots. Values sweep so a host-side viewer sees movement
            // rather than a pad lying perfectly still.
            let hz = thread_motion.load(std::sync::atomic::Ordering::Relaxed);
            if hz > 0 {
                let per_frame = (hz / 60).max(1);
                for _ in 0..per_frame {
                    let phase = f32::from(tick as u16 % 360) * std::f32::consts::PI / 180.0;
                    input.send(vec![gsa_client_core::InputEvent::GamepadMotion {
                        seat: 0,
                        // Degrees per second.
                        gyro: [phase.sin() * 90.0, phase.cos() * 90.0, 0.0],
                        // m/s², including gravity: a pad at rest is not zero.
                        accel: [0.0, 9.81, 0.0],
                        ts_us: 0,
                    }]);
                }
            }
            // A touchpad swipe and a battery report, once, a couple of
            // seconds in: enough to prove the host accepts both without
            // flooding the log with them.
            if tick == 120 {
                for (step, phase) in [
                    (0.0, gsa_protocol::input::TouchPhase::Down),
                    (0.5, gsa_protocol::input::TouchPhase::Move),
                    (1.0, gsa_protocol::input::TouchPhase::Up),
                ] {
                    input.send(vec![gsa_client_core::InputEvent::GamepadTouch {
                        seat: 0,
                        pointer: 0,
                        phase,
                        x: step,
                        y: 0.5,
                        pressure: 1.0,
                        ts_us: 0,
                    }]);
                }
                input.send(vec![gsa_client_core::InputEvent::GamepadBattery {
                    seat: 0,
                    state: gsa_protocol::input::BatteryState::Discharging,
                    percent: Some(77),
                    ts_us: 0,
                }]);
            }
            tick = tick.wrapping_add(1);
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    });
    SyntheticPad {
        stop,
        motion_hz,
        handle: Some(handle),
    }
}

/// Stops the synthetic pad when dropped, so the session teardown is clean.
struct SyntheticPad {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Sampling rate the host asked for; 0 until it does.
    motion_hz: std::sync::Arc<std::sync::atomic::AtomicU32>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SyntheticPad {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Present a Moonlight host's stream in the same window (spec 16).
///
/// The point of this path is that only the *source* differs: frames arrive
/// from a different protocol, and everything after — the reference gate,
/// de-jitter release, decode, and presentation — is the same code the gsa
/// backend runs. A picture here is evidence the seam holds.
#[allow(clippy::too_many_arguments, reason = "dev harness flags, not an API")]
pub fn run_moonlight(
    addr: std::net::SocketAddr,
    app_id: u32,
    bitrate_mbps: u32,
    force_sw: bool,
    seconds: u64,
    synthetic_pad: bool,
    pad_kind: Option<&str>,
    dump_frame: Option<std::path::PathBuf>,
    codecs: &[String],
    mode: &str,
    host_mode_change: bool,
    hdr: bool,
    mapping: DisplayMapping,
    present_mode: &str,
    jitter: Option<crate::netsim::Jitter>,
    pacing: gsa_client_core::PacingMode,
    input_script: Option<Vec<crate::script::Step>>,
    dejitter: bool,
    float_window: bool,
    chase_refresh: bool,
) -> Result<()> {
    let offered = crate::decoder::offered_codecs(codecs, force_sw);
    let mut mode = parse_mode(mode, host_mode_change)?;
    // A request, not a guarantee: what actually arrives is reported per
    // session, since a host may answer in SDR without saying so.
    mode.hdr = hdr;
    tracing::info!(hdr, "HDR requested");
    let vsync = present_mode != "nosync";
    tracing::info!(vsync, "presentation mode");
    // One mode asks the host for less than the display can show, so the two
    // cadences cannot beat against each other. Applied before the launch, as
    // the rate is fixed at negotiation.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let display_hz = display_refresh_hz().map(|hz| hz.round() as u32);
    let limited = pacing.requested_fps(mode.fps, display_hz);
    if limited != mode.fps {
        tracing::info!(
            asked = mode.fps,
            using = limited,
            display_hz,
            "staying below the display's rate for this pacing mode"
        );
        mode.fps = limited;
    }
    report_rate_match(mode.fps);
    let event_loop = EventLoop::<AppEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    std::thread::Builder::new()
        .name("gsa-moonlight-net".into())
        .spawn(move || {
            moonlight_loop(
                addr,
                MoonlightRun {
                    app_id,
                    bitrate_mbps,
                    force_sw,
                    seconds,
                    synthetic_pad,
                    dump_frame,
                    offered,
                    mode,
                    mapping,
                    jitter,
                    pacing,
                    input_script,
                    dejitter,
                },
                &proxy,
            )
        })?;

    let mut app = App {
        pad_kind_override: pad_kind.and_then(parse_pad_kind),
        vsync,
        pacing,
        chase_refresh,
        window_level: if float_window {
            winit::window::WindowLevel::AlwaysOnTop
        } else {
            winit::window::WindowLevel::Normal
        },
        ..App::default()
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// The panel's own refresh rate, unrounded — what a rate match is judged
/// against. `None` when the system will not say.
fn display_refresh_hz() -> Option<f64> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGMainDisplayID};
    let rate = CGDisplayCopyDisplayMode(CGMainDisplayID()).map_or(0.0, |mode| {
        objc2_core_graphics::CGDisplayMode::refresh_rate(Some(&mode))
    });
    // Built-in panels report zero rather than their real rate.
    (rate >= 1.0).then_some(rate)
}

/// Say whether the stream's rate and the panel's can agree, and what a
/// variable-rate display would be asked for if not.
///
/// Reported per session because it is a property of the pairing, not of the
/// link: it holds on a perfect network and no amount of pacing removes it.
fn report_rate_match(content_fps: u32) {
    let Some(display_hz) = display_refresh_hz() else {
        tracing::info!("the display will not report its refresh rate; rate match unknown");
        return;
    };
    let m = crate::present::RateMatch::new(display_hz, f64::from(content_fps));
    if m.fits() {
        tracing::info!(
            display_hz = format!("{display_hz:.2}"),
            content_fps,
            refreshes_per_frame = m.refreshes_per_frame,
            "stream and display rates fit; no frame is ever held over"
        );
    } else {
        tracing::warn!(
            display_hz = format!("{display_hz:.2}"),
            content_fps,
            beat_period_s = m.beat_period_s.map(|s| format!("{s:.1}")),
            would_fit_at_hz = format!("{:.2}", m.suggested_display_hz()),
            "stream and display rates do not divide; a frame is held over on \
             that period, which is judder no pacing can remove"
        );
    }
}

/// A pad family named on the command line, for announcing something other than
/// what is really plugged in.
fn parse_pad_kind(name: &str) -> Option<gsa_client_core::PadKind> {
    match name {
        "xbox" => Some(gsa_client_core::PadKind::Xbox),
        "dualsense" => Some(gsa_client_core::PadKind::DualSense),
        "dualshock" => Some(gsa_client_core::PadKind::DualShock4),
        "generic" => Some(gsa_client_core::PadKind::Generic),
        // "auto", and anything unrecognised, announces the pad as it is.
        _ => None,
    }
}

#[allow(clippy::too_many_arguments, reason = "dev harness flags, not an API")]
/// Read `WIDTHxHEIGHT@FPS`, or `auto` for this display's own geometry.
fn parse_mode(text: &str, host_mode_change: bool) -> Result<gsa_backend_moonlight::StreamMode> {
    let (width, height, fps) = if text.eq_ignore_ascii_case("auto") {
        primary_display_mode().unwrap_or_else(|| {
            tracing::warn!("could not read this display; asking for 1080p60");
            (1920, 1080, 60)
        })
    } else {
        let (size, fps) = text.split_once('@').unwrap_or((text, "60"));
        let (width, height) = size
            .split_once(['x', 'X'])
            .context("mode must look like 1920x1080@60")?;
        (
            width.parse().context("mode width")?,
            height.parse().context("mode height")?,
            fps.parse().context("mode fps")?,
        )
    };

    // Both sides even, whether they came from a display or from the command
    // line. H.264 and HEVC carry colour at half resolution, so an odd side has
    // no whole number of chroma samples: measured against a real host, a
    // request for 2556x1179 negotiated, connected, played audio, and delivered
    // no video at all.
    let (width, height) = (width & !1, height & !1);
    tracing::info!(width, height, fps, "asking the host for this mode");
    Ok(gsa_backend_moonlight::StreamMode {
        width,
        height,
        fps,
        allow_host_mode_change: host_mode_change,
        ..Default::default()
    })
}

/// This display's size and refresh, for `--moonlight-mode auto`.
///
/// Read from Core Graphics rather than the window system: the mode has to be
/// known before the session starts, and winit only exposes monitors once its
/// event loop is running.
fn primary_display_mode() -> Option<(u32, u32, u32)> {
    use objc2_core_graphics::{
        CGDisplayCopyDisplayMode, CGDisplayPixelsHigh, CGDisplayPixelsWide, CGMainDisplayID,
    };
    let display = CGMainDisplayID();
    let width = CGDisplayPixelsWide(display) as u32;
    let height = CGDisplayPixelsHigh(display) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    // Built-in panels report 0 rather than their real rate; 60 is the safe
    // reading, and asking for more than the panel does buys nothing.
    let rate: f64 = CGDisplayCopyDisplayMode(display).map_or(0.0, |mode| {
        objc2_core_graphics::CGDisplayMode::refresh_rate(Some(&mode))
    });
    let fps = if rate >= 1.0 { rate.round() as u32 } else { 60 };
    Some((width, height, streamable_fps(fps)))
}

/// The rate to ask a host for, given a display that runs at `panel_hz`.
///
/// A panel's own rate is not a sensible request. A 240 Hz monitor asks a host
/// to encode 240 frames a second — work no game produces and no link carries,
/// paid for in encoder time and bitrate that would otherwise buy quality. The
/// request is snapped down to a rate hosts actually offer.
fn streamable_fps(panel_hz: u32) -> u32 {
    const OFFERED: [u32; 4] = [120, 90, 60, 30];
    OFFERED
        .into_iter()
        .find(|&rate| rate <= panel_hz)
        .unwrap_or(30)
}

/// Walk a script: press what it says, wait what it says, and save a frame
/// after every step.
///
/// A keypress is a down and an up with a gap between: an application that
/// samples input on a timer can miss a press and release in the same instant,
/// and a menu that misses one keypress walks somewhere else entirely.
fn run_script(
    steps: &[crate::script::Step],
    input: &std::sync::Arc<dyn gsa_client_core::InputSink>,
    shot_request: &std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    shots_dir: Option<&Path>,
) {
    use crate::script::Step;
    const KEY_HELD: std::time::Duration = std::time::Duration::from_millis(60);
    /// A frame decoded right after a keypress still shows the old screen, so
    /// the shot waits for the application to react.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

    tracing::info!(
        steps = steps.len(),
        runtime_s = crate::script::duration(steps).as_secs(),
        "driving the session from a script"
    );
    for (index, step) in steps.iter().enumerate() {
        match *step {
            Step::Wait(d) => std::thread::sleep(d),
            Step::Key(usage) => {
                let now = || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_micros() as u64)
                };
                input.send(vec![gsa_client_core::InputEvent::Key {
                    usage,
                    down: true,
                    ts_us: now(),
                }]);
                std::thread::sleep(KEY_HELD);
                input.send(vec![gsa_client_core::InputEvent::Key {
                    usage,
                    down: false,
                    ts_us: now(),
                }]);
                tracing::info!(step = index + 1, key = step.slug(), "script key");
            }
            Step::Shot => {}
        }
        if let Some(dir) = shots_dir {
            std::thread::sleep(SETTLE);
            let name = format!("step-{:02}-{}.bmp", index + 1, step.slug());
            if let Ok(mut slot) = shot_request.lock() {
                *slot = Some(dir.join(name));
            }
        }
    }
    tracing::info!("script finished");
}

/// Which decoded frame `--dump-frame` writes.
const DUMP_AT_FRAME: u64 = 120;

/// What a Moonlight run needs that is not the host address.
struct MoonlightRun {
    app_id: u32,
    bitrate_mbps: u32,
    force_sw: bool,
    seconds: u64,
    synthetic_pad: bool,
    dump_frame: Option<std::path::PathBuf>,
    offered: Vec<gsa_core::media::Codec>,
    mode: gsa_backend_moonlight::StreamMode,
    mapping: DisplayMapping,
    /// An imposed bad link, for exercising pacing on a LAN that has none.
    jitter: Option<crate::netsim::Jitter>,
    /// The latency-for-smoothness trade this run makes.
    pacing: gsa_client_core::PacingMode,
    /// A timed sequence to drive the session with, instead of a person.
    input_script: Option<Vec<crate::script::Step>>,
    /// Whether to smooth the imposed jitter — the control half of the A/B.
    dejitter: bool,
}

fn moonlight_loop(addr: std::net::SocketAddr, run: MoonlightRun, proxy: &EventLoopProxy<AppEvent>) {
    let MoonlightRun {
        app_id,
        bitrate_mbps,
        force_sw,
        seconds,
        synthetic_pad,
        dump_frame,
        offered,
        mode,
        mapping,
        jitter,
        pacing,
        input_script,
        dejitter,
    } = run;
    let outcome = (|| -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("client runtime")?;
        runtime.block_on(async {
            let mut session = crate::moonlight::paired_session(addr).await?;
            let mut stream = gsa_backend_moonlight::start(
                &mut session,
                addr.ip(),
                app_id,
                mode,
                bitrate_mbps.saturating_mul(1000),
                &offered,
            )
            .await
            .context("start moonlight session")?;

            // Everything from here is the shared core.
            let frames = stream.take_frames().context("frames already taken")?;
            let frames = match jitter {
                Some(jitter) => {
                    tracing::info!("imposing extra delay on every frame (netsim)");
                    crate::netsim::delayed(frames, jitter, gsa_core::time::MediaClock::new())
                }
                None => frames,
            };
            let mut core = gsa_client_core::StreamSession::with_capture_clock(
                frames,
                stream.recovery.clone(),
                gsa_core::time::MediaClock::new(),
                gsa_client_core::ClockSync::default(),
                stream.dropped.clone(),
                stream.recovered.clone(),
                // The host's stamps are a stream clock, so latency figures
                // from them would be fiction; cadence and jitter are real.
                gsa_client_core::CaptureClock::StreamPts,
            );

            core.set_pacing(pacing);
            core.dejitter_flag()
                .store(dejitter, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(dejitter, "de-jitter");

            // Play whatever audio arrives. The host may send none — that is a
            // host-side condition, not a client failure — so video continues
            // regardless.
            let _audio = crate::audio_playback::start(stream.audio_channel())
                .inspect_err(|e| tracing::warn!(error = %e, "audio playback unavailable"))
                .ok();

            // Input goes over the same control channel; the host exposes no
            // live quality knobs, so none are offered rather than shown and
            // silently ignored.
            let _ = proxy.send_event(AppEvent::Ready(
                stream.input.clone(),
                None,
                bitrate_mbps.saturating_mul(1_000_000),
            ));

            // Drive the session from a script when one was given, and save a
            // frame after every step: a sequence that walked the wrong menu
            // has to show *which* step went wrong, not merely that one did.
            let shot_request: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let _script = input_script.clone().map(|steps| {
                let input = stream.input.clone();
                let requests = shot_request.clone();
                let shots_dir = dump_frame
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf));
                std::thread::Builder::new()
                    .name("gsa-input-script".into())
                    .spawn(move || run_script(&steps, &input, &requests, shots_dir.as_deref()))
            });

            // A synthetic pad, for probing what the host does once a controller
            // exists. Real pads come from `GamepadCapture`; this exists so
            // protocol work does not wait on hardware being awake.
            let _pad = synthetic_pad.then(|| spawn_synthetic_pad(stream.input.clone()));

            let mut decoder = make_decoder(force_sw, stream.codec, mapping)?;
            let mut frames = 0u64;
            // The decoder cannot answer until a keyframe has configured it,
            // and can change answer if the stream reconfigures mid-session.
            let mut reported_format: Option<gsa_client_core::VideoFormat> = None;
            let deadline = (seconds > 0)
                .then(|| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
            let result = loop {
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break Ok(());
                }
                let Some(out) = core.recv_frame(decoder.as_mut()).await? else {
                    break Ok(());
                };
                frames += 1;
                // Not the first frame: a session often opens on a black
                // desktop, which looks exactly like a decode producing
                // nothing. By here the host has been sending for a while.
                if frames == DUMP_AT_FRAME
                    && let Some(path) = dump_frame.as_deref()
                    && let Err(e) = crate::frame_dump::write_bmp(&out.frame, path)
                {
                    tracing::warn!(error = %e, "could not write the decoded frame");
                }
                // A script asked for a frame; the next decoded one answers it.
                let wanted = shot_request.lock().ok().and_then(|mut slot| slot.take());
                if let Some(path) = wanted {
                    match crate::frame_dump::write_bmp(&out.frame, &path) {
                        Ok(()) => tracing::info!(path = %path.display(), "script shot"),
                        Err(e) => tracing::warn!(error = %e, "could not write the script shot"),
                    }
                }
                // Drain the host's own messages. The channel is unbounded, and
                // what arrives on it is the only record of what the host did
                // with the pad we announced.
                while let Ok(message) = stream.events.try_recv() {
                    tracing::info!(?message, "host control message");
                    // The pad is owned by the event-loop thread, so anything
                    // it has to play crosses over rather than being touched
                    // from here.
                    if let Some(gsa_client_core::BackendEvent::Feedback(
                        gsa_client_core::GamepadFeedback::Rumble { low, high, .. },
                    )) = message.neutral()
                    {
                        let _ = proxy.send_event(AppEvent::Rumble { low, high });
                    }
                    // Motion is opt-in: sampling starts here and not before.
                    if let Some(gsa_client_core::BackendEvent::MotionRequested {
                        rate_hz, ..
                    }) = message.neutral()
                    {
                        if let Some(pad) = &_pad {
                            pad.motion_hz
                                .store(u32::from(rate_hz), std::sync::atomic::Ordering::Relaxed);
                        }
                        // A physical pad is read on the event-loop thread, so
                        // the request has to cross over to it.
                        let _ = proxy.send_event(AppEvent::MotionRequested { rate_hz });
                    }
                }
                // The display path is the source of presentation truth; without
                // this the health stats would report every frame as unshown.
                core.frame_presented(out.capture_ts_us);
                if frames.is_multiple_of(30)
                    && let Some(format) = decoder.video_format()
                    && reported_format.as_ref() != Some(&format)
                {
                    reported_format = Some(format.clone());
                    let _ = proxy.send_event(AppEvent::VideoFormat(format));
                }
                if frames.is_multiple_of(120) {
                    let present = core.present_stats();
                    let stats = core.stats();
                    tracing::info!(
                        frames,
                        video = decoder
                            .video_format()
                            .map_or_else(|| "—".to_string(), |f| f.label()),
                        present_fps = f64::from(present.fps_x100) / 100.0,
                        low1_fps = f64::from(present.low1_fps_x100) / 100.0,
                        freezes = present.freezes,
                        stutters = present.stutters,
                        // Cadence breaks already present in the host's own
                        // capture stamps: the source hitched, we only carried
                        // it. Separates "fix the client" from "cannot".
                        src_stutters = present.src_stutters,
                        latency_absolute = core.latency_is_absolute(),
                        // The de-jitter's own signal. A flat zero means it has
                        // never measured anything, not that the link is clean.
                        // In: the spread the link delivered. Out: the spread
                        // after pacing. The pair is the only honest way to say
                        // whether the smoothing did anything.
                        jitter_in_us = core.jitter_us(),
                        jitter_out_us = core.released_jitter_us(),
                        // The price of that smoothing, invisible downstream.
                        mean_hold_us = core.mean_hold_us(),
                        frame_interval_us = core.frame_interval_us(),
                        dejitter_duty = {
                            let (ran, skipped) = core.dejitter_duty();
                            format!("{ran} paced / {skipped} skipped")
                        },
                        dropped = stats.frames_dropped_incomplete,
                        recovered = stats.frames_recovered,
                        "moonlight stream stats"
                    );
                }
                if proxy
                    .send_event(AppEvent::Frame(Box::new(out.frame)))
                    .is_err()
                {
                    break Ok(()); // window closed
                }
            };
            // Always tear the host session down, however this ended: leaving
            // one behind is what makes the next attempt fail with no picture.
            drop(core);
            drop(stream);
            let _ = session.cancel().await;
            result
        })
    })();

    let message = match outcome {
        Ok(()) => "stream ended".to_owned(),
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "moonlight session ended");
            format!("{e:#}")
        }
    };
    let _ = proxy.send_event(AppEvent::StreamEnded(message));
}

fn network_loop(
    addr: std::net::SocketAddr,
    source: Option<String>,
    force_sw: bool,
    auth: crate::pairing::Auth,
    proxy: &EventLoopProxy<AppEvent>,
) {
    let outcome = (|| -> Result<()> {
        // Multi-threaded so the input-writer task runs on its own worker,
        // independent of the frame-receive loop — otherwise, while parked
        // on `read_datagram` (idle screen = no frames), queued input isn't
        // flushed until the next frame wakes the runtime, delivering
        // keystrokes in a delayed burst.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("client runtime")?;
        runtime.block_on(async {
            let mut client = Client::connect(
                addr,
                "client-dev",
                crate::decoder::decoder_max_profile(force_sw),
                &[gsa_core::media::Codec::H264],
                auth.server_auth(),
            )
            .await?;
            let sources = client.list_sources().await?;
            tracing::info!("available sources:\n{}", crate::source_list(&sources));
            let source = crate::pick_source(&sources, source.as_deref())?;
            let params = client
                .start_session(SourceId(source.id.0), None, None, false)
                .await?;

            if let Some(sender) = client.take_input_sender() {
                let sender = std::sync::Arc::new(sender);
                let _ = proxy.send_event(AppEvent::Ready(
                    sender.clone(),
                    Some(sender),
                    params.bitrate_bps,
                ));
            }

            // Start audio playback; keep `_audio` alive for the session. Video
            // continues if the client has no audio device.
            let _audio = match client.take_audio_output() {
                Ok(rx) => crate::audio_playback::start(rx)
                    .inspect_err(|e| tracing::warn!(error = %e, "audio playback unavailable"))
                    .ok(),
                Err(e) => {
                    tracing::warn!(error = %e, "no audio output");
                    None
                }
            };

            // Host-pushed notifications (gamepad plugged, etc.) arrive on the
            // control stream, interleaved with frames.
            let mut control_rx = client.take_control_events();

            // The agent path negotiates its own codec; it offers H.264 today.
            let mut decoder = make_decoder(
                force_sw,
                gsa_core::media::Codec::H264,
                DisplayMapping::default(),
            )?;
            let mut frames = 0u64;
            // Latest agent-reported telemetry (target/emit Mb/s, ABR state), for the log.
            let mut target_mbps: Option<f64> = None;
            let mut emit_mbps: Option<f64> = None;
            let mut abr_on: Option<bool> = None;
            loop {
                tokio::select! {
                    frame = client.recv_frame(decoder.as_mut()) => {
                        let Some(out) = frame? else { break };
                        frames += 1;
                        // Push the rolling received bitrate to the HUD a few
                        // times a second; log the full stats less often.
                        if frames.is_multiple_of(30) {
                            let _ = proxy.send_event(AppEvent::RecvMbps(client.stats().recv_mbps));
                        }
                        if frames.is_multiple_of(300) {
                            let s = client.stats();
                            tracing::info!(
                                frames,
                                abr = ?abr_on,
                                target_mbps = ?target_mbps,
                                emit_mbps = ?emit_mbps,
                                recv_mbps = ?s.recv_mbps,
                                recent_p50 = ?s.recent_latency_ms_p50,
                                recent_p99 = ?s.recent_latency_ms_p99,
                                latency_ms_p50 = ?s.latency_ms_p50,
                                latency_ms_p99 = ?s.latency_ms_p99,
                                decode_ms_p50 = ?s.decode_ms_p50,
                                dropped = s.frames_dropped_incomplete,
                                "stream stats"
                            );
                        }
                        if proxy
                            .send_event(AppEvent::Frame(Box::new(out.frame)))
                            .is_err()
                        {
                            break; // window closed
                        }
                    }
                    event = async {
                        match &mut control_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<ControlEvent>>().await,
                        }
                    } => {
                        if let Some(event) = event {
                            if let ControlEvent::EncodeStats {
                                target_bitrate_bps,
                                emitted_bitrate_bps,
                                abr_enabled,
                                ..
                            } = event
                            {
                                target_mbps = Some(f64::from(target_bitrate_bps) / 1_000_000.0);
                                emit_mbps = Some(f64::from(emitted_bitrate_bps) / 1_000_000.0);
                                abr_on = Some(abr_enabled);
                            }
                            let _ = proxy.send_event(AppEvent::Notification(event));
                        }
                    }
                }
            }
            Ok(())
        })
    })();
    let message = match outcome {
        Ok(()) => "stream ended".to_string(),
        Err(e) => format!("stream failed: {e:#}"),
    };
    let _ = proxy.send_event(AppEvent::StreamEnded(message));
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    latest: Option<Box<DecodedFrame>>,
    /// Wait for the display's refresh, as a real device does. Off presents
    /// immediately, which measures delivery but not what anyone would see.
    vsync: bool,
    /// When the frame in `latest` became ready, and whether it has reached the
    /// screen — the two facts the display tax is computed from.
    latest_ready_at: Option<std::time::Instant>,
    latest_shown: bool,
    presentation: crate::present::PresentLedger,
    /// Redraws that reached no display, because the window is hidden.
    occluded: u64,
    /// The trade in force, which decides whether an unshown frame may be
    /// discarded when a newer one arrives.
    pacing: gsa_client_core::PacingMode,
    /// Frames waiting to be shown, when the mode refuses to drop any. Stays
    /// empty in every other mode, where only the newest is kept.
    pending: std::collections::VecDeque<Box<DecodedFrame>>,
    /// Redraw on every refresh rather than only when a frame arrives. Shows
    /// repeats honestly, at the cost of looking like a max-rate client to a
    /// variable-refresh display.
    chase_refresh: bool,
    /// Where the window sits in the stack. On top by default: this is a
    /// measuring instrument, and one that can be covered measures nothing.
    window_level: winit::window::WindowLevel,
    /// Stream size the window has already been sized to, so a resize happens
    /// once per geometry rather than on every frame.
    fitted: Option<(u32, u32)>,
    input: Option<std::sync::Arc<dyn gsa_client_core::InputSink>>,
    /// Live quality controls, when the backend has any.
    knobs: Option<std::sync::Arc<dyn gsa_client_core::SessionKnobs>>,
    /// Presented content rect (letterboxed), for normalizing cursor coords.
    content_rect: Option<(f32, f32, f32, f32)>,
    gamepad: Option<GamepadCapture>,
    /// The platform's own gamepad framework, when it can see the controller.
    /// It reports motion, a touch surface and battery, which the portable
    /// path cannot; it takes precedence and `gamepad` stays unused.
    #[cfg(target_os = "macos")]
    platform_pad: Option<crate::gamepad_gc::GcCapture>,
    /// Whether this pad has been announced to the host. Nothing richer than
    /// buttons works until it has been.
    pad_announced: bool,
    /// Non-zero once the host asks for motion.
    motion_hz: u16,
    /// Announce this family rather than the pad's own, for interop testing.
    pad_kind_override: Option<gsa_client_core::PadKind>,
    /// Controllers reported as ignored, so the warning is said once per count
    /// rather than every poll.
    reported_extra_pads: usize,
    toast: Option<Toast>,
    /// Client-side view of the live encode bitrate (bps), stepped by the [ / ]
    /// dev keybinds to exercise the manual bitrate knob (spec 04 ABR actuator).
    bitrate_bps: u32,
    /// Rolling received video goodput (Mb/s) from client-core stats, for the HUD.
    recv_mbps: Option<f64>,
    /// Agent-reported emitted bitrate (Mb/s) — the encoder's actual output.
    emitted_mbps: Option<f64>,
    /// Whether server-side ABR is on (toggled with `\`).
    abr_on: bool,
    /// Depth and range as the decoder reports them, for the title HUD.
    video_format: Option<gsa_client_core::VideoFormat>,
}

impl App {
    /// Refresh the window title with the live target bitrate (a lightweight HUD,
    /// since there's no on-screen text renderer) plus any active toast text.
    /// Size the window to the stream, once, so nothing is letterboxed.
    ///
    /// The renderer aspect-fits, so a window whose shape differs from the
    /// stream's shows margins — which is most of the time now that the host is
    /// asked for a client's own geometry. Matching the shape removes them, and
    /// at a size the display can hold it is also a pixel-for-pixel view of
    /// what the host sent, with no resampling in the way.
    fn fit_window_to(&mut self, width: u32, height: u32) {
        if self.fitted == Some((width, height)) {
            return;
        }
        self.fitted = Some((width, height));
        let Some(window) = self.window.clone() else {
            return;
        };

        // Leave room for the menu bar and dock rather than filling the screen:
        // a window larger than its display cannot be sized to fit at all.
        let (limit_w, limit_h) = window.current_monitor().map_or((1920, 1080), |m| {
            let size = m.size();
            (size.width * 9 / 10, size.height * 9 / 10)
        });
        let scale = f64::from(limit_w) / f64::from(width);
        let scale = scale.min(f64::from(limit_h) / f64::from(height)).min(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let wanted = winit::dpi::PhysicalSize::new(
            (f64::from(width) * scale) as u32,
            (f64::from(height) * scale) as u32,
        );
        tracing::info!(
            stream = format!("{width}x{height}"),
            window = format!("{}x{}", wanted.width, wanted.height),
            "sizing the window to the stream"
        );
        let _ = window.request_inner_size(wanted);
    }

    /// Say what the display did, every couple of seconds.
    ///
    /// Reported from the event loop rather than the network thread, because
    /// only this side knows when a frame reached the screen — which is the
    /// whole point of the measurement.
    fn report_presentation(&mut self) {
        const EVERY: u64 = 120;
        // Counted on frames arriving rather than frames shown, so a covered
        // window still reports the delivery cadence it can measure.
        let seen = self.presentation.ready;
        if seen == 0 || !seen.is_multiple_of(EVERY) {
            return;
        }
        let Some(s) = self.presentation.summary() else {
            return;
        };
        tracing::info!(
            vsync = self.vsync,
            display_p50_ms = format!("{:.2}", f64::from(s.interval_p50_us) / 1000.0),
            frame_p50_ms = format!("{:.2}", f64::from(s.frame_p50_us) / 1000.0),
            frame_p99_ms = format!("{:.2}", f64::from(s.frame_p99_us) / 1000.0),
            frame_spread_ms = format!("{:.2}", f64::from(s.frame_spread_us) / 1000.0),
            ready_p50_ms = format!("{:.2}", f64::from(s.ready_p50_us) / 1000.0),
            ready_spread_ms = format!("{:.2}", f64::from(s.ready_spread_us) / 1000.0),
            wait_p50_ms = format!("{:.2}", f64::from(s.wait_p50_us) / 1000.0),
            wait_p99_ms = format!("{:.2}", f64::from(s.wait_p99_us) / 1000.0),
            repeats_pct = format!("{:.1}", s.repeat_pct()),
            unshown_pct = format!("{:.1}", s.superseded_pct()),
            ready = s.ready,
            presented = s.presented,
            "presentation"
        );
    }

    fn update_title(&self) {
        let Some(w) = &self.window else { return };
        let mbps = f64::from(self.bitrate_bps) / 1_000_000.0;
        let mut title = format!("gsa client-dev — target {mbps:.1} Mbps");
        if let Some(emit) = self.emitted_mbps {
            title.push_str(&format!(" · emit {emit:.1} Mbps"));
        }
        if let Some(rx) = self.recv_mbps {
            title.push_str(&format!(" · rx {rx:.1} Mbps"));
        }
        // What arrived, not what was asked for: the two can differ, and this
        // is the only place a session says which.
        if let Some(format) = &self.video_format {
            title.push_str(&format!(" · {}", format.label()));
        }
        title.push_str(if self.abr_on {
            " · ABR on"
        } else {
            " · ABR off"
        });
        title.push_str("  ([ / ] bitrate, \\ ABR)");
        if let Some(toast) = &self.toast {
            title.push_str(" — ");
            title.push_str(&toast.text);
        }
        w.set_title(&title);
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("gsa client-dev")
                        // Presentation can only be measured on a window that
                        // is actually on a display: macOS stops compositing a
                        // fully covered one, and the run then silently yields
                        // no timing data at all. Asking for focus is not
                        // enough — a background process does not get to take
                        // it — so the window sits above the others instead.
                        .with_active(true)
                        .with_window_level(self.window_level)
                        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0)),
                )
                .expect("create window"),
        );
        window.set_cursor_visible(false);
        window.set_window_level(self.window_level);
        window.focus_window();
        tracing::info!(
            level = ?self.window_level,
            visible = ?window.is_visible(),
            "window created"
        );
        let gpu = Gpu::new(window.clone(), self.vsync).expect("init wgpu");
        self.window = Some(window);
        self.gpu = Some(gpu);
        // Both are opened: the platform framework is the only source of
        // motion, touch and battery, but it populates lazily off the run loop
        // and may not see the pad for a few frames yet, so the portable path
        // covers the gap and every other OS. Whichever has the pad is used.
        self.gamepad = GamepadCapture::new();
    }

    /// Poll the controller between events. Winit would otherwise sleep until
    /// the next frame or keystroke, and a gamepad generates neither.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The framework fills its controller list from the run loop, so the
        // pad can appear several frames after the window does. Retry until it
        // does rather than deciding once at startup that there is none.
        #[cfg(target_os = "macos")]
        if self.platform_pad.is_none() && self.input.is_some() {
            self.platform_pad = crate::gamepad_gc::GcCapture::new();
        }
        // A pad that has gone away must be reported: the host keeps its
        // virtual controller plugged in until told otherwise, and a game then
        // sees a pad frozen at its last state rather than a removal.
        #[cfg(target_os = "macos")]
        if let (Some(pad), Some(input)) = (&self.platform_pad, &self.input)
            && !pad.is_connected()
        {
            tracing::info!("controller disconnected");
            input.send(vec![gsa_client_core::InputEvent::GamepadDisconnect {
                seat: 0,
                ts_us: 0,
            }]);
            self.platform_pad = None;
            // A reconnect is a new pad and must announce itself again.
            self.pad_announced = false;
            self.motion_hz = 0;
        }
        // A second controller can arrive at any time, so this is checked here
        // rather than when the first was opened.
        #[cfg(target_os = "macos")]
        if self.platform_pad.is_some() {
            let count = crate::gamepad_gc::GcCapture::controller_count();
            if count > 1 && count != self.reported_extra_pads {
                self.reported_extra_pads = count;
                // One seat, one pad here. Real clients assign a seat per pad
                // (spec 07); saying so beats a second controller silently
                // doing nothing.
                tracing::warn!(
                    controllers = count,
                    "more than one controller connected; this harness drives only the first"
                );
            }
        }
        #[cfg(target_os = "macos")]
        if let (Some(pad), Some(input)) = (&mut self.platform_pad, &self.input) {
            if !self.pad_announced {
                // Announce before anything else: a host that has not been told
                // what the pad is builds a plain one and then drops motion,
                // touch and battery for it without complaint.
                let mut profile = pad.profile();
                if let Some(kind) = self.pad_kind_override {
                    profile.kind = kind;
                    // Capabilities have to move with the family. Hosts promote
                    // any pad advertising motion or a touchpad to a
                    // PlayStation-style device *whatever* family it claims,
                    // because those features need somewhere to go — so an
                    // Xbox pad that still advertises them is not a test of
                    // anything.
                    if !matches!(
                        kind,
                        gsa_client_core::PadKind::DualSense | gsa_client_core::PadKind::DualShock4
                    ) {
                        profile.caps = gsa_client_core::PadCaps::RUMBLE;
                    }
                }
                input.announce_pad(0, profile);
                self.pad_announced = true;
            }
            let events = pad.poll(self.motion_hz > 0);
            if !events.is_empty() {
                input.send(events);
            }
        }
        // Only one path drives the pad: the platform one when it has it, so
        // the host does not receive two snapshots per poll for one controller.
        let platform_active = {
            #[cfg(target_os = "macos")]
            {
                self.platform_pad.is_some()
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        };
        // While the platform framework can see a controller, the portable
        // path must stay silent — even before the platform one has finished
        // opening it. Whoever speaks first for a seat decides what the host
        // builds, and a snapshot builds the wrong thing.
        let waiting_for_platform = {
            #[cfg(target_os = "macos")]
            {
                crate::gamepad_gc::GcCapture::any_controller()
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        };
        if !platform_active
            && !waiting_for_platform
            && let (Some(gamepad), Some(input)) = (&mut self.gamepad, &self.input)
            && let Some(event) = gamepad.poll()
        {
            input.send(vec![event]);
        }
        // Drive the toast animation: it must keep redrawing even when no video
        // frames arrive (an idle display source produces none).
        if let Some(toast) = &self.toast {
            if toast.expired() {
                self.toast = None;
                self.update_title();
            } else if let Some(w) = &self.window {
                w.request_redraw();
            }
        }

        event_loop.set_control_flow(ControlFlow::wait_duration(GAMEPAD_POLL));
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Rumble { low, high } => {
                #[cfg(target_os = "macos")]
                if let Some(pad) = &mut self.platform_pad {
                    pad.rumble(low, high);
                }
                #[cfg(not(target_os = "macos"))]
                let _ = (low, high);
            }
            AppEvent::MotionRequested { rate_hz } => {
                tracing::info!(rate_hz, "host asked for motion");
                self.motion_hz = rate_hz;
            }
            AppEvent::Ready(input, knobs, bitrate) => {
                self.input = Some(input);
                self.knobs = knobs;
                self.bitrate_bps = bitrate;
                self.update_title();
            }
            AppEvent::Frame(frame) => {
                self.fit_window_to(frame.width, frame.height);
                // The smoothest mode is the one that refuses to drop: a frame
                // arriving while another is still unshown queues behind it
                // rather than replacing it, and latency grows by exactly the
                // depth of that queue. Every other mode discards, which is
                // what keeps latency from creeping.
                if !self.pacing.drops_unshown() && self.latest.is_some() && !self.latest_shown {
                    // Bounded: an unbounded queue on a stalled display is a
                    // memory leak, and beyond a second of frames the picture
                    // is so far behind that dropping is the kinder failure.
                    const MAX_PENDING: usize = 60;
                    if self.pending.len() < MAX_PENDING
                        && let Some(waiting) = self.latest.take()
                    {
                        self.pending.push_back(waiting);
                    }
                }
                let displaced_unshown = self.latest.is_some() && !self.latest_shown;
                self.presentation
                    .on_ready(displaced_unshown, std::time::Instant::now());
                self.latest = Some(frame);
                self.latest_ready_at = Some(std::time::Instant::now());
                self.latest_shown = false;
                // Frames arriving but nothing ever drawn is a different fault
                // from a covered window, and it is silent unless named: the
                // compositor stops asking a window on a sleeping display to
                // redraw at all, so neither branch of `render` is reached.
                if self.presentation.ready.is_multiple_of(240)
                    && self.presentation.presented == 0
                    && self.occluded == 0
                {
                    tracing::warn!(
                        frames = self.presentation.ready,
                        "frames are decoding but the window has never been asked to redraw; \
                         the display may be asleep or the window never opened"
                    );
                }
                self.report_presentation();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::RecvMbps(mbps) => {
                self.recv_mbps = mbps;
                self.update_title();
            }
            AppEvent::VideoFormat(format) => {
                self.video_format = Some(format);
                self.update_title();
            }
            AppEvent::Notification(event) => {
                let (connected, text) = match event {
                    // Encoder telemetry updates the HUD, not a toast.
                    ControlEvent::EncodeStats {
                        emitted_bitrate_bps,
                        ..
                    } => {
                        self.emitted_mbps = Some(f64::from(emitted_bitrate_bps) / 1_000_000.0);
                        self.update_title();
                        return;
                    }
                    ControlEvent::GamepadConnected { seat } => {
                        (true, format!("controller connected (seat {seat})"))
                    }
                    ControlEvent::GamepadDisconnected { seat } => {
                        (false, format!("controller disconnected (seat {seat})"))
                    }
                };
                tracing::info!(text, "host notification");
                self.toast = Some(Toast {
                    connected,
                    at: Instant::now(),
                    text,
                });
                self.update_title();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::StreamEnded(message) => {
                tracing::info!(message, "exiting");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                // A queued frame is older than `latest` and must be shown
                // first, or "never drop a frame" would still show them out of
                // order — which is worse than dropping.
                if self.latest_shown
                    && let Some(next) = self.pending.pop_front()
                {
                    self.latest = Some(next);
                    self.latest_ready_at = Some(std::time::Instant::now());
                    self.latest_shown = false;
                }
                if let (Some(gpu), Some(frame)) = (&mut self.gpu, &self.latest) {
                    self.content_rect = Some(gpu.content_rect(frame));
                    let toast = self.toast.as_ref().map(|t| (t.color(), t.slide()));
                    // `render` returns once the frame is handed to the
                    // surface; under vsync that call is where the wait for the
                    // display's refresh is spent, so the clock is read after.
                    match gpu.render(frame, toast) {
                        // Nothing was shown — an occluded or timed-out
                        // surface. Not a present, not a repeat, and worth
                        // saying: a hidden window produces no presentation
                        // data at all, and silence reads as "no problem".
                        Ok(false) => {
                            self.occluded = self.occluded.saturating_add(1);
                            if self.occluded.is_multiple_of(600) {
                                tracing::warn!(
                                    skipped = self.occluded,
                                    "the window is not visible, so nothing is reaching a display; \
                                     presentation figures cannot be measured until it is raised"
                                );
                            }
                        }
                        Ok(true) => {
                            if self.occluded > 0 {
                                tracing::info!(
                                    skipped = self.occluded,
                                    "the window is visible again; measuring from here"
                                );
                                self.occluded = 0;
                            }
                            let now = std::time::Instant::now();
                            if self.latest_shown {
                                // Redrawn with nothing new: the display asked
                                // for a frame and the stream had none.
                                self.presentation.on_repeat(now);
                            } else {
                                let waited = self
                                    .latest_ready_at
                                    .map_or_else(Default::default, |ready| now - ready);
                                self.presentation.on_present(waited, now);
                                self.latest_shown = true;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "render failed"),
                    }
                    self.report_presentation();
                    // Only chase the next refresh when asked to. Redrawing
                    // after every present makes this a max-rate client from
                    // the compositor's point of view — repeating the same
                    // frame most refreshes — and a variable-refresh display
                    // then has no reason to slow down to match the content.
                    // Presenting only when a frame arrives is what lets it.
                    if self.vsync
                        && (self.chase_refresh || !self.pending.is_empty())
                        && let Some(w) = &self.window
                    {
                        w.request_redraw();
                    }
                }
            }
            // Input capture → agent (spec 07 client side).
            WindowEvent::KeyboardInput {
                event: key,
                is_synthetic: false,
                ..
            } => {
                use winit::keyboard::{KeyCode, PhysicalKey};
                // Dev-only local bitrate knob ([ down / ] up, ±25%) to exercise the
                // manual bitrate path — intercepted, not forwarded to the host.
                // (F7/F8 are macOS media keys the OS swallows, so use brackets.)
                if key.state == winit::event::ElementState::Pressed
                    && matches!(
                        key.physical_key,
                        PhysicalKey::Code(KeyCode::BracketLeft | KeyCode::BracketRight)
                    )
                {
                    if self.knobs.is_some() {
                        let up = key.physical_key == PhysicalKey::Code(KeyCode::BracketRight);
                        let stepped = if up {
                            u64::from(self.bitrate_bps) * 5 / 4
                        } else {
                            u64::from(self.bitrate_bps) * 3 / 4
                        };
                        self.bitrate_bps = (stepped as u32).clamp(200_000, 100_000_000);
                        if let Some(knobs) = &self.knobs {
                            knobs.set_bitrate(self.bitrate_bps);
                        }
                        tracing::info!(
                            bitrate_bps = self.bitrate_bps,
                            mbps = f64::from(self.bitrate_bps) / 1_000_000.0,
                            "bitrate knob ([ = down, ] = up)"
                        );
                        self.update_title();
                    }
                    return;
                }
                // Toggle server-side ABR with `\` — intercepted, not sent to the host.
                if key.state == winit::event::ElementState::Pressed
                    && key.physical_key == PhysicalKey::Code(KeyCode::Backslash)
                {
                    if self.knobs.is_some() {
                        self.abr_on = !self.abr_on;
                        if let Some(knobs) = &self.knobs {
                            knobs.set_abr(self.abr_on);
                        }
                        tracing::info!(abr_on = self.abr_on, "ABR toggled (\\)");
                        self.update_title();
                    }
                    return;
                }
                if let (Some(input), Some(ev)) = (
                    &self.input,
                    crate::input_capture::key_event(key.physical_key, key.state),
                ) {
                    input.send(vec![ev]);
                }
            }
            WindowEvent::MouseInput { button, state, .. } => {
                if let (Some(input), Some(ev)) = (
                    &self.input,
                    crate::input_capture::mouse_button(button, state),
                ) {
                    input.send(vec![ev]);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(input) = &self.input {
                    input.send(vec![crate::input_capture::mouse_wheel(delta)]);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let (Some(input), Some((rx, ry, rw, rh))) = (&self.input, self.content_rect) {
                    // Map window pixel coords into [0,1] over the presented
                    // content rect (ignore the letterbox margins).
                    let nx = (position.x as f32 - rx) / rw;
                    let ny = (position.y as f32 - ry) / rh;
                    if (0.0..=1.0).contains(&nx) && (0.0..=1.0).contains(&ny) {
                        input.send(vec![crate::input_capture::mouse_move_abs(nx, ny)]);
                    }
                }
            }
            _ => {}
        }
    }
}

const SHADER: &str = r#"
@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // Canonical fullscreen triangle: (-1,-1), (3,-1), (-1,3).
    var out: VsOut;
    let x = f32((i >> 1u) & 1u) * 4.0 - 1.0;
    let y = f32(i & 1u) * 4.0 - 1.0;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(frame_tex, frame_samp, in.uv);
}
"#;

/// Solid-colour quad for the notification toast. `rect` is (x0, y0, x1, y1) in
/// clip space; `color` is premultiplied-alpha-friendly straight RGBA.
const BAR_SHADER: &str = r#"
struct Bar { rect: vec4<f32>, color: vec4<f32> };
@group(0) @binding(0) var<uniform> bar: Bar;

@vertex
fn vs_bar(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var xs = array<f32, 6>(bar.rect.x, bar.rect.z, bar.rect.x, bar.rect.z, bar.rect.z, bar.rect.x);
    var ys = array<f32, 6>(bar.rect.y, bar.rect.y, bar.rect.w, bar.rect.y, bar.rect.w, bar.rect.w);
    return vec4<f32>(xs[i], ys[i], 0.0, 1.0);
}

@fragment
fn fs_bar() -> @location(0) vec4<f32> {
    return bar.color;
}
"#;

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    texture: Option<FrameTexture>,
    bar_pipeline: wgpu::RenderPipeline,
    bar_uniform: wgpu::Buffer,
    bar_bind: wgpu::BindGroup,
}

struct FrameTexture {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    width: u32,
    height: u32,
    order: PixelOrder,
}

impl Gpu {
    fn new(window: Arc<Window>, vsync: bool) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window).context("create surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no adapter")?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .context("request device")?;

        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface unsupported")?;
        // Vsync is what a phone, a TV and a monitor all do, so it is the only
        // mode under which presentation timing means anything. The immediate
        // mode is kept for measuring delivery in isolation.
        config.present_mode = if vsync {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("present"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bind_layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("present"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(config.format.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Toast overlay: a solid, alpha-blended quad driven by a small uniform.
        let bar_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bar"),
            source: wgpu::ShaderSource::Wgsl(BAR_SHADER.into()),
        });
        let bar_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bar-uniform"),
            size: 32, // vec4 rect + vec4 color
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bar_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bar"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bar_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bar"),
            layout: &bar_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: bar_uniform.as_entire_binding(),
            }],
        });
        let bar_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bar"),
            bind_group_layouts: &[Some(&bar_bind_layout)],
            ..Default::default()
        });
        let bar_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bar"),
            layout: Some(&bar_layout),
            vertex: wgpu::VertexState {
                module: &bar_shader,
                entry_point: Some("vs_bar"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &bar_shader,
                entry_point: Some("fs_bar"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            sampler,
            bind_layout,
            texture: None,
            bar_pipeline,
            bar_uniform,
            bar_bind,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    fn ensure_texture(&mut self, width: u32, height: u32, order: PixelOrder) {
        if matches!(&self.texture, Some(t) if t.width == width && t.height == height && t.order == order)
        {
            return;
        }
        // Match the decoder's byte order so no CPU swizzle ever happens
        // (VideoToolbox emits BGRA, openh264 RGBA).
        let format = match order {
            PixelOrder::Rgba => wgpu::TextureFormat::Rgba8UnormSrgb,
            PixelOrder::Bgra => wgpu::TextureFormat::Bgra8UnormSrgb,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.texture = Some(FrameTexture {
            texture,
            bind,
            width,
            height,
            order,
        });
    }

    /// The presented (letterboxed) content rectangle in surface pixels:
    /// `(x, y, width, height)` — for mapping cursor coords back to [0,1].
    fn content_rect(&self, frame: &DecodedFrame) -> (f32, f32, f32, f32) {
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let (fw, fh) = (frame.width as f32, frame.height as f32);
        let scale = (sw / fw).min(sh / fh);
        let (vw, vh) = (fw * scale, fh * scale);
        ((sw - vw) / 2.0, (sh - vh) / 2.0, vw, vh)
    }

    /// Draw `frame`, returning whether it actually reached the surface. An
    /// occluded or timed-out surface presents nothing, and reporting that as a
    /// present is how a hidden window comes to look like a 600 Hz display.
    fn render(&mut self, frame: &DecodedFrame, toast: Option<([f32; 4], f32)>) -> Result<bool> {
        self.ensure_texture(frame.width, frame.height, frame.order);

        // A toast is showing: write its quad (full width, bottom, slid by `s`).
        if let Some((color, slide)) = toast {
            let bar_h = 0.14;
            let off = (1.0 - slide) * (bar_h + 0.04); // hidden below the screen
            let rect = [-1.0f32, -1.0 - off, 1.0, -1.0 + bar_h - off];
            let mut bytes = [0u8; 32];
            for (i, v) in rect.iter().chain(color.iter()).enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            self.queue.write_buffer(&self.bar_uniform, 0, &bytes);
        }
        let FrameTexture {
            texture,
            bind,
            width: fw,
            height: fh,
            ..
        } = self.texture.as_ref().expect("just ensured");

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );

        use wgpu::CurrentSurfaceTexture as Cst;
        #[allow(clippy::items_after_statements)]
        let output = match self.surface.get_current_texture() {
            Cst::Success(o) | Cst::Suboptimal(o) => o,
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(o) | Cst::Suboptimal(o) => o,
                    other => return Err(anyhow::anyhow!("surface after reconfigure: {other:?}")),
                }
            }
            // Nothing reached the screen. Saying so matters: counting these
            // as presents makes an occluded window look like a 600 Hz display.
            Cst::Timeout | Cst::Occluded => return Ok(false),
            Cst::Validation => return Err(anyhow::anyhow!("surface validation error")),
        };
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });

            // Aspect-fit letterbox via viewport.
            let (sw, sh) = (self.config.width as f32, self.config.height as f32);
            let (fw, fh) = (*fw as f32, *fh as f32);
            let scale = (sw / fw).min(sh / fh);
            let (vw, vh) = (fw * scale, fh * scale);
            pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw, vh, 0.0, 1.0);

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.draw(0..3, 0..1);

            // Toast overlay: full-surface viewport, alpha-blended quad on top.
            if toast.is_some() {
                pass.set_viewport(0.0, 0.0, sw, sh, 0.0, 1.0);
                pass.set_pipeline(&self.bar_pipeline);
                pass.set_bind_group(0, &self.bar_bind, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(output);
        Ok(true)
    }
}

#[cfg(test)]
mod mode_tests {
    use super::streamable_fps;

    /// A fast panel must not turn into a fast request: the host pays for every
    /// frame asked of it, and 240 Hz of desktop is not 240 Hz of game.
    #[test]
    fn a_panels_rate_is_snapped_to_one_a_host_offers() {
        assert_eq!(streamable_fps(240), 120);
        assert_eq!(streamable_fps(144), 120);
        assert_eq!(streamable_fps(120), 120);
        assert_eq!(streamable_fps(90), 90);
        assert_eq!(streamable_fps(60), 60);
        // Slower than every offered rate still streams, at the lowest.
        assert_eq!(streamable_fps(24), 30);
    }
}
