//! Write one decoded frame to a file, as evidence that decode produced a
//! picture rather than merely returning success.
//!
//! A decoder wired to the wrong codec, or fed the wrong parameter sets, does
//! not always fail: it can report frames while emitting green, grey, or noise.
//! Counting frames cannot tell those apart, and on a headless run there is no
//! window to look at, so the pixels themselves have to be recoverable.
//!
//! BMP because it needs no dependency: a 54-byte header, rows bottom-up, and
//! `sips -s format png` converts it for anything that cannot read BMP.

use std::io::Write;

use gsa_client_core::{DecodedFrame, PixelOrder};

/// Write `frame` to `path` as a 24-bit BMP.
pub fn write_bmp(frame: &DecodedFrame, path: &std::path::Path) -> anyhow::Result<()> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    anyhow::ensure!(
        frame.pixels.len() >= frame.order.frame_bytes(width, height),
        "frame is {} bytes, short of {}x{}",
        frame.pixels.len(),
        width,
        height
    );
    // BMP rows are padded to a 4-byte boundary.
    let stride = (width * 3).next_multiple_of(4);
    let pixel_bytes = stride * height;

    let mut out = Vec::with_capacity(54 + pixel_bytes);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + pixel_bytes) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
    out.extend_from_slice(&40u32.to_le_bytes()); // header size
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // no compression
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution and palette counts

    // BMP stores blue, green, red, and its rows run bottom to top.
    let swap_red_blue = matches!(frame.order, PixelOrder::Rgba);
    for y in (0..height).rev() {
        let start = out.len();
        match frame.order {
            // Planar HDR: the display path converts on the GPU, but a
            // screenshot has to be a picture a person can open, so this is the
            // one place the conversion is still done on the CPU. It runs on
            // the handful of frames a script asks for, not on every frame.
            PixelOrder::P010Bt2020Pq { full_range } => {
                let (offset, luma_span, chroma_span) = if full_range {
                    (0.0f32, 1023.0f32, 1023.0f32)
                } else {
                    (64.0, 876.0, 896.0)
                };
                let luma_plane = &frame.pixels[..width * height * 2];
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                let chroma_plane = &frame.pixels[width * height * 2..];
                let sample = |plane: &[u8], index: usize| -> f32 {
                    f32::from(u16::from_le_bytes([plane[index * 2], plane[index * 2 + 1]]) >> 6)
                };
                for x in 0..width {
                    let yy = (sample(luma_plane, y * width + x) - offset) / luma_span;
                    let ci = (y.min(ch - 1) / 2).min(ch - 1) * cw + (x / 2).min(cw - 1);
                    let cb = (sample(chroma_plane, ci * 2) - 512.0) / chroma_span;
                    let cr = (sample(chroma_plane, ci * 2 + 1) - 512.0) / chroma_span;
                    // BT.2020 non-constant luminance, then PQ decoded and
                    // referred to diffuse white so the dump is viewable.
                    let coded = [
                        yy + 1.474_60 * cr,
                        yy - 0.164_55 * cb - 0.571_35 * cr,
                        yy + 1.881_40 * cb,
                    ];
                    let light =
                        coded.map(|c| crate::decoder_vt::pq_eotf_nits(c.clamp(0.0, 1.0)) / 203.0);
                    let px = [
                        1.660_50 * light[0] - 0.587_64 * light[1] - 0.072_85 * light[2],
                        -0.124_55 * light[0] + 1.132_90 * light[1] - 0.008_35 * light[2],
                        -0.018_12 * light[0] - 0.100_57 * light[1] + 1.118_69 * light[2],
                    ];
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let byte = |v: f32| (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
                    out.extend_from_slice(&[byte(px[2]), byte(px[1]), byte(px[0])]);
                }
            }
            _ => {
                let row = &frame.pixels[y * width * 4..(y + 1) * width * 4];
                for px in row.chunks_exact(4) {
                    let (b, g, r) = if swap_red_blue {
                        (px[2], px[1], px[0])
                    } else {
                        (px[0], px[1], px[2])
                    };
                    out.extend_from_slice(&[b, g, r]);
                }
            }
        }
        out.resize(start + stride, 0);
    }

    let mut file = std::fs::File::create(path)?;
    file.write_all(&out)?;
    // Mean and spread separate the two ways a dump disappoints: a picture that
    // is genuinely dark, and a decode that produced a flat fill. A real frame
    // varies across the image; a broken one is one value everywhere.
    let (mean, spread) = brightness(frame);
    tracing::info!(
        path = %path.display(),
        width,
        height,
        mean,
        spread,
        "wrote decoded frame"
    );
    Ok(())
}

/// Mean brightness and its spread (max - min), each 0-255.
fn brightness(frame: &DecodedFrame) -> (u8, u8) {
    let (mut total, mut min, mut max) = (0u64, 255u8, 0u8);
    let pixels: Vec<u8> = frame
        .pixels
        .chunks_exact(4)
        .map(|px| ((u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3) as u8)
        .collect();
    for value in &pixels {
        total += u64::from(*value);
        min = min.min(*value);
        max = max.max(*value);
    }
    let mean = (total / pixels.len().max(1) as u64) as u8;
    (mean, max.saturating_sub(min))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(order: PixelOrder) -> DecodedFrame {
        // One red pixel over one blue pixel, in the given channel order.
        let (red, blue) = match order {
            PixelOrder::Rgba => ([255, 0, 0, 255], [0, 0, 255, 255]),
            _ => ([0, 0, 255, 255], [255, 0, 0, 255]),
        };
        DecodedFrame {
            width: 1,
            height: 2,
            pixels: [red, blue].concat(),
            order,
        }
    }

    /// Both decoders' channel orders must land the same colours in the file,
    /// or the evidence says "wrong colours" for a decode that was correct.
    #[test]
    fn either_channel_order_writes_the_same_picture() {
        let dir = std::env::temp_dir();
        let (a, b) = (dir.join("gsa-rgba.bmp"), dir.join("gsa-bgra.bmp"));
        write_bmp(&frame(PixelOrder::Rgba), &a).unwrap();
        write_bmp(&frame(PixelOrder::Bgra), &b).unwrap();
        let (one, two) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_eq!(one, two);

        // Rows run bottom-up, so the blue pixel is written first, and BMP
        // orders channels blue, green, red.
        assert_eq!(&one[54..57], &[255, 0, 0]);
        let stride = 4;
        assert_eq!(&one[54 + stride..54 + stride + 3], &[0, 0, 255]);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    /// A flat fill and a real picture must be distinguishable from the log
    /// alone, since a headless run has no window to look at.
    #[test]
    fn spread_separates_a_flat_fill_from_a_picture() {
        let flat = DecodedFrame {
            width: 2,
            height: 1,
            pixels: vec![16, 16, 16, 255, 16, 16, 16, 255],
            order: PixelOrder::Bgra,
        };
        assert_eq!(brightness(&flat), (16, 0));
        let varied = DecodedFrame {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 0, 255, 200, 200, 200, 255],
            order: PixelOrder::Bgra,
        };
        let (mean, spread) = brightness(&varied);
        assert_eq!(mean, 100);
        assert_eq!(spread, 200);
    }

    #[test]
    fn a_short_frame_is_refused_rather_than_read_past() {
        let short = DecodedFrame {
            width: 64,
            height: 64,
            pixels: vec![0; 16],
            order: PixelOrder::Bgra,
        };
        assert!(write_bmp(&short, &std::env::temp_dir().join("gsa-short.bmp")).is_err());
    }
}
