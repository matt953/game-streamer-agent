//! VideoToolbox **hardware** decoder for the dev harness (macOS): Annex-B in,
//! IOSurface-backed NV12 `CVPixelBuffer` out. Zero-copy by construction — the
//! decoded frame is never mapped in the measured path; pixel readback exists
//! only as an explicit, sampled verification helper. Session creation
//! *requires* the hardware decoder: there is no software fallback here.

#[cfg(target_os = "macos")]
mod imp;
#[cfg(target_os = "macos")]
pub use imp::{DecodedSurface, VtDecoder};

#[cfg(not(target_os = "macos"))]
mod stub {
    use gsa_core::media::Codec;
    use gsa_core::{Error, Result};

    #[derive(Debug)]
    pub struct DecodedSurface;
    impl DecodedSurface {
        #[must_use]
        pub fn width(&self) -> u32 {
            0
        }
        #[must_use]
        pub fn height(&self) -> u32 {
            0
        }
        pub fn read_luma_region(&self, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
            Err(Error::Decode("VideoToolbox is macOS-only".into()))
        }
    }

    #[derive(Debug)]
    pub struct VtDecoder;
    impl VtDecoder {
        pub fn new(_codec: Codec) -> Result<Self> {
            Err(Error::Decode("VideoToolbox is macOS-only".into()))
        }
        pub fn decode(&mut self, _annex_b: &[u8]) -> Result<Option<DecodedSurface>> {
            Err(Error::Decode("VideoToolbox is macOS-only".into()))
        }
    }
}
#[cfg(not(target_os = "macos"))]
pub use stub::{DecodedSurface, VtDecoder};
