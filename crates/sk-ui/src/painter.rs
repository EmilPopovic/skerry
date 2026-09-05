// SPDX-License-Identifier: Apache-2.0
//! Drawing text, hairlines and the two icon shapes, in device pixels.

use sk_gpu::{Color, Direction, DrawList, Rect};
use sk_text::{Fonts, GlyphKey, LayoutCache, RunKey, SUBPIXEL_BUCKETS, ShapedRun, Weight};

/// Which end of a box a string is anchored to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    /// Anchored at the left edge, truncated on the right.
    Left,
    /// Anchored at the right edge. Used for numbers, so digits line up under
    /// each other and a column of sizes can be compared by eye.
    Right,
    /// Centred in the box. For a mark in a button, where neither edge means
    /// anything and being one pixel off centre is the whole of how a button
    /// looks wrong.
    Center,
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

    /// The same style centred in its box.
    #[must_use]
    pub const fn centred(self) -> Self {
        Self { align: Align::Center, ..self }
    }
}

/// How wide `text` is when shaped, in device pixels.
///
/// The run comes out of the same cache the draw will use, so measuring a string
/// before drawing it costs a hash lookup.
#[must_use]
pub fn advance_of(
    cache: &mut LayoutCache,
    fonts: &mut Fonts,
    text: &str,
    weight: Weight,
    px_size: f32,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    cache.get_or_shape_label(text, weight, px_size, fonts).advance
}

/// Which byte offset in `text` is nearest `want` device pixels from its start.
///
/// **Shaped, not counted, and binary-searched rather than scanned.** A cluster is
/// not a character and a ligature is not two, so the only correct answer comes
/// from shaping — but shaping *every* prefix is O(n) shapings of O(n) work each,
/// on the UI thread inside a click handler, over a string bounded by `PATH_MAX`
/// and not by a filename. Release build, 500 clicks each, one click costing:
///
/// | characters | scanned | searched |
/// | --- | --- | --- |
/// | 26 | 133 µs | 47 µs |
/// | 49 | 460 µs | 87 µs |
/// | 128 | **3.10 ms** | 257 µs |
/// | 252 | **11.96 ms** | 564 µs |
///
/// The two in bold are a click that costs more than the whole 8 ms frame budget,
/// and a path of either length is ordinary.
///
/// Deliberately **not** through the layout cache: each prefix is a distinct
/// string, the cache holds 256 of them and clears wholesale at the cap, so one
/// click used to evict every chrome string in the window and the next frame
/// re-shaped all of them.
///
/// A free function rather than only a [`Painter`] method because the caller that
/// needs it most is an event handler, which has the faces and no frame to draw
/// into.
#[must_use]
pub fn caret_offset(
    fonts: &mut Fonts,
    text: &str,
    style: TextStyle,
    px_size: f32,
    want: f32,
) -> usize {
    if want <= 0.0 || text.is_empty() {
        return 0;
    }
    let advance = |fonts: &mut Fonts, offset: usize| {
        if offset == 0 { 0.0 } else { fonts.shape(&text[..offset], px_size, style.weight).advance }
    };
    // Both ends are always character boundaries, and the midpoint is walked back
    // to one — a caret that lands inside `é` panics on the next slice.
    let mut low = 0usize;
    let mut high = text.len();
    while high - low > 1 {
        let mut mid = usize::midpoint(low, high);
        while mid > low && !text.is_char_boundary(mid) {
            mid -= 1;
        }
        if mid == low {
            break;
        }
        if advance(fonts, mid) <= want {
            low = mid;
        } else {
            high = mid;
        }
    }
    let before = advance(fonts, low);
    let after = advance(fonts, high);
    if (want - before).abs() <= (after - want).abs() { low } else { high }
}

/// How wide the first `bytes` of `text` are, in device pixels.
///
/// **Deliberately not through the layout cache**, for the reason
/// [`caret_offset`] gives: a prefix is a distinct string, the cache holds 256 and
/// clears wholesale at the cap, so drawing a selection would evict every chrome
/// string in the window once per frame.
///
/// The companion of `caret_offset` and its inverse: that one takes a pixel and
/// gives a byte, this takes a byte and gives a pixel. A selection needs both —
/// the pointer speaks pixels and the clipboard speaks bytes.
#[must_use]
pub fn advance_to(
    fonts: &mut Fonts,
    text: &str,
    weight: Weight,
    px_size: f32,
    bytes: usize,
) -> f32 {
    let bytes = bytes.min(text.len());
    if bytes == 0 || !text.is_char_boundary(bytes) {
        return 0.0;
    }
    fonts.shape(&text[..bytes], px_size, weight).advance
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

    /// Device pixels back to logical ones.
    ///
    /// **The inverse of [`Self::px`], for the one direction that needs it.** A
    /// shaped advance comes back in device pixels, because a glyph is measured in
    /// them; everything above the `Painter` places things in logical ones. Crossing
    /// back was a bare division somebody had to remember to write, and a band
    /// heading's rule was drawn without it — correct at 100%, a quarter of the
    /// label's width too far right at 125%, and a whole label's width out at 200%.
    #[must_use]
    pub fn logical(&self, device: f32) -> f32 {
        if self.scale <= 0.0 {
            return device;
        }
        device / self.scale
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

    /// A one-device-pixel vertical rule, for a column boundary.
    ///
    /// The sibling of [`Painter::hairline`] and separate from it because the
    /// snapping goes on the axis the rule is *thin* along: a vertical rule snapped
    /// on y is two columns of half-grey where one column of the divider colour
    /// belongs, which at 125% is the difference between a line and a smudge.
    pub fn vline(&mut self, x: f32, y: f32, height: f32, color: Color) {
        let top = self.snap(y);
        let rect = Rect::new(self.snap(x), top, 1.0, self.snap(y + height) - top);
        self.list.rect(rect, color, 0.0);
    }

    /// A filled triangle inscribed in a logical rectangle.
    ///
    /// For an arrowhead: the one shape a rectangle cannot make, and the
    /// alternative is a font glyph whose presence on somebody else's machine is
    /// somebody else's decision. Measured on this machine's chain, `↻` and `⟳`
    /// are both missing and render as a box.
    pub fn triangle(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        at: Direction,
    ) {
        let rect = self.rect(x, y, width, height);
        self.list.triangle(rect, color, at);
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
                Align::Center => (left + (limit - advance) / 2.0).round(),
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

    /// The caret for a text field, sized by the text rather than by the box.
    ///
    /// **A caret as tall as its box is a caret that looks wrong in every box but
    /// one.** The three fields here are 40, 28 and 22 logical pixels tall around
    /// the same 13-pixel text, so a caret derived from the box was a thin cursor
    /// in one and a scrollbar in another. Ascent plus descent of the face it sits
    /// in is what every text field on the desktop draws, and it is the same
    /// number `baseline` already asks the face for.
    ///
    /// `x` is logical; `baseline` is what [`Painter::baseline`] returned, which is
    /// already in device pixels.
    #[must_use]
    pub fn caret(&mut self, x: f32, baseline: f32, logical_size: f32) -> Rect {
        let px_size = self.px(logical_size);
        let metrics = self.fonts.metrics(Weight::Regular, px_size);
        Rect::new(
            self.snap(x),
            (baseline - metrics.ascent).round(),
            self.snap(1.5).max(1.0),
            (metrics.ascent + metrics.descent).round().max(1.0),
        )
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

    /// The mark for a mounted filesystem: a stack of two platters.
    ///
    /// Two rounded bars rather than a drive-shaped outline, because at fifteen
    /// pixels a drive shape is a rectangle and this is the shape every disk icon
    /// has been since the platters were real.
    pub fn disk(&mut self, box_rect: Rect, color: Color) {
        let unit = box_rect.height / 16.0;
        let height = unit * 4.5;
        for row in 0..2 {
            let bar = Rect::new(
                box_rect.x,
                box_rect.y + unit * 3.0 + row as f32 * (height + unit * 1.5),
                box_rect.width,
                height,
            );
            self.list.rect(bar, color, unit * 2.0);
        }
    }

    /// A thumbnail, fitted inside `box_rect` and centred in it.
    ///
    /// **A key and a rectangle, and no `wgpu` type in sight** — [ADR
    /// 0003](../../../docs/adr/0003-wgpu-first-backend.md). Where the picture
    /// lives is the renderer's, and whether it has arrived yet is the renderer's
    /// too: an image the atlas does not hold draws nothing, which is what a cell
    /// looks like while its thumbnail is still being decoded.
    ///
    /// `tint` carries only its alpha, so a thumbnail can be faded in or dimmed
    /// without a second code path; its colour channels are ignored.
    pub fn image(&mut self, key: sk_gpu::ImageKey, box_rect: Rect, tint: Color) {
        self.list.image(key, box_rect, tint);
    }

    /// A thumbnail standing in for a picture, filling `box_rect` exactly.
    ///
    /// Unlike [`Self::image`], which never enlarges: a stand-in has to land where
    /// the picture it precedes will land, or the panel jumps when it arrives.
    pub fn standby(&mut self, key: sk_gpu::ImageKey, box_rect: Rect, tint: Color) {
        self.list.standby(key, box_rect, tint);
    }

    /// The one preview picture, filling `box_rect` exactly.
    ///
    /// The caller has already fitted it: a preview is rendered at the size it will
    /// be drawn at, so the shape is known before the pixels are asked for.
    pub fn preview(&mut self, key: sk_gpu::ImageKey, box_rect: Rect, tint: Color) {
        self.list.preview(key, box_rect, tint);
    }

    /// A check box: a framed square with a block of `ink` in it when it is on.
    ///
    /// **No tick, because a tick is a diagonal.** The display list has rectangles,
    /// rounded rectangles and a triangle with its apex on one of four edges — see
    /// [`Self::triangle`], which says why a font glyph is not the alternative
    /// either: `↻` and `⟳` are both missing from this machine's chain and render as
    /// a box, and which glyphs somebody else has is somebody else's decision. A
    /// block inside a frame is a state a person reads at fourteen pixels without
    /// one, and it is the shape the panel that asks about a taken name already drew
    /// by hand — this is that shape with one home instead of two.
    ///
    /// **Proportional, not fourteen pixels.** The panel's box is fourteen and the
    /// one over a list entry's mark is sixteen, so the inner block is a fraction of
    /// the frame rather than a constant; at any size the two read as the same
    /// control.
    ///
    /// It draws no ground. Over a panel there is nothing to hide, and over a mark
    /// the caller paints one — which is the caller's business, because what is
    /// behind the box is what the caller put there.
    pub fn checkbox(&mut self, box_rect: Rect, ink: Color, frame: Color, on: bool) {
        let ring = self.px(1.0).round().max(1.0);
        self.list.stroke(box_rect, frame, self.px(3.0), ring);
        if !on {
            return;
        }
        let inset = (box_rect.width * (4.0 / 14.0)).round();
        let block = Rect::new(
            box_rect.x + inset,
            box_rect.y + inset,
            (box_rect.width - inset * 2.0).max(1.0),
            (box_rect.height - inset * 2.0).max(1.0),
        );
        self.list.rect(block, ink, self.px(1.0));
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
/// Listing zero, which `sk_model::Listing` never hands out — its counter starts
/// at one — so this cannot collide with any cell of any real listing.
fn ellipsis_key(px_size: f32) -> RunKey {
    RunKey::cell(0, 0, 0, Weight::Regular, px_size)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{TextStyle, caret_offset, ellipsis_key};
    use sk_gpu::Color;
    use sk_text::{Fonts, RunKey, Weight};

    /// `sk_model::Listing`'s version counter starts at one, so listing zero is
    /// reserved and nothing real can collide with the ellipsis.
    #[test]
    fn the_ellipsis_key_cannot_collide_with_a_row() {
        let ellipsis = ellipsis_key(13.0);
        assert!(sk_model::Listing::default().version() >= 1, "listing zero is reserved");
        assert_ne!(ellipsis, RunKey::cell(1, 0, 0, Weight::Regular, 13.0));
        assert_ne!(ellipsis, RunKey::cell(u32::MAX, 0, 0, Weight::Regular, 13.0));
        assert_eq!(ellipsis, ellipsis_key(13.0), "and it is stable at one size");
        assert_ne!(ellipsis, ellipsis_key(16.25), "and distinct across sizes");
    }

    /// The binary search has to give the answer the scan gave, character for
    /// character — including on a string where a character is two bytes and one
    /// where two characters are one cluster.
    #[test]
    fn a_caret_lands_where_shaping_every_prefix_would_have_put_it() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let style = TextStyle::new(13.0, Color::hex(0xffffff));
        let px_size = 13.0;
        for text in [
            "report.pdf",
            "café-étude.txt",
            "/home/emil/repos/skerry/crates/sk-app/src/view.rs",
            "a",
            "",
        ] {
            // Every character boundary, and a point either side of each.
            let boundaries: Vec<usize> =
                text.char_indices().map(|(at, _)| at).chain(std::iter::once(text.len())).collect();
            let advance = |fonts: &mut Fonts, offset: usize| {
                if offset == 0 {
                    0.0
                } else {
                    fonts.shape(&text[..offset], px_size, style.weight).advance
                }
            };
            for want_at in &boundaries {
                let want = advance(&mut fonts, *want_at);
                // The scan the search replaced, kept here as the oracle.
                let mut best = (0usize, f32::MAX);
                for offset in &boundaries {
                    let distance = (advance(&mut fonts, *offset) - want).abs();
                    if distance < best.1 {
                        best = (*offset, distance);
                    }
                }
                let found = caret_offset(&mut fonts, text, style, px_size, want);
                // The offsets may differ where two of them are the same width —
                // a zero-width joiner, a combining mark — and the *answer* is
                // the position, so that is what is compared.
                let (mine, theirs) = (advance(&mut fonts, found), advance(&mut fonts, best.0));
                assert!(
                    (mine - theirs).abs() < 0.01,
                    "{text:?} at {want}: {found} rather than {}",
                    best.0
                );
                assert!(text.is_char_boundary(found), "{text:?}: {found} is inside a character");
            }
            // Off either end, and never a panic.
            assert_eq!(caret_offset(&mut fonts, text, style, px_size, -10.0), 0);
            assert_eq!(caret_offset(&mut fonts, text, style, px_size, 1e6), text.len());
        }
    }
}
