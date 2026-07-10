//! Windows.Graphics.Capture desktop capture (spec 02). WGC hands us a
//! D3D11 texture per frame on an OS thread-pool callback; we copy it through
//! a staging texture into a BGRA8 `CpuFrame` for the software encoder.
//!
//! The zero-copy `GpuHandle::Platform` path (feed the texture straight to
//! NVENC / Media Foundation) is the M4 hardware-encode work, not this.

use std::sync::{Arc, Mutex, OnceLock};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::{BOOL, IInspectable, Interface, PCWSTR};

use gsa_capture_api::{
    CpuFrame, FrameSink, GpuFrame, GpuHandle, RenderSource, SourceConfig, SourceDescriptor,
};
use gsa_core::media::{PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_protocol::control::{SourceInfo, SourceKind};
use gsa_protocol::input::{InputDisposition, InputEvent};

/// Frame-pool depth. Two is WGC's documented minimum for a free-threaded
/// pool and all we need: the sink is depth-1 latest-frame-wins anyway.
const POOL_BUFFERS: i32 = 2;

/// Ceiling on the advertised frame rate. The monitor may refresh far faster
/// than the software encoder can consume; the encoder's rate control is sized
/// off this number, and the depth-1 sink discards the surplus.
const MAX_ADVERTISED_FPS: u32 = 60;

/// A capturable monitor.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// Stable id derived from `name` — see [`display_id`].
    pub id: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Whether this Windows build supports Windows.Graphics.Capture at all
/// (it needs 1903+). Reported by `gsa doctor`.
#[must_use]
pub fn capture_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// Enumerate capturable monitors. Unlike macOS this needs no permission
/// grant, so it only fails if the Win32 calls themselves fail.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    ensure_dpi_aware();
    let mut out = Vec::new();
    for monitor in monitors()? {
        let Some(info) = monitor_info(monitor) else {
            continue;
        };
        out.push(info);
    }
    Ok(out)
}

/// Desktop capture of a single monitor via Windows.Graphics.Capture.
#[derive(Debug)]
pub struct DesktopCapture {
    source_id: gsa_core::id::SourceId,
    display: DisplayInfo,
    clock: MediaClock,
    running: Option<Running>,
}

/// Live capture state. Every field is a WinRT (agile) object, so this — and
/// therefore `DesktopCapture` — is `Send` without an unsafe assertion. The
/// D3D11 objects, which are not agile, live in [`D3dState`] instead.
#[derive(Debug)]
struct Running {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived: i64,
    sink: FrameSink,
}

impl DesktopCapture {
    #[must_use]
    pub fn new(source_id: gsa_core::id::SourceId, display: DisplayInfo, clock: MediaClock) -> Self {
        Self {
            source_id,
            display,
            clock,
            running: None,
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

        let (device, context) = create_device()?;
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

        let state = Arc::new(Mutex::new(D3dState {
            device,
            context,
            winrt_device,
            staging: None,
            size,
        }));
        let clock = self.clock.clone();
        let frame_sink = sink.clone();
        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _args| {
                // Stamp before the readback: this is the origin of every
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

        self.running = Some(Running {
            pool,
            session,
            frame_arrived,
            sink,
        });
        tracing::info!(
            display = self.display.name,
            width = self.display.width,
            height = self.display.height,
            "Windows Graphics Capture started"
        );
        Ok(())
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
        if let Some(running) = self.running.take() {
            let _ = running.pool.RemoveFrameArrived(running.frame_arrived);
            let _ = running.session.Close();
            let _ = running.pool.Close();
            running.sink.close();
        }
        Ok(())
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// D3D11 objects touched only from inside the `FrameArrived` callback.
struct D3dState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Held for `Direct3D11CaptureFramePool::Recreate` on a size change.
    winrt_device: IDirect3DDevice,
    /// Readback texture, sized to `size`; rebuilt when the size changes.
    staging: Option<ID3D11Texture2D>,
    size: SizeInt32,
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
    /// Copy the newest WGC frame into a CPU buffer and submit it.
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
            self.size = content;
            self.staging = None;
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
            return Ok(());
        }

        let surface = frame.Surface()?;
        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
        // SAFETY: `access` is the surface's own DXGI interface bridge, and
        // WGC surfaces are always backed by an ID3D11Texture2D.
        let texture = unsafe { access.GetInterface::<ID3D11Texture2D>()? };

        let staging = self.staging(&texture)?;
        // SAFETY: both textures live for this call and have identical
        // descriptions apart from usage/CPU-access flags.
        unsafe { self.context.CopyResource(&staging, &texture) };

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

        sink.submit(GpuFrame {
            handle: GpuHandle::Cpu(CpuFrame {
                data: Arc::new(data),
                stride,
            }),
            format: PixelFormat::Bgra8,
            width: self.size.Width as u32,
            height: self.size.Height as u32,
            capture_ts_us,
            dirty_rects: None,
        });
        Ok(())
    }

    /// The readback texture, created on first use from the source's own
    /// description so format and size always agree.
    fn staging(&mut self, source: &ID3D11Texture2D) -> windows::core::Result<ID3D11Texture2D> {
        if let Some(staging) = &self.staging {
            return Ok(staging.clone());
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `source` is a live texture; `GetDesc` only writes `desc`.
        unsafe { source.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;

        let mut staging = None;
        // SAFETY: a valid staging description; the out-param is initialized
        // on success and left `None` otherwise.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?
        };
        let staging = staging.expect("CreateTexture2D succeeded without a texture");
        self.staging = Some(staging.clone());
        Ok(staging)
    }
}

/// Put the process in the MTA for the lifetime of the process. WGC's
/// free-threaded pool calls back on thread-pool threads, so there is no
/// apartment we could scope this to; the cookie is deliberately leaked.
fn ensure_mta() {
    static MTA: OnceLock<()> = OnceLock::new();
    MTA.get_or_init(|| {
        // SAFETY: no preconditions; failure only means some other component
        // already established the process's apartment, which suits us.
        if let Err(e) = unsafe { CoIncrementMTAUsage() } {
            tracing::warn!(error = %e, "CoIncrementMTAUsage failed");
        }
    });
}

/// Report monitor geometry in physical pixels, which is what WGC captures.
/// Without this a DPI-unaware process sees scaled logical sizes and the
/// frame-pool size disagrees with the texture it gets back.
fn ensure_dpi_aware() {
    static DPI: OnceLock<()> = OnceLock::new();
    DPI.get_or_init(|| {
        // SAFETY: no preconditions. Fails harmlessly if awareness was already
        // set (e.g. by an application manifest).
        let set =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if let Err(e) = set {
            tracing::debug!(error = %e, "process DPI awareness already set");
        }
    });
}

/// Stable source id for a monitor: FNV-1a over its GDI device name.
/// `HMONITOR` values churn across sessions and monitor indices shift when a
/// display is unplugged, but `\\.\DISPLAYn` is reproducible — which is what
/// `--source <id>` needs. Never 0; that id is reserved for the test pattern.
fn display_id(device: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in device.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

/// `EnumDisplayMonitors` callback: append each monitor to the caller's `Vec`.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<HMONITOR>` handed to EnumDisplayMonitors
    // below, which outlives the (synchronous) enumeration.
    let out = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    out.push(monitor);
    TRUE
}

fn monitors() -> Result<Vec<HMONITOR>> {
    let mut out: Vec<HMONITOR> = Vec::new();
    // SAFETY: enumeration is synchronous, so the pointer to `out` stays valid
    // for every `collect_monitor` call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut out as isize),
        )
    };
    if !ok.as_bool() {
        return Err(Error::Capture("EnumDisplayMonitors failed".into()));
    }
    Ok(out)
}

/// Geometry + refresh rate for one monitor, or `None` if Windows won't
/// describe it (it was unplugged between enumeration and this call).
fn monitor_info(monitor: HMONITOR) -> Option<DisplayInfo> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `cbSize` marks this as the EX form, so Windows writes at most
    // `size_of::<MONITORINFOEXW>()` bytes.
    let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;
    let name = wide_to_string(&info.szDevice);
    Some(DisplayInfo {
        id: display_id(&name),
        width: (rect.right - rect.left).unsigned_abs(),
        height: (rect.bottom - rect.top).unsigned_abs(),
        refresh_hz: refresh_hz(&info.szDevice),
        name,
    })
}

/// Current refresh rate for a GDI device, defaulting to 60 Hz — the values
/// 0 and 1 mean "the hardware default" rather than a real frequency.
fn refresh_hz(device: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device` is a NUL-terminated wide string inside MONITORINFOEXW,
    // and `dmSize` bounds what Windows writes into `mode`.
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &raw mut mode,
        )
    };
    if !ok.as_bool() || mode.dmDisplayFrequency < 2 {
        return 60;
    }
    mode.dmDisplayFrequency
}

fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|c| *c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

/// Re-resolve the `HMONITOR` for a display at `start` time; the handle we
/// enumerated earlier may have been invalidated by a topology change.
fn find_monitor(display: &DisplayInfo) -> Result<HMONITOR> {
    monitors()?
        .into_iter()
        .find(|m| monitor_info(*m).is_some_and(|i| i.id == display.id))
        .ok_or_else(|| Error::Capture(format!("display {} vanished", display.name)))
}

/// A BGRA-capable hardware D3D11 device and its immediate context.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: null adapter selects the default one for the driver type; the
    // out-params are written only on success.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| Error::Capture(format!("D3D11CreateDevice: {e}")))?;
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(Error::Capture(
            "D3D11CreateDevice returned no device".into(),
        )),
    }
}

/// Wrap a D3D11 device as the WinRT `IDirect3DDevice` the frame pool wants.
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi = device
        .cast::<IDXGIDevice>()
        .map_err(|e| Error::Capture(format!("device is not a DXGI device: {e}")))?;
    // SAFETY: `dxgi` is a live IDXGIDevice, the documented input.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .map_err(|e| Error::Capture(format!("CreateDirect3D11DeviceFromDXGIDevice: {e}")))?;
    inspectable
        .cast::<IDirect3DDevice>()
        .map_err(|e| Error::Capture(format!("not an IDirect3DDevice: {e}")))
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

#[cfg(test)]
mod tests {
    use super::display_id;

    #[test]
    fn display_ids_are_stable_distinct_and_nonzero() {
        assert_eq!(display_id(r"\\.\DISPLAY1"), display_id(r"\\.\DISPLAY1"));
        assert_ne!(display_id(r"\\.\DISPLAY1"), display_id(r"\\.\DISPLAY2"));
        assert_ne!(display_id(r"\\.\DISPLAY1"), 0);
    }
}
