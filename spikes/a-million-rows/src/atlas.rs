// SPDX-License-Identifier: MIT OR Apache-2.0
//! Glyph atlases and the bounded staging queue.
//!
//! Step 5 of the frame loop in docs/04-architecture.md is explicit that glyph
//! upload is *bounded per frame*, "so a thousand new thumbnails cannot blow the
//! budget". That bound is implemented here and the number of frames that hit it
//! is reported, because a bound nobody counts is a bound nobody believes.
//!
//! Two textures rather than one: an R8 mask for outline glyphs, which is the
//! overwhelming majority and where upload bandwidth matters, and a small RGBA
//! surface for colour glyphs, which a filename containing an emoji needs and
//! which cannot be expressed as coverage.

use std::collections::HashMap;

use swash::scale::image::{Content, Image};
use swash::GlyphId;

pub const MASK_SIDE: u32 = 2048;
pub const COLOR_SIDE: u32 = 1024;

/// Glyphs uploaded per frame. 128 covers a full viewport of newly seen text at
/// a 45-row viewport without letting a cold cache monopolise a frame.
pub const UPLOAD_BUDGET: usize = 128;

/// One padding pixel on each side so nearest sampling at the edge of a glyph
/// cannot pick up its neighbour.
const PAD: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub face: u16,
    pub glyph: GlyphId,
    /// Size in quarter-pixels, so a fractional scale factor is part of the key
    /// rather than rounded away.
    pub quarter_px: u16,
    pub subpixel: u8,
}

impl GlyphKey {
    pub fn px_size(&self) -> f32 {
        f32::from(self.quarter_px) / 4.0
    }
}

/// Where a rasterized glyph landed, in atlas pixels, plus the offset from the
/// pen position to its top-left corner.
#[derive(Clone, Copy)]
pub struct Placed {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub color: bool,
}

/// A shelf allocator: rows of increasing height, each packed left to right.
/// Wrong for long-lived heterogeneous images and right for glyphs, which arrive
/// in a handful of heights and never move.
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
        for row in self.rows.iter_mut() {
            // Only reuse a row within 25% of the requested height; a looser fit
            // wastes atlas area faster than an extra row does.
            if height <= row.height
                && height * 4 >= row.height * 3
                && row.used + width <= self.side
            {
                let x = row.used;
                row.used += width;
                return Some((x + PAD, row.y + PAD));
            }
        }
        let y = self.rows.last().map_or(0, |r| r.y + r.height);
        if y + height > self.side {
            return None;
        }
        self.rows.push(Row { y, height, used: width });
        Some((PAD, y + PAD))
    }

    fn utilization(&self) -> f32 {
        let used: u32 = self.rows.iter().map(|r| r.used * r.height).sum();
        used as f32 / (self.side * self.side) as f32
    }
}

/// A rasterized glyph waiting for its turn on the upload queue.
struct Pending {
    placed: Placed,
    data: Vec<u8>,
}

pub struct Atlas {
    entries: HashMap<GlyphKey, Placed>,
    mask: Shelves,
    color: Shelves,
    pending: Vec<Pending>,
    pub full: bool,
    pub uploads: u64,
    pub bytes_uploaded: u64,
    pub frames_at_budget: u64,
    pub deferred_glyphs: u64,
}

impl Atlas {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            mask: Shelves::new(MASK_SIDE),
            color: Shelves::new(COLOR_SIDE),
            pending: Vec::new(),
            full: false,
            uploads: 0,
            bytes_uploaded: 0,
            frames_at_budget: 0,
            deferred_glyphs: 0,
        }
    }

    pub fn get(&self, key: &GlyphKey) -> Option<Placed> {
        self.entries.get(key).copied()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn utilization(&self) -> (f32, f32) {
        (self.mask.utilization(), self.color.utilization())
    }

    /// Packs a rasterized glyph and queues its pixels. The entry becomes
    /// visible to `get` immediately even though the texels arrive later this
    /// frame, because the row that needed it is being built right now.
    pub fn insert(&mut self, key: GlyphKey, image: &Image) -> Option<Placed> {
        let color = image.content == Content::Color;
        let (width, height) = (image.placement.width, image.placement.height);

        // A space, or any glyph with no coverage, is a real and common case.
        if width == 0 || height == 0 {
            let placed = Placed {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                left: image.placement.left,
                top: image.placement.top,
                color,
            };
            self.entries.insert(key, placed);
            return Some(placed);
        }

        let shelves = if color { &mut self.color } else { &mut self.mask };
        let (x, y) = match shelves.allocate(width, height) {
            Some(position) => position,
            None => {
                // A real implementation grows or evicts here. A spike records
                // that it happened, because if it happens the measurement is
                // no longer measuring the intended thing.
                self.full = true;
                return None;
            }
        };

        let placed = Placed {
            x,
            y,
            width,
            height,
            left: image.placement.left,
            top: image.placement.top,
            color,
        };
        self.entries.insert(key, placed);
        self.pending.push(Pending { placed, data: image.data.clone() });
        Some(placed)
    }

    /// Uploads at most `UPLOAD_BUDGET` glyphs. Anything left over waits for the
    /// next frame; its entry stays in the map so the row does not re-rasterize.
    pub fn flush(&mut self, queue: &wgpu::Queue, mask: &wgpu::Texture, color: &wgpu::Texture) {
        let count = self.pending.len().min(UPLOAD_BUDGET);
        if self.pending.len() > UPLOAD_BUDGET {
            self.frames_at_budget += 1;
            self.deferred_glyphs += (self.pending.len() - UPLOAD_BUDGET) as u64;
        }

        for item in self.pending.drain(..count) {
            let (texture, channels) = if item.placed.color {
                (color, 4u32)
            } else {
                (mask, 1u32)
            };
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
            self.bytes_uploaded += u64::from(item.placed.width * item.placed.height * channels);
        }
    }
}
