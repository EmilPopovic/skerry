// SPDX-License-Identifier: Apache-2.0
//! The faces we found, and rasterization at exactly the size text will be drawn.
//!
//! Crispness is a property of one function here: [`Fonts::rasterize`] renders a
//! glyph at the device size and subpixel offset the caller will draw it at, and
//! nothing downstream is allowed to scale what comes out. Spike A's golden frame
//! asserted that and still produced fuzzy text, because the *surface* was being
//! resampled — which is why `sk-plat` reports a physical size and this crate
//! takes a device size rather than a logical one.

use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
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
    /// out of the chain, and one axis is what a `TextStyle`, a `RunKey` and a
    /// glyph key each carry. Splitting it into a family and a style would be two
    /// enums threaded through every one of them, and the product of the two is
    /// four combinations of which this program draws five things — so the fifth
    /// and sixth entries below are named rather than computed.
    Mono,
    /// Emphasis in rendered markdown — [ADR
    /// 0029](../../../docs/adr/0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md).
    ///
    /// **A real face, never a synthesised slant.** A sheared regular is what a
    /// toolkit does when it has nothing, and it is legible in the way a stretched
    /// bitmap is legible; where the machine has no italic this falls back to the
    /// regular face and emphasis is carried by nothing, which is honest.
    Italic,
    /// Strong emphasis inside emphasis, which every README has.
    BoldItalic,
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

/// A face's bytes: mapped where only root can rewrite the file, read where
/// anybody else can.
///
/// **Mapped by default.** `NotoSansCJK-Regular.ttc` is 18 MiB and a UI session
/// touches a few kilobytes of it — the tables it needs and the outlines for the
/// glyphs it draws. Reading the file copied all 18 MiB into the process before
/// the first frame; mapping it costs one syscall and lets the kernel fault in
/// what is actually used.
///
/// **Read when a truncation could reach it.** A page of a mapping past the end
/// of a truncated file raises `SIGBUS` on access — `MAP_PRIVATE` included, since
/// truncation unmaps private copies too — and that is the window dying in a
/// shaper. `cp` over a font in `~/.local/share/fonts` truncates it in place.
/// A package manager replacing a system font writes a new inode instead, which
/// a mapping of the old one never sees, so only root's own `cp` could do it to
/// a file root alone can write.
///
/// **What reading costs is the size of the faces read**, resident from start-up
/// on, and the time to copy them before the first frame. A machine whose chain
/// is all system fonts pays nothing; one with a CJK collection installed in the
/// home directory pays its whole size.
///
/// The question is asked of the file, from the descriptor about to be mapped,
/// rather than of the directory it was found in: a font somebody owns in
/// `/usr/local/share/fonts` can be truncated by them, and a root-owned,
/// read-only font behind a `~/.local/share/fonts` link into the Nix store cannot.
enum Bytes {
    Mapped(Mapping),
    Read(Box<[u8]>),
}

impl Bytes {
    fn open(path: &Path) -> Option<Self> {
        // Non-blocking so a FIFO named like a font cannot hold start-up open,
        // and refused below with everything else that is not a regular file.
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let file = rustix::fs::open(path, flags, Mode::empty()).ok()?;
        let stat = rustix::fs::fstat(&file).ok()?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return None;
        }
        let len = usize::try_from(stat.st_size).ok().filter(|&len| len > 0)?;
        if only_root_can_rewrite(stat.st_uid, stat.st_mode, host_root_is_unmapped()) {
            return Mapping::of(&file, len).map(Self::Mapped);
        }
        let mut bytes = Vec::with_capacity(len);
        // `take`, so a file that grows while it is read is read as it was
        // measured rather than for as long as somebody keeps writing.
        std::fs::File::from(file).take(len as u64).read_to_end(&mut bytes).ok()?;
        Some(Self::Read(bytes.into_boxed_slice()))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Mapped(mapping) => mapping.bytes(),
            Self::Read(bytes) => bytes,
        }
    }
}

/// Whether nobody but root can write a file with this owner and mode.
///
/// The owner counts even without a write bit, because an owner can `chmod` one
/// back. An ACL that grants a named user write shows in the group bits, which
/// are the ACL's mask when it has one.
///
/// **The overflow uid counts as root only where root is unmapped.** A user
/// namespace shows every owner it does not map as the overflow uid, which inside
/// a Flatpak is every file the host's root owns — `/run/host/fonts` included —
/// and reading those would copy the whole chain into every sandboxed window. On
/// the host the same number is `nobody`, and an NFS export with `root_squash`
/// gives it to whatever a client wrote as root, so there it is anybody's.
fn only_root_can_rewrite(owner: u32, mode: u32, host_root_unmapped: bool) -> bool {
    let roots = owner == 0 || (owner == OVERFLOW_UID && host_root_unmapped);
    roots && mode & 0o022 == 0
}

/// `/proc/sys/kernel/overflowuid` as the kernel ships it. A machine that changed
/// it reads those fonts rather than mapping them, which is the safe direction.
const OVERFLOW_UID: u32 = 65_534;

/// Whether this process's user namespace leaves the uid outside it that is 0
/// unmapped, read once from `/proc/self/uid_map`.
///
/// Unreadable is answered as mapped, which reads the overflow uid's fonts rather
/// than trusting them. In a namespace nested inside another, the map names the
/// parent's uids rather than the host's, and a parent's 0 that is a person's own
/// uid on the host reads as mapped — the same safe direction.
fn host_root_is_unmapped() -> bool {
    static UNMAPPED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *UNMAPPED.get_or_init(|| {
        std::fs::read_to_string("/proc/self/uid_map").is_ok_and(|map| root_unmapped_in(&map))
    })
}

/// Whether a `uid_map` — lines of *inside outside count* — maps no range that
/// starts at outside uid 0. A range covers 0 only by starting there.
fn root_unmapped_in(map: &str) -> bool {
    !map.lines().any(|line| {
        line.split_whitespace().nth(1).and_then(|outside| outside.parse::<u32>().ok()) == Some(0)
    })
}

/// A read-only mapping of a font file.
struct Mapping {
    address: *mut std::ffi::c_void,
    len: usize,
}

impl Mapping {
    fn of(file: &OwnedFd, len: usize) -> Option<Self> {
        // SAFETY: a private read-only mapping of `len` bytes of a file we just
        // measured. Nothing else in the process aliases it, and it is unmapped
        // exactly once, in `Drop`.
        let address = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE,
                file,
                0,
            )
        }
        .ok()?;
        Some(Self { address, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes for as long as `self`
        // lives, and the returned slice borrows `self`. It stays readable because
        // `Bytes::open` maps only a file root alone can rewrite, so nothing this
        // program runs beside can truncate it into `SIGBUS` under the slice.
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
// one thread and may be used on another, and `munmap` may run on any thread.
unsafe impl Send for Mapping {}
// SAFETY: `&Mapping` offers only `bytes`, a shared read of memory nobody writes.
unsafe impl Sync for Mapping {}

/// A font file's bytes and what `swash` needs to read them. `swash` borrows from
/// the bytes, so they outlive every `FontRef` derived from them.
pub(crate) struct Face {
    data: Bytes,
    offset: u32,
    key: CacheKey,
    name: String,
    /// Kept alongside the name because the name does not identify the bytes: two
    /// machines can hold the same `NotoColorEmoji.ttf` from two font packages
    /// and shape the same string into different pixels.
    path: PathBuf,
    /// Whether the face draws in colour — an emoji face — which is what a
    /// presentation selector chooses between.
    pub(crate) colour: bool,
}

impl Face {
    fn open(path: &Path) -> Option<Self> {
        let data = Bytes::open(path)?;
        let font = FontRef::from_index(data.bytes(), 0)?;
        let (offset, key) = (font.offset, font.key);
        let name = path.file_name()?.to_string_lossy().into_owned();
        // The four ways a face carries colour glyphs: layered outlines, PNG
        // strikes from Google and from Apple, and SVG.
        let colour = [b"COLR", b"CBDT", b"sbix", b"SVG "]
            .iter()
            .any(|tag| font.table(swash::tag_from_bytes(tag)).is_some());
        Some(Self { data, offset, key, name, path: path.to_path_buf(), colour })
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

/// The italic faces, in the same order and by the same rule as the regular ones.
const UI_ITALIC: &[&str] = &[
    "interitalic",
    "notosansitalic",
    "cantarelloblique",
    "adwaitasansitalic",
    "opensansitalic",
    "robotoitalic",
    "ubuntui",
    "liberationsansitalic",
    "dejavusansoblique",
];

const UI_BOLD_ITALIC: &[&str] = &[
    "interbolditalic",
    "notosansbolditalic",
    "adwaitasansbolditalic",
    "opensansbolditalic",
    "robotobolditalic",
    "ubuntubi",
    "liberationsansbolditalic",
    "dejavusansboldoblique",
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
    italic: u16,
    bold_italic: u16,
    pub(crate) shape_context: ShapeContext,
    scale_context: ScaleContext,
    pub(crate) scratch: crate::shape::Scratch,
    /// Whether a code point the text leaves undecided changes what its cluster
    /// draws, by cluster, offset and weight — see `Fonts::what_shows`.
    pub(crate) shows: std::collections::HashMap<(String, usize, u8), bool>,
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
        // **Regular where there is no italic, and bold where there is no bold
        // italic.** The same bargain the bold face makes one line up: a face that
        // is not there is emphasis a reader does not get, and a sheared regular is
        // worse than plain text at every size this draws at.
        let italic = take(
            &mut faces,
            &mut opened,
            &candidates,
            UI_ITALIC,
            &mut missing,
            "an italic UI sans",
        )
        .or(regular);
        let bold_italic = take(
            &mut faces,
            &mut opened,
            &candidates,
            UI_BOLD_ITALIC,
            &mut missing,
            "a bold italic UI sans",
        )
        .or(italic)
        .or(bold);

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
            italic: italic.unwrap_or(regular),
            bold_italic: bold_italic.unwrap_or(regular),
            shape_context: ShapeContext::new(),
            scale_context: ScaleContext::new(),
            scratch: crate::shape::Scratch::default(),
            shows: std::collections::HashMap::new(),
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
            Weight::Italic => self.italic,
            Weight::BoldItalic => self.bold_italic,
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
        if key.face == crate::symbols::FACE {
            return crate::symbols::rasterize(key);
        }
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

    fn scratch(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-faces-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// **The defect, as a unit: `cp` over a font the window has open.** `cp`
    /// opens its destination with `O_TRUNC`, so a font in `~/.local/share/fonts`
    /// replaced that way is truncated under whatever holds it. A mapping of it
    /// faults on the next page it touches and the window dies with `SIGBUS`; with
    /// the mapping forced on, this test does exactly that. A copy cannot fault,
    /// so the face is still a face after its file is gone.
    #[test]
    fn a_font_somebody_can_truncate_is_still_drawn_after_they_do() {
        let Some(fonts) = fonts() else { return };
        let system = fonts.paths()[usize::from(fonts.primary(Weight::Regular))].to_path_buf();
        let root = scratch("truncated");
        let copy = root.join(system.file_name().unwrap());
        std::fs::copy(&system, &copy).unwrap();
        // The copy has to be an ordinary user's to be the case under test, and a
        // suite run as root would otherwise make it root's own.
        if rustix::process::geteuid().is_root() {
            let somebody = Some(rustix::process::Uid::from_raw(4242));
            if rustix::fs::chown(&copy, somebody, None).is_err() {
                return;
            }
        }

        let face = super::Face::open(&copy).expect("a copy of a face the chain opened");
        assert!(matches!(face.data, super::Bytes::Read(_)), "a font somebody can rewrite");
        std::fs::OpenOptions::new().write(true).truncate(true).open(&copy).unwrap();
        assert_eq!(std::fs::metadata(&copy).unwrap().len(), 0);

        let bytes = face.as_ref().data;
        let touched = bytes.iter().step_by(4096).fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        std::hint::black_box(touched);
        assert_ne!(face.as_ref().charmap().map('a'), 0, "the face lost its glyphs");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half: a font the distribution installed is still mapped, so the
    /// 18 MiB of a CJK collection is not copied into every window that opens.
    ///
    /// The expectation is spelled out from the file's own `stat` — root's, and
    /// writable by nobody else, which is what a package manager installs — rather
    /// than asked of the predicate the code under test asks.
    #[test]
    fn a_font_the_distribution_installed_is_mapped() {
        let Some(fonts) = fonts() else { return };
        let mut checked = 0;
        for path in fonts.paths() {
            let stat = rustix::fs::stat(path).unwrap();
            if stat.st_uid != 0 || stat.st_mode & 0o022 != 0 {
                continue;
            }
            let face = super::Face::open(path).unwrap();
            assert!(matches!(face.data, super::Bytes::Mapped(_)), "{} was read", path.display());
            checked += 1;
        }
        // Inside a user namespace root's files show as the overflow uid and none
        // of them is checked here; anywhere else a distribution's chain has one.
        let distributed = fonts.paths().iter().any(|path| path.starts_with("/usr/share/fonts"));
        if distributed && !super::host_root_is_unmapped() {
            assert!(checked > 0, "no face in the chain was root's: {:?}", fonts.paths());
        }
    }

    #[test]
    fn only_a_file_root_owns_and_nobody_else_can_write_counts_as_roots() {
        let roots = |owner, mode| super::only_root_can_rewrite(owner, mode, false);
        assert!(roots(0, 0o100_644), "a distribution's font");
        assert!(roots(0, 0o100_444), "a font in the Nix store");
        assert!(!roots(0, 0o100_664), "group-writable, or an ACL granting somebody write");
        assert!(!roots(0, 0o100_646), "world-writable");
        assert!(!roots(1000, 0o100_644), "a font somebody installed for themselves");
        assert!(!roots(1000, 0o100_444), "read-only to its owner, who can chmod it back");
        assert!(!roots(65_534, 0o100_644), "nobody's, or root_squash's, on the host");
    }

    #[test]
    fn the_overflow_uid_is_roots_only_where_root_is_unmapped() {
        let inside = |owner| super::only_root_can_rewrite(owner, 0o100_644, true);
        assert!(inside(65_534), "the host's font, seen from inside a Flatpak");
        assert!(inside(0));
        assert!(!inside(1000));

        let unmapped = super::root_unmapped_in;
        assert!(!unmapped("         0          0 4294967295\n"), "the initial namespace");
        assert!(unmapped("      1000       1000          1\n"), "a Flatpak's");
        assert!(unmapped("         0       1000          1\n"), "unshare -r");
        assert!(!unmapped("      1000          0          1\n"), "the parent's root mapped in");
        assert!(!unmapped("         0     100000      65536\n         1          0          1\n"));
        assert!(unmapped(""), "a namespace nobody has written a map for yet");
    }

    /// A FIFO named like a font would hold start-up open forever: opening one
    /// for reading blocks until something writes. Without `O_NONBLOCK` this test
    /// hangs rather than fails.
    #[test]
    fn a_fifo_named_like_a_font_is_refused_without_blocking() {
        let root = scratch("fifo");
        let path = root.join("Inter-Regular.ttf");
        let made = rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        );
        if made.is_ok() {
            assert!(super::Bytes::open(&path).is_none());
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
