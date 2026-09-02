//! What a frame is: a list of quads, grouped by clip region.
//!
//! `sk-ui` fills this and `sk-gpu` submits it. Nothing in here knows what a row
//! or a column is, and nothing in here knows what a GPU is.

use sk_text::GlyphKey;

/// A rectangle in device pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Rect {
    /// A rectangle from its corner and size.
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    /// Right edge.
    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.width
    }

    /// Bottom edge.
    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.height
    }

    /// Whether `(x, y)` is inside, with the left and top edges included.
    #[must_use]
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// This rectangle inset by `amount` on every side, never below zero size.
    #[must_use]
    pub fn inset(self, amount: f32) -> Self {
        Self {
            x: self.x + amount,
            y: self.y + amount,
            width: (self.width - amount * 2.0).max(0.0),
            height: (self.height - amount * 2.0).max(0.0),
        }
    }

    /// The overlap of two rectangles, possibly zero-sized.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Self {
            x,
            y,
            width: (self.right().min(other.right()) - x).max(0.0),
            height: (self.bottom().min(other.bottom()) - y).max(0.0),
        }
    }

    /// Whether this rectangle has no area.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// A colour, as non-premultiplied sRGB with an alpha channel.
///
/// The surface format is deliberately not sRGB, so the value written here is the
/// value the compositor receives. Blending happens in that space, which is what
/// every other toolkit does and therefore what a theme's colours were picked
/// against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color(u32);

impl Color {
    /// From 8-bit components.
    #[must_use]
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self((alpha as u32) << 24 | (blue as u32) << 16 | (green as u32) << 8 | (red as u32))
    }

    /// From a `0xRRGGBB` literal, fully opaque.
    #[must_use]
    pub const fn hex(value: u32) -> Self {
        Self::rgba((value >> 16) as u8, (value >> 8) as u8, value as u8, 255)
    }

    /// The same colour at `alpha` of its opacity.
    #[must_use]
    pub const fn with_alpha(self, alpha: u8) -> Self {
        Self(self.0 & 0x00ff_ffff | (alpha as u32) << 24)
    }

    /// Linear interpolation towards `other`, in the same space as blending.
    #[must_use]
    pub fn mix(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let lerp = |a: u32, b: u32| {
            (a as f32).mul_add(1.0 - amount, b as f32 * amount).round().clamp(0.0, 255.0) as u8
        };
        Self::rgba(
            lerp(self.0 & 0xff, other.0 & 0xff),
            lerp(self.0 >> 8 & 0xff, other.0 >> 8 & 0xff),
            lerp(self.0 >> 16 & 0xff, other.0 >> 16 & 0xff),
            lerp(self.0 >> 24, other.0 >> 24),
        )
    }

    /// Fully transparent.
    pub const NONE: Self = Self(0);

    /// The four 8-bit components, red first.
    ///
    /// Exists so a contrast assertion can be a test rather than a judgement —
    /// see `sk_ui::theme`.
    #[must_use]
    pub const fn components(self) -> (u8, u8, u8, u8) {
        (self.0 as u8, (self.0 >> 8) as u8, (self.0 >> 16) as u8, (self.0 >> 24) as u8)
    }

    pub(crate) const fn packed(self) -> u32 {
        self.0
    }

    /// Whether this colour would draw nothing.
    #[must_use]
    pub const fn is_invisible(self) -> bool {
        self.0 >> 24 == 0
    }
}

pub(crate) const KIND_RECT: u32 = 0;
pub(crate) const KIND_MASK: u32 = 1;
pub(crate) const KIND_COLOR: u32 = 2;
/// The four triangles, in [`Direction`]'s order. One kind each rather than a
/// direction packed into a field, because a field that means something different
/// for one kind is a field nobody can read.
pub(crate) const KIND_TRIANGLE: u32 = 3;

/// Which way a triangle points.
///
/// The one shape a rectangle cannot make and the interface needs: an arrowhead.
/// Back, forward and parent are arrows, a reload mark is an arc with an arrowhead
/// on it, and a tree's disclosure is a chevron — four marks that would otherwise
/// be four font glyphs whose presence is somebody else's decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// Apex at the top edge.
    Up,
    /// Apex at the bottom edge.
    Down,
    /// Apex at the left edge.
    Left,
    /// Apex at the right edge.
    Right,
}

impl Direction {
    const fn kind(self) -> u32 {
        KIND_TRIANGLE
            + match self {
                Self::Up => 0,
                Self::Down => 1,
                Self::Left => 2,
                Self::Right => 3,
            }
    }
}

/// One instance. 48 bytes, `repr(C)`, no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quad {
    /// Destination in device pixels: x, y, width, height.
    pub rect: [f32; 4],
    /// Source in atlas pixels: x, y, width, height.
    pub uv: [f32; 4],
    pub color: u32,
    pub kind: u32,
    /// Corner radius in device pixels. Zero is a plain rectangle.
    pub radius: f32,
    /// Border thickness in device pixels. Zero fills.
    pub border: f32,
}

/// One thing to draw, in the order the caller wrote it.
///
/// A glyph carries its key rather than an atlas rectangle because rasterization
/// happens inside the renderer, after the list is built. Keeping both in one
/// array is what makes draw order the call order, with no sorting pass.
pub(crate) enum Item {
    Quad(Quad),
    Glyph {
        key: GlyphKey,
        x: f32,
        y: f32,
        color: Color,
    },
    /// A thumbnail. Where it landed in the atlas is the renderer's to know, the
    /// same way a glyph's placement is — this records the intent and the box.
    Image {
        key: crate::images::ImageKey,
        rect: Rect,
        alpha: Color,
    },
    /// The one preview, drawn at a rectangle the caller has already fitted.
    Preview {
        key: crate::images::ImageKey,
        rect: Rect,
        alpha: Color,
    },
    /// A thumbnail standing in for a picture that is still being fetched.
    ///
    /// **The same texture as [`Self::Image`] and the opposite scaling rule.** An
    /// image in a row is never enlarged, because a 16-pixel icon blown up to a
    /// cell is a worse icon; a thumbnail standing in for a preview is *always*
    /// enlarged, because it has to land where the picture it precedes will be.
    Standby {
        key: crate::images::ImageKey,
        rect: Rect,
        alpha: Color,
    },
}

/// A run of items sharing one scissor rectangle.
pub(crate) struct Group {
    pub clip: Rect,
    pub end: usize,
}

/// A frame's worth of drawing.
///
/// Clip regions are a scissor rectangle rather than a per-quad test: a frame has
/// two or three of them — the header, the list, the status bar — so the cost is
/// two or three draw calls and the quads stay 48 bytes.
#[derive(Default)]
pub struct DrawList {
    pub(crate) items: Vec<Item>,
    pub(crate) groups: Vec<Group>,
    clips: Vec<Rect>,
    bounds: Rect,
}

impl DrawList {
    /// Clears the list for a new frame of `physical` device pixels.
    pub fn begin(&mut self, physical: (u32, u32)) {
        self.items.clear();
        self.groups.clear();
        self.clips.clear();
        self.bounds = Rect::new(0.0, 0.0, physical.0 as f32, physical.1 as f32);
    }

    /// How many items the frame holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the frame would draw nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Every glyph the frame will draw, in order, with its pen position.
    ///
    /// The hook a golden-frame test and a staleness test use: a frame's text can
    /// be compared without a GPU and without a display server. It is what makes
    /// "a warm cache draws the same frame as a cold one" an assertion rather than
    /// a hope.
    pub fn glyphs(&self) -> impl Iterator<Item = (GlyphKey, f32, f32)> + '_ {
        self.items.iter().filter_map(|item| match item {
            Item::Glyph { key, x, y, .. } => Some((*key, *x, *y)),
            Item::Quad(_) | Item::Image { .. } | Item::Preview { .. } | Item::Standby { .. } => {
                None
            }
        })
    }

    /// Starts a clip region, intersected with whatever is already in force.
    pub fn push_clip(&mut self, rect: Rect) {
        self.close_group();
        let clip = self.clip().intersect(rect);
        self.clips.push(clip);
    }

    /// Ends the innermost clip region.
    pub fn pop_clip(&mut self) {
        self.close_group();
        self.clips.pop();
    }

    /// The clip currently in force.
    #[must_use]
    pub fn clip(&self) -> Rect {
        self.clips.last().copied().unwrap_or(self.bounds)
    }

    /// A filled rectangle, optionally with rounded corners.
    pub fn rect(&mut self, rect: Rect, color: Color, radius: f32) {
        if color.is_invisible() || rect.is_empty() {
            return;
        }
        self.items.push(Item::Quad(Quad {
            rect: [rect.x, rect.y, rect.width, rect.height],
            uv: [0.0; 4],
            color: color.packed(),
            kind: KIND_RECT,
            radius,
            border: 0.0,
        }));
    }

    /// An outlined rectangle. The border is drawn inside `rect`, so a focus ring
    /// on a row cannot overlap the row above it.
    pub fn stroke(&mut self, rect: Rect, color: Color, radius: f32, thickness: f32) {
        if color.is_invisible() || rect.is_empty() || thickness <= 0.0 {
            return;
        }
        self.items.push(Item::Quad(Quad {
            rect: [rect.x, rect.y, rect.width, rect.height],
            uv: [0.0; 4],
            color: color.packed(),
            kind: KIND_RECT,
            radius,
            border: thickness,
        }));
    }

    /// A filled triangle inscribed in `rect`, pointing `direction`.
    ///
    /// Antialiased analytically like the rounded rectangles, because an
    /// arrowhead nine pixels tall with staircase edges is the difference between
    /// a toolbar that looks drawn and one that looks rendered.
    pub fn triangle(&mut self, rect: Rect, color: Color, direction: Direction) {
        if color.is_invisible() || rect.is_empty() {
            return;
        }
        self.items.push(Item::Quad(Quad {
            rect: [rect.x, rect.y, rect.width, rect.height],
            uv: [0.0; 4],
            color: color.packed(),
            kind: direction.kind(),
            radius: 0.0,
            border: 0.0,
        }));
    }

    /// One glyph, at the pen position `x` and baseline `y` in device pixels.
    ///
    /// The atlas placement is resolved by the renderer, which is the only thing
    /// that knows where a glyph landed. This records the intent.
    pub fn glyph(&mut self, key: GlyphKey, x: f32, y: f32, color: Color) {
        if color.is_invisible() {
            return;
        }
        self.items.push(Item::Glyph { key, x, y, color });
    }

    /// One thumbnail, fitted inside `rect`.
    ///
    /// **The rectangle is where it may go, not where it goes.** The image keeps
    /// its aspect ratio inside the box and is centred in it, because a photograph
    /// stretched to a square is worse than a photograph with space beside it —
    /// and the renderer is the only thing that knows the image's shape.
    ///
    /// An image the atlas does not hold draws nothing, which is what a cell looks
    /// like while its thumbnail is still being decoded.
    pub fn image(&mut self, key: crate::images::ImageKey, rect: Rect, alpha: Color) {
        if rect.is_empty() || alpha.is_invisible() {
            return;
        }
        self.items.push(Item::Image { key, rect, alpha });
    }

    /// The one preview, filling `rect` exactly.
    ///
    /// **Not fitted here, unlike [`Self::image`].** A preview is rendered at the
    /// size it will be drawn at, so the caller already knows the shape and has
    /// already placed it; fitting it again would be the renderer second-guessing a
    /// decision made with better information.
    pub fn preview(&mut self, key: crate::images::ImageKey, rect: Rect, alpha: Color) {
        if rect.is_empty() || alpha.is_invisible() {
            return;
        }
        self.items.push(Item::Preview { key, rect, alpha });
    }

    /// A thumbnail filling `rect`, standing in for the picture that will replace it.
    pub fn standby(&mut self, key: crate::images::ImageKey, rect: Rect, alpha: Color) {
        if rect.is_empty() || alpha.is_invisible() {
            return;
        }
        self.items.push(Item::Standby { key, rect, alpha });
    }

    /// Which preview this frame draws, if it draws one.
    ///
    /// **The first, and a frame draws at most one.** A panel shows one page, so
    /// the renderer binds this and rebuilds the bind group only when it changes;
    /// a frame naming two would draw whichever got bound, which the assertion
    /// below is there to catch rather than to tolerate.
    #[must_use]
    pub fn first_preview(&self) -> Option<crate::images::ImageKey> {
        let mut found: Option<crate::images::ImageKey> = None;
        for item in &self.items {
            if let Item::Preview { key, .. } = item {
                debug_assert!(
                    found.is_none_or(|first| first == *key),
                    "a frame named two different previews, and one of them would not draw"
                );
                found = Some(*key);
            }
        }
        found
    }

    /// Closes the last group. Called by the renderer before submitting, so a
    /// caller cannot forget it and lose the final group's quads.
    pub fn finish(&mut self) {
        self.close_group();
    }

    fn close_group(&mut self) {
        let clip = self.clip();
        if self.groups.last().is_some_and(|last| last.end == self.items.len()) {
            return;
        }
        self.groups.push(Group { clip, end: self.items.len() });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Color, DrawList, Rect};

    #[test]
    fn a_rect_knows_its_edges_and_its_overlap() {
        let rect = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert!((rect.right() - 110.0).abs() < f32::EPSILON);
        assert!((rect.bottom() - 70.0).abs() < f32::EPSILON);
        assert!(rect.contains(10.0, 20.0), "the top-left corner is inside");
        assert!(!rect.contains(110.0, 20.0), "the right edge is not");
        let clipped = rect.intersect(Rect::new(0.0, 0.0, 50.0, 30.0));
        assert_eq!(clipped, Rect::new(10.0, 20.0, 40.0, 10.0));
    }

    #[test]
    fn two_rectangles_that_miss_intersect_to_nothing_rather_than_to_a_negative() {
        let overlap = Rect::new(0.0, 0.0, 10.0, 10.0).intersect(Rect::new(50.0, 50.0, 10.0, 10.0));
        assert!(overlap.is_empty());
        assert!(overlap.width >= 0.0 && overlap.height >= 0.0);
    }

    #[test]
    fn a_colour_round_trips_through_hex_and_mixes_towards_the_target() {
        assert_eq!(Color::hex(0x1122_33), Color::rgba(0x11, 0x22, 0x33, 0xff));
        let black = Color::hex(0x0000_00);
        let white = Color::hex(0xffff_ff);
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
        assert_eq!(black.mix(white, 0.5), Color::rgba(128, 128, 128, 255));
    }

    #[test]
    fn an_invisible_colour_emits_no_quad() {
        let mut list = DrawList::default();
        list.begin((100, 100));
        list.rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::NONE, 0.0);
        list.rect(Rect::new(0.0, 0.0, 0.0, 10.0), Color::hex(0xff_ff_ff), 0.0);
        assert!(list.is_empty(), "a transparent colour and a zero width both draw nothing");
    }

    #[test]
    fn nested_clips_intersect_and_unwind() {
        let mut list = DrawList::default();
        list.begin((200, 200));
        assert_eq!(list.clip(), Rect::new(0.0, 0.0, 200.0, 200.0));
        list.push_clip(Rect::new(0.0, 50.0, 200.0, 100.0));
        list.push_clip(Rect::new(20.0, 0.0, 60.0, 200.0));
        assert_eq!(list.clip(), Rect::new(20.0, 50.0, 60.0, 100.0));
        list.pop_clip();
        assert_eq!(list.clip(), Rect::new(0.0, 50.0, 200.0, 100.0));
        list.pop_clip();
        assert_eq!(list.clip(), Rect::new(0.0, 0.0, 200.0, 200.0));
    }

    #[test]
    fn each_clip_change_becomes_one_group_and_empty_ones_are_not_recorded() {
        let mut list = DrawList::default();
        list.begin((200, 200));
        let white = Color::hex(0xff_ff_ff);
        list.rect(Rect::new(0.0, 0.0, 10.0, 10.0), white, 0.0);
        list.push_clip(Rect::new(0.0, 0.0, 50.0, 50.0));
        list.rect(Rect::new(0.0, 0.0, 10.0, 10.0), white, 0.0);
        list.pop_clip();
        list.finish();
        assert_eq!(list.groups.len(), 2, "one group per clip that had items in it");
        assert_eq!(list.groups[0].end, 1);
        assert_eq!(list.groups[1].end, 2);
    }
}
