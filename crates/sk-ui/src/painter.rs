//! Drawing text, hairlines and the two icon shapes, in device pixels.

use sk_gpu::{Color, DrawList, Rect};
use sk_text::{Fonts, GlyphKey, LayoutCache, RunKey, SUBPIXEL_BUCKETS, ShapedRun, Weight};

/// Which end of a box a string is anchored to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    /// Anchored at the left edge, truncated on the right.
    Left,
    /// Anchored at the right edge. Used for numbers, so digits line up under
    /// each other and a column of sizes can be compared by eye.
    Right,
}

/// How a string is drawn.
///
/// A struct rather than four more parameters: a text call has seven independent
/// inputs and the four that describe *appearance* belong together — they are
/// what a theme would set, and they are what a caller reads back at a glance.
#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    /// Which face.
    pub weight: Weight,
    /// Size in logical pixels.
    pub size: f32,
    /// The colour.
    pub color: Color,
    /// Which edge of the box the string is anchored to.
    pub align: Align,
}

impl TextStyle {
    /// Regular weight, left aligned.
    #[must_use]
    pub const fn new(size: f32, color: Color) -> Self {
        Self { weight: Weight::Regular, size, color, align: Align::Left }
    }

    /// The same style in the bold face.
    #[must_use]
    pub const fn bold(self) -> Self {
        Self { weight: Weight::Bold, ..self }
    }

    /// The same style anchored to the right edge.
    #[must_use]
    pub const fn right(self) -> Self {
        Self { align: Align::Right, ..self }
    }
}

/// Everything one frame's drawing needs, borrowed for the length of the frame.
pub struct Painter<'a> {
    /// The frame being built.
    pub list: &'a mut DrawList,
    /// The faces, for shaping and rasterization.
    pub fonts: &'a mut Fonts,
    /// Shaped runs kept between frames.
    pub cache: &'a mut LayoutCache,
    /// Colours and sizes.
    pub theme: &'a crate::Theme,
    /// Device pixels per logical pixel.
    pub scale: f32,
}

impl Painter<'_> {
    /// Logical pixels to device pixels.
    #[must_use]
    pub fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    /// Logical pixels to a whole device pixel.
    ///
    /// For anything with an edge a person can see: a hairline, a row boundary, a
    /// text baseline. A one-pixel line at a fractional position is two grey
    /// lines, and that is the single most common way a scaled interface looks
    /// cheap.
    #[must_use]
    pub fn snap(&self, logical: f32) -> f32 {
        (logical * self.scale).round()
    }

    /// A rectangle from logical coordinates, snapped to the device grid.
    ///
    /// Both edges are snapped rather than the origin and the size, so adjacent
    /// rows share an edge exactly and cannot leave a seam between them.
    #[must_use]
    pub fn rect(&self, x: f32, y: f32, width: f32, height: f32) -> Rect {
        let left = self.snap(x);
        let top = self.snap(y);
        Rect::new(left, top, self.snap(x + width) - left, self.snap(y + height) - top)
    }

    /// A one-device-pixel line, whatever the scale.
    ///
    /// Deliberately not scaled: a divider is a hairline at 100% and it should
    /// still be a hairline at 200%, because its job is to separate two regions
    /// without becoming a design element of its own.
    pub fn hairline(&mut self, x: f32, y: f32, width: f32, color: Color) {
        let left = self.snap(x);
        let rect = Rect::new(left, self.snap(y), self.snap(x + width) - left, 1.0);
        self.list.rect(rect, color, 0.0);
    }

    /// Draws the text of one listing cell, keyed by which cell it is.
    ///
    /// Truncates with an ellipsis when it does not fit. The ellipsis is a real
    /// `…` from the font rather than three periods, because three periods are
    /// three times as wide and read as punctuation.
    pub fn cell(
        &mut self,
        key: RunKey,
        text: &str,
        box_rect: Rect,
        baseline: f32,
        color: Color,
        align: Align,
    ) -> f32 {
        self.draw(Source::Cell(key), text, box_rect, baseline, color, align)
    }

    /// Draws a chrome string — a path, a header, a status line — keyed by its
    /// own content, because the text at a fixed position changes every frame.
    pub fn label(&mut self, text: &str, style: TextStyle, box_rect: Rect, baseline: f32) -> f32 {
        let px_size = self.px(style.size);
        self.draw(
            Source::Label { weight: style.weight, px_size },
            text,
            box_rect,
            baseline,
            style.color,
            style.align,
        )
    }

    /// The one implementation.
    ///
    /// Nothing here allocates. The shaped run is borrowed straight out of the
    /// cache and the ellipsis is copied into a fixed array, because this runs
    /// once per visible cell per frame — a few thousand times a second at forty
    /// rows and 120 Hz, which is where a per-call `Vec` would show up.
    fn draw(
        &mut self,
        source: Source,
        text: &str,
        box_rect: Rect,
        baseline: f32,
        color: Color,
        align: Align,
    ) -> f32 {
        let px_size = source.px_size();
        let limit = box_rect.width;
        let baseline = baseline.round();
        let left = box_rect.x;
        if limit <= 0.0 || text.is_empty() {
            return 0.0;
        }

        // Split the borrow: `list`, `fonts` and `cache` are distinct fields, so a
        // shaped run can be read out of the cache while quads go into the list.
        let Self { list, fonts, cache, .. } = self;

        let advance = source.resolve(cache, text, fonts).advance;
        if advance <= limit {
            let start = match align {
                Align::Left => left,
                Align::Right => (left + limit - advance).round(),
            };
            for glyph in &source.resolve(cache, text, fonts).glyphs {
                emit(list, glyph, start, baseline, px_size, color);
            }
            return advance;
        }

        // Reserve the ellipsis before deciding what fits, so the result is never
        // one glyph wider than the column it was given.
        let mut ellipsis: [Option<sk_text::ShapedGlyph>; 4] = [None; 4];
        let tail = {
            let run = cache.get_or_shape(ellipsis_key(px_size), "…", fonts);
            for (slot, glyph) in ellipsis.iter_mut().zip(&run.glyphs) {
                *slot = Some(*glyph);
            }
            run.advance
        };

        let run = source.resolve(cache, text, fonts);
        let (count, _) = run.truncate_at((limit - tail).max(0.0));
        // Where the first dropped glyph would have started, which is exactly
        // where the ellipsis belongs.
        let used = run.glyphs.get(count).map_or(run.advance, |glyph| glyph.x);
        for glyph in run.glyphs.iter().take(count) {
            emit(list, glyph, left, baseline, px_size, color);
        }
        for glyph in ellipsis.iter().flatten() {
            emit(list, glyph, left + used, baseline, px_size, color);
        }
        limit
    }

    /// The baseline for text vertically centred in a box.
    ///
    /// **Logical in, device out**, like everything else here. Mixing the two was
    /// worth stating as a rule rather than a habit: the first version of this
    /// function added a logical box position to a device-pixel ascent, which is
    /// arithmetic that happens to look right at exactly 100% scale.
    ///
    /// Centred on the ascent and descent rather than on the em box, so a row of
    /// mostly-lowercase filenames sits where the eye expects it instead of a
    /// pixel high.
    #[must_use]
    pub fn baseline(&mut self, box_top: f32, box_height: f32, logical_size: f32) -> f32 {
        let px_size = self.px(logical_size);
        let metrics = self.fonts.metrics(Weight::Regular, px_size);
        let top = self.snap(box_top);
        let height = self.px(box_height);
        (top + (height - (metrics.ascent + metrics.descent)) / 2.0 + metrics.ascent).round()
    }

    /// The mark for a directory: a folder, from two rounded rectangles.
    ///
    /// Geometric rather than from an icon theme, because icon themes belong to
    /// `sk-desktop` and arrive in M7. Solid rather than outlined: at sixteen
    /// logical pixels an outlined folder is four hairlines around a hole, and a
    /// solid one reads at a glance.
    pub fn folder(&mut self, box_rect: Rect, color: Color) {
        let unit = box_rect.height / 16.0;
        let tab = Rect::new(
            box_rect.x + unit,
            box_rect.y + unit * 2.0,
            box_rect.width * 0.45,
            unit * 3.5,
        );
        let body = Rect::new(
            box_rect.x,
            box_rect.y + unit * 4.0,
            box_rect.width,
            box_rect.height - unit * 5.0,
        );
        self.list.rect(tab, color, unit);
        self.list.rect(body, color, unit * 1.6);
    }

    /// The mark for a file: an outlined page with two lines of text in it.
    pub fn file(&mut self, box_rect: Rect, color: Color) {
        let unit = box_rect.height / 16.0;
        let page = Rect::new(
            box_rect.x + box_rect.width * 0.18,
            box_rect.y + unit,
            box_rect.width * 0.64,
            box_rect.height - unit * 2.0,
        );
        self.list.stroke(page, color, unit * 1.4, (unit * 1.3).max(1.0));
        for offset in [0.40, 0.60] {
            let rect = Rect::new(
                (page.x + page.width * 0.26).round(),
                (page.y + page.height * offset).round(),
                (page.width * 0.48).round(),
                (unit * 1.3).max(1.0).round(),
            );
            self.list.rect(rect, color, 0.0);
        }
    }
}

/// Where a shaped run comes from: a listing cell, or a string of chrome.
#[derive(Clone, Copy)]
enum Source {
    Cell(RunKey),
    Label { weight: Weight, px_size: f32 },
}

impl Source {
    fn px_size(self) -> f32 {
        match self {
            Self::Cell(key) => key.px_size(),
            Self::Label { px_size, .. } => px_size,
        }
    }

    fn resolve<'c>(
        self,
        cache: &'c mut LayoutCache,
        text: &str,
        fonts: &mut Fonts,
    ) -> &'c ShapedRun {
        match self {
            Self::Cell(key) => cache.get_or_shape(key, text, fonts),
            Self::Label { weight, px_size } => {
                cache.get_or_shape_label(text, weight, px_size, fonts)
            }
        }
    }
}

/// One glyph, positioned.
///
/// Free rather than a method so the caller can hold a borrow of the layout cache
/// across the loop that calls it.
fn emit(
    list: &mut DrawList,
    glyph: &sk_text::ShapedGlyph,
    origin: f32,
    baseline: f32,
    px_size: f32,
    color: Color,
) {
    let x = origin + glyph.x;
    // Horizontal subpixel positioning: the raster is made at the fractional
    // offset the glyph will actually sit at, and drawn at the whole pixel below
    // it. Rounding the pen instead is what makes a line of text look unevenly
    // spaced — which shows up as a wobble along a column of filenames, not as
    // blur, and is therefore easy to ship without noticing.
    let bucket = (x.fract() * f32::from(SUBPIXEL_BUCKETS)).round() as u8 % SUBPIXEL_BUCKETS;
    let key = GlyphKey {
        face: glyph.face,
        glyph: glyph.glyph,
        quarter_px: (px_size * 4.0).round().clamp(0.0, f32::from(u16::MAX)) as u16,
        subpixel: bucket,
    };
    list.glyph(key, x.floor(), (baseline + glyph.y).round(), color);
}

/// The cache key for the ellipsis at one size.
///
/// `u32::MAX` as the entry index: a listing cannot hold that many entries — the
/// index is a `u32` and the name arena would have to be larger than a `u32` can
/// address — so it cannot collide with a real row.
fn ellipsis_key(px_size: f32) -> RunKey {
    RunKey::cell(u32::MAX, 0, Weight::Regular, px_size)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::ellipsis_key;
    use sk_text::{RunKey, Weight};

    #[test]
    fn the_ellipsis_key_cannot_collide_with_a_row() {
        let ellipsis = ellipsis_key(13.0);
        assert_ne!(ellipsis, RunKey::cell(0, 0, Weight::Regular, 13.0));
        assert_ne!(ellipsis, RunKey::cell(u32::MAX - 1, 0, Weight::Regular, 13.0));
        assert_eq!(ellipsis, ellipsis_key(13.0), "and it is stable at one size");
        assert_ne!(ellipsis, ellipsis_key(16.25), "and distinct across sizes");
    }
}
