//! Decoder selection + the portable openh264 software decoder. On macOS the
//! default is hardware VideoToolbox decode (`decoder_vt`), with software as
//! the explicit fallback / cross-platform path.

use anyhow::Context;
use gsa_client_core::{DecodedFrame, VideoDecoder};
use gsa_core::media::{Codec, H264Profile};
use gsa_core::{Error, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// What this build can actually decode, richest first.
///
/// Asked of the platform rather than assumed: hardware AV1 decode needs an M3
/// or newer, and offering a codec this machine cannot decode yields a session
/// that negotiates, streams, and never shows a frame.
#[must_use]
pub fn decode_codecs(force_sw: bool) -> Vec<Codec> {
    #[cfg(target_os = "macos")]
    if !force_sw {
        return crate::decoder_vt::hardware_codecs();
    }
    let _ = force_sw;
    // The software path is H.264 only.
    vec![Codec::H264]
}

/// What to offer the host: `names` when given, otherwise everything this
/// build can decode.
///
/// A name this machine cannot decode is dropped rather than offered — the
/// point of naming codecs is to exercise the negotiation, not to negotiate a
/// stream that cannot be shown. H.264 always remains as the floor.
#[must_use]
pub fn offered_codecs(names: &[String], force_sw: bool) -> Vec<Codec> {
    let available = decode_codecs(force_sw);
    if names.is_empty() {
        return available;
    }
    let mut offered: Vec<Codec> = names
        .iter()
        .map(|name| match name.as_str() {
            "av1" => Codec::Av1,
            "hevc" => Codec::Hevc,
            _ => Codec::H264,
        })
        .filter(|codec| {
            let have = available.contains(codec);
            if !have {
                tracing::warn!(?codec, "asked for, but this machine cannot decode it");
            }
            have
        })
        .collect();
    if !offered.contains(&Codec::H264) {
        offered.push(Codec::H264);
    }
    offered
}

/// How HDR content is mapped onto an SDR display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayMapping {
    /// The absolute level, in nits, shown as full white.
    ///
    /// BT.2408's diffuse white is 203 nits — the level graphics and desktop
    /// content are authored against, so mapping it to full white is what makes
    /// an HDR-tagged desktop look like the desktop rather than a grey wash.
    /// Hosts differ in what they encode SDR white as, and the difference shows
    /// up as clipped highlights or a dim picture, so it is a knob.
    pub sdr_white_nits: f32,
}

impl Default for DisplayMapping {
    fn default() -> Self {
        Self {
            sdr_white_nits: 203.0,
        }
    }
}

impl DisplayMapping {
    /// Reject a level that would divide the whole picture into black or blow
    /// it out, rather than letting it produce a mystifying image.
    pub fn new(sdr_white_nits: f32) -> std::result::Result<Self, Error> {
        if !(1.0..=10_000.0).contains(&sdr_white_nits) {
            return Err(Error::Decode(format!(
                "SDR white must be between 1 and 10000 nits, not {sdr_white_nits}"
            )));
        }
        Ok(Self { sdr_white_nits })
    }
}

/// Pick a decoder for the codec the host agreed to send (`force_sw` pins
/// openh264, which is H.264 only). `mapping` only bears on HDR streams, which
/// only the hardware decoder can produce.
pub fn make_decoder(
    force_sw: bool,
    codec: Codec,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] mapping: DisplayMapping,
) -> anyhow::Result<Box<dyn VideoDecoder>> {
    #[cfg(target_os = "macos")]
    if !force_sw {
        tracing::info!(?codec, "using VideoToolbox hardware decoder");
        return Ok(Box::new(crate::decoder_vt::VideoToolboxDecoder::new(
            codec, mapping,
        )?));
    }
    let _ = force_sw;
    anyhow::ensure!(
        codec == Codec::H264,
        "software decoder cannot decode {codec:?}"
    );
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
            platform: None,
        }))
    }
}
