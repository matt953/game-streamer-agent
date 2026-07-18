//! A userspace UDP impairment relay (rate/delay/jitter/loss) — no kernel, no
//! root, so it runs unprivileged on any platform. The chaos harness drives it.

#![allow(dead_code)] // kept for future link-shaping harnesses

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

fn kbit_to_bytes(kbit: u32) -> u64 {
    u64::from(kbit) * 1000 / 8
}

/// A link profile applied by the [`Shaper`].
pub(crate) struct Shaping {
    pub(crate) rate_kbit: u32,
    pub(crate) delay_ms: u32,
    /// Delay jitter (±), applied per packet.
    pub(crate) jitter_ms: u32,
    pub(crate) loss_pct: f64,
    /// Storm model: drop everything for `burst_ms` every `burst_interval_ms`
    /// once active (0 disables). Models cellular loss bursts.
    pub(crate) burst_ms: u64,
    pub(crate) burst_interval_ms: u64,
    /// Tail-drop past this many queued packets (the bottleneck buffer).
    pub(crate) buffer_pkts: Option<u32>,
}

impl Shaping {
    pub(crate) fn describe(&self) -> String {
        let buf = self
            .buffer_pkts
            .map_or(String::new(), |p| format!(", buffer {p} pkt"));
        format!(
            "{} kbit, {} ms ±{} ms, {}% loss{buf}",
            self.rate_kbit, self.delay_ms, self.jitter_ms, self.loss_pct
        )
    }
}

/// Deterministic xorshift RNG (seeded loss/jitter, so runs are reproducible).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64) / (1u64 << 53) as f64
    }
}

/// A userspace UDP relay: the client connects here, we forward to the agent and
/// back, applying rate (token bucket), delay + jitter and loss in application
/// code — no kernel, no root. The rate/buffer bottleneck is on the downlink
/// (video); both directions carry delay + loss so RTT reflects the path.
pub(crate) struct Shaper {
    pub(crate) front: SocketAddr,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Shaper {
    /// Start relaying between a fresh front address (what the client connects
    /// to) and `agent`. Impairments stay off until [`activate`] so the QUIC
    /// handshake happens on a clean link.
    pub(crate) fn start(agent: SocketAddr, sh: &Shaping, seed: u64) -> Result<Self> {
        Self::start_with_schedule(agent, sh, seed, Vec::new())
    }

    /// [`start`], plus downlink rate changes applied at offsets after
    /// [`activate`] — the matrix uses this to degrade the link mid-session.
    pub(crate) fn start_with_schedule(
        agent: SocketAddr,
        sh: &Shaping,
        seed: u64,
        schedule: Vec<(Duration, u32)>,
    ) -> Result<Self> {
        // Two sockets: `front` faces the client, `back` faces the agent. Each
        // is cloned so one thread reads while the other writes.
        let front = UdpSocket::bind("127.0.0.1:0").context("bind shaper front")?;
        let back = UdpSocket::bind("127.0.0.1:0").context("bind shaper back")?;
        let front_addr = front.local_addr()?;
        for s in [&front, &back] {
            s.set_read_timeout(Some(Duration::from_millis(1)))?;
        }
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        // The client's address, learned by the uplink thread and used by the
        // downlink thread to send replies back.
        let client_addr: Arc<std::sync::Mutex<Option<SocketAddr>>> =
            Arc::new(std::sync::Mutex::new(None));

        // Downlink rate lives in a shared cell so a schedule can change it live.
        let rate_bytes: Arc<AtomicU64> = Arc::new(AtomicU64::new(kbit_to_bytes(sh.rate_kbit)));
        let delay = f64::from(sh.delay_ms) / 1000.0;
        let jitter = f64::from(sh.jitter_ms) / 1000.0;
        let loss = sh.loss_pct / 100.0;
        let buffer = sh.buffer_pkts;

        // Uplink client→agent: learn the client address; light impairment (the
        // uplink isn't the bottleneck), just delay + loss so RTT is symmetric.
        let up = Direction {
            recv: front.try_clone()?,
            send: back.try_clone()?,
            fixed_dest: Some(agent),
            learn: Some(client_addr.clone()),
            client_dest: None,
            rate_bytes: None,
            buffer: None,
            delay,
            jitter,
            loss,
            burst: None,
            burst_t0: None,
            active: active.clone(),
            stop: stop.clone(),
            rng: Rng::new(seed),
        };
        // Downlink agent→client: the shaped bottleneck (rate + buffer + delay).
        let down_burst = (sh.burst_ms > 0).then_some((sh.burst_interval_ms.max(1), sh.burst_ms));
        let down = Direction {
            recv: back,
            send: front,
            fixed_dest: None,
            learn: None,
            client_dest: Some(client_addr),
            rate_bytes: Some(rate_bytes.clone()),
            buffer,
            delay,
            jitter,
            loss,
            burst: down_burst,
            burst_t0: None,
            active: active.clone(),
            stop: stop.clone(),
            rng: Rng::new(seed ^ 0x9E37_79B9),
        };

        let mut handles = vec![
            std::thread::spawn(move || up.run()),
            std::thread::spawn(move || down.run()),
        ];
        if !schedule.is_empty() {
            // Apply rate changes at offsets from activation.
            let active = active.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                while !active.load(Ordering::Relaxed) {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let t0 = Instant::now();
                for (at, kbit) in schedule {
                    while t0.elapsed() < at {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    rate_bytes.store(kbit_to_bytes(kbit), Ordering::Relaxed);
                    eprintln!("[shaper] downlink rate -> {kbit} kbit");
                }
            }));
        }
        Ok(Self {
            front: front_addr,
            active,
            stop,
            handles,
        })
    }

    pub(crate) fn activate(&self) {
        self.active.store(true, Ordering::Relaxed);
    }
}

impl Drop for Shaper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// One direction of the relay.
struct Direction {
    recv: UdpSocket,
    send: UdpSocket,
    /// Where to forward to, if fixed (uplink → the agent).
    fixed_dest: Option<SocketAddr>,
    /// If set, record the source address of received packets here (uplink
    /// learns the client's address).
    learn: Option<Arc<std::sync::Mutex<Option<SocketAddr>>>>,
    /// If set, forward to the address learned by the other direction (downlink
    /// → the client).
    client_dest: Option<Arc<std::sync::Mutex<Option<SocketAddr>>>>,
    /// Token-bucket rate (bytes/s), shared so a schedule can change it live;
    /// `None` = unlimited.
    rate_bytes: Option<Arc<AtomicU64>>,
    /// Tail-drop past this many queued packets.
    buffer: Option<u32>,
    /// Storm bursts: (interval_ms, burst_ms); everything drops inside the
    /// burst window. `burst_t0` anchors the schedule at activation.
    burst: Option<(u64, u64)>,
    burst_t0: Option<Instant>,
    delay: f64,
    jitter: f64,
    loss: f64,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    rng: Rng,
}

impl Direction {
    fn run(mut self) {
        let mut queue: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
        let mut tokens = 0.0f64;
        let mut last = Instant::now();
        let mut buf = [0u8; 2048];
        // Fall back to `fixed_dest` when the learned address isn't known yet.
        let dest = || {
            self.client_dest
                .as_ref()
                .and_then(|d| *d.lock().unwrap())
                .or(self.fixed_dest)
        };
        while !self.stop.load(Ordering::Relaxed) {
            if let Ok((n, from)) = self.recv.recv_from(&mut buf) {
                if let Some(l) = &self.learn {
                    *l.lock().unwrap() = Some(from);
                }
                let on = self.active.load(Ordering::Relaxed);
                let in_burst = on
                    && self.burst.is_some_and(|(interval_ms, burst_ms)| {
                        let t0 = *self.burst_t0.get_or_insert_with(Instant::now);
                        t0.elapsed().as_millis() as u64 % interval_ms < burst_ms
                    });
                let drop_random = on && (in_burst || self.rng.unit() < self.loss);
                let over_buffer = on && self.buffer.is_some_and(|b| queue.len() as u32 >= b);
                if !drop_random && !over_buffer {
                    let hold = if on {
                        (self.delay + (self.rng.unit() * 2.0 - 1.0) * self.jitter).max(0.0)
                    } else {
                        0.0
                    };
                    let ready = Instant::now() + Duration::from_secs_f64(hold);
                    queue.push_back((ready, buf[..n].to_vec()));
                }
            }

            let now = Instant::now();
            match &self.rate_bytes {
                Some(cell) if self.active.load(Ordering::Relaxed) => {
                    let rate = cell.load(Ordering::Relaxed) as f64;
                    // Refill, capped at ~50 ms of burst so an idle gap can't
                    // release a flood.
                    tokens = (tokens + rate * (now - last).as_secs_f64()).min(rate * 0.05);
                }
                _ => tokens = f64::INFINITY,
            }
            last = now;

            if let Some(to) = dest() {
                while let Some((ready, pkt)) = queue.front() {
                    if *ready > now || tokens < pkt.len() as f64 {
                        break;
                    }
                    tokens -= pkt.len() as f64;
                    let (_, pkt) = queue.pop_front().unwrap();
                    let _ = self.send.send_to(&pkt, to);
                }
            }
        }
    }
}
