//! The immediate-mode pass: scroll offset in, quads out.
//!
//! Step 3 of the frame loop in docs/04-architecture.md — "over *visible* rows
//! only". The whole reason a million rows can be cheap is that this function's
//! cost is a function of the viewport, not of the listing. If any part of it
//! ever becomes O(entries), the spike has measured the wrong program.

use crate::atlas::{Atlas, GlyphKey};
use crate::data::{self, Store};
use crate::render::{Quad, KIND_COLOR, KIND_MASK, KIND_SOLID};
use crate::text::{Fonts, LayoutCache, RunKey, SUBPIXEL_BUCKETS};

/// Logical pixels, multiplied by the scale factor to reach device pixels.
pub struct Metrics {
    pub scale: f32,
    pub row_height: f32,
    pub font_size: f32,
    pub icon: f32,
    pub gutter: f32,
    pub column_size: f32,
    pub column_mtime: f32,
    pub column_kind: f32,
}

impl Metrics {
    pub fn new(scale: f32) -> Self {
        Self {
            scale,
            row_height: 24.0,
            font_size: 13.0,
            icon: 16.0,
            gutter: 8.0,
            column_size: 92.0,
            column_mtime: 132.0,
            column_kind: 104.0,
        }
    }

    fn device(&self, logical: f32) -> f32 {
        logical * self.scale
    }
}

const COLUMN_NAME: u8 = 0;
const COLUMN_SIZE: u8 = 1;
const COLUMN_MTIME: u8 = 2;
const COLUMN_KIND: u8 = 3;

// Enough contrast to judge the golden frames by eye; not a design.
const TEXT: u32 = 0xFF_E8_E8_EA;
const TEXT_DIM: u32 = 0xFF_9A_9A_A4;
const ZEBRA: u32 = 0xFF_20_20_26;
const SELECTION: u32 = 0xFF_2F_5C_9E;
const ICON_DIR: u32 = 0xFF_6E_9E_D8;
const ICON_FILE: u32 = 0xFF_7A_7A_86;

#[derive(Default)]
pub struct PassStats {
    pub rows: u32,
    pub quads: u32,
    pub glyphs: u32,
    /// Glyph quads whose destination rectangle did not equal their atlas source
    /// rectangle. The bar's "crisp glyphs at fractional scale" is this number
    /// being zero; anything else is a scaled glyph.
    pub scaled_glyphs: u32,
    /// Glyphs the atlas could not place because it is full — the measurement is
    /// no longer measuring the intended thing if this is not zero.
    pub dropped_glyphs: u32,
    pub ellipsized: u32,
}

pub struct Pass {
    pub quads: Vec<Quad>,
    pub stats: PassStats,
    selection: Vec<u32>,
}

impl Pass {
    pub fn new() -> Self {
        Self {
            quads: Vec::with_capacity(1 << 16),
            stats: PassStats::default(),
            selection: Vec::new(),
        }
    }

    /// A plausible selection: a contiguous block plus a few strays, so the
    /// selection path is exercised without becoming the workload.
    pub fn set_selection(&mut self, anchor: u32) {
        self.selection.clear();
        self.selection.extend(anchor..anchor + 37);
        self.selection.extend([anchor + 900, anchor + 1301, anchor + 5000]);
    }

    fn is_selected(&self, id: u32) -> bool {
        self.selection.binary_search(&id).is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &mut self,
        store: &Store,
        metrics: &Metrics,
        viewport: (u32, u32),
        scroll_px: f64,
        fonts: &mut Fonts,
        cache: &mut LayoutCache,
        atlas: &mut Atlas,
    ) {
        self.quads.clear();
        self.stats = PassStats::default();

        let row_height = metrics.device(metrics.row_height);
        let width = viewport.0 as f32;

        // The only two lines that touch the entry count at all.
        let first = (scroll_px / row_height as f64).floor().max(0.0) as u32;
        let visible = (viewport.1 as f32 / row_height).ceil() as u32 + 1;
        let last = (first + visible).min(store.len() as u32);

        let px_size = metrics.device(metrics.font_size);
        let quarter_px = (px_size * 4.0).round() as u16;
        let mut column_text = String::with_capacity(32);

        // Column geometry, right-aligned from the right edge inwards.
        let gutter = metrics.device(metrics.gutter);
        let kind_x = width - metrics.device(metrics.column_kind) - gutter;
        let mtime_x = kind_x - metrics.device(metrics.column_mtime);
        let size_x = mtime_x - metrics.device(metrics.column_size);
        let name_x = gutter + metrics.device(metrics.icon) + gutter;
        let name_limit = size_x - name_x - gutter;

        for id in first..last {
            let y = (id as f64 * row_height as f64 - scroll_px) as f32;
            let selected = self.is_selected(id);

            if selected {
                self.push_solid(0.0, y, width, row_height, SELECTION);
            } else if id % 2 == 1 {
                self.push_solid(0.0, y, width, row_height, ZEBRA);
            }

            let flags = store.flags(id);
            let is_dir = flags & data::FLAG_DIR != 0;
            let icon_side = metrics.device(metrics.icon);
            self.push_solid(
                gutter,
                y + (row_height - icon_side) * 0.5,
                icon_side,
                icon_side,
                if is_dir { ICON_DIR } else { ICON_FILE },
            );

            // Text sits on an integer device-pixel baseline. Vertical subpixel
            // positioning buys nothing at these sizes and costs an atlas
            // dimension.
            let baseline = (y + row_height * 0.5 + px_size * 0.34).round();
            let name = store.name(id);
            let colour = if flags & data::FLAG_HIDDEN != 0 { TEXT_DIM } else { TEXT };

            self.emit_run(
                RunKey { entry: id, column: COLUMN_NAME, quarter_px },
                name,
                name_x,
                baseline,
                Some(name_limit),
                colour,
                fonts,
                cache,
                atlas,
            );

            data::format_size(store.size(id), flags, &mut column_text);
            self.emit_run(
                RunKey { entry: id, column: COLUMN_SIZE, quarter_px },
                &column_text,
                size_x,
                baseline,
                None,
                TEXT_DIM,
                fonts,
                cache,
                atlas,
            );

            data::format_mtime(store.mtime(id), &mut column_text);
            self.emit_run(
                RunKey { entry: id, column: COLUMN_MTIME, quarter_px },
                &column_text,
                mtime_x,
                baseline,
                None,
                TEXT_DIM,
                fonts,
                cache,
                atlas,
            );

            self.emit_run(
                RunKey { entry: id, column: COLUMN_KIND, quarter_px },
                data::kind_of(name, flags),
                kind_x,
                baseline,
                None,
                TEXT_DIM,
                fonts,
                cache,
                atlas,
            );

            self.stats.rows += 1;
        }

        self.stats.quads = self.quads.len() as u32;
    }

    fn push_solid(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        self.quads.push(Quad {
            rect: [x, y, w, h],
            // A solid quad samples nothing; the shader branches on kind.
            uv: [0.0, 0.0, 0.0, 0.0],
            color,
            kind: KIND_SOLID,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_run(
        &mut self,
        key: RunKey,
        text: &str,
        origin_x: f32,
        baseline: f32,
        limit: Option<f32>,
        color: u32,
        fonts: &mut Fonts,
        cache: &mut LayoutCache,
        atlas: &mut Atlas,
    ) {
        // Not cloned: the returned borrow is tied to `cache`, which is disjoint
        // from `self`, `fonts` and `atlas`. A clone here would have added an
        // allocation per row per frame to the thing being measured.
        let run = cache.get_or_shape(key, text, fonts);

        // Ellipsis by truncation at the last glyph that fits. Real ellipsis
        // needs a `…` glyph appended; the cost difference is one glyph and the
        // layout question is the same.
        let cut = match limit {
            Some(limit) if run.advance > limit => {
                self.stats.ellipsized += 1;
                limit
            }
            _ => f32::INFINITY,
        };

        for glyph in &run.glyphs {
            if glyph.x > cut {
                break;
            }
            let pen_x = origin_x + glyph.x;
            let whole = pen_x.floor();
            let fraction = pen_x - whole;
            let bucket = (fraction * f32::from(SUBPIXEL_BUCKETS)).floor() as u8
                % SUBPIXEL_BUCKETS;

            let glyph_key = GlyphKey {
                face: glyph.face,
                glyph: glyph.glyph,
                quarter_px: key.quarter_px,
                subpixel: bucket,
            };

            let placed = match atlas.get(&glyph_key) {
                Some(placed) => placed,
                None => match fonts.rasterize(glyph_key, atlas) {
                    Some(placed) => placed,
                    None => {
                        self.stats.dropped_glyphs += 1;
                        continue;
                    }
                },
            };
            if placed.width == 0 || placed.height == 0 {
                continue;
            }

            let rect = [
                whole + placed.left as f32,
                baseline - placed.top as f32 + glyph.y,
                placed.width as f32,
                placed.height as f32,
            ];
            let uv = [
                placed.x as f32,
                placed.y as f32,
                placed.width as f32,
                placed.height as f32,
            ];
            // Crispness, as an assertion rather than an opinion: the glyph is
            // drawn at exactly the size it was rasterized at, or it is counted.
            if rect[2] != uv[2] || rect[3] != uv[3] {
                self.stats.scaled_glyphs += 1;
            }

            self.quads.push(Quad {
                rect,
                uv,
                color,
                kind: if placed.color { KIND_COLOR } else { KIND_MASK },
            });
            self.stats.glyphs += 1;
        }
    }
}
