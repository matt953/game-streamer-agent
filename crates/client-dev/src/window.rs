//! Windowed presentation: winit + wgpu. The network/decode loop runs on its
//! own thread (with a private tokio runtime) and posts decoded frames to
//! the event loop; presentation uploads the frame as a texture and draws an
//! aspect-fit quad (GPU scaling — HiDPI handled by physical-pixel surface).

use std::sync::Arc;

use anyhow::{Context, Result};
use gsa_client_core::{Client, DecodedFrame, PixelOrder};
use gsa_core::id::SourceId;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::decoder::make_decoder;

#[derive(Debug)]
enum AppEvent {
    Ready(gsa_client_core::InputSender),
    Frame(Box<DecodedFrame>),
    StreamEnded(String),
}

pub fn run(addr: std::net::SocketAddr, source_id: Option<u32>, force_sw: bool) -> Result<()> {
    let event_loop = EventLoop::<AppEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    std::thread::Builder::new()
        .name("gsa-client-net".into())
        .spawn(move || network_loop(addr, source_id, force_sw, &proxy))?;

    let mut app = App::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn network_loop(
    addr: std::net::SocketAddr,
    source_id: Option<u32>,
    force_sw: bool,
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
            )
            .await?;
            let sources = client.list_sources().await?;
            let source = match source_id {
                Some(id) => sources
                    .iter()
                    .find(|s| s.id.0 == id)
                    .with_context(|| format!("agent has no source {id}"))?,
                None => sources.first().context("agent offers no sources")?,
            };
            client.start_session(SourceId(source.id.0), None).await?;

            if let Some(sender) = client.take_input_sender() {
                let _ = proxy.send_event(AppEvent::Ready(sender));
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

            let mut decoder = make_decoder(force_sw)?;
            let mut frames = 0u64;
            while let Some(out) = client.recv_frame(decoder.as_mut()).await? {
                frames += 1;
                if frames.is_multiple_of(300) {
                    let s = client.stats();
                    tracing::info!(
                        frames,
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
    input: Option<gsa_client_core::InputSender>,
    /// Presented content rect (letterboxed), for normalizing cursor coords.
    content_rect: Option<(f32, f32, f32, f32)>,
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
        let gpu = Gpu::new(window.clone()).expect("init wgpu");
        self.window = Some(window);
        self.gpu = Some(gpu);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Ready(sender) => self.input = Some(sender),
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
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(frame)) = (&mut self.gpu, &self.latest) {
                    self.content_rect = Some(gpu.content_rect(frame));
                    if let Err(e) = gpu.render(frame) {
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

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    texture: Option<FrameTexture>,
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

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            sampler,
            bind_layout,
            texture: None,
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

    fn render(&mut self, frame: &DecodedFrame) -> Result<()> {
        self.ensure_texture(frame.width, frame.height, frame.order);
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
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(output);
        Ok(())
    }
}
