//! M0 factories: TestPattern sources + software encoder. Platform capture
//! and hardware-encoder probing (spec 03) replace these from M1.

use gsa_capture_api::{RenderSource, SourceDescriptor};
use gsa_core::id::SourceId;
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::Encoder;
use gsa_session::{EncoderFactory, SourceFactory};
use gsa_sources::TestPattern;

pub struct TestSources {
    clock: MediaClock,
}

impl TestSources {
    pub fn new(clock: MediaClock) -> Self {
        Self { clock }
    }
}

impl SourceFactory for TestSources {
    fn list(&self) -> Vec<SourceDescriptor> {
        vec![TestPattern::new(SourceId(0), self.clock.clone()).descriptor()]
    }

    fn create(&self, id: SourceId) -> Result<Box<dyn RenderSource>> {
        if id != SourceId(0) {
            return Err(Error::Session(format!("unknown source {id:?}")));
        }
        Ok(Box::new(TestPattern::new(id, self.clock.clone())))
    }
}

pub struct SwEncoders {
    clock: MediaClock,
}

impl SwEncoders {
    pub fn new(clock: MediaClock) -> Self {
        Self { clock }
    }
}

impl EncoderFactory for SwEncoders {
    fn create(&self) -> Result<Box<dyn Encoder>> {
        Ok(Box::new(gsa_encode_sw::SwEncoder::new(self.clock.clone())))
    }
}
