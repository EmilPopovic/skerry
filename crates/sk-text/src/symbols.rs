// SPDX-License-Identifier: Apache-2.0
//! Trusted, bundled vectors rasterized into the existing glyph mask cache.
//! See [ADR 0045](../../../docs/adr/0045-bundled-symbols-use-the-glyph-mask-cache.md).

use swash::zeno::{Cap, Fill, Join, Mask, Stroke, Style, Transform};

use crate::{GlyphImage, GlyphKey, SUBPIXEL_BUCKETS};

#[path = "symbols_data.rs"]
mod data;

/// Reserved cache identity; this face is never part of text fallback.
pub const FACE: u16 = u16::MAX;

struct Paint {
    path: &'static str,
    transform: [f32; 4],
    width: f32,
    dashed: bool,
    layer: u8,
}

/// Which of backing, tint and ink contain geometry, as three bits.
#[must_use]
pub fn layers(symbol: u16) -> u8 {
    data::SYMBOLS
        .get(usize::from(symbol))
        .map_or(0, |paths| paths.iter().fold(0, |bits, paint| bits | (1 << paint.layer)))
}

pub(crate) fn rasterize(key: GlyphKey) -> Option<GlyphImage> {
    let paths = data::SYMBOLS.get(usize::from(key.glyph / 3))?;
    let pixels_per_em = key.px_size();
    if key.face != FACE || !(0.25..=512.0).contains(&pixels_per_em) {
        return None;
    }
    let side = pixels_per_em.ceil() as u32;
    // One extra column holds the horizontal subpixel offset, just like text.
    let width = side + 1;
    let count = width as usize * side as usize;
    let mut pixels = vec![0u8; count];
    let mut scratch = vec![0u8; count];
    let unit = pixels_per_em / 24.0;
    let offset = f32::from(key.subpixel) / f32::from(SUBPIXEL_BUCKETS);
    for paint in paths.iter().filter(|paint| u16::from(paint.layer) == key.glyph % 3) {
        scratch.fill(0);
        let [sx, sy, x, y] = paint.transform;
        let transform = Transform::new(sx * unit, 0.0, 0.0, sy * unit, x * unit + offset, y * unit);
        let style = if paint.width > 0.0 {
            let mut stroke = Stroke::new(paint.width);
            stroke.join(Join::Round).cap(Cap::Round);
            if paint.dashed {
                stroke.dash(&[3.0, 3.0], 0.0);
            }
            Style::Stroke(stroke)
        } else {
            Style::Fill(Fill::NonZero)
        };
        Mask::new(paint.path)
            .style(style)
            .transform(Some(transform))
            .size(width, side)
            .render_into(&mut scratch, None);
        // Coverage union avoids dark seams where two rounded strokes meet.
        for (into, sample) in pixels.iter_mut().zip(&scratch) {
            *into = (*into).max(*sample);
        }
    }
    Some(GlyphImage {
        width,
        height: side,
        left: 0,
        top: side.cast_signed(),
        color: false,
        data: pixels,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_symbol_has_visible_ink_at_fractional_and_integer_sizes() {
        for symbol in 0..data::SYMBOLS.len() as u16 {
            for quarter_px in [52, 65, 80, 120, 192, 384] {
                let key = GlyphKey { face: FACE, glyph: symbol * 3 + 2, quarter_px, subpixel: 2 };
                let image = rasterize(key).unwrap();
                assert!(
                    image.data.iter().any(|&value| value > 100),
                    "symbol {symbol} at {quarter_px}"
                );
                assert_eq!(image.data.len(), (image.width * image.height) as usize);
            }
        }
    }

    #[test]
    fn untrusted_indices_and_unbounded_sizes_are_rejected() {
        for (glyph, quarter_px) in [(u16::MAX, 64), (0, 0), (0, u16::MAX)] {
            assert!(rasterize(GlyphKey { face: FACE, glyph, quarter_px, subpixel: 0 }).is_none());
        }
    }
}
