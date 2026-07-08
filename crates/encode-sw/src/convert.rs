//! BGRA → I420 color conversion (BT.601 limited range, integer math).
//! On hardware backends this happens on-GPU (spec 03); here it's the CPU
//! path for the software encoder.

use gsa_core::{Error, Result};
use openh264::formats::YUVBuffer;

/// Convert a BGRA8 image (row `stride` in bytes) to an I420 `YUVBuffer`.
/// Width and height must be even (4:2:0 chroma siting).
pub fn bgra_to_i420(bgra: &[u8], stride: usize, width: usize, height: usize) -> Result<YUVBuffer> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Encode(format!(
            "odd dimensions {width}x{height} for 4:2:0"
        )));
    }
    if stride < width * 4 || bgra.len() < stride * height {
        return Err(Error::Encode(
            "BGRA buffer smaller than stride*height".into(),
        ));
    }

    // One contiguous I420 buffer: Y plane, then U, then V
    // (YUVBuffer::from_vec layout).
    let y_len = width * height;
    let c_len = y_len / 4;
    let mut yuv = vec![0u8; y_len + 2 * c_len];
    let (y_plane, chroma) = yuv.split_at_mut(y_len);
    let (u_plane, v_plane) = chroma.split_at_mut(c_len);

    for row in 0..height {
        let src = row * stride;
        let dst = row * width;
        for col in 0..width {
            let p = src + col * 4;
            let (b, g, r) = (
                i32::from(bgra[p]),
                i32::from(bgra[p + 1]),
                i32::from(bgra[p + 2]),
            );
            y_plane[dst + col] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }

    // Chroma: average each 2x2 block.
    for cr in 0..height / 2 {
        for cc in 0..width / 2 {
            let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = (cr * 2 + dy) * stride + (cc * 2 + dx) * 4;
                    sb += i32::from(bgra[p]);
                    sg += i32::from(bgra[p + 1]);
                    sr += i32::from(bgra[p + 2]);
                }
            }
            let (b, g, r) = (sb / 4, sg / 4, sr / 4);
            let ci = cr * (width / 2) + cc;
            u_plane[ci] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            v_plane[ci] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }

    Ok(YUVBuffer::from_vec(yuv, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource;

    #[test]
    fn white_and_black_map_to_expected_luma() {
        let w = 4;
        let h = 2;
        let mut bgra = vec![0u8; w * 4 * h];
        // First 2x2 block white, second black.
        for px in 0..2 {
            for row in 0..2 {
                let p = row * w * 4 + px * 4;
                bgra[p] = 0xff;
                bgra[p + 1] = 0xff;
                bgra[p + 2] = 0xff;
            }
        }
        let yuv = bgra_to_i420(&bgra, w * 4, w, h).unwrap();
        let y = yuv.y();
        assert!(y[0] > 200, "white luma {}", y[0]);
        assert!(y[3] < 40, "black luma {}", y[3]);
    }

    #[test]
    fn rejects_odd_dimensions() {
        assert!(bgra_to_i420(&[0; 100], 12, 3, 2).is_err());
    }
}
