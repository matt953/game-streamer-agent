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

pub use gsa_core::media::SurroundLayout;

/// Opus multistream decoder for a surround feed (client side).
pub struct SurroundDecoder {
    inner: opusic_c::multistream::Decoder,
    channels: usize,
}

// SAFETY: the libopus decoder state has no thread affinity — the wrapper is
// moved into the receive thread and used from it alone, same as the stereo
// decoder (whose binding declares Send itself; the multistream one forgot).
unsafe impl Send for SurroundDecoder {}

impl std::fmt::Debug for SurroundDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurroundDecoder")
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

impl SurroundDecoder {
    /// Build a decoder for the host's stated layout. Channel counts are the
    /// three surround shapes hosts actually offer; anything else is refused
    /// rather than guessed at.
    ///
    /// # Errors
    /// An unsupported channel count, a mapping that does not match it, or a
    /// layout libopus itself rejects.
    pub fn new(layout: &SurroundLayout) -> Result<Self> {
        let inner = match layout.channels {
            6 => ms_decoder::<6>(layout),
            8 => ms_decoder::<8>(layout),
            12 => ms_decoder::<12>(layout),
            n => Err(Error::Decode(format!("unsupported surround channels: {n}"))),
        }?;
        Ok(Self {
            inner,
            channels: usize::from(layout.channels),
        })
    }

    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode an Opus packet into interleaved i16 PCM, `channels` wide.
    ///
    /// # Errors
    /// A packet libopus cannot decode.
    pub fn decode(&mut self, opus: &[u8]) -> Result<Vec<i16>> {
        self.decode_inner(opus)
    }

    /// Conceal a lost packet (Opus PLC) — one frame of covered audio.
    ///
    /// # Errors
    /// Concealment itself failing, which libopus permits but rarely does.
    pub fn conceal(&mut self) -> Result<Vec<i16>> {
        self.decode_inner(&[])
    }

    fn decode_inner(&mut self, opus: &[u8]) -> Result<Vec<i16>> {
        // Room for a 10 ms frame in case a host sends longer frames than the
        // 5 ms this crate encodes with; the true count comes back from opus.
        let mut out = vec![0u16; FRAME_SAMPLES * 2 * self.channels];
        let samples = self
            .inner
            .decode_to_slice(opus, &mut out, false)
            .map_err(|e| Error::Decode(format!("opus surround decode: {e:?}")))?;
        out.truncate(samples * self.channels);
        Ok(bytemuck::cast_slice(&out).to_vec())
    }
}

fn ms_decoder<const CH: usize>(layout: &SurroundLayout) -> Result<opusic_c::multistream::Decoder> {
    if layout.mapping.len() != CH {
        return Err(Error::Decode(format!(
            "surround mapping has {} entries for {CH} channels",
            layout.mapping.len()
        )));
    }
    let mut mapping = [0u8; CH];
    mapping.copy_from_slice(&layout.mapping);
    let config = opusic_c::multistream::Config::try_new(layout.streams, layout.coupled, mapping)
        .ok_or_else(|| Error::Decode("surround layout rejected by opus".into()))?;
    opusic_c::multistream::Decoder::new(config, SampleRate::Hz48000)
        .map_err(|e| Error::Decode(format!("opus surround decoder init: {e:?}")))
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
    fn surround_round_trip_keeps_the_channel_count() {
        // The classic 7.1 shape: five streams, three of them coupled pairs.
        let layout = SurroundLayout {
            channels: 8,
            streams: 5,
            coupled: 3,
            mapping: vec![0, 1, 2, 3, 4, 5, 6, 7],
        };
        let mut mapping = [0u8; 8];
        mapping.copy_from_slice(&layout.mapping);
        let config = opusic_c::multistream::Config::try_new(5, 3, mapping).unwrap();
        let mut enc =
            opusic_c::multistream::Encoder::new(config, SampleRate::Hz48000, Application::Audio)
                .unwrap();
        let mut dec = SurroundDecoder::new(&layout).unwrap();

        let pcm: Vec<i16> = (0..FRAME_SAMPLES * 2 * 8)
            .map(|i| ((i as f32 * 0.03).sin() * 5000.0) as i16)
            .collect();
        // encode_to_vec writes into the vec's spare capacity.
        let mut packet = Vec::with_capacity(8 * MAX_PACKET);
        enc.encode_to_vec(bytemuck::cast_slice(&pcm), &mut packet)
            .unwrap();

        let decoded = dec.decode(&packet).unwrap();
        assert_eq!(decoded.len() % 8, 0, "interleaving must survive");
        assert!(!decoded.is_empty());
        let concealed = dec.conceal().unwrap();
        assert_eq!(concealed.len() % 8, 0);
    }

    #[test]
    fn encode_rejects_wrong_frame_size() {
        let mut enc = OpusEncoder::new(96_000).unwrap();
        assert!(enc.encode(&[0i16; 100]).is_err());
    }
}
