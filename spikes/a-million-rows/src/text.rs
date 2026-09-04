// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shaping, the fallback chain and the layout cache — the `sk-text` half of
//! ADR 0004, at spike fidelity.
//!
//! The two things being measured here are the cost of shaping a row's worth of
//! text on the frame it becomes visible, and whether a cache keyed by
//! `EntryId` makes that cost disappear on the frames where the row is already
//! on screen.

use std::collections::HashMap;

use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::{Direction, ShapeContext};
use swash::text::cluster::{CharCluster, Parser, Status, Token};
use swash::text::{Codepoint, Script};
use swash::{FontRef, GlyphId};

use crate::atlas::{Atlas, GlyphKey, Placed};

/// Horizontal subpixel buckets. Four is the point where the next bucket stops
/// being distinguishable at text sizes and starts being atlas memory.
pub const SUBPIXEL_BUCKETS: u8 = 4;

/// A font we have mapped into memory. `swash` borrows from the bytes, so the
/// bytes outlive every `FontRef` derived from them.
struct Face {
    data: Vec<u8>,
    offset: u32,
    key: swash::CacheKey,
}

impl Face {
    fn open(path: &str, index: usize) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        let font = swash::FontRef::from_index(&data, index)?;
        let (offset, key) = (font.offset, font.key);
        Some(Self { data, offset, key })
    }

    fn as_ref(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

/// The fallback chain, in priority order. ADR 0004 says font discovery is ours;
/// a spike hardcodes the paths rather than parsing fontconfig, because the cost
/// under test is shaping and rasterization, not discovery.
pub struct Fonts {
    faces: Vec<Face>,
    shape: ShapeContext,
    scale: ScaleContext,
    scratch: Scratch,
}

const CHAIN: &[(&str, usize)] = &[
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansHebrew-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf", 0),
];

impl Fonts {
    pub fn open() -> Result<Self, String> {
        let mut faces = Vec::new();
        let mut missing = Vec::new();
        for (path, index) in CHAIN {
            match Face::open(path, *index) {
                Some(face) => faces.push(face),
                None => missing.push(*path),
            }
        }
        if faces.is_empty() {
            return Err("no usable font in the fallback chain".into());
        }
        if !missing.is_empty() {
            // Not fatal, but it changes what the measurement covers, so it is
            // reported rather than swallowed.
            eprintln!("note: fallback chain is short — missing {}", missing.join(", "));
        }
        Ok(Self {
            faces,
            shape: ShapeContext::new(),
            scale: ScaleContext::new(),
            scratch: Scratch::default(),
        })
    }

    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Rasterizes one glyph at exactly the device size and subpixel offset the
    /// caller will draw it at. Crispness is a property of this function: nothing
    /// downstream is allowed to scale what comes out of it.
    pub fn rasterize(&mut self, key: GlyphKey, atlas: &mut Atlas) -> Option<Placed> {
        // Split the borrow: `faces` is read while `scale` is mutated, and they
        // are disjoint fields of the same struct.
        let Self { faces, scale, .. } = self;
        let face = faces.get(key.face as usize)?;
        let offset_x = f32::from(key.subpixel) / f32::from(SUBPIXEL_BUCKETS);
        let mut scaler = scale
            .builder(face.as_ref())
            .size(key.px_size())
            .hint(true)
            .build();

        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .offset(swash::zeno::Vector::new(offset_x, 0.0))
        .render(&mut scaler, key.glyph)?;

        atlas.insert(key, &image)
    }
}

/// One shaped glyph, positioned relative to the run origin in device pixels.
#[derive(Clone, Copy)]
pub struct ShapedGlyph {
    pub face: u16,
    pub glyph: GlyphId,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    pub advance: f32,
}

/// Identifies a shaped run by what it is *for* rather than by its text, so a
/// lookup is a hash of eight bytes instead of a hash of a filename.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunKey {
    pub entry: u32,
    pub column: u8,
    /// Quarter-pixels, so a fractional scale factor is part of the key.
    pub quarter_px: u16,
}

/// Two-generation cache. A lookup checks `live`, then `stale` and promotes;
/// when `live` outgrows the cap it becomes `stale` wholesale and a fresh `live`
/// starts. Eviction is therefore O(1) amortized with no sweep, which matters
/// because a sweep is a frame spike and a frame spike is what the bar measures.
pub struct LayoutCache {
    live: HashMap<RunKey, ShapedRun>,
    stale: HashMap<RunKey, ShapedRun>,
    cap: usize,
    pub hits: u64,
    pub misses: u64,
    pub generations: u64,
}

impl LayoutCache {
    pub fn new(cap: usize) -> Self {
        Self {
            live: HashMap::with_capacity(cap * 2),
            stale: HashMap::with_capacity(cap * 2),
            cap,
            hits: 0,
            misses: 0,
            generations: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.live.len() + self.stale.len()
    }

    /// Drops everything. Stands in for a listing arriving and invalidating the
    /// snapshot the rows were shaped from — the `arrival` phase of the trace.
    pub fn invalidate(&mut self) {
        self.live.clear();
        self.stale.clear();
    }

    pub fn get_or_shape(
        &mut self,
        key: RunKey,
        text: &str,
        fonts: &mut Fonts,
    ) -> &ShapedRun {
        if self.live.contains_key(&key) {
            self.hits += 1;
        } else if let Some(run) = self.stale.remove(&key) {
            self.hits += 1;
            self.live.insert(key, run);
        } else {
            self.misses += 1;
            let run = shape(fonts, text, f32::from(key.quarter_px) / 4.0);
            if self.live.len() >= self.cap {
                self.generations += 1;
                std::mem::swap(&mut self.live, &mut self.stale);
                self.live.clear();
            }
            self.live.insert(key, run);
        }
        &self.live[&key]
    }
}

/// Shapes one line, walking the fallback chain per cluster.
///
/// Segmentation is per run rather than per paragraph: filenames are short, and a
/// full Unicode bidi pass over a thirty-character string would cost more than it
/// buys. What `segment` does keep from UAX #9 is documented there.
pub fn shape(fonts: &mut Fonts, text: &str, px_size: f32) -> ShapedRun {
    let Fonts { faces, shape: shaper_cx, scratch, .. } = fonts;
    segment(text, scratch);

    let mut glyphs = Vec::with_capacity(text.len());
    let mut pen = 0.0f32;

    for run_index in 0..scratch.runs.len() {
        let (start, end, script, rtl) = {
            let run = &scratch.runs[run_index];
            (run.start, run.end, run.script, run.rtl)
        };
        let segment_text = &text[start..end];

        // Glyphs are collected per cluster before being positioned, because an
        // RTL run needs its *clusters* reversed while the glyphs inside each
        // cluster keep their shaped order. Deriving advances back out of
        // already-positioned glyphs loses the last one and invents a first.
        scratch.glyphs.clear();
        scratch.cluster_starts.clear();

        let mut cluster = CharCluster::new();
        let mut parser = Parser::new(
            script,
            segment_text.char_indices().map(|(offset, ch)| Token {
                ch,
                offset: offset as u32,
                len: ch.len_utf8() as u8,
                info: ch.properties().into(),
                data: 0,
            }),
        );

        // Cluster-level fallback: the first face that maps the whole cluster
        // wins; a partial map is remembered in case nothing better appears.
        while parser.next(&mut cluster) {
            let mut chosen = 0usize;
            for (index, face) in faces.iter().enumerate() {
                let charmap = face.as_ref().charmap();
                match cluster.map(|ch| charmap.map(ch)) {
                    Status::Complete => {
                        chosen = index;
                        break;
                    }
                    Status::Keep => chosen = index,
                    Status::Discard => {}
                }
            }

            let face = faces[chosen].as_ref();
            let mut shaper = shaper_cx
                .builder(face)
                .script(script)
                .direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight })
                .size(px_size)
                .build();
            shaper.add_cluster(&cluster);

            scratch.cluster_starts.push(scratch.glyphs.len());
            let sink = &mut scratch.glyphs;
            shaper.shape_with(|glyph_cluster| {
                for glyph in glyph_cluster.glyphs {
                    sink.push((chosen as u16, glyph.id, glyph.advance, glyph.x, glyph.y));
                }
            });
        }

        let clusters = scratch.cluster_starts.len();
        for position in 0..clusters {
            let index = if rtl { clusters - 1 - position } else { position };
            let from = scratch.cluster_starts[index];
            let to = scratch
                .cluster_starts
                .get(index + 1)
                .copied()
                .unwrap_or(scratch.glyphs.len());
            for &(face, id, advance, x_offset, y_offset) in &scratch.glyphs[from..to] {
                glyphs.push(ShapedGlyph { face, glyph: id, x: pen + x_offset, y: y_offset });
                pen += advance;
            }
        }
    }

    ShapedRun { advance: pen, glyphs }
}

/// Reusable buffers for run segmentation. Shaping happens on a cache miss,
/// which during a fling is every visible row — allocating four vectors per row
/// per frame would have shown up in the number being measured.
#[derive(Default)]
struct Scratch {
    /// Per character: byte offset, script, and directional class.
    chars: Vec<(u32, Script, Class)>,
    prev_strong: Vec<i32>,
    next_strong: Vec<i32>,
    runs: Vec<Run>,
    glyphs: Vec<(u16, GlyphId, f32, f32, f32)>,
    cluster_starts: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Common, Inherited or Unknown: spaces, digits, punctuation, combining
    /// marks. Takes its direction from its neighbours.
    Neutral,
    LeftToRight,
    RightToLeft,
}

struct Run {
    start: usize,
    end: usize,
    script: Script,
    rtl: bool,
}

fn class_of(script: Script) -> Class {
    match script {
        Script::Common | Script::Inherited | Script::Unknown => Class::Neutral,
        Script::Arabic | Script::Hebrew => Class::RightToLeft,
        _ => Class::LeftToRight,
    }
}

/// Splits a string into runs that each have one script and one direction.
///
/// Not UAX #9. The one rule taken from it is the one whose absence is visible in
/// a file list: a neutral character is right-to-left only when the nearest
/// strong character on *both* sides is, so the `.webp` in `קובץ.webp` resolves
/// to the base direction and stays where a reader expects it, instead of being
/// swept into the Hebrew run and reordered to the far left.
fn segment(text: &str, scratch: &mut Scratch) {
    let Scratch { chars, prev_strong, next_strong, runs, .. } = scratch;
    chars.clear();
    runs.clear();
    for (offset, ch) in text.char_indices() {
        let script = ch.script();
        chars.push((offset as u32, script, class_of(script)));
    }

    prev_strong.clear();
    let mut last = -1i32;
    for (index, (_, _, class)) in chars.iter().enumerate() {
        prev_strong.push(last);
        if *class != Class::Neutral {
            last = index as i32;
        }
    }

    next_strong.clear();
    next_strong.resize(chars.len(), -1);
    let mut upcoming = -1i32;
    for index in (0..chars.len()).rev() {
        next_strong[index] = upcoming;
        if chars[index].2 != Class::Neutral {
            upcoming = index as i32;
        }
    }

    for index in 0..chars.len() {
        let (offset, script, class) = chars[index];
        let end = chars
            .get(index + 1)
            .map_or(text.len(), |next| next.0 as usize);

        let (script, rtl) = match class {
            Class::RightToLeft => (script, true),
            Class::LeftToRight => (script, false),
            Class::Neutral => {
                let before = prev_strong[index].try_into().ok().map(|i: usize| chars[i]);
                let after = next_strong[index].try_into().ok().map(|i: usize| chars[i]);
                let both_rtl = matches!(
                    (before, after),
                    (Some((_, _, Class::RightToLeft)), Some((_, _, Class::RightToLeft)))
                );
                if both_rtl {
                    (before.map_or(Script::Latin, |(_, s, _)| s), true)
                } else {
                    // Base direction is left to right. Attach to the adjacent
                    // left-to-right run so shaping still sees one run per word.
                    let host = match (before, after) {
                        (Some((_, s, Class::LeftToRight)), _) => s,
                        (_, Some((_, s, Class::LeftToRight))) => s,
                        _ => Script::Latin,
                    };
                    (host, false)
                }
            }
        };

        match runs.last_mut() {
            Some(run) if run.script == script && run.rtl == rtl => run.end = end,
            _ => runs.push(Run { start: offset as usize, end, script, rtl }),
        }
    }
}
