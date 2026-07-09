//! openh264 implementation of client-core's `VideoDecoder` seam. The real
//! apps use VideoToolbox/MediaCodec here (D9); this is the portable debug
//! decoder.

use anyhow::Context;
use gsa_client_core::{DecodedFrame, VideoDecoder};
use gsa_core::{Error, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

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
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);

        Ok(Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            rgba,
        }))
    }
}
