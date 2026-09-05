// SPDX-License-Identifier: Apache-2.0
//! The font database an SVG's text is shaped with, and the families it maps.
//!
//! # Loaded once, which is the measured half
//!
//! `load_system_fonts` walks every font directory on the machine and parses what
//! it finds: **73 ms and 881 faces here**, measured by the probe below. It was
//! called *per request*, so a directory of forty SVG icons paid for it forty
//! times — three seconds of a budget where one thumbnail is allowed sixty
//! milliseconds. Now it happens on the first SVG and never again.
//!
//! # The generics, which is the reasoned half
//!
//! `usvg::Options::default()` names **Times New Roman** as the family to use when
//! a document's text names none, and `fontdb`'s four generics default to *Times
//! New Roman*, *Arial* and *Courier New*. Those are Microsoft's fonts. usvg's own
//! font selector appends the *serif* generic to every query as a last resort — so
//! on a machine without them, `sans-serif`, an unknown named family and no family
//! at all all resolve to nothing and the text is dropped with a `log::warn!`
//! nothing reads.
//!
//! **This machine has them**, from `msttcorefonts`, so that failure is reasoned
//! from `fontdb`'s documented defaults and the test below rather than reproduced
//! here: before this file, an SVG's text rendered on this machine and rendered in
//! *Arial*. What is demonstrable here is the other half of the same change — SVG
//! text is now drawn in the families the machine and the window itself prefer,
//! Noto ahead of `DejaVu`, which is [`sk_text`'s own
//! order](../../../crates/sk-text/src/fonts.rs) and what `fc-match sans-serif`
//! answers.
//!
//! Two lists rather than one, because the two want different things: the window
//! picks *files* by stem, for a UI at thirteen pixels with a script-coverage
//! chain behind it; this picks *family names* for `fontdb`, whose generics are
//! four strings. Sharing them would mean the helper linking the text stack to
//! read a constant.

use std::sync::{Arc, OnceLock};

/// What the machine's fonts turned out to be.
pub struct Fonts {
    /// The database, shared by every request.
    pub db: Arc<resvg::usvg::fontdb::Database>,
    /// The family an SVG with no `font-family` at all is drawn in.
    pub default: String,
    /// What the four generics resolved to, for the probe's report.
    pub report: String,
}

/// The database, loaded and configured on the first SVG.
pub fn shared() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(load)
}

/// The candidates for each generic, best first.
///
/// **The window's own order**, which is `sk_text`'s: Noto first because it is
/// what a fresh GNOME has and what `fc-match sans-serif` answers on this
/// machine, then the humanist sans a file list reads well in, and `DejaVu` last
/// because it is on every distribution that ships a font at all — so the list
/// ends in something rather than in nothing. A machine with none of them gets
/// whatever it has, which is [`pick`]'s last resort.
const SANS: &[&str] = &[
    "Noto Sans",
    "Inter",
    "Cantarell",
    "Adwaita Sans",
    "Open Sans",
    "Ubuntu",
    "Liberation Sans",
    "DejaVu Sans",
];
const SERIF: &[&str] = &["Noto Serif", "Liberation Serif", "DejaVu Serif", "Tinos", "FreeSerif"];
const MONO: &[&str] = &[
    "Noto Sans Mono",
    "JetBrains Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "FreeMono",
];

fn load() -> Fonts {
    let started = std::time::Instant::now();
    let mut db = resvg::usvg::fontdb::Database::new();
    db.load_system_fonts();
    let faces = db.len();
    let sans = pick(&db, SANS);
    let serif = pick(&db, SERIF).or_else(|| sans.clone());
    let mono = pick(&db, MONO).or_else(|| sans.clone());
    if let Some(family) = &sans {
        db.set_sans_serif_family(family.clone());
        // **Cursive and fantasy resolve to the sans too.** Neither has a
        // candidate list worth writing: a machine that has a script face does not
        // agree with any other machine about which one it is, and the choice is
        // between somebody else's sans and nothing on screen.
        db.set_cursive_family(family.clone());
        db.set_fantasy_family(family.clone());
    }
    if let Some(family) = &serif {
        db.set_serif_family(family.clone());
    }
    if let Some(family) = &mono {
        db.set_monospace_family(family.clone());
    }
    // **The serif, because that is usvg's own last resort.** Its default font
    // selector appends the serif generic to every query, so a document naming a
    // font nobody has ends up here — and what is set above is what decides
    // whether that is a glyph or nothing.
    let default = serif.clone().or_else(|| sans.clone()).unwrap_or_default();
    let report = format!(
        "{faces} faces in {:.1} ms; sans {}, serif {}, mono {}",
        started.elapsed().as_secs_f64() * 1000.0,
        sans.as_deref().unwrap_or("none"),
        serif.as_deref().unwrap_or("none"),
        mono.as_deref().unwrap_or("none"),
    );
    Fonts { db: Arc::new(db), default, report }
}

/// The first of `candidates` the database holds, or any family at all.
///
/// **The last resort is whatever is there.** A container with one font — a build
/// image, a minimal desktop — should draw text in that font rather than none, and
/// "the first family the database lists" is a worse choice than the candidates
/// above it and a much better one than nothing.
fn pick(db: &resvg::usvg::fontdb::Database, candidates: &[&str]) -> Option<String> {
    for family in candidates {
        let query = resvg::usvg::fontdb::Query {
            families: &[resvg::usvg::fontdb::Family::Name(family)],
            ..resvg::usvg::fontdb::Query::default()
        };
        if db.query(&query).is_some() {
            return Some((*family).to_owned());
        }
    }
    db.faces().find_map(|face| face.families.first().map(|(name, _)| name.clone()))
}

#[cfg(test)]
mod tests {
    /// **`fontdb`'s defaults are Microsoft's font names**, which is the whole
    /// reason the file above exists — and the assertion that will fail if a
    /// future version of the crate fixes it, at which point half of this can go.
    #[test]
    fn the_defaults_this_replaces_are_fonts_a_linux_box_does_not_have() {
        let db = resvg::usvg::fontdb::Database::new();
        assert_eq!(db.family_name(&resvg::usvg::fontdb::Family::SansSerif), "Arial");
        assert_eq!(db.family_name(&resvg::usvg::fontdb::Family::Serif), "Times New Roman");
        assert_eq!(db.family_name(&resvg::usvg::fontdb::Family::Monospace), "Courier New");
        // And usvg's default for text that names no family at all.
        assert_eq!(resvg::usvg::Options::default().font_family, "Times New Roman");
    }

    /// **The machine's own fonts, and this is the point of the test.** What a
    /// runtime probe has to assert is that the four generics resolve to something
    /// this machine holds, whatever it holds — and on a machine without
    /// `msttcorefonts` that is the difference between an SVG's text and no text.
    #[test]
    fn the_generics_resolve_to_fonts_this_machine_has() {
        let fonts = super::shared();
        if fonts.db.is_empty() {
            // A machine with no fonts at all is a machine where an SVG's text
            // cannot be drawn, and saying so is the honest answer.
            assert!(fonts.default.is_empty());
            return;
        }
        assert!(!fonts.default.is_empty(), "no default family: {}", fonts.report);
        for family in [
            resvg::usvg::fontdb::Family::SansSerif,
            resvg::usvg::fontdb::Family::Serif,
            resvg::usvg::fontdb::Family::Monospace,
            resvg::usvg::fontdb::Family::Cursive,
            resvg::usvg::fontdb::Family::Fantasy,
        ] {
            let query = resvg::usvg::fontdb::Query {
                families: &[family],
                ..resvg::usvg::fontdb::Query::default()
            };
            assert!(fonts.db.query(&query).is_some(), "{family:?} resolves to nothing");
        }
        // And a family nobody has resolves to *something*, because usvg's last
        // resort is the serif generic and that is now a font rather than a name.
        let helvetica = resvg::usvg::fontdb::Query {
            families: &[
                resvg::usvg::fontdb::Family::Name("Helvetica"),
                resvg::usvg::fontdb::Family::Serif,
            ],
            ..resvg::usvg::fontdb::Query::default()
        };
        assert!(fonts.db.query(&helvetica).is_some(), "a document naming Helvetica gets nothing");
    }
}
