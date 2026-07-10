//! Source and encoder factories. Everywhere: the TestPattern source +
//! software encoder (GPU-free, CI-friendly). On macOS additionally: real
//! displays via ScreenCaptureKit paired with the VideoToolbox hardware
//! encoder (spec 02/03). On Windows: Windows Graphics Capture, paired with
//! NVENC when the host has one and the software encoder when it does not.

use gsa_capture_api::{RenderSource, SourceDescriptor};
use gsa_core::id::SourceId;
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::Encoder;
use gsa_protocol::control::SourceKind;
use gsa_session::{EncoderFactory, SourceFactory};
use gsa_sources::TestPattern;

/// SourceId reserved for the synthetic test pattern; real displays use their
/// platform display id.
const TEST_PATTERN_ID: SourceId = SourceId(0);

/// Whether this host can hardware-encode, decided once and shared.
///
/// The source and the encoder **must** agree: NVENC consumes GPU textures and
/// the software encoder consumes CPU frames, so a split decision would pair a
/// source with an encoder that cannot read it. One cached probe, two readers.
#[cfg(target_os = "windows")]
fn nvenc() -> Option<gsa_encode_nvenc::Support> {
    static SUPPORT: std::sync::OnceLock<Option<gsa_encode_nvenc::Support>> =
        std::sync::OnceLock::new();
    *SUPPORT.get_or_init(gsa_encode_nvenc::probe)
}

/// Where Windows capture should put its pixels, given [`nvenc`].
#[cfg(target_os = "windows")]
fn windows_capture_output() -> gsa_capture_windows::CaptureOutput {
    match nvenc() {
        Some(support) => gsa_capture_windows::CaptureOutput::GpuTexture {
            adapter_luid: Some(support.adapter_luid),
        },
        None => gsa_capture_windows::CaptureOutput::CpuReadback,
    }
}

pub struct Sources {
    clock: MediaClock,
}

impl Sources {
    pub fn new(clock: MediaClock) -> Self {
        Self { clock }
    }

    #[cfg(target_os = "macos")]
    fn create_display(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        let display = gsa_capture_macos::list_displays()?
            .into_iter()
            .find(|d| d.id == id.0)
            .ok_or_else(|| Error::Session(format!("unknown display {id:?}")))?;
        Ok(Box::new(gsa_capture_macos::DesktopCapture::new(
            id,
            display,
            self.clock.clone(),
        )))
    }

    #[cfg(target_os = "windows")]
    fn create_display(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        let display = gsa_capture_windows::list_displays()?
            .into_iter()
            .find(|d| d.id == id.0)
            .ok_or_else(|| Error::Session(format!("unknown display {id:?}")))?;
        Ok(Box::new(gsa_capture_windows::DesktopCapture::new(
            id,
            display,
            self.clock.clone(),
            windows_capture_output(),
        )))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn create_display(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        Err(Error::Session(format!("unknown source {id:?}")))
    }

    #[cfg(target_os = "macos")]
    fn display_descriptors(&self) -> Vec<SourceDescriptor> {
        match gsa_capture_macos::list_displays() {
            Ok(displays) => displays
                .into_iter()
                .map(|d| {
                    gsa_capture_macos::DesktopCapture::new(SourceId(d.id), d, self.clock.clone())
                        .descriptor()
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "display enumeration failed");
                Vec::new()
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn display_descriptors(&self) -> Vec<SourceDescriptor> {
        match gsa_capture_windows::list_displays() {
            Ok(displays) => displays
                .into_iter()
                .map(|d| {
                    gsa_capture_windows::DesktopCapture::new(
                        SourceId(d.id),
                        d,
                        self.clock.clone(),
                        windows_capture_output(),
                    )
                    .descriptor()
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "display enumeration failed");
                Vec::new()
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn display_descriptors(&self) -> Vec<SourceDescriptor> {
        Vec::new() // platform capture backends land at M4/M5
    }
}

impl SourceFactory for Sources {
    fn list(&self) -> Vec<SourceDescriptor> {
        let mut out = vec![TestPattern::new(TEST_PATTERN_ID, self.clock.clone()).descriptor()];
        out.extend(self.display_descriptors());
        out
    }

    fn create(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        if id == TEST_PATTERN_ID {
            return Ok(Box::new(TestPattern::new(id, self.clock.clone())));
        }
        self.create_display(id)
    }
}

pub struct Encoders {
    clock: MediaClock,
}

impl Encoders {
    pub fn new(clock: MediaClock) -> Self {
        Self { clock }
    }
}

impl EncoderFactory for Encoders {
    fn create(&self, source_kind: SourceKind) -> Result<Box<dyn Encoder>> {
        match source_kind {
            // Test pattern is CPU/BGRA → software encoder (all platforms).
            SourceKind::TestPattern => {
                Ok(Box::new(gsa_encode_sw::SwEncoder::new(self.clock.clone())))
            }
            // Real display / virtual display → hardware encoder.
            #[cfg(target_os = "macos")]
            SourceKind::Display | SourceKind::VirtualDisplay => Ok(Box::new(
                gsa_encode_videotoolbox::VideoToolboxEncoder::new(self.clock.clone()),
            )),
            // NVENC where the host has it, software where it does not. Gated
            // on the same cached probe the source uses, so the frame format
            // the source emits always matches this encoder's `input_formats`.
            #[cfg(target_os = "windows")]
            SourceKind::Display | SourceKind::VirtualDisplay => {
                if nvenc().is_some() {
                    Ok(Box::new(gsa_encode_nvenc::NvencEncoder::new(
                        self.clock.clone(),
                    )))
                } else {
                    Ok(Box::new(gsa_encode_sw::SwEncoder::new(self.clock.clone())))
                }
            }
            other => Err(Error::Encode(format!(
                "no encoder for source kind {other:?}"
            ))),
        }
    }
}
