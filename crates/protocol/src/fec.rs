//! Dependency-free Reed-Solomon erasure coding over GF(2^8) for the video
//! datagram path: any `k` of the `k + m` shards reconstruct the frame.
//!
//! In-tree rather than a crate: `client-core` must build on every target the
//! embedding apps ship (tvOS/watchOS/visionOS/Android), where external deps
//! with locking/alloc opinions routinely break tier-3 builds. The math is
//! small: log/exp tables, a Cauchy parity matrix (every square submatrix is
//! invertible, the erasure-coding property), and Gauss-Jordan elimination.

/// GF(2^8) with the AES-adjacent primitive polynomial x^8+x^4+x^3+x^2+1
/// (0x11d), generator 2. `EXP` is doubled so `mul` needs no modular reduce.
struct Tables {
    exp: [u8; 510],
    log: [u8; 256],
}

const TABLES: Tables = build_tables();

const fn build_tables() -> Tables {
    let mut exp = [0u8; 510];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    Tables { exp, log }
}

#[inline]
fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    TABLES.exp[TABLES.log[a as usize] as usize + TABLES.log[b as usize] as usize]
}

#[inline]
fn inv(a: u8) -> u8 {
    debug_assert!(a != 0, "zero has no inverse");
    TABLES.exp[255 - TABLES.log[a as usize] as usize]
}

/// Cauchy coefficient for parity row `j` over data column `i`:
/// `1 / ((k + j) ^ i)` — the x/y index sets are disjoint for `k + m <= 255`,
/// so the denominator is never zero and every submatrix is invertible.
#[inline]
fn coef(k: usize, j: usize, i: usize) -> u8 {
    inv(((k + j) ^ i) as u8)
}

/// Maximum data shards per FEC group. Reed-Solomon parity cost is
/// O(k_g x m_g) per group, so capping k_g makes whole-frame parity cost
/// linear in frame size instead of quadratic.
pub const MAX_GROUP_DATA: usize = 32;

/// One group's slice of a frame's shard space: its data shards are the
/// contiguous range `data_start..data_start + data_len`; its parity shards
/// occupy the global (past-all-data) indices
/// `parity_start..parity_start + parity_len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecGroup {
    pub data_start: usize,
    pub data_len: usize,
    pub parity_start: usize,
    pub parity_len: usize,
}

/// Deterministic group layout for a frame with `k` data and `m` parity
/// shards. Chunker and reassembler both derive it from the header's
/// (`chunk_count`, `parity_count`) alone — nothing extra on the wire.
///
/// Layout rule (exact): `groups = k.div_ceil(MAX_GROUP_DATA)`. Data is split
/// contiguously and evenly: the first `k % groups` groups get
/// `k / groups + 1` data shards, the rest get `k / groups`. Parity is
/// distributed the same way: group `g` gets `m / groups + (g < m % groups)`
/// parity shards, assigned global indices in group order starting at `k`.
///
/// For `k <= MAX_GROUP_DATA` this is a single group spanning the whole frame,
/// which matches the historical ungrouped encoding exactly.
pub fn group_layout(k: usize, m: usize) -> impl Iterator<Item = FecGroup> {
    let groups = k.div_ceil(MAX_GROUP_DATA).max(1);
    let (data_base, data_rem) = (k / groups, k % groups);
    let (parity_base, parity_rem) = (m / groups, m % groups);
    let mut data_start = 0;
    let mut parity_start = k;
    (0..groups).map(move |g| {
        let group = FecGroup {
            data_start,
            data_len: data_base + usize::from(g < data_rem),
            parity_start,
            parity_len: parity_base + usize::from(g < parity_rem),
        };
        data_start += group.data_len;
        parity_start += group.parity_len;
        group
    })
}

/// Compute `parity` shards over equal-length `data` shards.
///
/// # Panics
/// If shard lengths differ or `data.len() + parity > 255`.
#[must_use]
pub fn encode_parity(data: &[&[u8]], parity: usize) -> Vec<Vec<u8>> {
    let k = data.len();
    assert!(k >= 1 && k + parity <= 255, "shard counts out of range");
    let len = data[0].len();
    assert!(
        data.iter().all(|d| d.len() == len),
        "data shards must be equal length"
    );
    (0..parity)
        .map(|j| {
            let mut out = vec![0u8; len];
            for (i, shard) in data.iter().enumerate() {
                let c = coef(k, j, i);
                for (o, &b) in out.iter_mut().zip(shard.iter()) {
                    *o ^= mul(c, b);
                }
            }
            out
        })
        .collect()
}

/// Reconstruct the missing DATA shards in `shards` (data `0..k`, then parity),
/// in place. Every `Some` shard must have equal length. Returns `false` when
/// fewer than `k` shards are present (unrecoverable); parity slots are left
/// as-is.
pub fn reconstruct_data(shards: &mut [Option<Vec<u8>>], k: usize) -> bool {
    let total = shards.len();
    if k == 0 || total < k || total > 255 {
        return false;
    }
    let missing: Vec<usize> = (0..k).filter(|&i| shards[i].is_none()).collect();
    if missing.is_empty() {
        return true;
    }
    // Pick one available parity row per missing data shard.
    let parity_rows: Vec<usize> = (k..total)
        .filter(|&i| shards[i].is_some())
        .map(|i| i - k)
        .take(missing.len())
        .collect();
    if parity_rows.len() < missing.len() {
        return false;
    }
    let len = shards.iter().flatten().next().map_or(0, Vec::len);
    if len == 0 || shards.iter().flatten().any(|s| s.len() != len) {
        return false;
    }

    // Solve A·x = rhs where A picks the missing columns out of the chosen
    // parity rows. Invert A once (Gauss-Jordan over GF(2^8)); apply per byte.
    let e = missing.len();
    let mut a = vec![vec![0u8; e]; e];
    for (r, &j) in parity_rows.iter().enumerate() {
        for (c, &i) in missing.iter().enumerate() {
            a[r][c] = coef(k, j, i);
        }
    }
    let a_inv = match invert(a) {
        Some(m) => m,
        None => return false, // unreachable for a Cauchy submatrix
    };

    // rhs_r = parity_r ^ sum(coef * present data shards)
    let mut rhs = vec![vec![0u8; len]; e];
    for (r, &j) in parity_rows.iter().enumerate() {
        let row = &mut rhs[r];
        row.copy_from_slice(shards[k + j].as_ref().expect("chosen present"));
        for i in (0..k).filter(|&i| shards[i].is_some()) {
            let c = coef(k, j, i);
            let shard = shards[i].as_ref().expect("present");
            for (o, &b) in row.iter_mut().zip(shard.iter()) {
                *o ^= mul(c, b);
            }
        }
    }
    for (c, &idx) in missing.iter().enumerate() {
        let mut out = vec![0u8; len];
        for (r, row) in rhs.iter().enumerate() {
            let m = a_inv[c][r];
            for (o, &b) in out.iter_mut().zip(row.iter()) {
                *o ^= mul(m, b);
            }
        }
        shards[idx] = Some(out);
    }
    true
}

/// Gauss-Jordan inverse over GF(2^8); `None` if singular.
fn invert(mut a: Vec<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    let mut inv_m: Vec<Vec<u8>> = (0..n)
        .map(|i| (0..n).map(|j| u8::from(i == j)).collect())
        .collect();
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r][col] != 0)?;
        a.swap(col, pivot);
        inv_m.swap(col, pivot);
        let piv_inv = inv(a[col][col]);
        for j in 0..n {
            a[col][j] = mul(a[col][j], piv_inv);
            inv_m[col][j] = mul(inv_m[col][j], piv_inv);
        }
        for r in 0..n {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for j in 0..n {
                    let (arj, irj) = (mul(f, a[col][j]), mul(f, inv_m[col][j]));
                    a[r][j] ^= arj;
                    inv_m[r][j] ^= irj;
                }
            }
        }
    }
    Some(inv_m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes.
    fn bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn field_axioms_hold() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "a={a}");
        }
        assert_eq!(mul(0, 7), 0);
        // Distributivity spot-check.
        for &(a, b, c) in &[(3u8, 200u8, 90u8), (255, 254, 253), (17, 34, 51)] {
            assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
        }
    }

    #[test]
    fn any_k_of_n_reconstructs() {
        for &(k, m) in &[(1usize, 1usize), (2, 1), (5, 2), (10, 3), (20, 5)] {
            let data: Vec<Vec<u8>> = (0..k).map(|i| bytes(i as u64 + 7, 257)).collect();
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            let parity = encode_parity(&refs, m);

            // Erase the WORST case: the first m data shards.
            let mut shards: Vec<Option<Vec<u8>>> = data
                .iter()
                .enumerate()
                .map(|(i, d)| (i >= m.min(k)).then(|| d.clone()))
                .chain(parity.into_iter().map(Some))
                .collect();
            assert!(reconstruct_data(&mut shards, k), "k={k} m={m}");
            for (i, d) in data.iter().enumerate() {
                assert_eq!(shards[i].as_ref(), Some(d), "k={k} m={m} shard {i}");
            }
        }
    }

    #[test]
    fn mixed_data_and_parity_losses() {
        let k = 8;
        let data: Vec<Vec<u8>> = (0..k).map(|i| bytes(i as u64 + 99, 64)).collect();
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = encode_parity(&refs, 4);
        // Lose data shards 1 and 6, parity shards 0 and 3 -> still k=8 present.
        let mut shards: Vec<Option<Vec<u8>>> = data
            .iter()
            .enumerate()
            .map(|(i, d)| (i != 1 && i != 6).then(|| d.clone()))
            .chain(
                parity
                    .into_iter()
                    .enumerate()
                    .map(|(j, p)| (j != 0 && j != 3).then_some(p)),
            )
            .collect();
        assert!(reconstruct_data(&mut shards, k));
        assert_eq!(shards[1].as_ref(), Some(&data[1]));
        assert_eq!(shards[6].as_ref(), Some(&data[6]));
    }

    #[test]
    fn group_layout_partitions_exactly() {
        for k in 1..=520usize {
            for &m in &[0usize, 1, 5, 25, 255] {
                let groups: Vec<FecGroup> = group_layout(k, m).collect();
                assert_eq!(groups.len(), k.div_ceil(MAX_GROUP_DATA));
                // Data ranges tile 0..k contiguously; parity ranges tile
                // k..k+m contiguously; every group stays within GF(2^8).
                let mut next_data = 0;
                let mut next_parity = k;
                for g in &groups {
                    assert_eq!(g.data_start, next_data, "k={k} m={m}");
                    assert_eq!(g.parity_start, next_parity, "k={k} m={m}");
                    assert!(g.data_len >= 1 && g.data_len <= MAX_GROUP_DATA);
                    assert!(g.data_len + g.parity_len <= 255 || groups.len() == 1);
                    next_data += g.data_len;
                    next_parity += g.parity_len;
                }
                assert_eq!(next_data, k);
                assert_eq!(next_parity, k + m);
                // Even split: sizes differ by at most one, larger ones first.
                let sizes: Vec<usize> = groups.iter().map(|g| g.data_len).collect();
                assert!(sizes.windows(2).all(|w| w[0] >= w[1] && w[0] - w[1] <= 1));
                let psizes: Vec<usize> = groups.iter().map(|g| g.parity_len).collect();
                assert!(psizes.windows(2).all(|w| w[0] >= w[1] && w[0] - w[1] <= 1));
            }
        }
    }

    #[test]
    fn small_frames_are_a_single_whole_frame_group() {
        for k in 1..=MAX_GROUP_DATA {
            let groups: Vec<FecGroup> = group_layout(k, 3).collect();
            assert_eq!(
                groups,
                vec![FecGroup {
                    data_start: 0,
                    data_len: k,
                    parity_start: k,
                    parity_len: 3,
                }]
            );
        }
    }

    /// Grouped encode+reconstruct round-trips when each group's losses stay
    /// within its own parity budget.
    #[test]
    fn grouped_parity_recovers_per_group_losses() {
        let (k, m, len) = (100usize, 12usize, 96usize);
        let data: Vec<Vec<u8>> = (0..k).map(|i| bytes(i as u64 + 3, len)).collect();
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();

        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.resize(k + m, None);
        for g in group_layout(k, m) {
            let parity =
                encode_parity(&refs[g.data_start..g.data_start + g.data_len], g.parity_len);
            for (j, p) in parity.into_iter().enumerate() {
                shards[g.parity_start + j] = Some(p);
            }
        }
        // Erase the first `parity_len` data shards of every group (worst case).
        for g in group_layout(k, m) {
            for i in 0..g.parity_len.min(g.data_len) {
                shards[g.data_start + i] = None;
            }
        }
        for g in group_layout(k, m) {
            let mut gs: Vec<Option<Vec<u8>>> = shards[g.data_start..g.data_start + g.data_len]
                .iter()
                .cloned()
                .chain(
                    shards[g.parity_start..g.parity_start + g.parity_len]
                        .iter()
                        .cloned(),
                )
                .collect();
            assert!(reconstruct_data(&mut gs, g.data_len));
            for (off, s) in gs.into_iter().take(g.data_len).enumerate() {
                shards[g.data_start + off] = s;
            }
        }
        for (i, d) in data.iter().enumerate() {
            assert_eq!(shards[i].as_ref(), Some(d), "shard {i}");
        }
    }

    #[test]
    fn insufficient_shards_fail_cleanly() {
        let data: Vec<Vec<u8>> = (0..4).map(|i| bytes(i as u64, 32)).collect();
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = encode_parity(&refs, 1);
        let mut shards: Vec<Option<Vec<u8>>> = vec![
            None,
            None, // two losses, one parity
            Some(data[2].clone()),
            Some(data[3].clone()),
            Some(parity[0].clone()),
        ];
        assert!(!reconstruct_data(&mut shards, 4));
    }
}
