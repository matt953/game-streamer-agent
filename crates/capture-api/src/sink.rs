//! Latest-frame-wins frame channel (spec 01 threading model).
//!
//! Depth-1: if the encoder can't keep up, the oldest unencoded frame is
//! replaced. Video never queues depth — stale frames are worthless.
//! Push side is thread-agnostic (OS capture callbacks); the receive side
//! is one dedicated encode thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::frame::GpuFrame;

#[derive(Debug, Default)]
struct Shared {
    slot: Mutex<Option<GpuFrame>>,
    available: Condvar,
    dropped: AtomicU64,
    closed: AtomicBool,
}

/// Producer half: handed to a `RenderSource`.
#[derive(Debug, Clone)]
pub struct FrameSink {
    shared: Arc<Shared>,
}

/// Consumer half: owned by the encode thread.
#[derive(Debug)]
pub struct FrameReceiver {
    shared: Arc<Shared>,
}

/// Create a connected sink/receiver pair.
#[must_use]
pub fn frame_channel() -> (FrameSink, FrameReceiver) {
    let shared = Arc::new(Shared::default());
    (
        FrameSink {
            shared: shared.clone(),
        },
        FrameReceiver { shared },
    )
}

impl FrameSink {
    /// Submit a frame. Replaces (drops) any frame not yet consumed.
    pub fn submit(&self, frame: GpuFrame) {
        let mut slot = self.shared.slot.lock().expect("sink lock");
        if slot.replace(frame).is_some() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(slot);
        self.shared.available.notify_one();
    }

    /// Frames overwritten before the consumer took them.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Signal end-of-stream to the receiver.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.available.notify_all();
    }
}

impl FrameReceiver {
    /// Blocking receive of the newest frame, or `None` on timeout /
    /// closed-and-empty.
    pub fn recv_latest(&self, timeout: Duration) -> Option<GpuFrame> {
        let mut slot = self.shared.slot.lock().expect("sink lock");
        loop {
            if let Some(frame) = slot.take() {
                return Some(frame);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return None;
            }
            let (guard, res) = self
                .shared
                .available
                .wait_timeout(slot, timeout)
                .expect("sink lock");
            slot = guard;
            if res.timed_out() {
                return slot.take();
            }
        }
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CpuFrame, GpuHandle};
    use gsa_core::media::PixelFormat;
    use std::sync::Arc as StdArc;

    fn frame(ts: u64) -> GpuFrame {
        GpuFrame {
            handle: GpuHandle::Cpu(CpuFrame {
                data: StdArc::new(vec![0; 4]),
                stride: 4,
            }),
            format: PixelFormat::Bgra8,
            width: 1,
            height: 1,
            capture_ts_us: ts,
            dirty_rects: None,
        }
    }

    #[test]
    fn latest_frame_wins_and_counts_drops() {
        let (sink, rx) = frame_channel();
        sink.submit(frame(1));
        sink.submit(frame(2));
        sink.submit(frame(3));
        assert_eq!(sink.dropped(), 2);
        let got = rx.recv_latest(Duration::from_millis(10)).unwrap();
        assert_eq!(got.capture_ts_us, 3);
        assert!(rx.recv_latest(Duration::from_millis(1)).is_none());
    }

    #[test]
    fn close_unblocks_receiver() {
        let (sink, rx) = frame_channel();
        let t = std::thread::spawn(move || rx.recv_latest(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        sink.close();
        assert!(t.join().unwrap().is_none());
    }
}
