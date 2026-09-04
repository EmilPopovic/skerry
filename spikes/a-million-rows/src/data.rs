// SPDX-License-Identifier: MIT OR Apache-2.0
//! The columnar store from docs/04-architecture.md, at the size the budget cares
//! about.
//!
//! One name arena and parallel arrays indexed by `EntryId`. A `Vec<Entry>` would
//! be a shorter program measuring a data layout we have already decided not to
//! ship: 1M `String`s is 24 bytes of header and one allocation each before a
//! single character of filename exists.

/// A seeded generator, so the workload is byte-identical between runs and
/// between the two arms of the spike. SplitMix64 — three lines, no dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, bound: u32) -> u32 {
        (self.next() >> 33) as u32 % bound
    }
}

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_LINK: u16 = 1 << 1;
pub const FLAG_HIDDEN: u16 = 1 << 2;

pub struct Store {
    names: String,
    name_ref: Vec<(u32, u32)>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u16>,
}

impl Store {
    pub fn len(&self) -> usize {
        self.name_ref.len()
    }

    pub fn name(&self, id: u32) -> &str {
        let (offset, len) = self.name_ref[id as usize];
        &self.names[offset as usize..(offset + len) as usize]
    }

    pub fn size(&self, id: u32) -> u64 {
        self.size[id as usize]
    }

    pub fn mtime(&self, id: u32) -> i64 {
        self.mtime[id as usize]
    }

    pub fn flags(&self, id: u32) -> u16 {
        self.flags[id as usize]
    }

    /// Bytes held by the store itself, so the RSS figure can be attributed
    /// rather than guessed at.
    pub fn footprint(&self) -> usize {
        self.names.capacity()
            + self.name_ref.capacity() * 8
            + self.size.capacity() * 8
            + self.mtime.capacity() * 8
            + self.flags.capacity() * 2
    }

    /// A synthetic listing in the script mix a real machine produces. A spike
    /// that shapes only ASCII measures a fast path we are not allowed to assume.
    pub fn synthetic(count: usize, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let mut store = Self {
            names: String::with_capacity(count * 34),
            name_ref: Vec::with_capacity(count),
            size: Vec::with_capacity(count),
            mtime: Vec::with_capacity(count),
            flags: Vec::with_capacity(count),
        };
        let mut scratch = String::with_capacity(256);

        for id in 0..count {
            scratch.clear();
            let flags = compose_name(&mut rng, id, &mut scratch);

            let offset = store.names.len() as u32;
            store.names.push_str(&scratch);
            store.name_ref.push((offset, scratch.len() as u32));

            store.flags.push(flags);
            store.size.push(if flags & FLAG_DIR != 0 {
                0
            } else {
                // Log-ish distribution: a real tree is mostly small files and a
                // few very large ones, and the column has to render both.
                let magnitude = rng.below(34);
                (rng.next() % 1024).wrapping_shl(magnitude)
            });
            // A ten-year window ending at 2026-08-25, so the date column
            // exercises every month and both digit widths.
            store.mtime.push(1_787_000_000 - (rng.next() % 315_360_000) as i64);
        }
        store
    }
}

const STEMS: &[&str] = &[
    "report", "invoice", "backup", "archive", "config", "notes", "draft", "final",
    "screenshot", "recording", "dataset", "export", "import", "manifest", "index",
    "budget", "roadmap", "meeting", "contract", "receipt", "photo", "render",
    "build", "target", "release", "debug", "profile", "trace", "snapshot", "patch",
];

const EXTS: &[&str] = &[
    "txt", "md", "rs", "toml", "json", "png", "jpg", "webp", "pdf", "zip", "tar.zst",
    "mp4", "mkv", "flac", "csv", "log", "so", "py", "html", "svg",
];

/// Latin with diacritics, CJK, Arabic, Hebrew, Devanagari, and an explicitly
/// decomposed cluster. Each of these takes a different path through shaping.
const LATIN: &[&str] = &["résumé", "naïve", "Zürich", "São-Paulo", "Ångström", "Łódź"];
const KANA: &[&str] = &["プロジェクト", "スクリーンショット", "写真", "회의록"];

/// A pool of common ideographs, drawn from rather than a fixed handful of
/// words. The glyph *inventory* is what the atlas has to hold, and a listing of
/// CJK filenames has hundreds of distinct ideographs where a listing of English
/// ones has fifty letters. Measured at six fixed CJK words, the atlas held 287
/// entries and never came close to its allocator; that is not the workload.
const IDEOGRAPH_POOL: u32 = 512;
const ARABIC: &[&str] = &["ملف", "مستند", "صورة", "نسخة-احتياطية"];
const HEBREW: &[&str] = &["קובץ", "מסמך", "תמונה"];
const DEVANAGARI: &[&str] = &["दस्तावेज़", "छवि", "फ़ाइल"];
const EMOJI: &[&str] = &["📁", "🎬", "📸", "🗂️", "✅", "🔥"];
/// `e` + COMBINING ACUTE, which must shape to the same glyph as `é` above.
const DECOMPOSED: &str = "cafe\u{0301}-menu";

fn pick<'a>(rng: &mut Rng, from: &[&'a str]) -> &'a str {
    from[rng.below(from.len() as u32) as usize]
}

/// Returns the flags for the entry it wrote into `out`.
fn compose_name(rng: &mut Rng, id: usize, out: &mut String) -> u16 {
    let mut flags = 0;
    // Directories cluster at the top of a real sorted listing; here they are
    // sprinkled, which is strictly harder on the icon column.
    if rng.below(100) < 12 {
        flags |= FLAG_DIR;
    }
    if rng.below(100) < 4 {
        flags |= FLAG_LINK;
    }

    match rng.below(100) {
        0..=57 => {
            out.push_str(pick(rng, STEMS));
            out.push('-');
            out.push_str(pick(rng, STEMS));
            push_index(out, id);
        }
        58..=67 => out.push_str(pick(rng, LATIN)),
        68..=73 => push_ideographs(rng, out),
        74..=77 => out.push_str(pick(rng, KANA)),
        78..=82 => out.push_str(pick(rng, ARABIC)),
        83..=85 => out.push_str(pick(rng, HEBREW)),
        86..=88 => out.push_str(pick(rng, DEVANAGARI)),
        89..=92 => {
            out.push_str(pick(rng, EMOJI));
            out.push(' ');
            out.push_str(pick(rng, STEMS));
        }
        93..=94 => out.push_str(DECOMPOSED),
        95..=96 => {
            // Long enough to ellipsize at any plausible column width, which is
            // the case where layout does the most work per row.
            for _ in 0..9 {
                out.push_str(pick(rng, STEMS));
                out.push('_');
            }
            push_index(out, id);
        }
        _ => {
            flags |= FLAG_HIDDEN;
            out.push('.');
            out.push_str(pick(rng, STEMS));
            out.push_str("rc");
        }
    }

    if flags & FLAG_DIR == 0 {
        out.push('.');
        out.push_str(pick(rng, EXTS));
    }
    flags
}

/// Two to four ideographs from the pool. `0x4E00` is the start of the CJK
/// Unified Ideographs block; the stride keeps the sample spread across it
/// rather than clustered at the start.
fn push_ideographs(rng: &mut Rng, out: &mut String) {
    for _ in 0..2 + rng.below(3) {
        let codepoint = 0x4E00 + rng.below(IDEOGRAPH_POOL) * 3;
        if let Some(ch) = char::from_u32(codepoint) {
            out.push(ch);
        }
    }
}

fn push_index(out: &mut String, id: usize) {
    out.push('-');
    let mut digits = [0u8; 8];
    let mut n = id;
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &d in &digits[i..] {
        out.push(d as char);
    }
}

// ── column text ────────────────────────────────────────────────────────────────

pub fn format_size(bytes: u64, flags: u16, out: &mut String) {
    out.clear();
    if flags & FLAG_DIR != 0 {
        out.push_str("—");
        return;
    }
    const UNITS: &[&str] = &["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        push_u64(out, bytes);
    } else {
        push_u64(out, value as u64);
        out.push('.');
        out.push((b'0' + ((value * 10.0) as u64 % 10) as u8) as char);
    }
    out.push(' ');
    out.push_str(UNITS[unit]);
}

/// `YYYY-MM-DD hh:mm`, computed rather than pulled from a date crate — a spike
/// has no business adding a dependency to print a column.
pub fn format_mtime(unix: i64, out: &mut String) {
    out.clear();
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    push_u64(out, year as u64);
    out.push('-');
    push_two(out, month as u64);
    out.push('-');
    push_two(out, day as u64);
    out.push(' ');
    push_two(out, (secs / 3600) as u64);
    out.push(':');
    push_two(out, (secs % 3600 / 60) as u64);
}

/// Howard Hinnant's `civil_from_days`, proleptic Gregorian, era-based. Cited
/// because the magic constants are unreadable and correct:
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

pub fn kind_of(name: &str, flags: u16) -> &'static str {
    if flags & FLAG_DIR != 0 {
        return "Folder";
    }
    match name.rsplit('.').next().unwrap_or("") {
        "rs" => "Rust source",
        "py" => "Python source",
        "md" => "Markdown",
        "txt" | "log" => "Plain text",
        "toml" | "json" => "Configuration",
        "png" | "jpg" | "webp" | "svg" => "Image",
        "pdf" => "Document",
        "zip" | "zst" => "Archive",
        "mp4" | "mkv" => "Video",
        "flac" => "Audio",
        "csv" => "Spreadsheet",
        "html" => "Web page",
        "so" => "Shared library",
        _ => "File",
    }
}

fn push_u64(out: &mut String, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &d in &digits[i..] {
        out.push(d as char);
    }
}

fn push_two(out: &mut String, n: u64) {
    if n < 10 {
        out.push('0');
    }
    push_u64(out, n);
}
