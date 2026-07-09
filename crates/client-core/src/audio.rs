//! Client-side audio receive (spec 07): route audio datagrams → Opus decode
//! with packet-loss concealment → push interleaved-i16 PCM to the embedder's
//! output channel (which drives platform playback). Minimal PLC-based jitter
//! handling: in-order decode, conceal small gaps, drop reordered/dup packets.

use std::sync::mpsc::{Receiver, Sender, channel};

use gsa_audio::OpusDecoder;
use gsa_core::Result;
use gsa_protocol::datagram::AudioDatagramHeader;

/// Upper bound on frames concealed across one gap — beyond this a loss is a
/// stream break, not a blip, so don't synthesize a long burst of covered audio.
const MAX_CONCEAL: u16 = 5;

pub struct AudioReceive {
    decoder: OpusDecoder,
    last_seq: Option<u16>,
    out: Sender<Vec<i16>>,
}

impl AudioReceive {
    /// Create the receiver and the PCM output channel handed to the embedder.
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

    /// Decode one audio datagram (concealing small gaps) and push its PCM.
    pub fn handle(&mut self, datagram: &[u8]) {
        let Ok((hdr, opus)) = AudioDatagramHeader::parse(datagram) else {
            return;
        };
        if let Some(last) = self.last_seq {
            let delta = hdr.seq.wrapping_sub(last);
            if delta == 0 || delta > u16::MAX / 2 {
                return; // duplicate or reordered-late: drop
            }
            for _ in 0..(delta - 1).min(MAX_CONCEAL) {
                if let Ok(pcm) = self.decoder.conceal() {
                    let _ = self.out.send(pcm);
                }
            }
        }
        if let Ok(pcm) = self.decoder.decode(opus) {
            let _ = self.out.send(pcm);
        }
        self.last_seq = Some(hdr.seq);
    }
}
