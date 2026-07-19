//! Windows.Graphics.Capture of a single monitor (spec 02).
//!
//! WGC delivers an `ID3D11Texture2D` per frame on an OS thread-pool callback —
//! the same shape as ScreenCaptureKit's sample queue, so pixels never enter
//! the async runtime. What we do with that texture depends on the output mode:
//! read it back to CPU for the software encoder, or hand it to a hardware
//! encoder untouched.

use std::sync::{Arc, Mutex};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

use gsa_capture_api::{
    AudioReceiver, CpuFrame, FrameSink, GpuFrame, GpuHandle, PlatformFrame, RenderSource,
    SourceConfig, SourceDescriptor, audio_channel,
};
use gsa_core::media::{PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_protocol::control::{SourceInfo, SourceKind};
use gsa_protocol::input::{InputDisposition, InputEvent};

use crate::audio::AudioCapture;
use crate::device::{create_device, ensure_mta, winrt_device};
use crate::display::{DisplayInfo, find_monitor};
use crate::frame::D3D11Frame;

/// Frame-pool depth. Two is WGC's documented minimum for a free-threaded
/// pool and all we need: the sink is depth-1 latest-frame-wins anyway.
const POOL_BUFFERS: i32 = 2;

/// Textures in the GPU-output ring. The sink holds at most one frame and the
/// encoder at most one more, so three is one spare — enough that encode can
/// trail capture by a frame without capture ever waiting.
const RING_TEXTURES: usize = 3;

/// Ceiling on the advertised frame rate. The monitor may refresh far faster
/// than the encoder can consume; the encoder's rate control is sized off this
/// number, and the depth-1 sink discards the surplus.
const MAX_ADVERTISED_FPS: u32 = 60;

/// Where captured pixels go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutput {
    /// Read back to a BGRA8 `CpuFrame`. Costs a GPU→CPU copy per frame, and
    /// is what the software encoder needs.
    CpuReadback,
    /// Emit `GpuHandle::Platform(D3D11Frame)` — no readback. `adapter_luid`
    /// pins the GPU so the texture lands on the encoder's device; `None`
    /// takes DXGI's default adapter.
    GpuTexture { adapter_luid: Option<i64> },
}

/// Desktop capture of a single monitor via Windows.Graphics.Capture.
#[derive(Debug)]
pub struct DesktopCapture {
    source_id: gsa_core::id::SourceId,
    display: DisplayInfo,
    clock: MediaClock,
    output: CaptureOutput,
    running: Option<Running>,
    /// Audio receiver, populated in `start`, taken by the session via `audio()`.
    audio_rx: Option<AudioReceiver>,
}

/// Live capture state. The WinRT objects are agile, and the audio handle is a
/// plain flag plus a thread, so this — and therefore `DesktopCapture` — is
/// `Send` without an unsafe assertion. The D3D11 objects, which are not agile,
/// live in [`D3dState`] instead.
#[derive(Debug)]
struct Running {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived: i64,
    sink: FrameSink,
    /// System audio, when the host has an endpoint we can capture.
    audio: Option<AudioCapture>,
}

impl DesktopCapture {
    #[must_use]
    pub fn new(
        source_id: gsa_core::id::SourceId,
        display: DisplayInfo,
        clock: MediaClock,
        output: CaptureOutput,
    ) -> Self {
        Self {
            source_id,
            display,
            clock,
            output,
            running: None,
            audio_rx: None,
        }
    }

    /// Begin loopback capture of the host's system audio, stashing the
    /// receiver for `audio()`.
    ///
    /// Audio is independent of the video path — it works the same under either
    /// [`CaptureOutput`] — and a host with no audio endpoint should still
    /// stream, so a failure here is a warning rather than an error.
    fn start_audio(&mut self) -> Option<AudioCapture> {
        let (sink, rx) = audio_channel();
        match crate::audio::start(sink, self.clock.clone()) {
            Ok(capture) => {
                self.audio_rx = Some(rx);
                Some(capture)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no system audio; streaming video only");
                None
            }
        }
    }
}

impl RenderSource for DesktopCapture {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            info: SourceInfo {
                id: self.source_id,
                kind: SourceKind::Display,
                name: format!(
                    "Display {} ({}x{})",
                    self.display.name, self.display.width, self.display.height
                ),
            },
            modes: vec![VideoMode {
                width: self.display.width,
                height: self.display.height,
                fps: self.display.refresh_hz.min(MAX_ADVERTISED_FPS),
            }],
        }
    }

    fn start(&mut self, cfg: SourceConfig, sink: FrameSink) -> Result<()> {
        if self.running.is_some() {
            return Err(Error::Capture("capture already started".into()));
        }
        // WGC has no scaler: frames arrive at the monitor's native size, and
        // the encoder is already open at `cfg.mode`. Refuse rather than feed
        // it mismatched frames.
        if cfg.mode.width != self.display.width || cfg.mode.height != self.display.height {
            return Err(Error::Capture(format!(
                "display {} captures at its native {}x{}, not {}x{}",
                self.display.name,
                self.display.width,
                self.display.height,
                cfg.mode.width,
                cfg.mode.height
            )));
        }
        ensure_mta();
        let monitor = find_monitor(&self.display)?;

        let adapter_luid = match self.output {
            CaptureOutput::CpuReadback => None,
            CaptureOutput::GpuTexture { adapter_luid } => adapter_luid,
        };
        let (device, context) = create_device(adapter_luid)?;
        let winrt_device = winrt_device(&device)?;
        let item = capture_item(monitor)?;
        let size = item
            .Size()
            .map_err(|e| Error::Capture(format!("capture item size: {e}")))?;
        if size.Width as u32 != self.display.width || size.Height as u32 != self.display.height {
            return Err(Error::Capture(format!(
                "monitor {} reports {}x{} but WGC offers {}x{}",
                self.display.name, self.display.width, self.display.height, size.Width, size.Height
            )));
        }

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            size,
        )
        .map_err(|e| Error::Capture(format!("create frame pool: {e}")))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| Error::Capture(format!("create capture session: {e}")))?;
        session
            .SetIsCursorCaptureEnabled(true)
            .map_err(|e| Error::Capture(format!("enable cursor capture: {e}")))?;
        // Suppressing the yellow capture border needs Windows 11 (and, in
        // some policies, a packaged identity). Cosmetic — carry on without.
        if let Err(e) = session.SetIsBorderRequired(false) {
            tracing::debug!(error = %e, "capture border not suppressible");
        }

        let sink_mode = match self.output {
            CaptureOutput::CpuReadback => Buffers::Cpu { staging: None },
            CaptureOutput::GpuTexture { .. } => Buffers::Gpu { ring: Vec::new() },
        };
        let state = Arc::new(Mutex::new(D3dState {
            device,
            context,
            winrt_device,
            size,
            buffers: sink_mode,
            ring_saturated: 0,
        }));
        let clock = self.clock.clone();
        let frame_sink = sink.clone();
        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _args| {
                // Stamp before any copy: this is the origin of every
                // glass-to-glass number (spec 01).
                let capture_ts_us = clock.now_us();
                let pool = pool.ok()?;
                let mut state = state.lock().expect("d3d state");
                if let Err(e) = state.deliver(pool, &frame_sink, capture_ts_us) {
                    tracing::warn!(error = %e, "dropped a captured frame");
                }
                Ok(())
            },
        );
        let frame_arrived = pool
            .FrameArrived(&handler)
            .map_err(|e| Error::Capture(format!("subscribe FrameArrived: {e}")))?;
        session
            .StartCapture()
            .map_err(|e| Error::Capture(format!("StartCapture: {e}")))?;

        let audio = self.start_audio();
        self.running = Some(Running {
            pool,
            session,
            frame_arrived,
            sink,
            audio,
        });
        tracing::info!(
            display = self.display.name,
            width = self.display.width,
            height = self.display.height,
            output = ?self.output,
            "Windows Graphics Capture started"
        );
        Ok(())
    }

    fn audio(&mut self) -> Option<AudioReceiver> {
        self.audio_rx.take()
    }

    fn handle_input(&mut self, _event: InputEvent) -> InputDisposition {
        // Desktop capture injects at the OS level (spec 07); routed by the
        // session, not consumed here.
        InputDisposition::PassToOs
    }

    fn reconfigure(&mut self, _cfg: SourceConfig) -> Result<()> {
        Err(Error::Capture(
            "live reconfigure not yet implemented".into(),
        ))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut running) = self.running.take() {
            if let Some(audio) = &mut running.audio {
                audio.stop();
            }
            let _ = running.pool.RemoveFrameArrived(running.frame_arrived);
            let _ = running.session.Close();
            let _ = running.pool.Close();
            running.sink.close();
        }
        self.audio_rx = None;
        Ok(())
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Per-mode scratch buffers, rebuilt whenever the capture size changes.
enum Buffers {
    /// One staging texture we map and copy out of.
    Cpu { staging: Option<ID3D11Texture2D> },
    /// Textures we own, handed downstream by handle. See [`D3dState::free_slot`].
    Gpu { ring: Vec<Arc<D3D11Frame>> },
}

/// D3D11 objects touched only from inside the `FrameArrived` callback.
struct D3dState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Held for `Direct3D11CaptureFramePool::Recreate` on a size change.
    winrt_device: IDirect3DDevice,
    size: SizeInt32,
    buffers: Buffers,
    /// Frames dropped because every ring texture was still referenced
    /// downstream — a saturated encoder shows up here, not in the sink stats.
    ring_saturated: u64,
}

impl std::fmt::Debug for D3dState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3dState")
            .field("size", &(self.size.Width, self.size.Height))
            .finish_non_exhaustive()
    }
}

// SAFETY: a D3D11 device is free-threaded, but its immediate context is not.
// `D3dState` is reachable only through the `Mutex` the FrameArrived handler
// locks, so at most one thread ever touches the context. WGC's free-threaded
// pool may deliver frames on different thread-pool threads, which is exactly
// what this assertion is for.
unsafe impl Send for D3dState {}

impl D3dState {
    /// Take the newest WGC frame and hand it downstream in whichever form
    /// this capture was configured for.
    fn deliver(
        &mut self,
        pool: &Direct3D11CaptureFramePool,
        sink: &FrameSink,
        capture_ts_us: u64,
    ) -> windows::core::Result<()> {
        let frame = pool.TryGetNextFrame()?;
        let content = frame.ContentSize()?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            // The surfaces are still pool-sized with the content in their
            // top-left corner, so this frame is unusable. Resize and skip it.
            drop(frame);
            self.resize(pool, content)?;
            return Ok(());
        }

        let surface = frame.Surface()?;
        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
        // SAFETY: `access` is the surface's own DXGI interface bridge, and
        // WGC surfaces are always backed by an ID3D11Texture2D.
        let captured = unsafe { access.GetInterface::<ID3D11Texture2D>()? };

        match self.buffers {
            Buffers::Cpu { .. } => self.deliver_cpu(&captured, sink, capture_ts_us),
            Buffers::Gpu { .. } => self.deliver_gpu(&captured, sink, capture_ts_us),
        }
    }

    fn resize(
        &mut self,
        pool: &Direct3D11CaptureFramePool,
        content: SizeInt32,
    ) -> windows::core::Result<()> {
        self.size = content;
        self.buffers = match self.buffers {
            Buffers::Cpu { .. } => Buffers::Cpu { staging: None },
            Buffers::Gpu { .. } => Buffers::Gpu { ring: Vec::new() },
        };
        pool.Recreate(
            &self.winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            content,
        )?;
        tracing::warn!(
            width = content.Width,
            height = content.Height,
            "capture size changed; the open encoder still expects the old one"
        );
        Ok(())
    }

    /// Copy through a staging texture into a CPU buffer.
    fn deliver_cpu(
        &mut self,
        captured: &ID3D11Texture2D,
        sink: &FrameSink,
        capture_ts_us: u64,
    ) -> windows::core::Result<()> {
        let staging = self.staging(captured)?;
        // SAFETY: both textures live for this call and have identical
        // descriptions apart from usage/CPU-access flags.
        unsafe { self.context.CopyResource(&staging, captured) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` is a STAGING texture with CPU_ACCESS_READ, so
        // subresource 0 is mappable for reading.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let stride = mapped.RowPitch as usize;
        let len = stride * self.size.Height as usize;
        let mut data = Vec::<u8>::with_capacity(len);
        // SAFETY: the map gives us `RowPitch * height` readable bytes at
        // `pData`, and `data` was just reserved for exactly that many.
        unsafe {
            std::ptr::copy_nonoverlapping(mapped.pData.cast::<u8>(), data.as_mut_ptr(), len);
            data.set_len(len);
        }
        // SAFETY: matches the `Map` above, same resource and subresource.
        unsafe { self.context.Unmap(&staging, 0) };

        sink.submit(self.gpu_frame(
            GpuHandle::Cpu(CpuFrame {
                data: Arc::new(data),
                stride,
            }),
            capture_ts_us,
        ));
        Ok(())
    }

    /// Copy GPU→GPU into a texture we own, and pass it on by handle.
    ///
    /// The copy is not waste: WGC recycles its pool textures as soon as the
    /// callback returns, so handing the encoder the raw capture texture would
    /// race the recycle. This stays on the GPU — no readback, no conversion.
    fn deliver_gpu(
        &mut self,
        captured: &ID3D11Texture2D,
        sink: &FrameSink,
        capture_ts_us: u64,
    ) -> windows::core::Result<()> {
        self.build_ring(captured)?;
        let Some(slot) = self.free_slot() else {
            // Every ring texture is still referenced downstream. Dropping this
            // frame is exactly what the depth-1 sink would have done anyway.
            self.ring_saturated += 1;
            if self.ring_saturated.is_multiple_of(30) {
                tracing::debug!(dropped = self.ring_saturated, "gpu ring saturated");
            }
            return Ok(());
        };
        // SAFETY: both textures are the same size and format; `slot` is a
        // texture we own with no live handles (checked by `free_slot`).
        unsafe { self.context.CopyResource(slot.texture(), captured) };

        let handle = GpuHandle::Platform(slot.clone() as Arc<dyn PlatformFrame>);
        sink.submit(self.gpu_frame(handle, capture_ts_us));
        Ok(())
    }

    fn gpu_frame(&self, handle: GpuHandle, capture_ts_us: u64) -> GpuFrame {
        GpuFrame {
            handle,
            format: PixelFormat::Bgra8,
            width: self.size.Width as u32,
            height: self.size.Height as u32,
            capture_ts_us,
            dirty_rects: None,
        }
    }

    /// The readback texture, created on first use from the source's own
    /// description so format and size always agree.
    fn staging(&mut self, source: &ID3D11Texture2D) -> windows::core::Result<ID3D11Texture2D> {
        if let Buffers::Cpu {
            staging: Some(staging),
        } = &self.buffers
        {
            return Ok(staging.clone());
        }
        let mut desc = texture_desc(source);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;

        let staging = self.create_texture(&desc)?;
        self.buffers = Buffers::Cpu {
            staging: Some(staging.clone()),
        };
        Ok(staging)
    }

    /// Populate the GPU ring on first use, matching the captured texture.
    fn build_ring(&mut self, source: &ID3D11Texture2D) -> windows::core::Result<()> {
        let Buffers::Gpu { ring } = &self.buffers else {
            return Ok(());
        };
        if !ring.is_empty() {
            return Ok(());
        }
        let mut desc = texture_desc(source);
        desc.Usage = D3D11_USAGE_DEFAULT;
        // Hardware encoders register the texture as a shader resource; the
        // render-target bind is what lets D3D copy into it.
        desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        desc.CPUAccessFlags = 0;
        desc.MiscFlags = 0;

        let mut ring = Vec::with_capacity(RING_TEXTURES);
        for _ in 0..RING_TEXTURES {
            let texture = self.create_texture(&desc)?;
            ring.push(Arc::new(D3D11Frame::new(texture, self.device.clone())));
        }
        self.buffers = Buffers::Gpu { ring };
        Ok(())
    }

    /// A ring texture nothing downstream still holds.
    ///
    /// `strong_count == 1` means the ring is the only owner: the sink has
    /// dropped it and the encoder has finished with it. That is a fact about
    /// the `Arc`, not a guess about timing, so overwriting it cannot race a
    /// reader.
    fn free_slot(&self) -> Option<Arc<D3D11Frame>> {
        let Buffers::Gpu { ring } = &self.buffers else {
            return None;
        };
        ring.iter().find(|f| Arc::strong_count(f) == 1).cloned()
    }

    fn create_texture(
        &self,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> windows::core::Result<ID3D11Texture2D> {
        let mut texture = None;
        // SAFETY: a valid description; the out-param is initialized on success
        // and left `None` otherwise.
        unsafe {
            self.device
                .CreateTexture2D(desc, None, Some(&mut texture))?
        };
        Ok(texture.expect("CreateTexture2D succeeded without a texture"))
    }
}

fn texture_desc(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `texture` is live; `GetDesc` only writes `desc`.
    unsafe { texture.GetDesc(&mut desc) };
    desc
}

/// The `GraphicsCaptureItem` for a monitor, via the WinRT interop factory —
/// there is no projected constructor for this.
fn capture_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| Error::Capture(format!("capture interop factory: {e}")))?;
    // SAFETY: `monitor` was just re-resolved from EnumDisplayMonitors.
    unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }
        .map_err(|e| Error::Capture(format!("CreateForMonitor: {e}")))
}
