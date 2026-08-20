//! The on-screen stats overlay: text rasterised to a small RGBA image.
//!
//! A harness whose numbers live only in a log measures sessions; one that
//! shows them on the stream measures *runs a person is watching* — which is
//! when the numbers are wanted. The apps have this already; this is the dev
//! client's equivalent, and it deliberately reads the same way.
//!
//! Rasterised on the CPU because it is tiny and rare: a few hundred by a few
//! hundred pixels, redrawn when the stats tick (about once a second), not per
//! frame. The GPU just composites the result, so the cost per frame is one
//! textured quad.

use font8x8::legacy::BASIC_LEGACY;

/// Pixels per glyph cell before scaling.
const CELL: usize = 8;
/// Integer upscale, chosen for legibility at stream resolutions: an 8-pixel
/// glyph at 3× reads comfortably over 1080p video from a couch distance.
const SCALE: usize = 3;
/// Padding around the text block, in output pixels.
const PAD: usize = 12;
/// Extra pixels between lines: an 8-pixel cell leaves no leading of its own,
/// and packed lines of digits smear into each other.
const LEADING: usize = 14;

/// One rasterised overlay: tightly-packed RGBA, ready for a texture upload.
pub struct OverlayImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Draw `lines` as white-on-translucent-black, top-left aligned.
#[must_use]
pub fn rasterise(lines: &[String]) -> OverlayImage {
    let columns = lines.iter().map(String::len).max().unwrap_or(0);
    let width = columns * CELL * SCALE + PAD * 2;
    let height = lines.len() * (CELL * SCALE + LEADING) + PAD * 2;
    let mut pixels = vec![0u8; width * height * 4];

    // The background: enough alpha that the text survives any content behind
    // it, not so much that the stream is hidden.
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 0, 200]);
    }

    for (row, line) in lines.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let glyph = BASIC_LEGACY
                .get(ch as usize)
                .unwrap_or(&BASIC_LEGACY[b'?' as usize]);
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..CELL {
                    if bits & (1 << gx) == 0 {
                        continue;
                    }
                    // One glyph pixel becomes a SCALE×SCALE block.
                    for sy in 0..SCALE {
                        for sx in 0..SCALE {
                            let x = PAD + (col * CELL + gx) * SCALE + sx;
                            let y = PAD + row * (CELL * SCALE + LEADING) + gy * SCALE + sy;
                            let at = (y * width + x) * 4;
                            pixels[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
            }
        }
    }

    OverlayImage {
        pixels,
        #[allow(clippy::cast_possible_truncation)]
        width: width as u32,
        #[allow(clippy::cast_possible_truncation)]
        height: height as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::rasterise;

    /// The image is sized by the text, and the glyphs actually land in it:
    /// a rasteriser that silently draws nothing still produces a plausible
    /// texture, which is the failure this catches.
    #[test]
    fn text_produces_lit_pixels_inside_a_text_sized_image() {
        let image = rasterise(&["fps 120.0".to_owned(), "rtt 11.0".to_owned()]);
        assert!(image.width > 0 && image.height > 0);
        let lit = image
            .pixels
            .chunks_exact(4)
            .filter(|px| px[0] == 255 && px[3] == 255)
            .count();
        assert!(lit > 50, "expected glyph pixels, found {lit}");
        // And the background is translucent, not opaque: the stream must
        // stay visible behind the numbers.
        assert!(image.pixels.chunks_exact(4).any(|px| px[3] == 200));
    }

    /// Characters outside the basic set must not panic mid-session; they
    /// render as '?' instead.
    #[test]
    fn unknown_characters_do_not_panic() {
        let image = rasterise(&["µ → ±".to_owned()]);
        assert!(image.width > 0);
    }
}
