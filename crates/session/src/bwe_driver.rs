//! Drives the bandwidth estimator from the session tick: turns the send
//! history plus receiver packet feedback into estimator updates, and turns
//! probe decisions into pacer padding bursts.

use std::time::{Duration, Instant};

use gsa_protocol::control::PacketFeedback;

use crate::bwe::Bwe;
use crate::bwe::bandwidth::Bitrate;
use crate::bwe::prelude::{TwccClusterId, TwccSendRecord};
use crate::bwe::types::TwccPacketId;
use crate::pipeline::{ProbeJob, SendRecord};

pub struct BweDriver {
    bwe: Bwe,
    /// Outstanding probe cluster and when to consider it finished.
    active_probe: Option<(u64, Instant)>,
    /// Estimator-internal time must never run backwards; feeds are clamped.
    last_now: Instant,
    /// Anchor pairing the µs clocks to `Instant` math. Offsets are arbitrary;
    /// the estimator consumes deltas, and RTT uses agent-side times only.
    epoch: Instant,
    epoch_agent_us: u64,
}

impl BweDriver {
    #[must_use]
    pub fn new(start_bps: u32, now_agent_us: u64) -> Self {
        Self {
            bwe: Bwe::new(Bitrate::bps(u64::from(start_bps))),
            epoch: Instant::now(),
            epoch_agent_us: now_agent_us,
            active_probe: None,
            last_now: Instant::now(),
        }
    }

    fn instant_at(&self, us: u64) -> Instant {
        self.epoch + Duration::from_micros(us.saturating_sub(self.epoch_agent_us))
    }

    fn mono(&mut self, i: Instant) -> Instant {
        self.last_now = self.last_now.max(i);
        self.last_now
    }

    /// Media bytes handed to the wire (drives underuse detection). Callers
    /// with real-time visibility feed this at send time; the session layer
    /// feeds it from the send history just before the matching feedback.
    pub fn on_media_sent(&mut self, bytes: u32, sent_us: u64) {
        let at = self.mono(self.instant_at(sent_us));
        self.bwe.on_media_sent(
            crate::bwe::bandwidth::DataSize::bytes(i64::from(bytes)),
            false,
            at,
        );
    }

    /// The estimator wants the send rate it should assume media aims for.
    pub fn set_desired_bitrate(&mut self, bps: u32) {
        self.bwe.set_desired_bitrate(Bitrate::bps(u64::from(bps)));
    }

    /// Feed one feedback batch joined against the sent records it covers.
    /// `sent` must contain every record with seq ≤ the batch's highest seq;
    /// records absent from `feedback` count as lost.
    pub fn on_feedback(
        &mut self,
        sent: &[SendRecord],
        feedback: &PacketFeedback,
        now_agent_us: u64,
    ) {
        if sent.is_empty() {
            return;
        }
        for r in sent {
            if !r.padding {
                self.on_media_sent(r.bytes, r.sent_us);
            }
        }
        let now = self.mono(self.instant_at(now_agent_us));
        let arrivals: std::collections::HashMap<u32, u64> = feedback
            .samples
            .iter()
            .map(|&(seq, delta)| (seq, feedback.base_arrival_us + u64::from(delta)))
            .collect();
        let records: Vec<TwccSendRecord> = sent
            .iter()
            .map(|r| {
                let packet_id = match r.cluster {
                    Some(c) => TwccPacketId::with_cluster(u64::from(r.seq), c),
                    None => TwccPacketId::new(u64::from(r.seq)),
                };
                let remote = arrivals
                    .get(&r.seq)
                    .map(|&us| self.epoch + Duration::from_micros(us));
                TwccSendRecord::new(
                    packet_id,
                    self.instant_at(r.sent_us),
                    r.bytes as usize,
                    remote.is_some().then_some(now),
                    remote,
                )
            })
            .collect();
        self.bwe.update(records.iter(), now);
    }

    /// Periodic drive: may emit a probe burst for the pacer. Returns the job
    /// to queue, already registered with the estimator.
    pub fn on_tick(&mut self, now_agent_us: u64) -> Option<ProbeJob> {
        let now = self.mono(self.instant_at(now_agent_us));
        // Advance underuse detection even when media is silent.
        self.bwe
            .on_media_sent(crate::bwe::bandwidth::DataSize::bytes(0), false, now);
        if let Some((cluster, deadline)) = self.active_probe
            && now >= deadline
        {
            self.bwe.end_probe(now, TwccClusterId::from(cluster));
            self.active_probe = None;
        }
        let config = self.bwe.handle_timeout(now, true)?;
        let job = ProbeJob {
            cluster: *config.cluster(),
            rate_bps: config.target_bitrate().as_f64(),
            duration: config.target_duration(),
            min_packets: config.min_packet_count(),
            min_delta: config.min_probe_delta(),
        };
        // A cluster is spent once its burst plus a feedback round trip pass.
        self.active_probe = Some((job.cluster, now + job.duration + Duration::from_millis(250)));
        self.bwe.start_probe(config, now);
        Some(job)
    }

    /// Latest smoothed estimate (bps), if one has formed.
    pub fn estimate_bps(&mut self) -> Option<u64> {
        self.bwe.poll_estimate().map(|b| b.as_u64())
    }
}
