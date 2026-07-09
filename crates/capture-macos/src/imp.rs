//! ScreenCaptureKit desktop capture (spec 02). SCStream delivers
//! IOSurface-backed CVPixelBuffers on a dispatch queue; we retain each and
//! push it through the FrameSink with zero copy. VideoToolbox consumes the
//! same CVPixelBuffer downstream.

use std::any::Any;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

use gsa_capture_api::{
    FrameSink, GpuFrame, GpuHandle, PlatformFrame, RenderSource, SourceConfig, SourceDescriptor,
};
use gsa_core::media::PixelFormat;
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_protocol::control::{SourceInfo, SourceKind};
use gsa_protocol::input::{InputDisposition, InputEvent};

/// NV12 video-range FourCC ('420v') — VideoToolbox's native low-latency
/// input, so capture → encode needs no color conversion.
const PIXEL_FORMAT_NV12: u32 = u32::from_be_bytes(*b"420v");
const OUTPUT_TYPE_SCREEN: isize = 0;

/// A captured frame: an IOSurface-backed CVPixelBuffer, carried by handle to
/// the encoder. Downcast target for `GpuHandle::Platform`.
pub struct IoSurfaceFrame {
    pixel_buffer: CFRetained<CVPixelBuffer>,
}

impl IoSurfaceFrame {
    /// The underlying pixel buffer (handed straight to VTCompressionSession).
    #[must_use]
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.pixel_buffer
    }
}

impl std::fmt::Debug for IoSurfaceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoSurfaceFrame").finish_non_exhaustive()
    }
}

// SAFETY: an IOSurface-backed CVPixelBuffer is documented thread-safe; we
// only read it (submit to the encoder) and never mutate the pixels. The
// CoreFoundation refcount is atomic.
unsafe impl Send for IoSurfaceFrame {}
// SAFETY: see above.
unsafe impl Sync for IoSurfaceFrame {}

impl PlatformFrame for IoSurfaceFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Shared state the SCStreamOutput delegate needs on the callback queue.
struct DelegateState {
    sink: FrameSink,
    clock: MediaClock,
}

define_class!(
    // SAFETY: subclasses NSObject; no main-thread requirement (SCStream
    // callbacks run on the sample-handler queue we provide).
    #[unsafe(super(NSObject))]
    #[name = "GSAStreamOutput"]
    #[ivars = DelegateState]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            ty: SCStreamOutputType,
        ) {
            if ty.0 != OUTPUT_TYPE_SCREEN {
                return;
            }
            self.handle_sample(sample);
        }
    }
);

impl StreamOutput {
    fn new(state: DelegateState) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        // SAFETY: standard NSObject init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }

    fn handle_sample(&self, sample: &CMSampleBuffer) {
        // SAFETY: SCK provides a valid CVPixelBuffer image buffer on
        // screen-type sample buffers.
        let Some(pixel_buffer) = (unsafe { sample.image_buffer() }) else {
            return;
        };

        let width = CVPixelBufferGetWidth(&pixel_buffer) as u32;
        let height = CVPixelBufferGetHeight(&pixel_buffer) as u32;
        if width == 0 || height == 0 {
            return;
        }

        let state = self.ivars();
        state.sink.submit(GpuFrame {
            handle: GpuHandle::Platform(Arc::new(IoSurfaceFrame { pixel_buffer })),
            format: PixelFormat::Nv12,
            width,
            height,
            capture_ts_us: state.clock.now_us(),
            dirty_rects: None,
        });
    }
}

/// A capturable display, resolved from ScreenCaptureKit.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// Enumerate capturable displays. Blocks on ScreenCaptureKit's async query;
/// the Screen Recording TCC prompt surfaces here on first use.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let content = shareable_content()?;
    // SAFETY: `displays` returns a valid NSArray for a valid content object.
    let displays = unsafe { content.displays() };
    let mut out = Vec::new();
    for display in &displays {
        // SAFETY: each element is a valid SCDisplay.
        let (id, width, height) =
            unsafe { (display.displayID(), display.width() as u32, display.height() as u32) };
        out.push(DisplayInfo { id, width, height });
    }
    Ok(out)
}

/// Drive `getShareableContentWithCompletionHandler` synchronously.
///
/// The handler fires on an SCK-internal queue, so the payload must be `Send`:
/// we hand ownership across the channel as a raw pointer (`Retained` is
/// `!Send`) and reconstruct it on the caller's thread.
fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel::<std::result::Result<usize, String>>(1);
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let msg = if content.is_null() {
            Err(error_detail(error))
        } else {
            // SAFETY: non-null content from SCK; retain, then leak into a raw
            // pointer for transfer across the channel (reclaimed below).
            let retained = unsafe { Retained::retain(content) }.expect("non-null content");
            Ok(Retained::into_raw(retained) as usize)
        };
        let _ = tx.send(msg);
    });
    // SAFETY: valid escaping completion block; SCK invokes it once.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    match rx.recv_timeout(Duration::from_secs(10)) {
        // SAFETY: reclaim the exact pointer leaked in the handler — a single
        // ownership transfer, no double retain/release.
        Ok(Ok(addr)) => Ok(unsafe { Retained::from_raw(addr as *mut SCShareableContent) }
            .expect("non-null content")),
        Ok(Err(detail)) => Err(Error::Capture(format!(
            "ScreenCaptureKit unavailable (screen-recording permission?): {detail}"
        ))),
        Err(_) => Err(Error::Capture("ScreenCaptureKit query timed out".into())),
    }
}

/// Localized description of an optional NSError pointer.
fn error_detail(error: *mut NSError) -> String {
    NonNull::new(error)
        // SAFETY: non-null error pointer from SCK.
        .map(|e| unsafe { e.as_ref() }.localizedDescription().to_string())
        .unwrap_or_else(|| "unknown ScreenCaptureKit error".into())
}

/// Desktop capture of a single display via ScreenCaptureKit.
pub struct DesktopCapture {
    source_id: gsa_core::id::SourceId,
    display: DisplayInfo,
    clock: MediaClock,
    stream: Option<Retained<SCStream>>,
    delegate: Option<Retained<StreamOutput>>,
}

impl std::fmt::Debug for DesktopCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopCapture")
            .field("display", &self.display.id)
            .field("running", &self.stream.is_some())
            .finish()
    }
}

impl DesktopCapture {
    #[must_use]
    pub fn new(source_id: gsa_core::id::SourceId, display: DisplayInfo, clock: MediaClock) -> Self {
        Self { source_id, display, clock, stream: None, delegate: None }
    }
}

// SAFETY: `RenderSource: Send` — the session owns the source on one thread at
// a time (create/start/stop are never concurrent). SCStream's start/stop and
// ObjC release are safe to call from any single thread; the retained handles
// are only touched under that exclusive ownership.
unsafe impl Send for DesktopCapture {}

impl RenderSource for DesktopCapture {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            info: SourceInfo {
                id: self.source_id,
                kind: SourceKind::Display,
                name: format!(
                    "Display {} ({}x{})",
                    self.display.id, self.display.width, self.display.height
                ),
            },
            modes: Vec::new(),
        }
    }

    fn start(&mut self, cfg: SourceConfig, sink: FrameSink) -> Result<()> {
        if self.stream.is_some() {
            return Err(Error::Capture("capture already started".into()));
        }
        let content = shareable_content()?;
        // SAFETY: valid content object.
        let displays = unsafe { content.displays() };
        let display = displays
            .iter()
            // SAFETY: each element is a valid SCDisplay.
            .find(|d| unsafe { d.displayID() } == self.display.id)
            .ok_or_else(|| Error::Capture(format!("display {} vanished", self.display.id)))?;

        let empty_windows = NSArray::new();
        // SAFETY: valid display + empty exclusion list.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &empty_windows,
            )
        };

        // SAFETY: allocate + configure a fresh stream configuration.
        let config = unsafe { SCStreamConfiguration::new() };
        // SAFETY: setters on a freshly created configuration.
        unsafe {
            config.setWidth(cfg.mode.width as usize);
            config.setHeight(cfg.mode.height as usize);
            config.setPixelFormat(PIXEL_FORMAT_NV12);
            config.setShowsCursor(true);
            config.setQueueDepth(3);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: cfg.mode.fps.max(1) as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
        }

        let delegate = StreamOutput::new(DelegateState { sink, clock: self.clock.clone() });
        let output_proto = ProtocolObject::from_ref(&*delegate);

        // SAFETY: valid filter + config; nil stream delegate is allowed.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };

        let queue = DispatchQueue::new("com.gsa.capture.samples", None);
        // SAFETY: valid stream/output/queue; error reported via Result.
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output_proto,
                    SCStreamOutputType(OUTPUT_TYPE_SCREEN),
                    Some(&queue),
                )
                .map_err(|e| Error::Capture(format!("addStreamOutput: {e}")))?;
        }

        start_capture_blocking(&stream)?;
        self.stream = Some(stream);
        self.delegate = Some(delegate);
        tracing::info!(display = self.display.id, "ScreenCaptureKit capture started");
        Ok(())
    }

    fn handle_input(&mut self, _event: InputEvent) -> InputDisposition {
        // Desktop capture injects at the OS level (spec 07); routed by the
        // session, not consumed here.
        InputDisposition::PassToOs
    }

    fn reconfigure(&mut self, _cfg: SourceConfig) -> Result<()> {
        Err(Error::Capture("live reconfigure not yet implemented".into()))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            stop_capture_blocking(&stream)?;
        }
        self.delegate = None;
        Ok(())
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn start_capture_blocking(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Option<String>>(1);
    let handler = RcBlock::new(move |error: *mut NSError| {
        let detail = if error.is_null() { None } else { Some(error_detail(error)) };
        let _ = tx.send(detail);
    });
    // SAFETY: valid stream + escaping completion block.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(detail)) => Err(Error::Capture(format!("startCapture: {detail}"))),
        Err(_) => Err(Error::Capture("startCapture timed out".into())),
    }
}

fn stop_capture_blocking(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Option<String>>(1);
    let handler = RcBlock::new(move |error: *mut NSError| {
        let detail = if error.is_null() { None } else { Some(error_detail(error)) };
        let _ = tx.send(detail);
    });
    // SAFETY: valid stream + escaping completion block.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    // Best-effort: don't fail teardown on a stop error, just log.
    if let Ok(Some(detail)) = rx.recv_timeout(Duration::from_secs(5)) {
        tracing::warn!(detail, "stopCapture reported error");
    }
    Ok(())
}
