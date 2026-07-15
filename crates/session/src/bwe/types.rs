//! Identifier and record types the estimator consumes.

use std::ops::Deref;
use std::time::{Duration, Instant};

macro_rules! num_id {
    ($id:ident, $t:ty) => {
        impl Deref for $id {
            type Target = $t;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }
        impl From<$t> for $id {
            fn from(v: $t) -> Self {
                $id(v)
            }
        }
        impl std::fmt::Display for $id {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

/// Send-order transport sequence (u64 to track rollover; u32 on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TwccSeq(pub u64);
num_id!(TwccSeq, u64);

/// Identifies one probe cluster (a burst sent to measure capacity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TwccClusterId(pub u64);
num_id!(TwccClusterId, u64);

impl TwccClusterId {
    /// Post-increment: returns the current id and advances.
    pub fn inc(&mut self) -> TwccClusterId {
        let n = *self;
        self.0 += 1;
        n
    }
}

/// Sequence plus the probe cluster the packet belongs to, if any.
#[derive(Debug, Clone, Copy)]
pub struct TwccPacketId {
    seq: TwccSeq,
    cluster: Option<TwccClusterId>,
}

impl TwccPacketId {
    #[must_use]
    pub fn new(seq: impl Into<TwccSeq>) -> Self {
        Self {
            seq: seq.into(),
            cluster: None,
        }
    }

    #[must_use]
    pub fn with_cluster(seq: impl Into<TwccSeq>, cluster: impl Into<TwccClusterId>) -> Self {
        Self {
            seq: seq.into(),
            cluster: Some(cluster.into()),
        }
    }

    #[must_use]
    pub fn seq(&self) -> TwccSeq {
        self.seq
    }

    #[must_use]
    pub fn cluster(&self) -> Option<TwccClusterId> {
        self.cluster
    }
}

/// Receiver-side confirmation for one sent packet.
#[derive(Debug, Copy, Clone)]
pub struct TwccRecvReport {
    local_recv_time: Instant,
    remote_recv_time: Option<Instant>,
}

/// One sent packet's bookkeeping joined with its feedback, if any arrived.
#[derive(Debug)]
pub struct TwccSendRecord {
    packet_id: TwccPacketId,
    local_send_time: Instant,
    size: u16,
    recv_report: Option<TwccRecvReport>,
}

impl TwccSendRecord {
    #[must_use]
    pub fn new(
        packet_id: TwccPacketId,
        local_send_time: Instant,
        size: usize,
        local_recv_time: Option<Instant>,
        remote_recv_time: Option<Instant>,
    ) -> Self {
        Self {
            packet_id,
            local_send_time,
            size: size.min(u16::MAX as usize) as u16,
            recv_report: local_recv_time.map(|local_recv_time| TwccRecvReport {
                local_recv_time,
                remote_recv_time,
            }),
        }
    }

    #[must_use]
    pub fn seq(&self) -> TwccSeq {
        self.packet_id.seq()
    }

    #[must_use]
    pub fn cluster(&self) -> Option<TwccClusterId> {
        self.packet_id.cluster()
    }

    #[must_use]
    pub fn local_send_time(&self) -> Instant {
        self.local_send_time
    }

    #[must_use]
    pub fn local_recv_time(&self) -> Option<Instant> {
        self.recv_report.as_ref().map(|r| r.local_recv_time)
    }

    #[must_use]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    #[must_use]
    pub fn remote_recv_time(&self) -> Option<Instant> {
        self.recv_report.as_ref().and_then(|r| r.remote_recv_time)
    }

    /// Round trip from send to feedback arrival.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        let recv_report = self.recv_report.as_ref()?;
        Some(recv_report.local_recv_time - self.local_send_time)
    }
}

#[cfg(test)]
impl TwccSendRecord {
    pub(crate) fn test_new(
        packet_id: TwccPacketId,
        local_send_time: Instant,
        size: usize,
        local_recv_time: Instant,
        remote_recv_time: Option<Instant>,
    ) -> Self {
        Self::new(
            packet_id,
            local_send_time,
            size,
            Some(local_recv_time),
            remote_recv_time,
        )
    }
}

/// Which subsystem asked for the next timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reason {
    #[default]
    NotHappening,
    BweDelayControl,
    BweProbeControl,
    BweProbeEstimator,
}

/// Pick the earlier of two optional deadlines, carrying its tag.
pub(crate) trait Soonest {
    fn soonest(self, other: Self) -> Self;
}

impl<T: Default> Soonest for (Option<Instant>, T) {
    fn soonest(self, other: Self) -> Self {
        match (self, other) {
            ((Some(v1), s1), (Some(v2), s2)) => {
                if v1 < v2 {
                    (Some(v1), s1)
                } else {
                    (Some(v2), s2)
                }
            }
            ((None, _), (None, _)) => (None, T::default()),
            ((None, _), (v, s)) => (v, s),
            ((v, s), (None, _)) => (v, s),
        }
    }
}
