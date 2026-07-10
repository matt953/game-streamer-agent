//! Audio pipeline (spec 07): capture PCM → reframe to 5 ms → Opus → audio
//! datagrams, running beside the video pipeline off the same connection.
//! Blocking capture recv lives on a thread; sending is one tokio task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gsa_audio::{FRAME_INTERLEAVED, OpusEncoder};
use gsa_capture_api::AudioReceiver;
use gsa_core::time::wire_ts;
use gsa_protocol::datagram::AudioDatagramHeader;

/// Opus target bitrate for stereo game audio (spec 07: 96–160 kbps).
const AUDIO_BITRATE_BPS: u32 = 128_000;

/// Packets between heartbeat logs — one second of 5 ms frames. A silent stream
/// looks identical to a working one from the outside, so the heartbeat is the
/// only way to tell "no sound is playing" from "audio never reached the wire".
const HEARTBEAT_PACKETS: u16 = 1000 / gsa_audio::FRAME_MS as u16;

/// Accumulates variable-size capture buffers and emits exact 5 ms Opus frames
/// (`FRAME_INTERLEAVED` interleaved samples). Carries the capture timestamp of
/// the most recent input for wire stamping.
struct Reframer {
    buf: Vec<i16>,
    last_ts_us: u64,
}

impl Reframer {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(FRAME_INTERLEAVED * 4),
            last_ts_us: 0,
        }
    }

    fn push(&mut self, samples: &[i16], ts_us: u64) {
        self.buf.extend_from_slice(samples);
        self.last_ts_us = ts_us;
    }

    /// Pop one full frame, or `None` if fewer than a frame's samples buffered.
    fn next_frame(&mut self) -> Option<Vec<i16>> {
        if self.buf.len() < FRAME_INTERLEAVED {
            return None;
        }
        Some(self.buf.drain(..FRAME_INTERLEAVED).collect())
    }
}

pub struct AudioPipelineHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for AudioPipelineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPipelineHandle").finish()
    }
}

impl AudioPipelineHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPipelineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start encoding `rx`'s PCM into Opus audio datagrams on `conn`. The blocking
/// capture recv + Opus encode run on a thread; a tokio task sends the packets.
pub fn start(rx: AudioReceiver, conn: quinn::Connection) -> gsa_core::Result<AudioPipelineHandle> {
    let mut encoder = OpusEncoder::new(AUDIO_BITRATE_BPS)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let (tx, mut packet_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let thread = std::thread::Builder::new()
        .name("gsa-audio-enc".into())
        .spawn(move || {
            let mut reframer = Reframer::new();
            let mut seq: u16 = 0;
            while !stop_thread.load(Ordering::Acquire) {
                let Some(frame) = rx.recv(Duration::from_millis(100)) else {
                    continue; // timeout or closed; loop re-checks stop
                };
                reframer.push(&frame.samples, frame.capture_ts_us);
                while let Some(pcm) = reframer.next_frame() {
                    match encoder.encode(&pcm) {
                        Ok(opus) => {
                            seq = seq.wrapping_add(1);
                            let hdr = AudioDatagramHeader {
                                seq,
                                ts_us: wire_ts(reframer.last_ts_us),
                            };
                            if tx.send(hdr.encode_with_payload(&opus)).is_err() {
                                return; // sender task gone (connection closed)
                            }
                            if seq.is_multiple_of(HEARTBEAT_PACKETS) {
                                tracing::debug!(seq, bytes = opus.len(), "audio flowing");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "opus encode failed"),
                    }
                }
            }
        })
        .map_err(|e| gsa_core::Error::Session(format!("spawn audio thread: {e}")))?;
    tracing::info!(bitrate = AUDIO_BITRATE_BPS, "audio pipeline started");

    tokio::spawn(async move {
        while let Some(datagram) = packet_rx.recv().await {
            if conn.send_datagram(bytes::Bytes::from(datagram)).is_err() {
                return; // connection closed
            }
        }
    });

    Ok(AudioPipelineHandle {
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reframer_emits_exact_frames_and_keeps_remainder() {
        let mut r = Reframer::new();
        // One and a bit frames of samples.
        r.push(&vec![7i16; FRAME_INTERLEAVED + 20], 1_000);
        let frame = r.next_frame().expect("one full frame");
        assert_eq!(frame.len(), FRAME_INTERLEAVED);
        assert!(r.next_frame().is_none()); // 20 samples remain, < a frame
        assert_eq!(r.last_ts_us, 1_000);

        // Top up past the boundary → a second frame completes.
        r.push(&vec![9i16; FRAME_INTERLEAVED], 2_000);
        assert_eq!(r.next_frame().unwrap().len(), FRAME_INTERLEAVED);
        assert_eq!(r.last_ts_us, 2_000);
    }
}
