//! Opus audio codec (spec 07). 48 kHz stereo, 5 ms frames, in-band FEC.
//! Encoder is agent-side; decoder (with packet-loss concealment) is client-side.
//! libopus is built statically from source (opusic-c → cmake), no system dep.

use gsa_core::{Error, Result};
use opusic_c::{Application, Bitrate, Channels, Decoder, Encoder, InbandFec, SampleRate};

/// Sample rate (Hz). Opus operates natively at 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channels (stereo).
pub const CHANNELS: usize = 2;
/// Frame duration (ms) — short for low latency (spec 07).
pub const FRAME_MS: u32 = 5;
/// Samples per channel in one frame (48 kHz × 5 ms).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
/// Interleaved i16 samples in one stereo frame.
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS;

/// Upper bound on one encoded Opus packet.
const MAX_PACKET: usize = 1500;

/// Opus encoder for one stereo stream (agent side).
pub struct OpusEncoder {
    inner: Encoder,
}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusEncoder").finish_non_exhaustive()
    }
}

impl OpusEncoder {
    /// Low-latency stereo encoder at `bitrate_bps` with in-band FEC on.
    pub fn new(bitrate_bps: u32) -> Result<Self> {
        let mut inner = Encoder::new(Channels::Stereo, SampleRate::Hz48000, Application::LowDelay)
            .map_err(|e| Error::Encode(format!("opus encoder init: {e:?}")))?;
        inner
            .set_bitrate(Bitrate::Value(bitrate_bps))
            .map_err(|e| Error::Encode(format!("opus set bitrate: {e:?}")))?;
        // In-band FEC + an expected-loss hint so the encoder budgets redundancy.
        inner
            .set_inband_fec(InbandFec::Mode1)
            .map_err(|e| Error::Encode(format!("opus set fec: {e:?}")))?;
        inner
            .set_packet_loss(5)
            .map_err(|e| Error::Encode(format!("opus set packet-loss: {e:?}")))?;
        Ok(Self { inner })
    }

    /// Encode one frame of exactly [`FRAME_INTERLEAVED`] interleaved i16 samples
    /// into an Opus packet.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() != FRAME_INTERLEAVED {
            return Err(Error::Encode(format!(
                "expected {FRAME_INTERLEAVED} samples, got {}",
                pcm.len()
            )));
        }
        let mut out = Vec::with_capacity(MAX_PACKET);
        self.inner
            .encode_to_vec(bytemuck::cast_slice(pcm), &mut out)
            .map_err(|e| Error::Encode(format!("opus encode: {e:?}")))?;
        Ok(out)
    }
}

/// Opus decoder for one stereo stream (client side).
pub struct OpusDecoder {
    inner: Decoder,
}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusDecoder").finish_non_exhaustive()
    }
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let inner = Decoder::new(Channels::Stereo, SampleRate::Hz48000)
            .map_err(|e| Error::Decode(format!("opus decoder init: {e:?}")))?;
        Ok(Self { inner })
    }

    /// Decode an Opus packet into interleaved i16 PCM.
    pub fn decode(&mut self, opus: &[u8]) -> Result<Vec<i16>> {
        self.decode_inner(opus)
    }

    /// Conceal a lost packet (Opus PLC) — produces a frame of covered audio.
    pub fn conceal(&mut self) -> Result<Vec<i16>> {
        self.decode_inner(&[])
    }

    fn decode_inner(&mut self, opus: &[u8]) -> Result<Vec<i16>> {
        let mut out = vec![0u16; FRAME_INTERLEAVED];
        let samples = self
            .inner
            .decode_to_slice(opus, &mut out, false)
            .map_err(|e| Error::Decode(format!("opus decode: {e:?}")))?;
        out.truncate(samples * CHANNELS);
        Ok(bytemuck::cast_slice(&out).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let mut enc = OpusEncoder::new(128_000).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        // One 5 ms stereo frame of a quiet tone.
        let pcm: Vec<i16> = (0..FRAME_INTERLEAVED)
            .map(|i| ((i as f32 * 0.05).sin() * 6000.0) as i16)
            .collect();

        let packet = enc.encode(&pcm).unwrap();
        assert!(!packet.is_empty() && packet.len() < MAX_PACKET);

        let decoded = dec.decode(&packet).unwrap();
        assert_eq!(decoded.len(), FRAME_INTERLEAVED);

        // Packet-loss concealment yields a full frame too.
        let concealed = dec.conceal().unwrap();
        assert_eq!(concealed.len(), FRAME_INTERLEAVED);
    }

    #[test]
    fn encode_rejects_wrong_frame_size() {
        let mut enc = OpusEncoder::new(96_000).unwrap();
        assert!(enc.encode(&[0i16; 100]).is_err());
    }
}
