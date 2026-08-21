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
        /// Where the presenter reports ready→shown waits; the session loop
        /// drains them into the shared latency chain.
        Option<std::sync::Arc<std::sync::Mutex<Vec<u32>>>>,
    ),
    Frame(Box<DecodedFrame>),
    /// Rolling received video goodput (Mb/s), for the title HUD.
    RecvMbps(Option<f64>),
    /// Fresh overlay text from the session loop — the stream's half of the
    /// on-screen stats. The presenter appends its own half (pads, display)
    /// and rasterises.
    Overlay(Vec<String>),
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
    first_frame_s: u64,
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
    fullscreen: bool,
    disconnect_on_exit: bool,
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
                    first_frame_s,
                    synthetic_pad,
                    dump_frame,
                    offered,
                    mode,
                    mapping,
                    jitter,
                    pacing,
                    input_script,
                    dejitter,
                    disconnect_on_exit,
                },
                &proxy,
            )
        })?;

    let mut app = App {
        pad_kind_override: pad_kind.and_then(parse_pad_kind),
        vsync,
        pacing,
        chase_refresh,
        fullscreen,
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
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);
    /// How long a pointer move is given to take effect before a click.
    const POINTER_SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);

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
            Step::Point { x, y } => {
                let ts_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_micros() as u64);
                input.send(vec![gsa_client_core::InputEvent::MouseMove(
                    gsa_client_core::MouseMove::Absolute { x, y, ts_us },
                )]);
                // A full-screen game reads raw mouse motion rather than the
                // system cursor, so an absolute reposition alone changes
                // nothing it can see — the highlight stays where it was and
                // the click that follows lands on the wrong item, or on
                // nothing. A relative nudge is the event it is actually
                // watching for; equal and opposite, so the position set above
                // is what survives.
                for (dx, dy) in [(1.0, 1.0), (-1.0, -1.0)] {
                    input.send(vec![gsa_client_core::InputEvent::MouseMove(
                        gsa_client_core::MouseMove::Relative { dx, dy, ts_us },
                    )]);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // Long enough for the host to have actually applied the move
                // before anything is clicked. A shorter gap is a race: the
                // click lands at the pointer's old position, which is how a
                // run opened MOUSE instead of DISPLAY AND GRAPHICS while an
                // identical script had worked minutes earlier.
                std::thread::sleep(POINTER_SETTLE);
                tracing::info!(step = index + 1, x, y, "script pointer");
            }
            Step::Click => {
                let now = || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_micros() as u64)
                };
                for down in [true, false] {
                    input.send(vec![gsa_client_core::InputEvent::MouseButton {
                        button: gsa_client_core::MouseButton::Left,
                        down,
                        ts_us: now(),
                    }]);
                    if down {
                        std::thread::sleep(KEY_HELD);
                    }
                }
                tracing::info!(step = index + 1, "script click");
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
    /// Give up if the first frame has not decoded within this many seconds
    /// (0 = wait forever).
    ///
    /// A host still holding an abandoned session accepts the connection,
    /// negotiates, and then sends nothing. Every client-side signal looks
    /// healthy, so an unattended run sits on a grey window for its whole
    /// duration and reports a clean exit. Waiting on the first frame is what
    /// tells the two apart, and it costs a session that was going to fail
    /// anyway.
    first_frame_s: u64,
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
    /// Leave the host's app running on exit instead of quitting it; the next
    /// run of the same app rejoins it mid-session.
    disconnect_on_exit: bool,
}

fn moonlight_loop(addr: std::net::SocketAddr, run: MoonlightRun, proxy: &EventLoopProxy<AppEvent>) {
    let MoonlightRun {
        app_id,
        bitrate_mbps,
        force_sw,
        seconds,
        first_frame_s,
        synthetic_pad,
        dump_frame,
        offered,
        mode,
        mapping,
        jitter,
        pacing,
        input_script,
        dejitter,
        disconnect_on_exit,
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
            // Present waits measured by the presenter, drained into the
            // shared chain here — the sensor lives at the display, the
            // arithmetic in core, same as the apps.
            let present_feed: std::sync::Arc<std::sync::Mutex<Vec<u32>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let _ = proxy.send_event(AppEvent::Ready(
                stream.input.clone(),
                None,
                bitrate_mbps.saturating_mul(1_000_000),
                Some(present_feed.clone()),
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
            // A termination signal must run the same teardown as a clean
            // exit. Without this, Ctrl-C or a killed process skips the
            // session cancel below, the host keeps the session, and the next
            // connect resumes into a stream that never sends a frame — the
            // grey screen. (SIGKILL still cannot be caught; nothing can.)
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            let interrupted = async move {
                match (&mut sigint, &mut sigterm) {
                    (Some(int), Some(term)) => {
                        tokio::select! {
                            _ = int.recv() => {},
                            _ = term.recv() => {},
                        }
                    }
                    _ => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(interrupted);
            let result = loop {
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break Ok(());
                }
                // Only the first frame is waited on with a limit. A stream
                // that has started and then pauses is a legitimate idle
                // screen; one that never starts is a host holding a session
                // it will not give up.
                let received = if frames == 0 && first_frame_s > 0 {
                    let wait = std::time::Duration::from_secs(first_frame_s);
                    tokio::select! {
                        () = &mut interrupted => {
                            tracing::info!("termination signal; tearing the session down");
                            break Ok(());
                        }
                        r = tokio::time::timeout(wait, core.recv_frame(decoder.as_mut())) => {
                            match r {
                                Ok(received) => received,
                                Err(_) => {
                                    break Err(anyhow::anyhow!(
                                        "no video {first_frame_s}s after the session started — the host \
                                         is almost certainly still holding an earlier session. Clear it \
                                         with:\n  cargo run -q --release -p gsa-backend-moonlight \
                                         --example launch -- {addr} {app_id}"
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    tokio::select! {
                        () = &mut interrupted => {
                            tracing::info!("termination signal; tearing the session down");
                            break Ok(());
                        }
                        r = core.recv_frame(decoder.as_mut()) => r,
                    }
                };
                let Some(out) = received? else {
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
                    // The wire's measured round trip is telemetry for the
                    // latency chain, not a host message worth a log line each.
                    if let Some(gsa_client_core::BackendEvent::LinkRtt { rtt_us }) =
                        message.neutral()
                    {
                        core.on_link_rtt(rtt_us);
                        continue;
                    }
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
                        // Where the breaks entered, measured on arrival and
                        // before any pacing: the host never made the frame, or
                        // it was late reaching us. Only this pair says whether
                        // the client can do anything about them.
                        cadence = {
                            let (captured, delivered, slip) = core.arrival_cadence();
                            format!(
                                "{captured} captured-late / {delivered} delivered-late \
                                 (worst slip {:.0}ms)",
                                f64::from(slip) / 1000.0
                            )
                        },
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
                        // Milliseconds further behind the host than at the
                        // first frame. Every other figure here is a spread,
                        // and a queue that fills once and never drains has no
                        // spread — so this is the only one a steady backlog
                        // shows up in.
                        latency_growth_ms =
                            format!("{:+.0}", core.latency_growth_us() as f64 / 1000.0),
                        frame_interval_us = core.frame_interval_us(),
                        dejitter_duty = {
                            let (ran, skipped) = core.dejitter_duty();
                            format!("{ran} paced / {skipped} skipped")
                        },
                        dropped = stats.frames_dropped_incomplete,
                        recovered = stats.frames_recovered,
                        // Frames the session decoded and discarded unseen
                        // under the drop policy — the core's half of the
                        // supersede count; the presenter's own half is
                        // unshown_pct in the presentation line.
                        superseded = core.superseded(),
                        // Proof the pause exclusion ran, when content pauses:
                        // a quiet smoother alone cannot show the fix worked.
                        content_pauses = core.content_pauses(),
                        // What the stream is actually pushing, as against what
                        // was asked for. Input shares the link with video as
                        // reliable control messages, so a stream near the cap
                        // is the difference between "input is slow" being the
                        // link or the host — and without this the two cannot
                        // be told apart.
                        recv_mbps = stats
                            .recv_mbps
                            .map_or_else(|| "—".to_owned(), |m| format!("{m:.1}")),
                        // The latency chain, p50/p95/p99 per stage in ms, "—"
                        // where a stage was never measured. `total` composes
                        // half the round trip with every measured duration —
                        // the reference overlay's own arithmetic.
                        latency = {
                            let chain = core.latency_chain();
                            let stage = |s: Option<gsa_client_core::StagePercentiles>| {
                                s.map_or_else(
                                    || "—".to_owned(),
                                    |p| {
                                        format!(
                                            "{:.1}/{:.1}/{:.1}",
                                            f64::from(p.p50_us) / 1000.0,
                                            f64::from(p.p95_us) / 1000.0,
                                            f64::from(p.p99_us) / 1000.0
                                        )
                                    },
                                )
                            };
                            format!(
                                "rtt={} host={} decode={} hold={} present={} total={}{}",
                                stage(chain.rtt),
                                stage(chain.host),
                                stage(chain.decode),
                                stage(chain.hold),
                                stage(chain.present),
                                if chain.total_is_lower_bound { ">=" } else { "" },
                                stage(chain.total)
                            )
                        },
                        "moonlight stream stats"
                    );

                    if let Ok(mut samples) = present_feed.lock() {
                        for us in samples.drain(..) {
                            core.on_present_wait(us);
                        }
                    }
                    // The same figures, for eyes: the presenter composites
                    // these over the stream, appending its own half.
                    let chain = core.latency_chain();
                    let stage_ms = |s: Option<gsa_client_core::StagePercentiles>| {
                        s.map_or_else(
                            || "     —".to_owned(),
                            |p| {
                                format!(
                                    "{:5.1} {:5.1} {:5.1}",
                                    f64::from(p.p50_us) / 1000.0,
                                    f64::from(p.p95_us) / 1000.0,
                                    f64::from(p.p99_us) / 1000.0
                                )
                            },
                        )
                    };
                    let lines = vec![
                        format!(
                            "fps {:5.1}  low1 {:5.1}  {}",
                            f64::from(present.fps_x100) / 100.0,
                            f64::from(present.low1_fps_x100) / 100.0,
                            decoder
                                .video_format()
                                .map_or_else(|| "-".to_string(), |f| f.label())
                        ),
                        format!(
                            "recv {} Mb/s  frames {}  drop {}  sup {}",
                            stats
                                .recv_mbps
                                .map_or_else(|| "-".to_owned(), |m| format!("{m:.1}")),
                            frames,
                            stats.frames_dropped_incomplete,
                            core.superseded()
                        ),
                        format!(
                            "jitter {:.1} -> {:.1} ms  hold {:.1} ms",
                            f64::from(core.jitter_us()) / 1000.0,
                            f64::from(core.released_jitter_us()) / 1000.0,
                            f64::from(core.mean_hold_us()) / 1000.0
                        ),
                        {
                            // The HDR story in the core's vocabulary: what
                            // the wire carried, and what reached decoded
                            // frames after this client's re-attachment.
                            match decoder.hdr_status() {
                                None => "hdr -".to_owned(),
                                Some(h) => {
                                    let and = h.delivered.map_or_else(
                                        || "  out -".to_owned(),
                                        |(m, c, d)| {
                                            let yn = |v: bool| if v { "y" } else { "-" };
                                            format!(
                                                "  out mast {} cll {} 10+ {}",
                                                yn(m),
                                                yn(c),
                                                yn(d)
                                            )
                                        },
                                    );
                                    format!(
                                        "hdr mast {} cll {} 10+ {}{}",
                                        h.mastering.label(),
                                        h.light_level.label(),
                                        if h.hdr10_plus { "y" } else { "-" },
                                        and
                                    )
                                }
                            }
                        },
                        "latency  p50   p95   p99  (ms)".to_owned(),
                        format!("  rtt   {}", stage_ms(chain.rtt)),
                        format!("  host  {}", stage_ms(chain.host)),
                        format!("  decode{}", stage_ms(chain.decode)),
                        format!("  hold  {}", stage_ms(chain.hold)),
                        format!("  total {}", stage_ms(chain.total)),
                    ];
                    let _ = proxy.send_event(AppEvent::Overlay(lines));
                }
                if proxy
                    .send_event(AppEvent::Frame(Box::new(out.frame)))
                    .is_err()
                {
                    break Ok(()); // window closed
                }
            };
            drop(core);
            drop(stream);
            // Quit ends the host's app; disconnect leaves it running so the
            // next run of the same app rejoins it mid-session. The launch
            // path cancels anything else the host still holds, so nothing
            // left behind can block a later start.
            if disconnect_on_exit {
                tracing::info!("disconnected; the host keeps the app running");
            } else {
                let _ = session.cancel().await;
            }
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
                    // The gsa chain measures its total outright; the present
                    // stage is not composed into it.
                    None,
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
    /// The session loop's half of the on-screen stats, refreshed with its
    /// stats tick; the presenter appends its own half before rasterising.
    overlay_stream_lines: Vec<String>,
    /// The last rumble the host asked for, for the pad line.
    last_rumble: Option<(u16, u16)>,
    /// Where measured present waits go, when a session wants them.
    present_feed: Option<std::sync::Arc<std::sync::Mutex<Vec<u32>>>>,
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
    /// Take the whole display. Adaptive-Sync needs it on macOS, so a windowed
    /// run cannot measure VRR however the display is configured.
    fullscreen: bool,
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
        // Nothing to fit to in full-screen: the window already owns the
        // display. Resizing it there reconfigures the surface to the stream's
        // size while the drawable stays screen-sized, so the picture is drawn
        // into one corner of a mostly unpainted surface.
        if self.fullscreen {
            return;
        }
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
            // Whether HDR is actually reaching the panel, as opposed to being
            // requested: a PQ surface passes the signal through, anything else
            // means the shader tone-mapped it away.
            hdr_out = self.gpu.as_ref().is_some_and(|g| g.surface_is_pq),
            // Whether the panel held frames to its own grid or followed the
            // content. Only this figure distinguishes the two; every other one
            // above reads the same either way whenever the content rate
            // divides the refresh rate.
            grid = self
                .presentation
                .grid_fit(display_refresh_hz().unwrap_or_default())
                .map(|f| format!("{} r={:.3} n={}", f.verdict(), f.mean_residual, f.samples))
                .unwrap_or_else(|| "unknown".to_owned()),
            "presentation"
        );
    }

    /// Rebuild the on-screen stats from the session's lines plus what only
    /// the presenter knows: pacing mode, display behaviour, controllers.
    fn refresh_overlay(&mut self) {
        let mut lines = self.overlay_stream_lines.clone();
        lines.push(format!("mode    {}", self.pacing.label()));
        if let Some(s) = self.presentation.summary() {
            lines.push(format!(
                "present wait {:.1}ms  repeats {:.1}%  unshown {:.1}%",
                f64::from(s.wait_p50_us) / 1000.0,
                s.repeat_pct(),
                s.superseded_pct()
            ));
            if let Some(hz) = display_refresh_hz() {
                let grid = self.presentation.grid_fit(hz).map_or("grid ?", |f| {
                    if f.is_fixed() { "pinned" } else { "adapting" }
                });
                lines.push(format!("display {hz:.0}Hz {grid}"));
            }
        }
        // Controllers: what is plugged, what the host asked of it. Absent
        // rather than blank when nothing is connected.
        let pads = if self.gamepad.is_some() { 1 } else { 0 };
        #[cfg(target_os = "macos")]
        let pads = pads + usize::from(self.platform_pad.is_some());
        if pads > 0 {
            let motion = if self.motion_hz > 0 {
                format!("  motion {}Hz", self.motion_hz)
            } else {
                String::new()
            };
            let rumble = self
                .last_rumble
                .map(|(low, high)| format!("  rumble {low}/{high}"))
                .unwrap_or_default();
            lines.push(format!("pads    {pads}{motion}{rumble}"));
        }
        let image = crate::overlay::rasterise(&lines);
        if let Some(gpu) = &mut self.gpu {
            gpu.set_overlay(&image);
        }
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
                        .with_fullscreen(
                            self.fullscreen
                                .then(|| winit::window::Fullscreen::Borderless(None)),
                        )
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
            fullscreen = self.fullscreen,
            "window created"
        );
        // What the display will do about its rate, and whether this run is
        // even eligible for it. Reported per run so no figure can be
        // attributed to variable refresh without the evidence beside it.
        match crate::present::display_refresh() {
            Some(refresh) => {
                let (low, high) = refresh.range_hz();
                tracing::info!(
                    variable = refresh.is_variable(),
                    range_hz = format!("{low:.0}-{high:.0}"),
                    max_fps = refresh.max_fps,
                    fullscreen = self.fullscreen,
                    adaptive_sync_possible = refresh.is_variable() && self.fullscreen,
                    "display refresh"
                );
                if refresh.is_variable() && !self.fullscreen {
                    tracing::warn!(
                        "this display varies its refresh rate, but Adaptive-Sync needs \
                         full-screen; run with --fullscreen or read these figures as fixed-rate"
                    );
                }
            }
            None => tracing::info!("no screen reported its refresh range"),
        }
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
                self.last_rumble = Some((low, high));
                #[cfg(target_os = "macos")]
                if let Some(pad) = &mut self.platform_pad {
                    pad.rumble(low, high);
                }
                #[cfg(not(target_os = "macos"))]
                let _ = (low, high);
            }
            AppEvent::Overlay(lines) => {
                self.overlay_stream_lines = lines;
                self.refresh_overlay();
            }
            AppEvent::MotionRequested { rate_hz } => {
                tracing::info!(rate_hz, "host asked for motion");
                self.motion_hz = rate_hz;
            }
            AppEvent::Ready(input, knobs, bitrate, present_feed) => {
                self.input = Some(input);
                self.knobs = knobs;
                self.bitrate_bps = bitrate;
                self.present_feed = present_feed;
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
                                if let Some(feed) = &self.present_feed
                                    && let Ok(mut feed) = feed.lock()
                                    && feed.len() < 1024
                                {
                                    #[allow(clippy::cast_possible_truncation)]
                                    feed.push(waited.as_micros().min(u128::from(u32::MAX)) as u32);
                                }
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

/// Present a 10-bit BT.2020 PQ frame from its two planes.
///
/// The whole conversion lives here rather than on the CPU. Doing it per pixel
/// in Rust cost ~11.7 ms a frame at 1080p — more than decoding one — which
/// capped the harness at 85 fps and made it fall permanently behind a 120 fps
/// stream. The GPU does the same work as part of sampling.
///
/// `PQ_OUT` is substituted at build time: with an HDR surface the PQ signal is
/// passed straight through to the display, and only when the surface cannot
/// take it is anything tone-mapped away.
const HDR_SHADER: &str = r#"
@group(0) @binding(0) var luma_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;
@group(0) @binding(2) var chroma_tex: texture_2d<f32>;

const PQ_OUT: bool = __PQ_OUT__;
// Studio range is 64-940 for luma and 64-960 for chroma about 512; full range
// uses all 1024 codes. Reading one as the other crushes blacks and clips
// whites, so it is carried from the stream rather than assumed.
const LUMA_OFFSET: f32 = __LUMA_OFFSET__;
const LUMA_SPAN: f32 = __LUMA_SPAN__;
const CHROMA_SPAN: f32 = __CHROMA_SPAN__;
// BT.2408 diffuse white: the nit level that means "paper white" in PQ.
const SDR_WHITE_NITS: f32 = 203.0;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var out: VsOut;
    let x = f32((i >> 1u) & 1u) * 4.0 - 1.0;
    let y = f32(i & 1u) * 4.0 - 1.0;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

// SMPTE ST 2084, code value to absolute luminance in nits.
fn pq_eotf(code: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let e = pow(max(code, 0.0), 1.0 / m2);
    let num = max(e - c1, 0.0);
    let den = c2 - c3 * e;
    return 10000.0 * pow(num / max(den, 1e-6), 1.0 / m1);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Ten bits sit at the top of a 16-bit word, so a normalised sample is
    // short of full scale by exactly 65535/65472; correcting it here is what
    // keeps white at white.
    let to_code = 65535.0 / 65472.0;
    let y_raw = textureSample(luma_tex, frame_samp, in.uv).r * to_code;
    let c_raw = textureSample(chroma_tex, frame_samp, in.uv).rg * to_code;

    let y = (y_raw * 1023.0 - LUMA_OFFSET) / LUMA_SPAN;
    let cb = (c_raw.r * 1023.0 - 512.0) / CHROMA_SPAN;
    let cr = (c_raw.g * 1023.0 - 512.0) / CHROMA_SPAN;

    // BT.2020 non-constant luminance: the matrix applies to the PQ-encoded
    // signal, not to light, so this stays in the encoded domain.
    let r = y + 1.47460 * cr;
    let g = y - 0.16455 * cb - 0.57135 * cr;
    let b = y + 1.88140 * cb;
    let rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));

    if (PQ_OUT) {
        // The display decodes PQ itself; handing it anything else would mean
        // undoing the encoding only to have it reapplied.
        return vec4<f32>(rgb, 1.0);
    }

    // No HDR surface available: decode to light, refer it to diffuse white,
    // and convert BT.2020 to BT.709. Clamped first, so out-of-gamut values
    // cannot come back as negative light through the matrix.
    let nits = vec3<f32>(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b));
    let scene = clamp(nits / SDR_WHITE_NITS, vec3<f32>(0.0), vec3<f32>(1.0));
    let out_r = 1.66050 * scene.r - 0.58764 * scene.g - 0.07285 * scene.b;
    let out_g = -0.12455 * scene.r + 1.13290 * scene.g - 0.00835 * scene.b;
    let out_b = -0.01812 * scene.r - 0.10057 * scene.g + 1.11869 * scene.b;
    // The surface is *Srgb, so the hardware applies the OETF: emit light.
    return vec4<f32>(clamp(vec3<f32>(out_r, out_g, out_b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Textured quad for the stats overlay, positioned by a uniform rect.
///
/// The overlay's values are sRGB; on the PQ surface they come out dimmer than
/// on the SDR one, which is acceptable for an instrument readout and saves a
/// second encode path.
const OVERLAY_SHADER: &str = r#"
struct Rect { rect: vec4<f32> };
@group(0) @binding(0) var overlay_tex: texture_2d<f32>;
@group(0) @binding(1) var overlay_samp: sampler;
@group(0) @binding(2) var<uniform> r: Rect;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var xs = array<f32, 6>(r.rect.x, r.rect.z, r.rect.x, r.rect.z, r.rect.z, r.rect.x);
    var ys = array<f32, 6>(r.rect.y, r.rect.y, r.rect.w, r.rect.y, r.rect.w, r.rect.w);
    var us = array<f32, 6>(0.0, 1.0, 0.0, 1.0, 1.0, 0.0);
    var vs = array<f32, 6>(0.0, 0.0, 1.0, 0.0, 1.0, 1.0);
    var out: VsOut;
    out.pos = vec4<f32>(xs[i], ys[i], 0.0, 1.0);
    out.uv = vec2<f32>(us[i], vs[i]);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(overlay_tex, overlay_samp, in.uv);
}
"#;

/// Present an 8-bit biplanar SDR frame from its two planes.
///
/// The decoder's native output: converting to RGB in the decode call cost
/// about a millisecond a frame, and this shader does the same work as part of
/// sampling. Matrix and range are substituted at build time from the stream's
/// own tags — this host really does tag SDR as BT.601, and ignoring that
/// shifts every colour.
const SDR_YCBCR_SHADER: &str = r#"
@group(0) @binding(0) var luma_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;
@group(0) @binding(2) var chroma_tex: texture_2d<f32>;

const KR: f32 = __KR__;
const KB: f32 = __KB__;
const LUMA_OFFSET: f32 = __LUMA_OFFSET__;
const LUMA_SPAN: f32 = __LUMA_SPAN__;
const CHROMA_SPAN: f32 = __CHROMA_SPAN__;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var out: VsOut;
    let x = f32((i >> 1u) & 1u) * 4.0 - 1.0;
    let y = f32(i & 1u) * 4.0 - 1.0;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

// The surface is an *Srgb format: the hardware applies the encoding, so what
// the shader writes must be linear light.
fn srgb_to_linear(c: f32) -> f32 {
    if (c <= 0.04045) {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y_raw = textureSample(luma_tex, frame_samp, in.uv).r * 255.0;
    let c_raw = textureSample(chroma_tex, frame_samp, in.uv).rg * 255.0;

    let kg = 1.0 - KR - KB;
    let y = (y_raw - LUMA_OFFSET) / LUMA_SPAN;
    let cb = (c_raw.r - 128.0) / CHROMA_SPAN;
    let cr = (c_raw.g - 128.0) / CHROMA_SPAN;

    let r = y + 2.0 * (1.0 - KR) * cr;
    let g = y - 2.0 * KB * (1.0 - KB) / kg * cb - 2.0 * KR * (1.0 - KR) / kg * cr;
    let b = y + 2.0 * (1.0 - KB) * cb;
    let encoded = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(
        srgb_to_linear(encoded.r),
        srgb_to_linear(encoded.g),
        srgb_to_linear(encoded.b),
        1.0
    );
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
    /// Pipeline and layout for planar 10-bit HDR frames, which need two
    /// textures rather than one.
    hdr_pipeline: wgpu::RenderPipeline,
    /// The same, for a stream that uses the full 0-1023 range.
    hdr_pipeline_full: wgpu::RenderPipeline,
    hdr_bind_layout: wgpu::BindGroupLayout,
    /// Whether the surface is currently configured for PQ. Follows the
    /// content: PQ while HDR frames are on screen, sRGB otherwise — a surface
    /// interpreting one as the other shows every colour wrong.
    surface_is_pq: bool,
    /// Biplanar-SDR pipelines, indexed `[bt601][full_range]`.
    sdr_ycbcr: [[wgpu::RenderPipeline; 2]; 2],
    /// The two surface personalities this machine supports.
    sdr_format: wgpu::TextureFormat,
    pq_format: Option<wgpu::TextureFormat>,
    /// The toast pipeline for the PQ surface, when there is one.
    bar_pipeline_pq: Option<wgpu::RenderPipeline>,
    /// Stats overlay: pipelines per surface format, layout, position uniform,
    /// and the current text texture when there is one.
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_pipeline_pq: Option<wgpu::RenderPipeline>,
    overlay_bind_layout: wgpu::BindGroupLayout,
    overlay_uniform: wgpu::Buffer,
    overlay: Option<(wgpu::BindGroup, u32, u32)>,
    /// Wraps decoder surfaces as Metal textures, when the adapter is Metal.
    /// `None` falls back to the CPU-copy path.
    #[cfg(target_os = "macos")]
    vt_cache: Option<crate::vt_interop::VtTextureCache>,
    bar_pipeline: wgpu::RenderPipeline,
    bar_uniform: wgpu::Buffer,
    bar_bind: wgpu::BindGroup,
}

struct FrameTexture {
    texture: wgpu::Texture,
    /// Second plane, for planar formats. `None` for packed RGBA.
    chroma: Option<wgpu::Texture>,
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
        // 16-bit normalised textures are not on by default, and the planar
        // 10-bit path cannot be built without them. Requested only when the
        // adapter has them, so a device that lacks them still starts and
        // falls back to the packed path.
        let sixteen_bit = adapter
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: if sixteen_bit {
                wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
            } else {
                wgpu::Features::empty()
            },
            ..Default::default()
        }))
        .context("request device")?;
        if !sixteen_bit {
            tracing::warn!("no 16-bit texture support; 10-bit frames cannot be presented");
        }

        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface unsupported")?;

        // Ask for HDR10 before settling for SDR. `formats` deliberately lists
        // only what `Auto` can pick, which is never HDR, so the HDR-capable
        // formats have to be read from `format_capabilities` instead.
        // The surface must speak the *content's* language: SDR frames carry
        // sRGB values and HDR frames carry PQ, and a surface interpreting one
        // as the other shows every colour wrong without erroring. So the
        // surface starts sRGB, the PQ configuration is kept aside, and the
        // render path swaps between them when the stream's format changes.
        let caps = surface.get_capabilities(&adapter);
        let pq_format = caps
            .format_capabilities
            .iter()
            .find(|f| {
                f.color_spaces.contains(wgpu::SurfaceColorSpaces::BT2100_PQ)
                    && matches!(
                        f.format,
                        wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgb10a2Unorm
                    )
            })
            .map(|f| f.format);
        let sdr_format = config.format;
        match pq_format {
            Some(format) => tracing::info!(?format, "HDR surface available: BT.2100 PQ"),
            None => tracing::info!(
                format = ?sdr_format,
                "no HDR surface available; HDR frames will be tone-mapped to SDR"
            ),
        }
        let surface_is_pq = false;
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
        // Same as the SDR layout with a second texture: luma and chroma are
        // separate planes and separate resolutions, so they cannot share one.
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let hdr_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hdr"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
            ],
        });
        let hdr_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hdr"),
            bind_group_layouts: &[Some(&hdr_bind_layout)],
            ..Default::default()
        });
        // HDR frames are drawn to the PQ surface when the display has one and
        // tone-mapped onto the SDR surface when it does not, so the pipeline
        // targets whichever of those this machine will actually use.
        let hdr_target = pq_format.unwrap_or(sdr_format);
        let surface_takes_pq = pq_format.is_some();
        // One pipeline per range, rather than a uniform read on every pixel of
        // every frame for a value that changes once a session.
        let hdr_pipeline_for = |full_range: bool| {
            let (offset, luma_span, chroma_span) = if full_range {
                ("0.0", "1023.0", "1023.0")
            } else {
                ("64.0", "876.0", "896.0")
            };
            let source = HDR_SHADER
                .replace(
                    "__PQ_OUT__",
                    if surface_takes_pq { "true" } else { "false" },
                )
                .replace("__LUMA_OFFSET__", offset)
                .replace("__LUMA_SPAN__", luma_span)
                .replace("__CHROMA_SPAN__", chroma_span);
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("present-hdr"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("present-hdr"),
                layout: Some(&hdr_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(hdr_target.into())],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let hdr_pipeline = hdr_pipeline_for(false);
        let hdr_pipeline_full = hdr_pipeline_for(true);

        // The SDR biplanar pipelines: matrix and range fixed at build, like
        // the HDR ones — a uniform read per pixel for two per-session facts
        // would be the wrong trade.
        let sdr_ycbcr_for = |bt601: bool, full_range: bool| {
            let (kr, kb) = if bt601 {
                ("0.299", "0.114")
            } else {
                ("0.2126", "0.0722")
            };
            let (loff, lspan, cspan) = if full_range {
                ("0.0", "255.0", "255.0")
            } else {
                ("16.0", "219.0", "224.0")
            };
            let source = SDR_YCBCR_SHADER
                .replace("__KR__", kr)
                .replace("__KB__", kb)
                .replace("__LUMA_OFFSET__", loff)
                .replace("__LUMA_SPAN__", lspan)
                .replace("__CHROMA_SPAN__", cspan);
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("present-sdr-ycbcr"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("present-sdr-ycbcr"),
                layout: Some(&hdr_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(sdr_format.into())],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let sdr_ycbcr = [
            [sdr_ycbcr_for(false, false), sdr_ycbcr_for(false, true)],
            [sdr_ycbcr_for(true, false), sdr_ycbcr_for(true, true)],
        ];

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
        let bar_pipeline_for = |format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                        format,
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
            })
        };
        let bar_pipeline = bar_pipeline_for(sdr_format);
        let bar_pipeline_pq = pq_format.map(bar_pipeline_for);

        // The stats overlay: a textured quad whose position arrives by
        // uniform, compiled per surface format like the toast.
        let overlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay"),
            source: wgpu::ShaderSource::Wgsl(OVERLAY_SHADER.into()),
        });
        let overlay_bind_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("overlay"),
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let overlay_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let overlay_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("overlay"),
            bind_group_layouts: &[Some(&overlay_bind_layout)],
            ..Default::default()
        });
        let overlay_pipeline_for = |format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("overlay"),
                layout: Some(&overlay_layout),
                vertex: wgpu::VertexState {
                    module: &overlay_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &overlay_shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
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
            })
        };
        let overlay_pipeline = overlay_pipeline_for(sdr_format);
        let overlay_pipeline_pq = pq_format.map(overlay_pipeline_for);

        // Zero-copy adoption of decoder surfaces, where the adapter is Metal.
        #[cfg(target_os = "macos")]
        let vt_cache = crate::vt_interop::VtTextureCache::new(&device);
        #[cfg(target_os = "macos")]
        if vt_cache.is_some() {
            tracing::info!("decoder surfaces will be adopted zero-copy");
        }

        Ok(Self {
            hdr_pipeline,
            hdr_pipeline_full,
            hdr_bind_layout,
            surface_is_pq,
            sdr_ycbcr,
            sdr_format,
            pq_format,
            bar_pipeline_pq,
            overlay_pipeline,
            overlay_pipeline_pq,
            overlay_bind_layout,
            overlay_uniform,
            overlay: None,
            vt_cache,
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
        // Two planes at different resolutions, uploaded as they came out of
        // the decoder; the shader does the rest. The two planar layouts share
        // the machinery and differ only in sample width.
        if let PixelOrder::P010Bt2020Pq { .. } | PixelOrder::Nv12 { .. } = order {
            let (luma_format, chroma_format) = match order {
                PixelOrder::Nv12 { .. } => {
                    (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm)
                }
                _ => (
                    wgpu::TextureFormat::R16Unorm,
                    wgpu::TextureFormat::Rg16Unorm,
                ),
            };
            let plane = |label, w: u32, h: u32, format| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let luma = plane("luma", width, height, luma_format);
            let chroma = plane(
                "chroma",
                width.div_ceil(2),
                height.div_ceil(2),
                chroma_format,
            );
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("hdr"),
                layout: &self.hdr_bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &luma.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(
                            &chroma.create_view(&Default::default()),
                        ),
                    },
                ],
            });
            self.texture = Some(FrameTexture {
                texture: luma,
                chroma: Some(chroma),
                bind,
                width,
                height,
                order,
            });
            return;
        }

        // Match the decoder's byte order so no CPU swizzle ever happens
        // (VideoToolbox emits BGRA, openh264 RGBA).
        let format = match order {
            PixelOrder::Rgba => wgpu::TextureFormat::Rgba8UnormSrgb,
            PixelOrder::Bgra | PixelOrder::P010Bt2020Pq { .. } | PixelOrder::Nv12 { .. } => {
                wgpu::TextureFormat::Bgra8UnormSrgb
            }
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
            chroma: None,
            bind,
            width,
            height,
            order,
        });
    }

    /// Replace the stats overlay with a freshly rasterised image.
    fn set_overlay(&mut self, image: &crate::overlay::OverlayImage) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("overlay"),
            size: wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * 4),
                rows_per_image: Some(image.height),
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay"),
            layout: &self.overlay_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &texture.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.overlay_uniform.as_entire_binding(),
                },
            ],
        });
        self.overlay = Some((bind, image.width, image.height));
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
        // The surface follows the content: PQ for HDR frames (when the
        // display has a PQ mode), sRGB for everything else. Swapping is a
        // reconfigure, which happens only when the stream's format actually
        // changes — in practice once per session.
        let want_pq = frame.order.is_hdr() && self.pq_format.is_some();
        if want_pq != self.surface_is_pq {
            if want_pq {
                self.config.format = self.pq_format.expect("checked above");
                self.config.color_space = wgpu::SurfaceColorSpace::Bt2100Pq;
            } else {
                self.config.format = self.sdr_format;
                self.config.color_space = wgpu::SurfaceColorSpace::Auto;
            }
            self.surface.configure(&self.device, &self.config);
            self.surface_is_pq = want_pq;
            tracing::info!(pq = want_pq, format = ?self.config.format, "surface reconfigured");
        }
        // Zero-copy first: adopt the decoder's own surface as textures and
        // skip both per-frame copies. Falls back to the upload path for CPU
        // frames (software decode) or a non-Metal adapter.
        #[cfg(not(target_os = "macos"))]
        let adopted: Option<(wgpu::BindGroup, PixelOrder)> = None;
        #[cfg(target_os = "macos")]
        let adopted: Option<(wgpu::BindGroup, PixelOrder)> = frame
            .platform
            .as_ref()
            .and_then(|p| p.downcast_ref::<crate::decoder_vt::VtSurface>())
            .and_then(|surface| {
                use objc2_metal::MTLPixelFormat;
                let cache = self.vt_cache.as_ref()?;
                if surface.planar {
                    // Sample width follows the layout; everything else about
                    // the two planar paths is identical.
                    let (m_luma, w_luma, m_chroma, w_chroma) =
                        if matches!(frame.order, PixelOrder::Nv12 { .. }) {
                            (
                                MTLPixelFormat::R8Unorm,
                                wgpu::TextureFormat::R8Unorm,
                                MTLPixelFormat::RG8Unorm,
                                wgpu::TextureFormat::Rg8Unorm,
                            )
                        } else {
                            (
                                MTLPixelFormat::R16Unorm,
                                wgpu::TextureFormat::R16Unorm,
                                MTLPixelFormat::RG16Unorm,
                                wgpu::TextureFormat::Rg16Unorm,
                            )
                        };
                    let luma = cache.adopt_plane(
                        &self.device,
                        surface,
                        0,
                        m_luma,
                        w_luma,
                        frame.width,
                        frame.height,
                    )?;
                    let (cw, ch) = (frame.width.div_ceil(2), frame.height.div_ceil(2));
                    let chroma =
                        cache.adopt_plane(&self.device, surface, 1, m_chroma, w_chroma, cw, ch)?;
                    let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("vt-hdr"),
                        layout: &self.hdr_bind_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(
                                    &luma.texture.create_view(&Default::default()),
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(
                                    &chroma.texture.create_view(&Default::default()),
                                ),
                            },
                        ],
                    });
                    Some((bind, frame.order))
                } else {
                    let plane = cache.adopt_plane(
                        &self.device,
                        surface,
                        0,
                        MTLPixelFormat::BGRA8Unorm_sRGB,
                        wgpu::TextureFormat::Bgra8UnormSrgb,
                        frame.width,
                        frame.height,
                    )?;
                    let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("vt-sdr"),
                        layout: &self.bind_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(
                                    &plane.texture.create_view(&Default::default()),
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                        ],
                    });
                    Some((bind, frame.order))
                }
            });
        if adopted.is_none() {
            self.ensure_texture(frame.width, frame.height, frame.order);
        }

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
        if let (
            None,
            Some(FrameTexture {
                texture, chroma, ..
            }),
        ) = (&adopted, self.texture.as_ref())
        {
            let upload = |plane: &wgpu::Texture, bytes: &[u8], w: u32, h: u32, stride: u32| {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: plane,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    bytes,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(h),
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
            };
            match chroma {
                // Planar: luma then chroma, each in its own texture. Splitting at
                // the luma size is what the decoder packed them as.
                Some(chroma) => {
                    let (cw, ch) = (frame.width.div_ceil(2), frame.height.div_ceil(2));
                    let split = (frame.width * frame.height * 2) as usize;
                    let (luma_bytes, chroma_bytes) = frame.pixels.split_at(split);
                    upload(
                        texture,
                        luma_bytes,
                        frame.width,
                        frame.height,
                        frame.width * 2,
                    );
                    upload(chroma, chroma_bytes, cw, ch, cw * 4);
                }
                None => upload(
                    texture,
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    frame.width * 4,
                ),
            }
        }

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
            let (fw, fh) = (frame.width as f32, frame.height as f32);
            let scale = (sw / fw).min(sh / fh);
            let (vw, vh) = (fw * scale, fh * scale);
            pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw, vh, 0.0, 1.0);

            // The pipeline follows the frame's layout; the bind group is
            // either this frame's adopted surface or the persistent upload
            // texture.
            let order = match &adopted {
                Some((_, order)) => *order,
                None => match self.texture.as_ref() {
                    Some(t) => t.order,
                    None => return Ok(false),
                },
            };
            pass.set_pipeline(match order {
                PixelOrder::P010Bt2020Pq { full_range: false } => &self.hdr_pipeline,
                PixelOrder::P010Bt2020Pq { full_range: true } => &self.hdr_pipeline_full,
                PixelOrder::Nv12 { full_range, bt601 } => {
                    &self.sdr_ycbcr[usize::from(bt601)][usize::from(full_range)]
                }
                PixelOrder::Rgba | PixelOrder::Bgra => &self.pipeline,
            });
            match (&adopted, self.texture.as_ref()) {
                (Some((bind, _)), _) => pass.set_bind_group(0, bind, &[]),
                (None, Some(t)) => pass.set_bind_group(0, &t.bind, &[]),
                (None, None) => return Ok(false),
            }
            pass.draw(0..3, 0..1);

            // Toast overlay: full-surface viewport, alpha-blended quad on top.
            if toast.is_some() {
                pass.set_viewport(0.0, 0.0, sw, sh, 0.0, 1.0);
                pass.set_pipeline(if self.surface_is_pq {
                    self.bar_pipeline_pq.as_ref().unwrap_or(&self.bar_pipeline)
                } else {
                    &self.bar_pipeline
                });
                pass.set_bind_group(0, &self.bar_bind, &[]);
                pass.draw(0..6, 0..1);
            }

            // The stats overlay, top-left. Rasterised oversize and drawn
            // slightly smaller: the linear sampler softens the downscale,
            // which reads better than nearest-integer glyph blocks.
            if let Some((bind, w, h)) = &self.overlay {
                const DRAW_SCALE: f32 = 0.85;
                let (sw, sh) = (self.config.width as f32, self.config.height as f32);
                let (x0, y0) = (16.0f32, 16.0f32);
                let (x1, y1) = (x0 + *w as f32 * DRAW_SCALE, y0 + *h as f32 * DRAW_SCALE);
                let rect = [
                    x0 / sw * 2.0 - 1.0,
                    1.0 - y0 / sh * 2.0,
                    x1 / sw * 2.0 - 1.0,
                    1.0 - y1 / sh * 2.0,
                ];
                let mut bytes = [0u8; 16];
                for (i, v) in rect.iter().enumerate() {
                    bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
                }
                self.queue.write_buffer(&self.overlay_uniform, 0, &bytes);
                pass.set_pipeline(if self.surface_is_pq {
                    self.overlay_pipeline_pq
                        .as_ref()
                        .unwrap_or(&self.overlay_pipeline)
                } else {
                    &self.overlay_pipeline
                });
                pass.set_bind_group(0, bind, &[]);
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
