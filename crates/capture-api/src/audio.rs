//! Audio capture contracts (spec 07). Unlike video (latest-frame-wins),
//! captured audio is a FIFO — dropped samples are audible gaps — so buffers
//! flow in order through a bounded queue.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

/// One buffer of captured interleaved PCM (i16). `capture_ts_us` is the
/// agent-clock timestamp of the first sample.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    pub capture_ts_us: u64,
}

/// Producer half, handed to a capture backend.
#[derive(Debug, Clone)]
pub struct AudioSink {
    tx: SyncSender<AudioFrame>,
}

/// Consumer half, owned by the audio encode pipeline.
#[derive(Debug)]
pub struct AudioReceiver {
    rx: Receiver<AudioFrame>,
}

/// Create a connected audio sink/receiver pair. The queue is bounded (bounded
/// memory); a stalled consumer drops buffers rather than blocking the capture
/// callback — the encode side is fast enough that this shouldn't happen.
#[must_use]
pub fn audio_channel() -> (AudioSink, AudioReceiver) {
    let (tx, rx) = sync_channel(256);
    (AudioSink { tx }, AudioReceiver { rx })
}

impl AudioSink {
    /// Submit a buffer; non-blocking, drops on a full queue.
    pub fn submit(&self, frame: AudioFrame) {
        let _ = self.tx.try_send(frame);
    }
}

impl AudioReceiver {
    /// Blocking receive of the next buffer, or `None` on timeout / closed.
    pub fn recv(&self, timeout: Duration) -> Option<AudioFrame> {
        self.rx.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_preserved() {
        let (sink, rx) = audio_channel();
        for i in 0..3 {
            sink.submit(AudioFrame {
                samples: vec![i as i16],
                sample_rate: 48_000,
                channels: 2,
                capture_ts_us: i,
            });
        }
        for i in 0..3 {
            assert_eq!(rx.recv(Duration::from_millis(10)).unwrap().capture_ts_us, i);
        }
        assert!(rx.recv(Duration::from_millis(1)).is_none());
    }
}
