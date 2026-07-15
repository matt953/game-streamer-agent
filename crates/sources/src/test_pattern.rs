//! Deterministic test source (spec 09): a moving bar over a gradient with
//! the frame index written as marker blocks (`gsa_core::pattern`).
//! Drives the pipeline in CI with no GPU and no capture permissions, and
//! is the origin of automated glass-to-glass numbers (spec 04).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gsa_capture_api::{
    CpuFrame, FrameSink, GpuFrame, GpuHandle, RenderSource, SourceConfig, SourceDescriptor,
};
use gsa_core::media::{PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result, pattern};
use gsa_protocol::control::{SourceInfo, SourceKind};
use gsa_protocol::input::{InputDisposition, InputEvent};

#[derive(Debug)]
pub struct TestPattern {
    id: gsa_core::id::SourceId,
    clock: MediaClock,
    running: Option<Worker>,
}

#[derive(Debug)]
struct Worker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    /// Kept for `reconfigure` restarts.
    sink: FrameSink,
}

impl TestPattern {
    #[must_use]
    pub fn new(id: gsa_core::id::SourceId, clock: MediaClock) -> Self {
        Self {
            id,
            clock,
            running: None,
        }
    }
}

impl RenderSource for TestPattern {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            info: SourceInfo {
                id: self.id,
                kind: SourceKind::TestPattern,
                name: "Test pattern".into(),
            },
            modes: Vec::new(), // synthetic: any mode
        }
    }

    fn start(&mut self, cfg: SourceConfig, sink: FrameSink) -> Result<()> {
        if self.running.is_some() {
            return Err(Error::Capture("test pattern already started".into()));
        }
        let mode = cfg.mode;
        if (mode.width as usize) < pattern::MIN_WIDTH || mode.height < 32 || mode.fps == 0 {
            return Err(Error::Capture(format!(
                "unsupported mode {}x{}@{} (min {}x32@1)",
                mode.width,
                mode.height,
                mode.fps,
                pattern::MIN_WIDTH
            )));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let clock = self.clock.clone();
        let thread_sink = sink.clone();
        let handle = std::thread::Builder::new()
            .name("gsa-testpattern".into())
            .spawn(move || run_generator(mode, clock, thread_sink, &stop2))
            .map_err(|e| Error::Capture(format!("spawn generator: {e}")))?;
        self.running = Some(Worker { stop, handle, sink });
        Ok(())
    }

    fn handle_input(&mut self, _event: InputEvent) -> InputDisposition {
        InputDisposition::Consumed
    }

    fn reconfigure(&mut self, cfg: SourceConfig) -> Result<()> {
        // Synthetic source: restart the generator at the new mode, keeping
        // the existing sink.
        let worker = self
            .running
            .take()
            .ok_or_else(|| Error::Capture("reconfigure on a stopped source".into()))?;
        worker.stop.store(true, Ordering::Release);
        let sink = worker.sink.clone();
        worker
            .handle
            .join()
            .map_err(|_| Error::Capture("generator panicked".into()))?;
        self.start(cfg, sink)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(worker) = self.running.take() {
            worker.stop.store(true, Ordering::Release);
            worker
                .handle
                .join()
                .map_err(|_| Error::Capture("generator panicked".into()))?;
            worker.sink.close();
        }
        Ok(())
    }
}

impl Drop for TestPattern {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run_generator(mode: VideoMode, clock: MediaClock, sink: FrameSink, stop: &AtomicBool) {
    let (w, h) = (mode.width as usize, mode.height as usize);
    let stride = w * 4;
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(mode.fps));
    let mut next_deadline = Instant::now();
    let mut index: u32 = 0;
    // High-entropy fill: incompressible content makes the encoder produce near
    // its target, so a bandwidth cap actually congests (the normal gradient is
    // ~80 kbps and never fills a real pipe). For manually stressing ABR on a
    // hardware encoder — openh264 can't sustain it, so the CI rig doesn't use it.
    let noise = std::env::var_os("GSA_TESTPATTERN_NOISE").is_some();
    // Compressible-but-hungry load for the ABR-convergence rig (see `draw_detail`).
    let detail = std::env::var_os("GSA_TESTPATTERN_DETAIL").is_some();

    tracing::debug!(
        width = w,
        height = h,
        fps = mode.fps,
        "test pattern started"
    );

    while !stop.load(Ordering::Acquire) {
        let mut buf = vec![0u8; stride * h];
        if noise {
            draw_noise(&mut buf, w, h, stride, index);
        } else if detail {
            draw_detail(&mut buf, w, h, stride, index);
        } else {
            draw(&mut buf, w, h, stride, index);
        }
        pattern::write_marker_bgra(&mut buf, stride, index);

        sink.submit(GpuFrame {
            handle: GpuHandle::Cpu(CpuFrame {
                data: Arc::new(buf),
                stride,
            }),
            format: PixelFormat::Bgra8,
            width: mode.width,
            height: mode.height,
            capture_ts_us: clock.now_us(),
            dirty_rects: None,
        });

        index = index.wrapping_add(1);
        next_deadline += frame_interval;
        let now = Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else {
            // Fell behind (loaded CI box): resynchronize instead of bursting.
            next_deadline = now;
        }
    }
    // NOTE: the sink is deliberately NOT closed here — `reconfigure`
    // restarts the generator on the same sink. `stop()` closes it.
    tracing::debug!(frames = index, "test pattern stopped");
}

/// Background gradient + a bright moving vertical bar so encoded output has
/// real temporal motion (exercises P-frames, not just static IDR quality).
fn draw(buf: &mut [u8], w: usize, h: usize, stride: usize, index: u32) {
    let bar_w = (w / 16).max(8);
    let bar_x = (index as usize * 8) % w;
    for y in 0..h {
        let row = y * stride;
        let g = (y * 255 / h) as u8;
        for x in 0..w {
            let px = row + x * 4;
            let in_bar =
                (x >= bar_x && x < bar_x + bar_w) || (bar_x + bar_w > w && x < (bar_x + bar_w) % w);
            if in_bar {
                buf[px] = 0x20; // B
                buf[px + 1] = 0xc0; // G
                buf[px + 2] = 0xff; // R
            } else {
                buf[px] = g / 2;
                buf[px + 1] = 0x28;
                buf[px + 2] = 64 + g / 3;
            }
            buf[px + 3] = 0xff;
        }
    }
}

/// Per-pixel pseudo-random fill (deterministic from `index`+position) — the
/// content barely compresses, so the encoder produces near its target bitrate.
/// For the chaos rig only; the marker is written over it afterwards.
fn draw_noise(buf: &mut [u8], w: usize, h: usize, stride: usize, index: u32) {
    for y in 0..h {
        let row = y * stride;
        for x in 0..w {
            let px = row + x * 4;
            let mut v = (index as usize).wrapping_mul(2_654_435_761)
                ^ y.wrapping_mul(40_503)
                ^ x.wrapping_mul(2_246_822_519);
            v ^= v >> 13;
            v = v.wrapping_mul(0x5bd1_e995);
            v ^= v >> 15;
            buf[px] = v as u8;
            buf[px + 1] = (v >> 8) as u8;
            buf[px + 2] = (v >> 16) as u8;
            buf[px + 3] = 0xff;
        }
    }
}

/// A moving plaid of low-frequency gradient ramps that deforms over time: carries
/// several Mb/s at a high ceiling but quantizes down cleanly under a tight cap,
/// while staying cheap enough for the sw encoder to keep real-time.
fn draw_detail(buf: &mut [u8], w: usize, h: usize, stride: usize, index: u32) {
    // Triangle wave over an 8-bit phase → linear ramps (cheap, deterministic).
    let tri = |p: i32| -> u8 {
        let m = (p & 0xff) as u8;
        if m < 128 { m * 2 } else { (255 - m) * 2 }
    };
    let t = index as i32;
    for y in 0..h {
        let row = y * stride;
        let yy = y as i32;
        for x in 0..w {
            let px = row + x * 4;
            let xx = x as i32;
            buf[px] = tri((xx + yy) * 4 + t * 5); // B
            buf[px + 1] = tri(yy * 4 + t * 3); // G
            buf[px + 2] = tri(xx * 4 + t * 2); // R
            buf[px + 3] = 0xff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsa_capture_api::frame_channel;
    use gsa_core::id::SourceId;

    #[test]
    fn produces_frames_with_increasing_markers() {
        let mut src = TestPattern::new(SourceId(0), MediaClock::new());
        let (sink, rx) = frame_channel();
        src.start(
            SourceConfig {
                mode: VideoMode {
                    width: 320,
                    height: 64,
                    fps: 120,
                },
            },
            sink,
        )
        .unwrap();

        let mut last = None;
        for _ in 0..5 {
            let f = rx.recv_latest(Duration::from_secs(2)).expect("frame");
            let GpuHandle::Cpu(cpu) = &f.handle else {
                panic!("expected CPU frame")
            };
            // Luma via green channel is fine: marker blocks are pure black/white.
            // Cover the full marker height (BLOCK rows).
            let luma: Vec<u8> = (0..gsa_core::pattern::BLOCK)
                .flat_map(|y| (0..320).map(move |x| (y, x)))
                .map(|(y, x)| cpu.data[y * cpu.stride + x * 4 + 1])
                .collect();
            let idx = gsa_core::pattern::read_marker_luma(&luma, 320, 320).unwrap();
            if let Some(prev) = last {
                assert!(idx > prev, "marker must increase: {prev} -> {idx}");
            }
            last = Some(idx);
        }
        src.stop().unwrap();
    }

    #[test]
    fn rejects_tiny_modes() {
        let mut src = TestPattern::new(SourceId(0), MediaClock::new());
        let (sink, _rx) = frame_channel();
        let bad = SourceConfig {
            mode: VideoMode {
                width: 64,
                height: 64,
                fps: 30,
            },
        };
        assert!(src.start(bad, sink).is_err());
    }
}
