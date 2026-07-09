//! Source and encoder factories. Everywhere: the TestPattern source +
//! software encoder (GPU-free, CI-friendly). On macOS additionally: real
//! displays via ScreenCaptureKit paired with the VideoToolbox hardware
//! encoder (spec 02/03).

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

    #[cfg(not(target_os = "macos"))]
    fn create_display(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        Err(Error::Session(format!("unknown source {id:?}")))
    }
}

impl SourceFactory for Sources {
    fn list(&self) -> Vec<SourceDescriptor> {
        let mut out = vec![TestPattern::new(TEST_PATTERN_ID, self.clock.clone()).descriptor()];
        #[cfg(target_os = "macos")]
        {
            match gsa_capture_macos::list_displays() {
                Ok(displays) => {
                    for d in displays {
                        out.push(
                            gsa_capture_macos::DesktopCapture::new(
                                SourceId(d.id),
                                d,
                                self.clock.clone(),
                            )
                            .descriptor(),
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "display enumeration failed"),
            }
        }
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
            other => Err(Error::Encode(format!(
                "no encoder for source kind {other:?}"
            ))),
        }
    }
}
