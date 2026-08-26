//! Glyph atlases and the bounded upload queue.
//!
//! Step 5 of the frame loop in
//! [docs/04-architecture.md](../../../docs/04-architecture.md#the-frame-loop-precisely)
//! is explicit that glyph upload is *bounded per frame*, "so a thousand new
//! thumbnails cannot blow the budget". That bound is here, and the number of
//! frames that hit it is counted, because a bound nobody counts is a bound
//! nobody believes.
//!
//! Two textures rather than one: an R8 coverage mask for outline glyphs, which
//! is nearly all of them and where upload bandwidth matters, and a smaller RGBA
//! surface for colour glyphs, which a filename containing an emoji needs and
//! which cannot be expressed as coverage.

use std::collections::HashMap;

use sk_text::{GlyphImage, GlyphKey};

pub(crate) const MASK_SIDE: u32 = 2048;
pub(crate) const COLOR_SIDE: u32 = 1024;

/// Glyph *bytes* uploaded per frame, and a count cap behind it.
///
/// The bound started as a count of 128 and that was the wrong quantity. A single
/// frame that opens the statistics overlay on a cold cache needs about 350 new
/// glyphs; at 128 a frame, two frames drew the remaining two hundred as blanks —
/// visible in the first capture of that overlay as text with holes in it, since
/// the atlas *entry* exists as soon as the glyph is rasterized and only the
/// texels are late.
///
/// A glyph mask at 13 px is around sixty bytes. 256 KiB is therefore four
/// thousand of them, which is more than any screen holds, while still bounding
/// the case the budget exists for — a colour emoji is 4 KiB and a thousand of
/// those would be 4 MiB in one frame.
const UPLOAD_BUDGET_BYTES: usize = 256 * 1024;
const UPLOAD_BUDGET_GLYPHS: usize = 4096;

/// One padding pixel on each side, so nearest sampling at the edge of a glyph
/// cannot pick up its neighbour.
const PAD: u32 = 1;

/// Where a rasterized glyph landed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placed {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub color: bool,
}

/// A shelf allocator: rows of increasing height, each packed left to right.
///
/// Wrong for long-lived heterogeneous images, right for glyphs, which arrive in
/// a handful of heights and never move.
struct Shelves {
    side: u32,
    rows: Vec<Row>,
}

struct Row {
    y: u32,
    height: u32,
    used: u32,
}

impl Shelves {
    fn new(side: u32) -> Self {
        Self { side, rows: Vec::new() }
    }

    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        let (width, height) = (width + PAD * 2, height + PAD * 2);
        if width > self.side {
            return None;
        }
        for row in &mut self.rows {
            // Only reuse a row within 25% of the requested height: a looser fit
            // wastes atlas area faster than an extra row does.
            if height <= row.height && height * 4 >= row.height * 3 && row.used + width <= self.side
            {
                let x = row.used;
                row.used += width;
                return Some((x + PAD, row.y + PAD));
            }
        }
        let y = self.rows.last().map_or(0, |row| row.y + row.height);
        if y + height > self.side {
            return None;
        }
        self.rows.push(Row { y, height, used: width });
        Some((PAD, y + PAD))
    }

    fn utilization(&self) -> f32 {
        let used: u32 = self.rows.iter().map(|row| row.used * row.height).sum();
        used as f32 / (self.side * self.side) as f32
    }
}

struct Pending {
    placed: Placed,
    data: Vec<u8>,
}

/// Glyph placements, and the pixels not yet uploaded.
pub(crate) struct Atlas {
    entries: HashMap<GlyphKey, Placed>,
    mask: Shelves,
    color: Shelves,
    pending: Vec<Pending>,
    /// Set when a glyph could not be placed. A real full atlas needs eviction;
    /// until one is measured as necessary, this is recorded and reported rather
    /// than silently dropping text.
    pub full: bool,
    pub uploads: u64,
    pub frames_at_budget: u64,
    pub deferred: u64,
}

impl Atlas {
    pub fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(4096),
            mask: Shelves::new(MASK_SIDE),
            color: Shelves::new(COLOR_SIDE),
            pending: Vec::new(),
            full: false,
            uploads: 0,
            frames_at_budget: 0,
            deferred: 0,
        }
    }

    pub fn get(&self, key: GlyphKey) -> Option<Placed> {
        self.entries.get(&key).copied()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn utilization(&self) -> (f32, f32) {
        (self.mask.utilization(), self.color.utilization())
    }

    /// Packs a rasterized glyph and queues its pixels.
    ///
    /// The entry becomes visible to [`Atlas::get`] immediately even though the
    /// texels may arrive a frame later, because the row that needed it is being
    /// built right now — and a glyph drawn from a not-yet-uploaded slot is one
    /// frame of a blank rather than a re-rasterization every frame.
    pub fn insert(&mut self, key: GlyphKey, image: &GlyphImage) -> Option<Placed> {
        // A space, or any glyph with no coverage, is a real and common case.
        if image.width == 0 || image.height == 0 {
            let placed = Placed {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                left: image.left,
                top: image.top,
                color: image.color,
            };
            self.entries.insert(key, placed);
            return Some(placed);
        }

        let shelves = if image.color { &mut self.color } else { &mut self.mask };
        let Some((x, y)) = shelves.allocate(image.width, image.height) else {
            self.full = true;
            return None;
        };

        let placed = Placed {
            x,
            y,
            width: image.width,
            height: image.height,
            left: image.left,
            top: image.top,
            color: image.color,
        };
        self.entries.insert(key, placed);
        self.pending.push(Pending { placed, data: image.data.clone() });
        Some(placed)
    }

    /// Glyphs whose texels have not been uploaded yet.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Uploads what fits in this frame's budget; the rest wait for the next one.
    pub fn flush(&mut self, queue: &wgpu::Queue, mask: &wgpu::Texture, color: &wgpu::Texture) {
        let mut bytes = 0usize;
        let mut count = 0usize;
        for item in &self.pending {
            let next = bytes + item.data.len();
            if count > 0 && (next > UPLOAD_BUDGET_BYTES || count >= UPLOAD_BUDGET_GLYPHS) {
                break;
            }
            bytes = next;
            count += 1;
        }
        if count < self.pending.len() {
            self.frames_at_budget += 1;
            self.deferred += (self.pending.len() - count) as u64;
        }

        for item in self.pending.drain(..count) {
            let (texture, channels) = if item.placed.color { (color, 4) } else { (mask, 1) };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: item.placed.x, y: item.placed.y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &item.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(item.placed.width * channels),
                    rows_per_image: Some(item.placed.height),
                },
                wgpu::Extent3d {
                    width: item.placed.width,
                    height: item.placed.height,
                    depth_or_array_layers: 1,
                },
            );
            self.uploads += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Atlas, MASK_SIDE, PAD, Shelves};
    use sk_text::{GlyphImage, GlyphKey};

    fn image(width: u32, height: u32) -> GlyphImage {
        GlyphImage {
            width,
            height,
            left: 0,
            top: 0,
            color: false,
            data: vec![0xff; (width * height) as usize],
        }
    }

    fn key(glyph: u16) -> GlyphKey {
        GlyphKey { face: 0, glyph, quarter_px: 52, subpixel: 0 }
    }

    #[test]
    fn every_placement_leaves_a_pad_pixel_around_it() {
        let mut shelves = Shelves::new(64);
        let (x, y) = shelves.allocate(10, 10).unwrap();
        assert_eq!((x, y), (PAD, PAD));
        let (x2, _) = shelves.allocate(10, 10).unwrap();
        assert!(x2 >= x + 10 + PAD, "{x2} overlaps the glyph at {x}");
    }

    #[test]
    fn a_glyph_wider_than_the_atlas_is_refused_rather_than_wrapped() {
        let mut shelves = Shelves::new(64);
        assert!(shelves.allocate(200, 10).is_none());
    }

    #[test]
    fn a_zero_sized_glyph_is_a_placement_and_not_a_failure() {
        let mut atlas = Atlas::new();
        let placed = atlas.insert(key(3), &image(0, 0)).expect("a space is placeable");
        assert_eq!((placed.width, placed.height), (0, 0));
        assert!(atlas.get(key(3)).is_some());
        assert!(!atlas.full);
    }

    #[test]
    fn a_full_atlas_is_recorded_rather_than_dropping_text_silently() {
        let mut atlas = Atlas::new();
        let tall = MASK_SIDE / 4;
        for glyph in 0..12u16 {
            atlas.insert(key(glyph), &image(tall, tall));
        }
        assert!(atlas.full, "twelve quarter-atlas glyphs cannot fit and it must say so");
    }

    #[test]
    fn a_full_screen_of_new_text_fits_in_one_frames_upload_budget() {
        let mut atlas = Atlas::new();
        // Forty rows of three columns of ten characters, all new.
        for glyph in 0..1200u16 {
            atlas.insert(key(glyph % 512), &image(7, 9));
        }
        assert!(
            atlas.pending() * 63 < super::UPLOAD_BUDGET_BYTES,
            "{} pending glyphs would not fit",
            atlas.pending()
        );
    }

    #[test]
    fn a_second_insert_of_the_same_key_is_found_by_get() {
        let mut atlas = Atlas::new();
        let first = atlas.insert(key(7), &image(8, 12)).unwrap();
        let found = atlas.get(key(7)).unwrap();
        assert_eq!((first.x, first.y), (found.x, found.y));
        assert_eq!(atlas.len(), 1);
    }
}
