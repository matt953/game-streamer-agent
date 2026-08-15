//! Audio receive: RTP depacketization into the shared Opus decoder.
//!
//! Audio shares the video socket (see [`crate::MediaSocket`]) and is told
//! apart by its packet type. Unlike video there is no reference chain, so a
//! lost packet costs one frame of sound rather than everything until the next
//! keyframe — which is why a small gap is concealed and a large one simply
//! resumes.
//!
//! **Not yet exercised against a real host.** The dev host produces no audio
//! at all — the stock client gets none from it either — so this path is
//! covered by round-trip tests through the same Opus codec the wire uses, and
//! its behaviour on live packets is unproven. Treat a first live session as a
//! test, not a regression check.

use gsa_audio::OpusDecoder;
use gsa_core::Result;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Bytes of RTP header before the Opus payload.
const HEADER_LEN: usize = 12;

/// Packet type carrying Opus audio.
pub const AUDIO_DATA: u8 = 97;

/// Packet type carrying Reed-Solomon parity for the audio stream.
pub const AUDIO_PARITY: u8 = 127;

/// Beyond this many consecutive lost packets a gap is a break in the stream,
/// not a blip; synthesising more would invent sound that never existed.
const MAX_CONCEAL: u16 = 5;

/// Decodes the audio stream into interleaved PCM for the embedder to play.
pub struct AudioReceive {
    decoder: OpusDecoder,
    last_seq: Option<u16>,
    out: Sender<Vec<i16>>,
}

impl std::fmt::Debug for AudioReceive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioReceive")
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

impl AudioReceive {
    /// Create the receiver and the PCM channel handed to the embedder.
    pub fn new() -> Result<(Self, Receiver<Vec<i16>>)> {
        let (out, rx) = channel();
        Ok((
            Self {
                decoder: OpusDecoder::new()?,
                last_seq: None,
                out,
            },
            rx,
        ))
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
        // Parity packets carry a different header and are only useful for
        // rebuilding lost data packets. Recovering from them is not
        // implemented, and guessing at it against a host that sends no audio
        // would be untestable; concealment covers the same gaps audibly.
        if datagram[1] != AUDIO_DATA {
            return;
        }
        let seq = u16::from_be_bytes([datagram[2], datagram[3]]);
        if let Some(last) = self.last_seq {
            let delta = seq.wrapping_sub(last);
            if delta == 0 || delta > u16::MAX / 2 {
                return; // duplicate, or reordered so late it is useless
            }
            for _ in 0..(delta - 1).min(MAX_CONCEAL) {
                if let Ok(pcm) = self.decoder.conceal() {
                    let _ = self.out.send(pcm);
                }
            }
        }
        if let Ok(pcm) = self.decoder.decode(&datagram[HEADER_LEN..]) {
            let _ = self.out.send(pcm);
        }
        self.last_seq = Some(seq);
    }
}

#[cfg(test)]
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
        let (mut rx, pcm) = AudioReceive::new().unwrap();
        rx.handle(&packet(1, &opus_frame(), AUDIO_DATA));
        let decoded = pcm.try_recv().expect("audio decoded");
        assert!(!decoded.is_empty(), "a decoded frame must carry samples");
    }

    #[test]
    fn conceals_a_small_gap() {
        let (mut rx, pcm) = AudioReceive::new().unwrap();
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
        let (mut rx, pcm) = AudioReceive::new().unwrap();
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
        let (mut rx, pcm) = AudioReceive::new().unwrap();
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
        let (mut rx, pcm) = AudioReceive::new().unwrap();
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
