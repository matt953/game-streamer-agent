//! Decoder selection + the portable openh264 software decoder. On macOS the
//! default is hardware VideoToolbox decode (`decoder_vt`), with software as
//! the explicit fallback / cross-platform path.

use anyhow::Context;
use gsa_client_core::{DecodedFrame, VideoDecoder};
use gsa_core::media::H264Profile;
use gsa_core::{Error, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// Pick the platform's best decoder (`force_sw` pins openh264).
pub fn make_decoder(force_sw: bool) -> anyhow::Result<Box<dyn VideoDecoder>> {
    #[cfg(target_os = "macos")]
    if !force_sw {
        tracing::info!("using VideoToolbox hardware decoder");
        return Ok(Box::new(crate::decoder_vt::VideoToolboxDecoder::new()));
    }
    let _ = force_sw;
    tracing::info!("using openh264 software decoder");
    Ok(Box::new(OpenH264Decoder::new()?))
}

/// Highest H.264 profile the decoder chosen by `make_decoder` can handle.
/// Keep in sync with `make_decoder`.
#[must_use]
pub fn decoder_max_profile(force_sw: bool) -> H264Profile {
    #[cfg(target_os = "macos")]
    if !force_sw {
        return H264Profile::High;
    }
    let _ = force_sw;
    H264Profile::ConstrainedBaseline
}

pub struct OpenH264Decoder {
    inner: Decoder,
}

impl OpenH264Decoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            inner: Decoder::new().context("openh264 decoder init")?,
        })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>> {
        let Some(yuv) = self
            .inner
            .decode(access_unit)
            .map_err(|e| Error::Decode(format!("openh264: {e}")))?
        else {
            return Ok(None); // buffering (parameter sets)
        };

        let (width, height) = yuv.dimensions();
        let mut pixels = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut pixels);

        Ok(Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            pixels,
            order: gsa_client_core::PixelOrder::Rgba,
        }))
    }
}
