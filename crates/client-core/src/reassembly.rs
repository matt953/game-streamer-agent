//! Datagram → frame reassembly (spec 04): frames complete out of nothing
//! but chunk arrival; anything stale is dropped, never waited on (video is
//! newest-wins). Frames carry Reed-Solomon parity shards computed per
//! deterministic group (`fec::group_layout`, derived from the header's
//! chunk/parity counts alone): each group reconstructs from any `data_len`
//! of its own `data_len + parity_len` shards, so isolated datagram loss no
//! longer kills whole frames and FEC cost stays linear in frame size.

use std::collections::HashMap;

use gsa_core::media::FrameKind;
use gsa_protocol::datagram::{VideoDatagramHeader, total_shards};
use gsa_protocol::fec;

/// A frame ready for the decoder, released in id order.
#[derive(Debug)]
pub struct CompletedFrame {
    pub frame_id: u32,
    pub kind: FrameKind,
    pub capture_ts_us: u32,
    pub data: Vec<u8>,
}

/// Completed frames wait for at most this many newer completions while an
/// older frame's recovery shards are still in flight. A P-frame is
/// undecodable without its predecessor, so delivering it early buys nothing
/// and forfeits the older frame's recovery.
const HOLD_LIMIT: usize = 2;

/// How many in-flight frame ids we track before pruning the oldest.
/// Loopback/LAN never sees more than 2-3 concurrent; the window guards
/// against pathological reordering and id-space garbage.
const MAX_PENDING: usize = 8;

#[derive(Debug)]
struct Pending {
    /// Data shards then parity shards (`total_shards` slots).
    shards: Vec<Option<Vec<u8>>>,
    data_count: u16,
    received_data: u16,
    received_total: u16,
    frame_len: u32,
    epoch: u8,
}

impl Pending {
    fn new(header: &VideoDatagramHeader) -> Self {
        Self {
            shards: vec![None; total_shards(header.chunk_count, header.parity_count)],
            data_count: header.chunk_count,
            received_data: 0,
            received_total: 0,
            frame_len: header.frame_len,
            epoch: header.session_epoch,
        }
    }

    fn matches(&self, header: &VideoDatagramHeader) -> bool {
        self.epoch == header.session_epoch
            && self.data_count == header.chunk_count
            && self.shards.len() == total_shards(header.chunk_count, header.parity_count)
            && self.frame_len == header.frame_len
    }

    /// True when every FEC group's data is present or recoverable: present
    /// data + present parity of the group >= the group's data count. Cheap
    /// (<= 255 slots scanned); callers pre-filter on `received_total`.
    fn recoverable(&self) -> bool {
        let k = usize::from(self.data_count);
        let m = self.shards.len() - k;
        fec::group_layout(k, m).all(|g| {
            let present = |range: std::ops::Range<usize>| {
                self.shards[range].iter().filter(|s| s.is_some()).count()
            };
            let data = present(g.data_start..g.data_start + g.data_len);
            data == g.data_len
                || data + present(g.parity_start..g.parity_start + g.parity_len) >= g.data_len
        })
    }
}

#[derive(Debug, Default)]
pub struct Reassembler {
    pending: HashMap<u32, Pending>,
    /// Completed frames not yet released (oldest-first): held while an older
    /// pending frame can still recover.
    held: Vec<CompletedFrame>,
    /// Frames discarded incomplete (stats / future NACK trigger).
    dropped: u64,
    /// Frames completed only thanks to parity reconstruction (stats).
    recovered: u64,
    /// Highest frame id delivered (drop-older policy).
    latest_delivered: Option<u32>,
}

/// Wrap-aware "a is older than b".
fn older(a: u32, b: u32) -> bool {
    let age = b.wrapping_sub(a);
    age > 0 && age < u32::MAX / 2
}

impl Reassembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one shard; returns the full frame bytes when the frame
    /// completes — directly, or via parity reconstruction.
    pub fn push(&mut self, header: VideoDatagramHeader, payload: &[u8]) -> Vec<CompletedFrame> {
        // Shards at or older than the last delivered frame are stale: drop.
        // `age == 0` matters — every frame carries parity, so shards routinely
        // trail their frame's completion and must not re-open it. Held frames
        // are complete too: their trailing shards are equally stale.
        if let Some(latest) = self.latest_delivered {
            let age = latest.wrapping_sub(header.frame_id);
            if age < u32::MAX / 2 {
                return Vec::new();
            }
        }
        if self.held.iter().any(|h| h.frame_id == header.frame_id) {
            return Vec::new();
        }

        let entry = self
            .pending
            .entry(header.frame_id)
            .or_insert_with(|| Pending::new(&header));
        // Epoch bump (encoder reset) or inconsistent shard geometry: restart.
        if !entry.matches(&header) {
            *entry = Pending::new(&header);
        }
        let idx = header.chunk_index as usize;
        if entry.shards[idx].is_none() {
            entry.shards[idx] = Some(payload.to_vec());
            entry.received_total += 1;
            if header.chunk_index < header.chunk_count {
                entry.received_data += 1;
            }
        }

        let complete_direct = entry.received_data == entry.data_count;
        let reconstructable =
            !complete_direct && entry.received_total >= entry.data_count && entry.recoverable();
        if !(complete_direct || reconstructable) {
            if self.pending.len() > MAX_PENDING {
                self.prune_oldest();
            }
            return self.drain();
        }

        let done = self
            .pending
            .remove(&header.frame_id)
            .expect("just inserted");
        let data = if complete_direct {
            Some(assemble_data(&done))
        } else {
            match reconstruct(done) {
                Some(f) => {
                    self.recovered += 1;
                    Some(f)
                }
                None => {
                    // Malformed shard geometry (attacker or bug): count and drop.
                    self.dropped += 1;
                    None
                }
            }
        };
        if let Some(data) = data {
            let frame = CompletedFrame {
                frame_id: header.frame_id,
                kind: header.kind,
                capture_ts_us: header.capture_ts_us,
                data,
            };
            let at = self
                .held
                .iter()
                .position(|h| older(frame.frame_id, h.frame_id))
                .unwrap_or(self.held.len());
            self.held.insert(at, frame);
        }
        self.drain()
    }

    /// Release held frames in order. A frame waits while an older pending
    /// frame can still recover; it goes out once nothing older is pending,
    /// it is an IDR (the reference chain resets anyway), or the hold limit
    /// is reached (the recovery window has passed — give up on the blocker).
    fn drain(&mut self) -> Vec<CompletedFrame> {
        let mut out = Vec::new();
        while let Some(head) = self.held.first() {
            let blocked = self.pending.keys().any(|&id| older(id, head.frame_id));
            let force = head.kind == FrameKind::Idr || self.held.len() > HOLD_LIMIT;
            if blocked && !force {
                break;
            }
            let frame = self.held.remove(0);
            self.latest_delivered = Some(frame.frame_id);
            self.prune_older_than(frame.frame_id);
            out.push(frame);
        }
        out
    }

    #[must_use]
    pub fn frames_dropped(&self) -> u64 {
        self.dropped
    }

    /// Frames saved by parity reconstruction (would have dropped without FEC).
    #[must_use]
    pub fn frames_recovered(&self) -> u64 {
        self.recovered
    }

    fn prune_older_than(&mut self, completed: u32) {
        let before = self.pending.len();
        self.pending.retain(|id, _| {
            let age = completed.wrapping_sub(*id);
            !(age < u32::MAX / 2 && age > 0)
        });
        self.dropped += (before - self.pending.len()) as u64;
    }

    fn prune_oldest(&mut self) {
        // Oldest = smallest id in wrap-order relative to the newest seen.
        if let Some(&newest) = self.pending.keys().max_by_key(|id| **id)
            && let Some(&oldest) = self
                .pending
                .keys()
                .max_by_key(|id| newest.wrapping_sub(**id))
        {
            self.pending.remove(&oldest);
            self.dropped += 1;
        }
    }
}

/// All data shards present: concatenate them as-is (wire truth, no trim).
fn assemble_data(done: &Pending) -> Vec<u8> {
    let k = done.data_count as usize;
    let mut frame = Vec::with_capacity(
        done.shards[..k]
            .iter()
            .map(|c| c.as_ref().map_or(0, Vec::len))
            .sum(),
    );
    for c in &done.shards[..k] {
        frame.extend_from_slice(c.as_ref().expect("all data shards received"));
    }
    frame
}

/// Reed-Solomon reconstruction: pad received shards to the uniform shard
/// length, recover each FEC group's missing data shards, concatenate, trim to
/// `frame_len`. `None` on inconsistent shard geometry (never panics on wire
/// input).
fn reconstruct(done: Pending) -> Option<Vec<u8>> {
    let k = done.data_count as usize;
    let m = done.shards.len() - k;
    if m == 0 {
        return None;
    }
    // Uniform shard length across the whole frame: every full data shard and
    // every parity shard (any group) has it; only the frame's last data shard
    // may be shorter. Reconstruction only runs when some shard beyond the
    // received data exists, so it's always known.
    let shard_len = done
        .shards
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|s| (i, s.len())))
        .filter(|&(i, _)| i != k - 1)
        .map(|(_, len)| len)
        .max()?;
    if shard_len == 0 {
        return None;
    }

    let mut shards: Vec<Option<Vec<u8>>> = done
        .shards
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            s.and_then(|mut s| {
                if s.len() > shard_len || (s.len() < shard_len && i != k - 1) {
                    return None; // inconsistent geometry
                }
                s.resize(shard_len, 0);
                Some(s)
            })
        })
        .collect();

    // Recover group by group: each group is an independent Reed-Solomon code
    // over its own data shards (mirrors the chunker's `fec::group_layout`).
    for g in fec::group_layout(k, m) {
        let data_range = g.data_start..g.data_start + g.data_len;
        if shards[data_range.clone()].iter().all(Option::is_some) {
            continue;
        }
        let mut group: Vec<Option<Vec<u8>>> = Vec::with_capacity(g.data_len + g.parity_len);
        for i in data_range
            .clone()
            .chain(g.parity_start..g.parity_start + g.parity_len)
        {
            group.push(shards[i].take());
        }
        if !fec::reconstruct_data(&mut group, g.data_len) {
            return None;
        }
        for (slot, recovered) in shards[data_range].iter_mut().zip(group) {
            *slot = recovered;
        }
    }

    let mut frame = Vec::with_capacity(k * shard_len);
    for s in &shards[..k] {
        frame.extend_from_slice(s.as_ref()?);
    }
    frame.truncate((done.frame_len as usize).min(frame.len()));
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expectation helper: the data payloads of released frames.
    fn datas(v: Vec<CompletedFrame>) -> Vec<Vec<u8>> {
        v.into_iter().map(|f| f.data).collect()
    }
    use gsa_core::media::FrameKind;
    use gsa_protocol::datagram::chunk_video_frame;

    fn hdr(frame_id: u32, index: u16, count: u16) -> VideoDatagramHeader {
        VideoDatagramHeader {
            seq: 0,
            session_epoch: 0,
            frame_id,
            kind: FrameKind::P,
            chunk_index: index,
            chunk_count: count,
            parity_count: 0,
            frame_len: 0,
            capture_ts_us: 0,
        }
    }

    #[test]
    fn single_chunk_completes_immediately() {
        let mut r = Reassembler::new();
        assert_eq!(datas(r.push(hdr(1, 0, 1), b"abc")), vec![b"abc".to_vec()]);
    }

    #[test]
    fn multi_chunk_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(datas(r.push(hdr(2, 1, 3), b"BB")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(2, 0, 3), b"AA")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(2, 2, 3), b"CC")), vec![b"AABBCC".to_vec()]);
    }

    #[test]
    fn duplicate_chunks_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(datas(r.push(hdr(3, 0, 2), b"xx")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(3, 0, 2), b"xx")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(3, 1, 2), b"yy")), vec![b"xxyy".to_vec()]);
    }

    #[test]
    fn stale_frames_dropped_after_newer_completes() {
        let mut r = Reassembler::new();
        assert_eq!(datas(r.push(hdr(5, 0, 2), b"old")), Vec::<Vec<u8>>::new()); // incomplete
        // Newer completions hold briefly (frame 5 could still recover)...
        assert_eq!(datas(r.push(hdr(6, 0, 1), b"a")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(7, 0, 1), b"b")), Vec::<Vec<u8>>::new());
        // ...until the hold limit passes: everything releases, 5 is dropped.
        assert_eq!(
            datas(r.push(hdr(8, 0, 1), b"c")),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(r.frames_dropped(), 1);
        // Late chunk of the stale frame is ignored.
        assert_eq!(datas(r.push(hdr(5, 1, 2), b"old")), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn window_pruning_bounds_memory() {
        let mut r = Reassembler::new();
        for id in 0..50u32 {
            let _ = r.push(hdr(id, 0, 2), b"partial"); // never completes
        }
        assert!(r.pending.len() <= MAX_PENDING + 1);
        assert!(r.frames_dropped() > 0);
    }

    /// Round-trip through the real chunker: drop data shards up to the parity
    /// budget and the frame must still reconstruct byte-identical. 9700 bytes
    /// makes the last data shard short (the padded-shard path).
    #[test]
    fn parity_recovers_lost_data_shards() {
        let frame: Vec<u8> = (0..9_700u32).map(|i| (i * 7 % 251) as u8).collect();
        let template = hdr(9, 0, 0);
        // 300 permille of ceil(9700/1000)=10 data shards -> 3 parity shards.
        let datagrams = chunk_video_frame(
            template,
            &frame,
            1000 + gsa_protocol::datagram::VIDEO_HEADER_LEN,
            300,
        )
        .unwrap();

        let mut r = Reassembler::new();
        let mut out = None;
        for (i, d) in datagrams.iter().enumerate() {
            if [0usize, 4, 9].contains(&i) {
                continue; // lose three data shards, incl. the short last one
            }
            let (h, payload) = VideoDatagramHeader::parse(d).unwrap();
            if let Some(f) = r.push(h, payload).pop() {
                out = Some(f.data);
            }
        }
        assert_eq!(out, Some(frame));
        assert_eq!(r.frames_recovered(), 1);
    }

    /// Grouped round-trip at the ~100 Mb/s shape: k=175 data shards -> six
    /// FEC groups (30 + 5x29 data; parity 5 + 5x4). Drop shards inside
    /// several different groups, each within that group's own parity budget
    /// (including the frame's short last shard), and the frame must
    /// reconstruct byte-identical.
    #[test]
    fn grouped_parity_recovers_losses_across_groups() {
        let frame: Vec<u8> = (0..17_450u32).map(|i| (i * 13 % 251) as u8).collect();
        // payload 100 -> 175 data shards (last one 50 bytes); 140 permille
        // -> 25 parity shards.
        let max_datagram = 100 + gsa_protocol::datagram::VIDEO_HEADER_LEN;
        let datagrams = chunk_video_frame(hdr(11, 0, 0), &frame, max_datagram, 140).unwrap();
        assert_eq!(datagrams.len(), 200);

        // Group layout: data starts 0/30/59/88/117/146; parity budgets
        // 5/4/4/4/4/4. Losses: 5 data in group 0, 4 data in group 3
        // (mixed with its parity staying), the short last data shard 174 in
        // group 5, and one parity shard of group 1 (parity loss alone must
        // not matter).
        let lost = [0usize, 7, 15, 22, 29, 88, 90, 99, 100, 174, 180];
        let mut r = Reassembler::new();
        let mut out = None;
        for (i, d) in datagrams.iter().enumerate() {
            if lost.contains(&i) {
                continue;
            }
            let (h, payload) = VideoDatagramHeader::parse(d).unwrap();
            if let Some(f) = r.push(h, payload).pop() {
                out = Some(f.data);
            }
        }
        assert_eq!(out, Some(frame));
        assert_eq!(r.frames_recovered(), 1);
        assert_eq!(r.frames_dropped(), 0);
    }

    /// One group losing more than its parity budget makes the frame
    /// unrecoverable even though every other group is intact: the frame never
    /// completes (no panic), and a newer frame still flows.
    #[test]
    fn one_overwhelmed_group_drops_the_frame_cleanly() {
        let frame: Vec<u8> = (0..17_450u32).map(|i| (i * 17 % 249) as u8).collect();
        let max_datagram = 100 + gsa_protocol::datagram::VIDEO_HEADER_LEN;
        let datagrams = chunk_video_frame(hdr(20, 0, 0), &frame, max_datagram, 140).unwrap();

        // Group 1 (data 30..59, 4 parity) loses 5 data shards: dead. Every
        // other shard arrives, including all 25 parity shards.
        let lost = [30usize, 31, 32, 33, 34];
        let mut r = Reassembler::new();
        for (i, d) in datagrams.iter().enumerate() {
            if lost.contains(&i) {
                continue;
            }
            let (h, payload) = VideoDatagramHeader::parse(d).unwrap();
            assert_eq!(
                datas(r.push(h, payload)),
                Vec::<Vec<u8>>::new(),
                "shard {i} must not complete"
            );
        }
        // Newer frames complete; once the hold window passes, the dead frame
        // is pruned and the queue releases.
        assert_eq!(datas(r.push(hdr(21, 0, 1), b"next")), Vec::<Vec<u8>>::new());
        assert_eq!(datas(r.push(hdr(22, 0, 1), b"more")), Vec::<Vec<u8>>::new());
        assert_eq!(
            datas(r.push(hdr(23, 0, 1), b"go")),
            vec![b"next".to_vec(), b"more".to_vec(), b"go".to_vec()]
        );
        assert_eq!(r.frames_dropped(), 1);
    }

    /// Reassembler must never panic on inconsistent shard geometry: parseable
    /// but adversarial headers (mismatched counts, wrong payload sizes,
    /// shifting parity claims) across many pseudo-random pushes.
    #[test]
    fn inconsistent_geometry_never_panics() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rand = move || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut r = Reassembler::new();
        for _ in 0..20_000 {
            let frame_id = (rand() % 6) as u32;
            let count = (rand() % 300 + 1) as u16;
            let parity = (rand() % 256) as u8;
            let index =
                (rand() as usize % gsa_protocol::datagram::total_shards(count, parity)) as u16;
            let header = VideoDatagramHeader {
                seq: 0,
                session_epoch: (rand() % 3) as u8,
                frame_id,
                kind: FrameKind::P,
                chunk_index: index,
                chunk_count: count,
                parity_count: parity,
                frame_len: (rand() % 5_000) as u32,
                capture_ts_us: 0,
            };
            let payload = vec![rand() as u8; (rand() % 40) as usize];
            let _ = r.push(header, &payload);
        }
    }

    #[test]
    fn too_many_losses_still_drop_the_frame() {
        let frame: Vec<u8> = vec![0xAB; 5_000];
        let datagrams = chunk_video_frame(
            hdr(10, 0, 0),
            &frame,
            1000 + gsa_protocol::datagram::VIDEO_HEADER_LEN,
            200,
        )
        .unwrap();
        // 5 data + 1 parity; lose two data shards -> unrecoverable.
        let mut r = Reassembler::new();
        for (i, d) in datagrams.iter().enumerate() {
            if i == 0 || i == 2 {
                continue;
            }
            let (h, payload) = VideoDatagramHeader::parse(d).unwrap();
            assert_eq!(datas(r.push(h, payload)), Vec::<Vec<u8>>::new());
        }
    }
    #[test]
    fn late_parity_survives_a_newer_frame_completing() {
        // Frame 10: 5 data + 1 parity, one data shard lost, parity delayed.
        let frame: Vec<u8> = (0..5_000).map(|i| (i * 7 % 251) as u8).collect();
        let dgs = chunk_video_frame(
            hdr(10, 0, 0),
            &frame,
            1000 + gsa_protocol::datagram::VIDEO_HEADER_LEN,
            200,
        )
        .unwrap();
        let mut r = Reassembler::new();
        // Deliver frame 10's data minus shard 2, no parity yet.
        for (i, d) in dgs.iter().enumerate() {
            if i == 2 || i == 5 {
                continue; // shard 2 lost; parity (index 5) still in flight
            }
            let (h, p) = VideoDatagramHeader::parse(d).unwrap();
            assert_eq!(datas(r.push(h, p)), Vec::<Vec<u8>>::new());
        }
        // Frame 11 completes fully in the meantime.
        let f11: Vec<u8> = vec![9; 800];
        for d in chunk_video_frame(
            hdr(11, 0, 0),
            &f11,
            1000 + gsa_protocol::datagram::VIDEO_HEADER_LEN,
            200,
        )
        .unwrap()
        .iter()
        {
            let (h, p) = VideoDatagramHeader::parse(d).unwrap();
            let _ = r.push(h, p);
        }
        // Frame 10's parity finally arrives: 10 recovers and 11 releases
        // right behind it, in order.
        let (h, p) = VideoDatagramHeader::parse(&dgs[5]).unwrap();
        let out = r.push(h, p);
        assert_eq!(out.len(), 2, "recovered frame and its held successor");
        assert_eq!(out[0].frame_id, 10);
        assert_eq!(out[0].data, frame, "late parity must still recover");
        assert_eq!(out[1].frame_id, 11);
        assert_eq!(r.frames_recovered(), 1);
    }
}
