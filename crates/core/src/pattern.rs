//! Test-pattern frame-index marker: pure math shared by the TestPattern
//! source (writes it) and test clients (read it back after decode) so the
//! e2e pipeline can prove that real pixels survived encode → wire → decode.
//!
//! The frame index is written as 32 solid blocks along the top-left edge
//! (one per bit, MSB first): white = 1, black = 0. Solid 8x8 blocks survive
//! lossy H.264 at any reasonable bitrate.

/// Side length in pixels of one bit-block.
pub const BLOCK: usize = 8;
/// Number of bit-blocks (one u32).
pub const BLOCKS: usize = 32;
/// Minimum frame width for the marker to fit.
pub const MIN_WIDTH: usize = BLOCK * BLOCKS;

/// Write the marker into a BGRA8 buffer (`stride` in bytes).
///
/// # Panics
/// Panics if the buffer is smaller than `BLOCK` rows of `MIN_WIDTH` pixels.
pub fn write_marker_bgra(buf: &mut [u8], stride: usize, index: u32) {
    assert!(stride >= MIN_WIDTH * 4, "frame too narrow for marker");
    for bit in 0..BLOCKS {
        let on = (index >> (31 - bit)) & 1 == 1;
        let v: u8 = if on { 0xff } else { 0x00 };
        for y in 0..BLOCK {
            let row = y * stride + bit * BLOCK * 4;
            for x in 0..BLOCK {
                let px = row + x * 4;
                buf[px] = v; // B
                buf[px + 1] = v; // G
                buf[px + 2] = v; // R
                buf[px + 3] = 0xff; // A
            }
        }
    }
}

/// Read the marker back from a luma (Y) plane (`stride` in bytes).
/// Samples the center of each block and thresholds at mid-gray, so it
/// tolerates lossy-codec ringing.
///
/// Returns `None` if the plane is too small.
#[must_use]
pub fn read_marker_luma(y_plane: &[u8], stride: usize, width: usize) -> Option<u32> {
    if width < MIN_WIDTH || stride < width {
        return None;
    }
    let mut index = 0u32;
    for bit in 0..BLOCKS {
        let cx = bit * BLOCK + BLOCK / 2;
        let cy = BLOCK / 2;
        let sample = *y_plane.get(cy * stride + cx)?;
        if sample > 0x80 {
            index |= 1 << (31 - bit);
        }
    }
    Some(index)
}

/// Read the marker from a packed 4-byte-per-pixel image (RGBA or BGRA —
/// marker blocks are pure black/white so the green channel is a valid
/// brightness proxy in either order). `width` in pixels, tightly packed.
#[must_use]
pub fn read_marker_rgba(buf: &[u8], width: usize) -> Option<u32> {
    if width < MIN_WIDTH {
        return None;
    }
    let mut index = 0u32;
    for bit in 0..BLOCKS {
        let cx = bit * BLOCK + BLOCK / 2;
        let cy = BLOCK / 2;
        let green = *buf.get((cy * width + cx) * 4 + 1)?;
        if green > 0x80 {
            index |= 1 << (31 - bit);
        }
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BT.601-ish luma from BGRA, good enough for the threshold test.
    fn bgra_to_luma(buf: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
        let mut y = vec![0u8; w * h];
        for r in 0..h {
            for c in 0..w {
                let p = r * stride + c * 4;
                let (b, g, rr) = (buf[p] as u32, buf[p + 1] as u32, buf[p + 2] as u32);
                y[r * w + c] = ((66 * rr + 129 * g + 25 * b + 128) / 256 + 16) as u8;
            }
        }
        y
    }

    #[test]
    fn marker_round_trip() {
        let (w, h) = (MIN_WIDTH, 16);
        let stride = w * 4;
        let mut buf = vec![0x40u8; stride * h];
        for index in [0u32, 1, 0xdead_beef, u32::MAX, 60] {
            write_marker_bgra(&mut buf, stride, index);
            let luma = bgra_to_luma(&buf, stride, w, h);
            assert_eq!(read_marker_luma(&luma, w, w), Some(index));
            assert_eq!(read_marker_rgba(&buf, w), Some(index));
        }
    }
}
