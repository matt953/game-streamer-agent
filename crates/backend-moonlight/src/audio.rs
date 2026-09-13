//! Audio receive: RTP depacketization into the shared Opus decoder.
//!
//! Audio shares the video socket (see [`crate::MediaSocket`]) and is
//! distinguished by packet type. There is no reference chain, so a lost packet
//! costs one frame of sound: short gaps are concealed, long ones resume
//! directly.
//!
//! The stream is 48 kHz stereo, one Opus frame per packet. A host whose
//! capture device is held exclusively by another application sends no audio
//! packets at all; silence here can be a host condition rather than a client
//! fault.

#[cfg(feature = "native")]
use gsa_audio::{OpusDecoder, SurroundDecoder};
#[cfg(feature = "native")]
use gsa_core::Result;
#[cfg(feature = "native")]
use gsa_core::media::SurroundLayout;
#[cfg(feature = "native")]
use std::sync::mpsc::{Receiver, Sender, channel};

/// Bytes of RTP header before the Opus payload.
const HEADER_LEN: usize = 12;

/// Packet type carrying Opus audio.
pub const AUDIO_DATA: u8 = 97;

/// Packet type carrying Reed-Solomon parity for the audio stream.
pub const AUDIO_PARITY: u8 = 127;

/// Maximum consecutive packets to conceal. Past this the gap is a break in the
/// stream rather than a blip, and concealment would synthesise sound the host
/// never sent.
const MAX_CONCEAL: u16 = 5;

/// Where the Opus frames go once the wire has been stripped off them.
///
/// Natively a libopus decoder producing PCM for the embedder; in the browser
/// the frames are handed to WebCodecs and this only tracks gaps.
pub trait OpusSink {
    /// One Opus frame, in stream order.
    fn frame(&mut self, opus: &[u8]);
    /// `count` frames are missing before the next one; conceal them.
    fn lost(&mut self, count: u16);
}

/// Stereo or a host-declared surround layout — one decode path either way.
#[cfg(feature = "native")]
enum Decode {
    Stereo(OpusDecoder),
    Surround(SurroundDecoder),
}

#[cfg(feature = "native")]
impl Decode {
    fn decode(&mut self, opus: &[u8]) -> gsa_core::Result<Vec<i16>> {
        match self {
            Self::Stereo(d) => d.decode(opus),
            Self::Surround(d) => d.decode(opus),
        }
    }

    fn conceal(&mut self) -> gsa_core::Result<Vec<i16>> {
        match self {
            Self::Stereo(d) => d.conceal(),
            Self::Surround(d) => d.conceal(),
        }
    }
}

/// The native sink: libopus into interleaved PCM for the embedder to play.
#[cfg(feature = "native")]
pub struct PcmSink {
    decoder: Decode,
    out: Sender<Vec<i16>>,
}

#[cfg(feature = "native")]
impl std::fmt::Debug for PcmSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmSink").finish_non_exhaustive()
    }
}

#[cfg(feature = "native")]
impl PcmSink {
    /// `surround` is the host's declared multistream layout, or `None` for
    /// stereo; the PCM comes out interleaved at that channel count.
    pub fn new(surround: Option<&SurroundLayout>) -> Result<(Self, Receiver<Vec<i16>>)> {
        let (out, rx) = channel();
        let decoder = match surround {
            Some(layout) => Decode::Surround(SurroundDecoder::new(layout)?),
            None => Decode::Stereo(OpusDecoder::new()?),
        };
        Ok((Self { decoder, out }, rx))
    }
}

#[cfg(feature = "native")]
impl OpusSink for PcmSink {
    fn frame(&mut self, opus: &[u8]) {
        if let Ok(pcm) = self.decoder.decode(opus) {
            let _ = self.out.send(pcm);
        }
    }

    fn lost(&mut self, count: u16) {
        for _ in 0..count {
            if let Ok(pcm) = self.decoder.conceal() {
                let _ = self.out.send(pcm);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
type SinkBox = Box<dyn OpusSink + Send>;
#[cfg(target_arch = "wasm32")]
type SinkBox = Box<dyn OpusSink>;

/// Strips the wire off the audio stream and keeps it in order for the sink.
pub struct AudioReceive {
    sink: SinkBox,
    last_seq: Option<u16>,
}

impl std::fmt::Debug for AudioReceive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioReceive")
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

impl AudioReceive {
    /// Create the receiver and the PCM channel the embedder plays from.
    /// `surround` is the host's declared multistream layout, or `None` for
    /// stereo; the PCM comes out interleaved at that channel count.
    #[cfg(feature = "native")]
    pub fn new(surround: Option<&SurroundLayout>) -> Result<(Self, Receiver<Vec<i16>>)> {
        let (sink, rx) = PcmSink::new(surround)?;
        Ok((Self::with_sink(Box::new(sink)), rx))
    }

    /// A receiver feeding any sink.
    #[must_use]
    pub fn with_sink(sink: SinkBox) -> Self {
        Self {
            sink,
            last_seq: None,
        }
    }

    /// True for datagrams this receiver should be given.
    #[must_use]
    pub fn owns(datagram: &[u8]) -> bool {
        matches!(datagram.get(1), Some(&AUDIO_DATA | &AUDIO_PARITY))
    }

    /// Handle one audio datagram.
    pub fn handle(&mut self, datagram: &[u8]) {
        if datagram.len() <= HEADER_LEN {
            return;
        }
        // Parity packets carry a different header and are not Opus. FEC
        // recovery is not implemented; concealment covers the same gaps.
        if datagram[1] != AUDIO_DATA {
            return;
        }
        let seq = u16::from_be_bytes([datagram[2], datagram[3]]);
        if let Some(last) = self.last_seq {
            let delta = seq.wrapping_sub(last);
            if delta == 0 || delta > u16::MAX / 2 {
                // Duplicate, or so far behind that the wrap-aware delta reads
                // as backwards: replaying it would stutter the output.
                return;
            }
            let missing = (delta - 1).min(MAX_CONCEAL);
            if missing > 0 {
                self.sink.lost(missing);
            }
        }
        self.sink.frame(&datagram[HEADER_LEN..]);
        self.last_seq = Some(seq);
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{AUDIO_DATA, AUDIO_PARITY, AudioReceive, HEADER_LEN};

    /// Wrap real Opus in the header the wire uses.
    fn packet(seq: u16, opus: &[u8], packet_type: u8) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN];
        out[0] = 0x80;
        out[1] = packet_type;
        out[2..4].copy_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(opus);
        out
    }

    /// Encode a frame of tone with the same codec the host uses, so the test
    /// exercises a real payload rather than bytes that merely look like one.
    fn opus_frame() -> Vec<u8> {
        let mut encoder = gsa_audio::OpusEncoder::new(96_000).expect("encoder");
        // One frame's worth of interleaved samples, as the encoder expects.
        let pcm: Vec<i16> = (0..480)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        encoder.encode(&pcm).expect("encode")
    }

    #[test]
    fn decodes_a_real_opus_packet() {
        let (mut rx, pcm) = AudioReceive::new(None).unwrap();
        rx.handle(&packet(1, &opus_frame(), AUDIO_DATA));
        let decoded = pcm.try_recv().expect("audio decoded");
        assert!(!decoded.is_empty(), "a decoded frame must carry samples");
    }

    #[test]
    fn conceals_a_small_gap() {
        let (mut rx, pcm) = AudioReceive::new(None).unwrap();
        let frame = opus_frame();
        rx.handle(&packet(1, &frame, AUDIO_DATA));
        let _ = pcm.try_recv();
        // Two packets lost: one concealment each, then the real frame.
        rx.handle(&packet(4, &frame, AUDIO_DATA));
        let received: Vec<_> = std::iter::from_fn(|| pcm.try_recv().ok()).collect();
        assert_eq!(received.len(), 3, "two concealed frames plus the real one");
    }

    #[test]
    fn a_long_gap_resumes_rather_than_inventing_sound() {
        let (mut rx, pcm) = AudioReceive::new(None).unwrap();
        let frame = opus_frame();
        rx.handle(&packet(1, &frame, AUDIO_DATA));
        let _ = pcm.try_recv();
        rx.handle(&packet(500, &frame, AUDIO_DATA));
        let received: Vec<_> = std::iter::from_fn(|| pcm.try_recv().ok()).collect();
        // Capped concealment plus the real frame — not 499 invented frames.
        assert_eq!(received.len(), 6);
    }

    #[test]
    fn duplicates_and_late_packets_are_dropped() {
        let (mut rx, pcm) = AudioReceive::new(None).unwrap();
        let frame = opus_frame();
        rx.handle(&packet(10, &frame, AUDIO_DATA));
        let _ = pcm.try_recv();
        rx.handle(&packet(10, &frame, AUDIO_DATA)); // duplicate
        rx.handle(&packet(9, &frame, AUDIO_DATA)); // arrived too late
        assert!(
            pcm.try_recv().is_err(),
            "replaying old audio would stutter the output"
        );
    }

    #[test]
    fn parity_packets_are_not_fed_to_the_decoder() {
        let (mut rx, pcm) = AudioReceive::new(None).unwrap();
        rx.handle(&packet(1, &opus_frame(), AUDIO_PARITY));
        assert!(pcm.try_recv().is_err(), "parity is not an Opus frame");
    }

    #[test]
    fn claims_only_its_own_packet_types() {
        assert!(AudioReceive::owns(&[0x80, AUDIO_DATA, 0, 0]));
        assert!(AudioReceive::owns(&[0x80, AUDIO_PARITY, 0, 0]));
        // Video shares the socket and must not be handed over.
        assert!(!AudioReceive::owns(&[0x90, 0x00, 0, 0]));
        assert!(!AudioReceive::owns(&[]));
    }
}
