//! Media receive, independent of what carries the datagrams.
//!
//! A [`MediaLink`] yields datagrams stamped at arrival; the
//! [`MediaAssembler`] turns them into complete access units and audio frames.
//! Natively the link is a UDP socket on its own thread
//! ([`crate::UdpMediaLink`]); in the browser it is the altc tunnel.

use crate::{AudioReceive, Depacketizer, Received};
use gsa_client_backend_api::BackendFrame;

/// One datagram off the wire.
#[derive(Debug, Clone)]
pub struct MediaDatagram {
    pub bytes: Vec<u8>,
    /// Arrival on the client's media clock, in µs. Stamped where the bytes
    /// were read, never later: a frame stamped when it is released would make
    /// paced presentation look like network delay.
    pub arrival_us: u64,
}

/// A source of media datagrams. The link keeps the host informed of where to
/// send (pings) itself; the session only reads.
pub trait MediaLink {
    /// The next datagram, or `None` once the link is closed.
    fn recv(&mut self) -> impl std::future::Future<Output = Option<MediaDatagram>>;
}

/// Counters the receive loop keeps for the shared health stats.
#[derive(Debug, Default)]
pub struct Counters {
    /// Frames the wire could not deliver whole.
    pub dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames rebuilt from parity: loss with no visible cost.
    pub recovered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Any media datagram, used to tell streaming from silence.
    pub datagrams: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Counters {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Deterministic packet-loss injection for chaos runs.
///
/// Drops a share of received datagrams before anything inspects them, which
/// exercises FEC recovery, the reference gate and the repair path against a
/// real host without degrading the network. Deterministic so a failure can be
/// re-run; off unless `GSA_MOONLIGHT_LOSS` is set.
#[derive(Debug)]
pub struct LossInjector {
    /// Drop probability in parts per thousand.
    per_mille: u32,
    state: u32,
    dropped: u64,
}

impl LossInjector {
    /// From the environment, natively; a browser has no environment.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let per_mille: u32 = std::env::var("GSA_MOONLIGHT_LOSS").ok()?.parse().ok()?;
            Self::new(per_mille)
        }
    }

    /// A fixed drop rate in parts per thousand; `None` for zero.
    #[must_use]
    pub fn new(per_mille: u32) -> Option<Self> {
        (per_mille > 0).then(|| {
            tracing::warn!(per_mille, "injecting packet loss for a chaos run");
            Self {
                per_mille: per_mille.min(1000),
                state: 0x2545_f491,
                dropped: 0,
            }
        })
    }

    /// True when this datagram should be discarded.
    pub fn drops(&mut self) -> bool {
        // xorshift from a fixed seed: cheap, and repeatable, so a chaos run
        // that finds a bug replays exactly.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        let drop = self.state % 1000 < self.per_mille;
        if drop {
            self.dropped += 1;
        }
        drop
    }
}

/// Convert the host's 90 kHz stream clock to microseconds.
///
/// Wraps with the field, which the shared clock handling already expects.
#[must_use]
pub fn stream_clock_us(ticks: u32) -> u32 {
    // 90 kHz → µs is ×100/9; done in 64-bit so the multiply cannot overflow
    // before the division brings it back into range.
    ((u64::from(ticks) * 100 / 9) & u64::from(u32::MAX)) as u32
}

/// Video datagrams carry packet type 0; audio uses its own types.
fn is_video(datagram: &[u8]) -> bool {
    datagram.len() > 1 && datagram[1] == 0
}

/// Turns datagrams into frames: video through the depacketiser and FEC,
/// audio through its receiver, both routed by packet type because the two
/// streams share one link.
pub struct MediaAssembler {
    depacketizer: Depacketizer,
    audio: AudioReceive,
    frames: tokio::sync::mpsc::UnboundedSender<BackendFrame>,
    counters: Counters,
    loss: Option<LossInjector>,
}

impl std::fmt::Debug for MediaAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaAssembler")
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl MediaAssembler {
    #[must_use]
    pub fn new(
        audio: AudioReceive,
        frames: tokio::sync::mpsc::UnboundedSender<BackendFrame>,
        counters: Counters,
        loss: Option<LossInjector>,
    ) -> Self {
        Self {
            depacketizer: Depacketizer::new(),
            audio,
            frames,
            counters,
            loss,
        }
    }

    /// Feed one datagram. Returns `false` once the frame consumer is gone,
    /// which is the signal to stop receiving.
    pub fn on_datagram(&mut self, datagram: &MediaDatagram) -> bool {
        // Drop before anything inspects the packet, so the rest of the path
        // cannot tell an injected loss from a real one.
        if let Some(injector) = self.loss.as_mut()
            && injector.drops()
        {
            return true;
        }
        let bytes = &datagram.bytes;
        Counters::bump(&self.counters.datagrams);
        if AudioReceive::owns(bytes) {
            self.audio.handle(bytes);
            return true;
        }
        if !is_video(bytes) {
            return true;
        }
        self.depacketizer.push(bytes);
        while let Some(event) = self.depacketizer.next_event() {
            match event {
                Received::Frame(frame) => {
                    if frame.recovered {
                        Counters::bump(&self.counters.recovered);
                    }
                    let out = BackendFrame {
                        data: frame.data,
                        frame_id: frame.frame_index,
                        keyframe: frame.keyframe,
                        // The host's 90 kHz stream clock, in µs. Its origin
                        // is unknown, so absolute latency from it is
                        // meaningless; the *gaps* between frames are real
                        // host-side timing, which is what the de-jitter
                        // window measures. Stamping arrival here instead
                        // would make every frame look perfectly timed and the
                        // window would never engage.
                        capture_ts_us: stream_clock_us(frame.timestamp),
                        host_latency_us: frame.host_latency_us,
                        arrival_us: datagram.arrival_us,
                    };
                    if self.frames.send(out).is_err() {
                        return false; // consumer gone
                    }
                }
                Received::Lost(loss) => {
                    tracing::debug!(?loss, "frame lost");
                    Counters::bump(&self.counters.dropped);
                }
                Received::Nothing => {}
            }
        }
        true
    }
}

/// Read datagrams from `link` into `assembler` until either side is done.
pub async fn receive_media<L: MediaLink>(mut link: L, mut assembler: MediaAssembler) {
    while let Some(datagram) = link.recv().await {
        if !assembler.on_datagram(&datagram) {
            return;
        }
    }
    tracing::info!("media link closed");
}

#[cfg(test)]
mod tests {
    use super::stream_clock_us;

    #[test]
    fn the_stream_clock_converts_and_wraps() {
        assert_eq!(stream_clock_us(90_000), 1_000_000);
        assert_eq!(stream_clock_us(9), 100);
        // Past the 32-bit boundary the value wraps like the field it fills.
        assert_eq!(
            stream_clock_us(u32::MAX),
            ((u64::from(u32::MAX) * 100 / 9) & 0xffff_ffff) as u32
        );
    }
}
