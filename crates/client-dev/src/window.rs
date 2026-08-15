//! Windowed presentation: winit + wgpu. The network/decode loop runs on its
//! own thread (with a private tokio runtime) and posts decoded frames to
//! the event loop; presentation uploads the frame as a texture and draws an
//! aspect-fit quad (GPU scaling — HiDPI handled by physical-pixel surface).

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use gsa_client_core::{Client, ControlEvent, DecodedFrame, PixelOrder};
use gsa_core::id::SourceId;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::decoder::make_decoder;
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
    /// Agent-pushed notification (e.g. host confirmed the virtual pad plugged).
    Notification(ControlEvent),
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
pub fn run_moonlight(
    addr: std::net::SocketAddr,
    app_id: u32,
    bitrate_mbps: u32,
    force_sw: bool,
    seconds: u64,
    synthetic_pad: bool,
) -> Result<()> {
    let event_loop = EventLoop::<AppEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    std::thread::Builder::new()
        .name("gsa-moonlight-net".into())
        .spawn(move || {
            moonlight_loop(
                addr,
                app_id,
                bitrate_mbps,
                force_sw,
                seconds,
                synthetic_pad,
                &proxy,
            )
        })?;

    let mut app = App::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "dev harness flags, not an API")]
fn moonlight_loop(
    addr: std::net::SocketAddr,
    app_id: u32,
    bitrate_mbps: u32,
    force_sw: bool,
    seconds: u64,
    synthetic_pad: bool,
    proxy: &EventLoopProxy<AppEvent>,
) {
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
                gsa_backend_moonlight::StreamMode::default(),
                bitrate_mbps.saturating_mul(1000),
            )
            .await
            .context("start moonlight session")?;

            // Everything from here is the shared core.
            let mut core = gsa_client_core::StreamSession::with_capture_clock(
                stream.take_frames().context("frames already taken")?,
                stream.recovery.clone(),
                gsa_core::time::MediaClock::new(),
                gsa_client_core::ClockSync::default(),
                stream.dropped.clone(),
                stream.recovered.clone(),
                // The host's stamps are a stream clock, so latency figures
                // from them would be fiction; cadence and jitter are real.
                gsa_client_core::CaptureClock::StreamPts,
            );

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

            // A synthetic pad, for probing what the host does once a controller
            // exists. Real pads come from `GamepadCapture`; this exists so
            // protocol work does not wait on hardware being awake.
            let _pad = synthetic_pad.then(|| spawn_synthetic_pad(stream.input.clone()));

            let mut decoder = make_decoder(force_sw)?;
            let mut frames = 0u64;
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
                // Drain the host's own messages. The channel is unbounded, and
                // what arrives on it is the only record of what the host did
                // with the pad we announced.
                while let Ok(message) = stream.events.try_recv() {
                    tracing::info!(?message, "host control message");
                    // Motion is opt-in: sampling starts here and not before.
                    if let Some(gsa_client_core::BackendEvent::MotionRequested { rate_hz, .. }) =
                        message.neutral()
                        && let Some(pad) = &_pad
                    {
                        pad.motion_hz
                            .store(u32::from(rate_hz), std::sync::atomic::Ordering::Relaxed);
                    }
                }
                // The display path is the source of presentation truth; without
                // this the health stats would report every frame as unshown.
                core.frame_presented(out.capture_ts_us);
                if frames.is_multiple_of(120) {
                    let present = core.present_stats();
                    let stats = core.stats();
                    tracing::info!(
                        frames,
                        present_fps = f64::from(present.fps_x100) / 100.0,
                        low1_fps = f64::from(present.low1_fps_x100) / 100.0,
                        freezes = present.freezes,
                        stutters = present.stutters,
                        latency_absolute = core.latency_is_absolute(),
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

            let mut decoder = make_decoder(force_sw)?;
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
    input: Option<std::sync::Arc<dyn gsa_client_core::InputSink>>,
    /// Live quality controls, when the backend has any.
    knobs: Option<std::sync::Arc<dyn gsa_client_core::SessionKnobs>>,
    /// Presented content rect (letterboxed), for normalizing cursor coords.
    content_rect: Option<(f32, f32, f32, f32)>,
    gamepad: Option<GamepadCapture>,
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
}

impl App {
    /// Refresh the window title with the live target bitrate (a lightweight HUD,
    /// since there's no on-screen text renderer) plus any active toast text.
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
                        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0)),
                )
                .expect("create window"),
        );
        window.set_cursor_visible(false);
        let gpu = Gpu::new(window.clone()).expect("init wgpu");
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.gamepad = GamepadCapture::new();
    }

    /// Poll the controller between events. Winit would otherwise sleep until
    /// the next frame or keystroke, and a gamepad generates neither.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let (Some(gamepad), Some(input)) = (&mut self.gamepad, &self.input)
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
            AppEvent::Ready(input, knobs, bitrate) => {
                self.input = Some(input);
                self.knobs = knobs;
                self.bitrate_bps = bitrate;
                self.update_title();
            }
            AppEvent::Frame(frame) => {
                self.latest = Some(frame);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::RecvMbps(mbps) => {
                self.recv_mbps = mbps;
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
                if let (Some(gpu), Some(frame)) = (&mut self.gpu, &self.latest) {
                    self.content_rect = Some(gpu.content_rect(frame));
                    let toast = self.toast.as_ref().map(|t| (t.color(), t.slide()));
                    if let Err(e) = gpu.render(frame, toast) {
                        tracing::warn!(error = %e, "render failed");
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
    fn new(window: Arc<Window>) -> Result<Self> {
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
        config.present_mode = wgpu::PresentMode::AutoNoVsync; // newest-wins, no vsync queueing
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

    fn render(&mut self, frame: &DecodedFrame, toast: Option<([f32; 4], f32)>) -> Result<()> {
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
        let output = match self.surface.get_current_texture() {
            Cst::Success(o) | Cst::Suboptimal(o) => o,
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(o) | Cst::Suboptimal(o) => o,
                    other => return Err(anyhow::anyhow!("surface after reconfigure: {other:?}")),
                }
            }
            Cst::Timeout | Cst::Occluded => return Ok(()), // skip this frame
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
        Ok(())
    }
}
