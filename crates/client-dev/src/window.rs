//! Windowed presentation: winit + softbuffer. The network/decode loop runs
//! on its own thread (with a private tokio runtime) and posts decoded
//! frames to the event loop; presentation is a dumb blit.

use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{Context, Result};
use gsa_client_core::{Client, DecodedFrame};
use gsa_core::id::SourceId;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::decoder::OpenH264Decoder;

#[derive(Debug)]
enum AppEvent {
    Frame(Box<DecodedFrame>),
    StreamEnded(String),
}

pub fn run(addr: std::net::SocketAddr, source_id: Option<u32>) -> Result<()> {
    let event_loop = EventLoop::<AppEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    std::thread::Builder::new()
        .name("gsa-client-net".into())
        .spawn(move || network_loop(addr, source_id, &proxy))?;

    let mut app = App::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn network_loop(
    addr: std::net::SocketAddr,
    source_id: Option<u32>,
    proxy: &EventLoopProxy<AppEvent>,
) {
    let outcome = (|| -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("client runtime")?;
        runtime.block_on(async {
            let mut client = Client::connect(addr, "client-dev").await?;
            let sources = client.list_sources().await?;
            let source = match source_id {
                Some(id) => sources
                    .iter()
                    .find(|s| s.id.0 == id)
                    .with_context(|| format!("agent has no source {id}"))?,
                None => sources.first().context("agent offers no sources")?,
            };
            client.start_session(SourceId(source.id.0), None).await?;

            let mut decoder = OpenH264Decoder::new()?;
            let mut frames = 0u64;
            while let Some(out) = client.recv_frame(&mut decoder).await? {
                frames += 1;
                if frames.is_multiple_of(300) {
                    let s = client.stats();
                    tracing::info!(
                        frames,
                        latency_ms_p50 = ?s.latency_ms_p50,
                        latency_ms_p99 = ?s.latency_ms_p99,
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
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    latest: Option<Box<DecodedFrame>>,
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
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("surface");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Frame(frame) => {
                self.latest = Some(frame);
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
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

impl App {
    fn redraw(&mut self) {
        let (Some(window), Some(surface), Some(frame)) =
            (&self.window, &mut self.surface, &self.latest)
        else {
            return;
        };
        let size = window.inner_size();
        let (Some(sw), Some(sh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if surface.resize(sw, sh).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        // Dumb 1:1 blit, top-left anchored; black elsewhere. (Scaling can
        // come later — this is a debug harness.)
        buffer.fill(0);
        let copy_w = frame.width.min(size.width) as usize;
        let copy_h = frame.height.min(size.height) as usize;
        for row in 0..copy_h {
            let src = row * frame.width as usize * 4;
            let dst = row * size.width as usize;
            for col in 0..copy_w {
                let p = src + col * 4;
                let (r, g, b) = (
                    frame.rgba[p] as u32,
                    frame.rgba[p + 1] as u32,
                    frame.rgba[p + 2] as u32,
                );
                buffer[dst + col] = (r << 16) | (g << 8) | b;
            }
        }
        let _ = buffer.present();
    }
}
