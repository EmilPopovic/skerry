//! The faces we found, and rasterization at exactly the size text will be drawn.
//!
//! Crispness is a property of one function here: [`Fonts::rasterize`] renders a
//! glyph at the device size and subpixel offset the caller will draw it at, and
//! nothing downstream is allowed to scale what comes out. Spike A's golden frame
//! asserted that and still produced fuzzy text, because the *surface* was being
//! resampled — which is why `sk-plat` reports a physical size and this crate
//! takes a device size rather than a logical one.

use std::path::{Path, PathBuf};

use sk_model::{Problem, ProblemKind};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::{CacheKey, FontRef};

use crate::discover::{self, Candidate};

/// Horizontal subpixel buckets.
///
/// Four is where the next bucket stops being distinguishable at text sizes and
/// starts being atlas memory. Without any, a run of text quantizes every glyph
/// to a whole pixel and the spacing visibly stutters.
pub const SUBPIXEL_BUCKETS: u8 = 4;

/// Which face a run wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Weight {
    /// Body text.
    #[default]
    Regular,
    /// Column headers, the path bar, anything that has to win attention.
    Bold,
    /// A file's own text, where the columns are the content.
    ///
    /// **Not a weight, and it is here anyway.** This enum is what picks a face
    /// out of the chain, and a third face is what a mono preview needs; a second
    /// axis for one caller would be two enums to thread through every
    /// `TextStyle`, `RunKey` and glyph key in the program. If a fourth face ever
    /// arrives — an italic — that is the moment to split them.
    Mono,
}

/// Where a glyph raster is identified from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey {
    /// Index into the fallback chain.
    pub face: u16,
    /// The face's own glyph id.
    pub glyph: u16,
    /// Size in quarter device pixels, so a fractional scale is part of the key
    /// rather than rounded away.
    pub quarter_px: u16,
    /// Which of [`SUBPIXEL_BUCKETS`] horizontal offsets.
    pub subpixel: u8,
}

impl GlyphKey {
    /// The size to rasterize at, in device pixels.
    #[must_use]
    pub fn px_size(self) -> f32 {
        f32::from(self.quarter_px) / 4.0
    }
}

/// A rasterized glyph, in whichever form the font provided.
///
/// `swash`'s own image type is deliberately not exposed: `sk-gpu` uploads this
/// and must not need to know what shaped it. That is the seam
/// [docs/07-dependencies.md](../../../docs/07-dependencies.md) names for ADR 0004.
#[derive(Clone, Debug)]
pub struct GlyphImage {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
    /// Offset from the pen position to the left edge.
    pub left: i32,
    /// Offset from the baseline to the top edge, upward positive.
    pub top: i32,
    /// Whether `data` is premultiplied RGBA rather than single-channel coverage.
    pub color: bool,
    /// Coverage, or premultiplied RGBA.
    pub data: Vec<u8>,
}

/// Vertical metrics at one size, in device pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Metrics {
    /// Baseline to the top of the tallest ascender.
    pub ascent: f32,
    /// Baseline to the bottom of the lowest descender, positive downward.
    pub descent: f32,
    /// The distance between two baselines the font recommends.
    pub line_height: f32,
}

/// A read-only mapping of a font file.
///
/// Mapped rather than read. `NotoSansCJK-Regular.ttc` is 18 MiB and a UI session
/// touches a few kilobytes of it — the tables it needs and the outlines for the
/// glyphs it draws. Reading the file copied all 18 MiB into the process before
/// the first frame; mapping it costs one syscall and lets the kernel fault in
/// what is actually used.
///
/// A font file truncated while mapped would fault on access. That is accepted:
/// the alternative is copying every font on the machine at start-up, and a
/// package manager replacing a font file writes a new inode rather than
/// truncating the old one.
struct Mapping {
    address: *mut std::ffi::c_void,
    len: usize,
}

impl Mapping {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        if len == 0 {
            return None;
        }
        // SAFETY: a private read-only mapping of `len` bytes of a file we just
        // measured. Nothing else in the process aliases it, and it is unmapped
        // exactly once, in `Drop`.
        let address = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE,
                &file,
                0,
            )
        }
        .ok()?;
        Some(Self { address, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes for as long as `self`
        // lives, and the returned slice borrows `self`.
        unsafe { std::slice::from_raw_parts(self.address.cast::<u8>(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the same address and length `mmap` returned, unmapped once.
        let _ = unsafe { rustix::mm::munmap(self.address, self.len) };
    }
}

// SAFETY: a read-only mapping is immutable for its whole life, so sharing the
// address across threads cannot race. This is needed because `Fonts` is built on
// one thread and may be used on another.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

/// A font file mapped into memory. `swash` borrows from the bytes, so the
/// mapping outlives every `FontRef` derived from it.
pub(crate) struct Face {
    data: Mapping,
    offset: u32,
    key: CacheKey,
    name: String,
    /// Kept alongside the name because the name does not identify the bytes: two
    /// machines can hold the same `NotoColorEmoji.ttf` from two font packages
    /// and shape the same string into different pixels.
    path: PathBuf,
}

impl Face {
    fn open(path: &Path) -> Option<Self> {
        let data = Mapping::open(path)?;
        let font = FontRef::from_index(data.bytes(), 0)?;
        let (offset, key) = (font.offset, font.key);
        let name = path.file_name()?.to_string_lossy().into_owned();
        Some(Self { data, offset, key, name, path: path.to_path_buf() })
    }

    pub(crate) fn as_ref(&self) -> FontRef<'_> {
        FontRef { data: self.data.bytes(), offset: self.offset, key: self.key }
    }
}

/// The families a UI font is chosen from, most wanted first.
///
/// Ordered by how a file list actually reads: a humanist sans with open
/// apertures and unambiguous digits beats a grotesque at 13 px. The last two
/// entries are the ones present on essentially every Linux install, so the list
/// ends in something rather than in nothing.
const UI_REGULAR: &[&str] = &[
    "interregular",
    "interv",
    "notosansregular",
    "cantarellregular",
    "adwaitasansregular",
    "opensansregular",
    "robotoregular",
    "ubuntur",
    "liberationsansregular",
    "dejavusans",
];

const UI_BOLD: &[&str] = &[
    "interbold",
    "notosansbold",
    "cantarellbold",
    "adwaitasansbold",
    "opensansbold",
    "robotobold",
    "ubuntub",
    "liberationsansbold",
    "dejavusansbold",
];

/// Script coverage, appended after the UI faces.
///
/// A file list has to render the names on the disk, not the names in the
/// language of the interface: a `~/Downloads` with one Japanese filename and one
/// emoji is the common case, not the exotic one.
/// The face a file's own text is drawn in.
///
/// **Noto first, because CI has it and the golden frames pin it by hash.** After
/// that, whatever the machine happens to have: the list is in the order a person
/// would rank them for reading code, and the last entry is on every distribution
/// that ships any font at all. A machine with none of them falls back to the UI
/// sans, which is legible and not aligned — recorded in `missing` and reported,
/// because it changes what the program looks like.
const UI_MONO: &[&str] = &[
    "notosansmonoregular",
    "notosansmono",
    "dejavusansmono",
    "liberationmonoregular",
    "jetbrainsmonoregular",
    "cascadiamonoregular",
    "ubuntumonor",
    "freemono",
];

const FALLBACK: &[&str] = &[
    "notosanscjkregular",
    "notosanscjkjpregular",
    "notosansjpregular",
    "notosanscjk",
    "notosansarabicregular",
    "notosanshebrewregular",
    "notosansdevanagariregular",
    "notosansthairegular",
    "notosanssymbols2regular",
    "notocoloremoji",
    "twemoji",
    "unifont",
];

/// Every face we will shape with, and the contexts that do it.
pub struct Fonts {
    pub(crate) faces: Vec<Face>,
    regular: u16,
    bold: u16,
    mono: u16,
    pub(crate) shape_context: ShapeContext,
    scale_context: ScaleContext,
    pub(crate) scratch: crate::shape::Scratch,
    /// Names of the faces that were wanted and not found. Reported rather than
    /// swallowed, because it changes what the program can render.
    missing: Vec<&'static str>,
}

impl Fonts {
    /// Finds and opens the faces, or explains why it could not.
    pub fn discover() -> Result<Self, Problem> {
        let candidates = discover::scan();
        let mut faces = Vec::new();
        let mut missing = Vec::new();
        let mut opened = std::collections::HashSet::new();

        let regular =
            take(&mut faces, &mut opened, &candidates, UI_REGULAR, &mut missing, "a UI sans");
        let bold =
            take(&mut faces, &mut opened, &candidates, UI_BOLD, &mut missing, "a bold UI sans")
                // A machine with one font is a machine that still has to draw a
                // column header. Faux bold is worse than one weight, so the regular
                // face stands in and the header is distinguished by colour instead.
                .or(regular);
        // The same bargain: a text preview in a proportional face is a preview
        // whose columns do not line up, which is worse than one and not as bad as
        // none.
        let mono = take(&mut faces, &mut opened, &candidates, UI_MONO, &mut missing, "a mono face")
            .or(regular);

        // Deduplicated by path, because two needles can name one file:
        // `notosanscjkregular` and `notosanscjk` both resolve to
        // NotoSansCJK-Regular.ttc, and the chain held it twice — a second
        // mapping of 18 MiB, and a second miss for every glyph the first copy
        // did not have.
        for wanted in FALLBACK {
            let Some(candidate) = discover::pick(&candidates, &[wanted]) else { continue };
            if !opened.insert(candidate.path.clone()) {
                continue;
            }
            if let Some(face) = Face::open(&candidate.path) {
                faces.push(face);
            }
        }

        let Some(regular) = regular else {
            return Err(Problem::plain(
                ProblemKind::NoFont,
                format!("scanned {} font files, none of them a usable sans", candidates.len()),
            ));
        };

        Ok(Self {
            faces,
            regular,
            bold: bold.unwrap_or(regular),
            mono: mono.unwrap_or(regular),
            shape_context: ShapeContext::new(),
            scale_context: ScaleContext::new(),
            scratch: crate::shape::Scratch::default(),
            missing,
        })
    }

    /// How many faces are in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// Whether there are no faces at all, which [`Fonts::discover`] refuses to
    /// produce.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// The file name of each face, in chain order, for the log.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.faces.iter().map(|face| face.name.as_str()).collect()
    }

    /// The file behind each face, in chain order.
    ///
    /// Separate from [`Fonts::names`] because the golden-frame harness hashes
    /// them: the names are the same on two machines whose font packages are not.
    #[must_use]
    pub fn paths(&self) -> Vec<&Path> {
        self.faces.iter().map(|face| face.path.as_path()).collect()
    }

    /// Faces that were wanted and not found.
    #[must_use]
    pub fn missing(&self) -> &[&'static str] {
        &self.missing
    }

    /// The index of the face for `weight`.
    #[must_use]
    pub fn primary(&self, weight: Weight) -> u16 {
        match weight {
            Weight::Regular => self.regular,
            Weight::Bold => self.bold,
            Weight::Mono => self.mono,
        }
    }

    /// Vertical metrics for `weight` at `px_size` device pixels.
    #[must_use]
    pub fn metrics(&self, weight: Weight, px_size: f32) -> Metrics {
        let Some(face) = self.faces.get(self.primary(weight) as usize) else {
            return Metrics::default();
        };
        // `scale` takes pixels per em; `linear_scale` takes a ratio. Passing a
        // pixel size to the second one multiplies font units by 13 instead of
        // dividing them by 2048, and every baseline lands four thousand pixels
        // below the window.
        let metrics = face.as_ref().metrics(&[]).scale(px_size);
        Metrics {
            ascent: metrics.ascent,
            descent: metrics.descent,
            // Some fonts report zero leading and a tight ascent+descent, which
            // sets rows so close that descenders touch the next row's caps.
            line_height: (metrics.ascent + metrics.descent + metrics.leading).max(px_size * 1.2),
        }
    }

    /// Rasterizes one glyph at exactly the size and offset it will be drawn at.
    #[must_use]
    pub fn rasterize(&mut self, key: GlyphKey) -> Option<GlyphImage> {
        // Split the borrow: `faces` is read while `scale_context` is mutated,
        // and they are disjoint fields.
        let Self { faces, scale_context, .. } = self;
        let face = faces.get(key.face as usize)?;
        let offset_x = f32::from(key.subpixel) / f32::from(SUBPIXEL_BUCKETS);
        let mut scaler =
            scale_context.builder(face.as_ref()).size(key.px_size()).hint(true).build();

        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .offset(swash::zeno::Vector::new(offset_x, 0.0))
        .render(&mut scaler, key.glyph)?;

        Some(GlyphImage {
            width: image.placement.width,
            height: image.placement.height,
            left: image.placement.left,
            top: image.placement.top,
            color: image.content == Content::Color,
            data: image.data,
        })
    }
}

fn take(
    faces: &mut Vec<Face>,
    opened: &mut std::collections::HashSet<std::path::PathBuf>,
    candidates: &[Candidate],
    wanted: &[&str],
    missing: &mut Vec<&'static str>,
    label: &'static str,
) -> Option<u16> {
    let candidate = discover::pick(candidates, wanted)?;
    let Some(face) = Face::open(&candidate.path) else {
        missing.push(label);
        return None;
    };
    opened.insert(candidate.path.clone());
    faces.push(face);
    Some((faces.len() - 1) as u16)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Fonts, GlyphKey, SUBPIXEL_BUCKETS, Weight};

    /// Every machine this runs on has fonts; a machine that does not is a
    /// machine where the program correctly refuses to start.
    fn fonts() -> Option<Fonts> {
        Fonts::discover().ok()
    }

    #[test]
    fn discovery_finds_a_sans_and_reports_the_chain() {
        let Some(fonts) = fonts() else { return };
        assert!(!fonts.is_empty());
        assert!(!fonts.names().is_empty());
    }

    /// **The mono face is a different face, or it says it is not.**
    ///
    /// A text preview in a proportional face is one whose columns do not line up,
    /// and the fallback to the UI sans is deliberate rather than silent — so this
    /// asserts the pair: either `Mono` resolves somewhere else in the chain, or
    /// `missing` names it. The reference machine and CI both have
    /// `NotoSansMono-Regular.ttf`, so on both this takes the first branch.
    #[test]
    fn a_mono_face_is_its_own_face_or_is_reported_absent() {
        let fonts = Fonts::discover().expect("no usable font on this machine");
        let mono = fonts.primary(Weight::Mono);
        if mono == fonts.primary(Weight::Regular) {
            assert!(
                fonts.missing().iter().any(|what| what.contains("mono")),
                "the mono face fell back to the sans and nothing said so"
            );
        }
        assert!((mono as usize) < fonts.len(), "the mono index is not in the chain");
    }

    /// A face in the chain twice is a second mapping of the same file and a
    /// second miss for every glyph it does not have. Two `FALLBACK` needles
    /// resolving to one file is enough to produce one: `notosanscjkregular` and
    /// `notosanscjk` both name NotoSansCJK-Regular.ttc, and this asserted
    /// nothing while the chain on this machine held it twice.
    #[test]
    fn no_face_is_in_the_chain_twice() {
        let Some(fonts) = fonts() else { return };
        let mut names = fonts.names();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "{:?}", fonts.names());
    }

    /// Every number here is checked against the *size it was asked for*, not
    /// against another number derived the same way.
    ///
    /// The first version of this test asserted only `ascent > 0` and
    /// `line_height >= ascent + descent`, and passed while every metric was 13×
    /// too large — because both sides of that comparison were wrong by the same
    /// factor. Text rendered four thousand pixels below the window and the suite
    /// was green.
    #[test]
    fn metrics_are_in_pixels_of_the_size_they_were_asked_for() {
        let Some(fonts) = fonts() else { return };
        for size in [11.0_f32, 13.0, 16.25, 32.0] {
            let metrics = fonts.metrics(Weight::Regular, size);
            assert!(metrics.ascent > size * 0.5, "ascent {} at {size} px", metrics.ascent);
            assert!(metrics.ascent < size * 1.5, "ascent {} at {size} px", metrics.ascent);
            assert!(metrics.descent >= 0.0 && metrics.descent < size, "{metrics:?} at {size}");
            assert!(metrics.line_height >= metrics.ascent + metrics.descent, "{metrics:?}");
            assert!(
                metrics.line_height < size * 2.0,
                "line height {} at {size}",
                metrics.line_height
            );
        }
    }

    /// A shaped run's width has to be in the same units as the metrics, or a
    /// column is laid out in one unit and filled in another.
    #[test]
    fn a_shaped_advance_is_in_pixels_of_the_size_it_was_asked_for() {
        let Some(mut fonts) = fonts() else { return };
        for size in [11.0_f32, 13.0, 32.0] {
            let run = fonts.shape("nnnnnnnnnn", size, Weight::Regular);
            let per_glyph = run.advance / 10.0;
            assert!(per_glyph > size * 0.3, "{per_glyph} per glyph at {size} px");
            assert!(per_glyph < size * 1.2, "{per_glyph} per glyph at {size} px");
        }
    }

    #[test]
    fn a_rasterized_glyph_is_no_larger_than_the_size_it_was_asked_for() {
        let Some(mut fonts) = fonts() else { return };
        let key = GlyphKey {
            face: fonts.primary(Weight::Regular),
            glyph: 40,
            quarter_px: 13 * 4,
            subpixel: 0,
        };
        if let Some(image) = fonts.rasterize(key) {
            assert!(image.width <= 32 && image.height <= 32, "{}x{}", image.width, image.height);
            let channels = if image.color { 4 } else { 1 };
            assert_eq!(image.data.len(), (image.width * image.height * channels) as usize);
        }
    }

    #[test]
    fn every_subpixel_bucket_is_a_distinct_key() {
        let mut seen = std::collections::HashSet::new();
        for bucket in 0..SUBPIXEL_BUCKETS {
            let key = GlyphKey { face: 0, glyph: 1, quarter_px: 52, subpixel: bucket };
            assert!(seen.insert(key), "bucket {bucket} collided");
        }
    }
}
