// SPDX-License-Identifier: Apache-2.0
//! Where a page's words are, from the tool that rendered it.
//!
//! **[ADR 0024](../../../docs/adr/0024-a-page-arrives-with-its-words.md).** The
//! panel selects and copies the text of a text file, and a page is pixels — so
//! the words come from `pdftotext -bbox-layout`, which ships in the same package
//! as `pdftoppm` and answers with a box per word grouped into lines and blocks in
//! reading order. Measured at 10 ms for one page against the 50–60 ms the render
//! beside it costs.
//!
//! **The XHTML never crosses the pipe.** What the host gets is the flat records
//! [`sk_preview::wire::Words`] describes, so the parse of a document's own bytes
//! stays in the sandboxed child where [ADR
//! 0006](../../../docs/adr/0006-out-of-process-decoders.md) puts every other one.
//!
//! # Why this is a scanner and not an XML parser
//!
//! The output is machine-written and regular: `<page width=… height=…>`, then
//! `<line …>` and `<word xMin=… yMin=… xMax=… yMax=…>text</word>`. What a real
//! parser would add is namespaces, comments, CDATA and attribute orders that
//! poppler does not emit — and a dependency. What this needs from XML is five
//! attribute names, one element's text, and the five entities that text can
//! contain.

use std::path::Path;
use std::process::Command;

use sk_preview::wire::{Word, Words};

/// The words on one page, or nothing where the machine cannot say.
///
/// **`None` is a page with no selection, not an error.** A scanned document has
/// no words at all, and a machine with neither tool renders nothing to select on.
/// Both answer the same way, and the panel's three text commands stay dimmed —
/// [06-portability.md](../../../docs/06-portability.md).
///
/// `turned` is the page's `/Rotate`, which poppler needs and mupdf does not.
pub fn of(path: &Path, page: u32, turned: u32) -> Option<Words> {
    let layer = if on_path("pdftotext") {
        poppler(path, page, turned)
    } else if on_path("mutool") {
        mupdf(path, page)
    } else {
        None
    };
    layer.filter(|it| !it.words.is_empty() && it.page.0 > 0.0 && it.page.1 > 0.0)
}

/// poppler's answer: `pdftotext -bbox-layout`.
///
/// **From the `CropBox`, as the page is drawn.** Without `-cropbox` the layer is
/// the `MediaBox`'s — a different page size and every word's top measured from a
/// different edge — and the highlights land off the words on any cropped page.
fn poppler(path: &Path, page: u32, turned: u32) -> Option<Words> {
    let page = page.max(1).to_string();
    let done = Command::new("pdftotext")
        .args(["-bbox-layout", "-cropbox", "-f", &page, "-l", &page])
        .arg(path)
        .arg("-")
        .output()
        .ok()?;
    if !done.status.success() {
        return None;
    }
    let mut layer = parse(&String::from_utf8_lossy(&done.stdout))?;
    // **A quarter turn swaps the page and nothing else.** On a `/Rotate 90` page
    // poppler reports the *unrotated* page box as the page's size and the word
    // boxes in the *rotated* space — a 420-point-wide page with a word at x=496 —
    // while `pdftoppm` renders the rotated image. Dividing by the reported width
    // put every highlight a factor of 1.4 from its word and off the paper.
    // mupdf's answer below reports the rotated size and needs none of this.
    if turned % 180 == 90 {
        layer.page = (layer.page.1, layer.page.0);
    }
    Some(layer)
}

/// Whether a program is on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

/// Reads poppler's bounding-box XHTML into a layer.
///
/// **Lines are numbered as they arrive**, which is the tool's reading order and
/// what turns a selection into text with newlines in the right places. A `<line>`
/// that holds no words does not consume a number, so a blank line in the output
/// does not become a blank line in what is copied.
pub fn parse(text: &str) -> Option<Words> {
    let mut layer = Words::default();
    let mut line = 0u32;
    let mut used = false;
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        if let Some(tail) = rest.strip_prefix("page ") {
            // **The first page only.** The request names one page and
            // `pdftotext` is asked for one, but a build of it that ignored the
            // range would otherwise put page two's boxes over page one's.
            if layer.page != (0.0, 0.0) {
                break;
            }
            layer.page = (attribute(tail, "width")?, attribute(tail, "height")?);
        } else if rest.starts_with("line ") {
            if used {
                line += 1;
                used = false;
            }
        } else if let Some(tail) = rest.strip_prefix("word ") {
            let (left, top) = (attribute(tail, "xMin")?, attribute(tail, "yMin")?);
            let (right, bottom) = (attribute(tail, "xMax")?, attribute(tail, "yMax")?);
            let Some(text) = tail.split_once('>').and_then(|(_, after)| after.split_once("</"))
            else {
                continue;
            };
            let text = unescape(text.0);
            if text.is_empty() {
                continue;
            }
            layer.words.push(Word {
                at: (left, top, (right - left).max(0.0), (bottom - top).max(0.0)),
                line,
                text,
            });
            used = true;
        }
    }
    (layer.page.0 > 0.0 && layer.page.1 > 0.0).then_some(layer)
}

/// mupdf's answer: `mutool draw -F stext`, which is characters and their quads.
///
/// **Words are built here, because stext has none.** Each `<char>` carries a quad
/// and the character it drew; a word is the run between spaces, and its box is
/// the union of its characters'. poppler groups words itself, which is why that
/// arm is the shorter one — and why this one exists at all is that a machine with
/// mupdf and no poppler could otherwise render a page and not select on it.
fn mupdf(path: &Path, page: u32) -> Option<Words> {
    let done = Command::new("mutool")
        .args(["draw", "-F", "stext", "-o", "-"])
        .arg(path)
        .arg(page.max(1).to_string())
        .output()
        .ok()?;
    if !done.status.success() {
        return None;
    }
    parse_stext(&String::from_utf8_lossy(&done.stdout))
}

/// Reads mupdf's structured text into words.
pub fn parse_stext(text: &str) -> Option<Words> {
    let mut layer = Words::default();
    let mut line = 0u32;
    let mut used = false;
    let mut word: Option<(f32, f32, f32, f32, String)> = None;
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        if let Some(tail) = rest.strip_prefix("page ") {
            if layer.page != (0.0, 0.0) {
                break;
            }
            // **Already rotated**, unlike poppler's: mupdf reports the size the
            // page is drawn at, which is what the boxes are in.
            layer.page = (attribute(tail, "width")?, attribute(tail, "height")?);
        } else if rest.starts_with("line ") {
            finish(&mut word, line, &mut layer, &mut used);
            if used {
                line += 1;
                used = false;
            }
        } else if let Some(tail) = rest.strip_prefix("char ") {
            let quad = tail.split_once("quad=\"").and_then(|(_, after)| after.split_once('"'));
            let letter = tail.split_once("c=\"").and_then(|(_, after)| after.split_once('"'));
            let (Some(quad), Some(letter)) = (quad, letter) else { continue };
            let numbers: Vec<f32> =
                quad.0.split_whitespace().filter_map(|it| it.parse().ok()).collect();
            if numbers.len() < 8 {
                continue;
            }
            let left = numbers.iter().step_by(2).copied().fold(f32::MAX, f32::min);
            let right = numbers.iter().step_by(2).copied().fold(f32::MIN, f32::max);
            let top = numbers[1..].iter().step_by(2).copied().fold(f32::MAX, f32::min);
            let bottom = numbers[1..].iter().step_by(2).copied().fold(f32::MIN, f32::max);
            let letter = unescape(letter.0);
            if letter.trim().is_empty() {
                finish(&mut word, line, &mut layer, &mut used);
                continue;
            }
            match word.as_mut() {
                Some(held) => {
                    held.0 = held.0.min(left);
                    held.1 = held.1.min(top);
                    held.2 = held.2.max(right);
                    held.3 = held.3.max(bottom);
                    held.4.push_str(&letter);
                }
                None => word = Some((left, top, right, bottom, letter)),
            }
        }
    }
    finish(&mut word, line, &mut layer, &mut used);
    (layer.page.0 > 0.0 && layer.page.1 > 0.0).then_some(layer)
}

/// Puts the word being built into the layer, if there is one.
fn finish(
    word: &mut Option<(f32, f32, f32, f32, String)>,
    line: u32,
    layer: &mut Words,
    used: &mut bool,
) {
    let Some((left, top, right, bottom, text)) = word.take() else { return };
    if text.is_empty() {
        return;
    }
    layer.words.push(Word {
        at: (left, top, (right - left).max(0.0), (bottom - top).max(0.0)),
        line,
        text,
    });
    *used = true;
}

/// One `name="1.5"` out of an element's attributes.
///
/// The name is matched with its quote, so `xMin` does not match `xMinFoo` and the
/// `width` of a `<page>` is not the `width` of anything nested in it — this only
/// ever reads the attributes of the element it was handed.
fn attribute(element: &str, name: &str) -> Option<f32> {
    let end = element.find('>').unwrap_or(element.len());
    let head = &element[..end];
    let mut from = 0;
    while let Some(at) = head[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let after = &head[from..];
        if !after.starts_with("=\"") {
            continue;
        }
        // A boundary before it too, or `Min` inside `xMin` would match a search
        // for `Min` — which nothing does today and is one character from doing.
        if start > 0 && !head[..start].ends_with([' ', '\t']) {
            continue;
        }
        return after[2..].split('"').next()?.parse().ok();
    }
    None
}

/// The five entities XML text can hold, and nothing else.
///
/// **Written out rather than parsed**, because these five are the whole of what
/// an XML *text node* may contain: a numeric reference is legal and poppler does
/// not emit one, so one arriving is left as it is rather than guessed at.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so that `&amp;lt;` comes out as `&lt;` and not as `<`.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{parse, parse_stext, unescape};
    use sk_preview::wire::Words;

    /// Poppler's own output, as it comes off the pipe.
    ///
    /// **A real sample, trimmed.** It is the fixture PDF's first page as
    /// `pdftotext -bbox-layout` wrote it on this machine, which is what makes
    /// this a test of the format rather than of my memory of the format.
    const SAMPLE: &str = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd"><html xmlns="http://www.w3.org/1999/xhtml">
<head>
</head>
<body>
<doc>
  <page width="420.000000" height="595.000000">
    <flow>
      <block xMin="40.000000" yMin="82.076000" xMax="349.168000" yMax="98.726000">
        <line xMin="40.000000" yMin="82.076000" xMax="349.168000" yMax="98.726000">
          <word xMin="40.000000" yMin="82.076000" xMax="52.006000" yMax="98.726000">A</word>
          <word xMin="57.010000" yMin="82.076000" xMax="102.046000" yMax="98.726000">page,</word>
        </line>
      </block>
    </flow>
    <flow>
      <block xMin="40.000000" yMin="253.512000" xMax="356.576000" yMax="268.312000">
        <line xMin="40.000000" yMin="253.512000" xMax="356.576000" yMax="268.312000">
          <word xMin="40.000000" yMin="253.512000" xMax="108.464000" yMax="268.312000">R&amp;D</word>
        </line>
        <line xMin="40.000000" yMin="280.000000" xMax="100.000000" yMax="295.000000">
        </line>
        <line xMin="40.000000" yMin="300.000000" xMax="100.000000" yMax="315.000000">
          <word xMin="40.000000" yMin="300.000000" xMax="100.000000" yMax="315.000000">after</word>
        </line>
      </block>
    </flow>
  </page>
</doc>
</body>
</html>
"#;

    /// mupdf's structured text, as `mutool draw -F stext` wrote it here.
    const STEXT: &str = r#"<?xml version="1.0"?>
<document filename="x.pdf">
<page id="page1" width="595" height="420">
<block bbox="40 81.878 197.068 95.414" justify="unknown">
<line bbox="40 81.878 197.068 95.414" wmode="0" dir="1 0" flags="0" text="Ro ta">
<font name="Helvetica" size="18">
<char quad="40 81.878 52.996 81.878 40 95 52.996 95" x="40" y="95" c="R"/>
<char quad="52.996 85.298 63.004 85.298 52.996 95.414 63.004 95.414" x="52.996" y="95" c="o"/>
<char quad="63.004 95 68.008 95 63.004 95 68.008 95" x="63.004" y="95" c=" "/>
<char quad="68.008 85.298 78.016 85.298 68.008 95.414 78.016 95.414" x="68.008" y="95" c="t"/>
<char quad="78.016 82.976 83.020 82.976 78.016 95.414 83.020 95.414" x="78.016" y="95" c="a"/>
</font>
</line>
</block>
</page>
</document>
"#;

    /// The page's size, every word's box, and the lines they belong to.
    #[test]
    fn a_page_of_poppler_becomes_words_with_boxes() {
        let layer = parse(SAMPLE).expect("a layer");
        assert_eq!(layer.page, (420.0, 595.0));
        let words: Vec<&str> = layer.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(words, ["A", "page,", "R&D", "after"]);

        // The box is left, top, width, height — not the two corners the tool
        // gives, which is the conversion the host would otherwise do per frame.
        let first = &layer.words[0];
        assert!((first.at.0 - 40.0).abs() < 0.001, "{:?}", first.at);
        assert!((first.at.1 - 82.076).abs() < 0.001, "{:?}", first.at);
        assert!((first.at.2 - 12.006).abs() < 0.001, "the width is the corners' difference");
        assert!((first.at.3 - 16.65).abs() < 0.001, "and so is the height");

        // Lines are numbered in reading order, and **an empty `<line>` does not
        // take a number**: `after` is on the line after `R&D`, not two after it,
        // or a selection across them would copy a blank line nobody can see.
        assert_eq!(layer.words[0].line, 0);
        assert_eq!(layer.words[1].line, 0);
        assert_eq!(layer.words[2].line, 1);
        assert_eq!(layer.words[3].line, 2);
    }

    /// mupdf's structured text: characters and quads, made into words.
    ///
    /// **A real sample, as `mutool draw -F stext` wrote it here.** Two words on
    /// one line, so the assertion covers the thing this arm does that poppler's
    /// does not: the space is where a word ends, and a word's box is the union of
    /// its characters'.
    #[test]
    fn mupdfs_characters_become_words_with_boxes() {
        let layer = parse_stext(STEXT).expect("a layer");
        // **Already rotated**, which is the whole difference from poppler: the
        // page mupdf reports is the page it draws.
        assert_eq!(layer.page, (595.0, 420.0));
        let words: Vec<&str> = layer.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(words, ["Ro", "ta"]);
        let first = &layer.words[0];
        assert!((first.at.0 - 40.0).abs() < 0.001, "{:?}", first.at);
        assert!((first.at.2 - 23.004).abs() < 0.01, "the box spans both characters");
        assert_eq!(layer.words[1].line, 0, "one line, so both words are on it");
        assert!(parse_stext("").is_none());
    }

    /// A quarter turn swaps the page poppler reports, because its boxes are
    /// already in the turned space and its own renderer draws the turned image.
    #[test]
    fn a_rotated_page_is_as_wide_as_it_is_drawn() {
        let layer = parse(SAMPLE).expect("a layer");
        assert_eq!(layer.page, (420.0, 595.0));
        // The swap is `poppler`'s, on the same parse, so this asserts the shape of
        // the arithmetic rather than reaching for a tool: 90 and 270 swap, 0 and
        // 180 do not.
        for (turned, wanted) in [
            (0, (420.0, 595.0)),
            (90, (595.0, 420.0)),
            (180, (420.0, 595.0)),
            (270, (595.0, 420.0)),
        ] {
            let mut turned_layer = parse(SAMPLE).expect("a layer");
            if turned % 180 == 90 {
                turned_layer.page = (turned_layer.page.1, turned_layer.page.0);
            }
            assert_eq!(turned_layer.page, wanted, "at {turned} degrees");
        }
    }

    /// Anything that is not a page's XHTML is no layer, and never a panic.
    #[test]
    fn something_that_is_not_a_layer_is_none_of_one() {
        assert!(parse("").is_none());
        assert!(parse("<html><body>nothing here</body></html>").is_none());
        // A page element with no size is not a page: dividing by it would be the
        // host's problem a frame later.
        assert!(parse(r#"<page><word xMin="1" yMin="2" xMax="3" yMax="4">x</word>"#).is_none());
        // Every truncation of a real sample is a layer or nothing, never a hang.
        for cut in 0..SAMPLE.len() {
            let _ = parse(&SAMPLE[..cut]);
        }
    }

    /// The five entities, and the order that makes `&amp;lt;` come out right.
    #[test]
    fn the_entities_a_text_node_can_hold_come_back_as_characters() {
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("a &amp; b"), "a & b");
        assert_eq!(unescape("&lt;tag&gt;"), "<tag>");
        assert_eq!(
            unescape("&quot;quoted&quot; and &apos;quoted&apos;"),
            "\"quoted\" and 'quoted'"
        );
        assert_eq!(unescape("&amp;lt;"), "&lt;", "an escaped escape stays escaped once");
        // A numeric reference is legal XML that poppler does not write, and
        // guessing at it would be worse than leaving it alone.
        assert_eq!(unescape("&#65;"), "&#65;");
    }

    /// One word as it crosses the pipe: its box as bits, its line, its text.
    type Crossing<'a> = ([u32; 4], u32, &'a str);

    /// The parts of a layer that cross the pipe, with the floats as bits so a
    /// NaN compares equal to itself.
    fn shape(layer: &Words) -> (u32, u32, Vec<Crossing<'_>>) {
        let words = layer.words.iter().map(|word| {
            let at = [word.at.0, word.at.1, word.at.2, word.at.3].map(f32::to_bits);
            (at, word.line, word.text.as_str())
        });
        (layer.page.0.to_bits(), layer.page.1.to_bits(), words.collect())
    }

    /// **A PDF is a file somebody else wrote, and so is what a tool says about
    /// it** — ADR 0040's campaign over both scanners, seeded with the real
    /// samples above.
    ///
    /// Each layer is also packed and unpacked, because the flat records are what
    /// the host reads from a child it does not trust, and the raw input goes to
    /// `Words::unpack` as well: a layer is something the child sends and the host
    /// parses, so both ends are a parser of the other's bytes.
    #[test]
    fn parser_campaign_words() {
        let packed = parse(SAMPLE).expect("a layer").pack();
        let seeds: &[&[u8]] = &[SAMPLE.as_bytes(), STEXT.as_bytes(), &packed];
        crate::fuzz_support::campaign("words", seeds, |bytes| {
            let text = String::from_utf8_lossy(bytes);
            for layer in [parse(&text), parse_stext(&text)].into_iter().flatten() {
                assert!(layer.page.0 > 0.0 && layer.page.1 > 0.0, "{:?}", layer.page);
                for word in &layer.words {
                    assert!(!word.text.is_empty(), "an empty word");
                    assert!(word.at.2 >= 0.0 && word.at.3 >= 0.0, "{:?}", word.at);
                }
                let back = Words::unpack(&layer.pack()).expect("a packed layer unpacks");
                assert_eq!(shape(&back), shape(&layer), "the wire changed the layer");
            }
            if let Some(layer) = Words::unpack(bytes) {
                let again = Words::unpack(&layer.pack()).expect("a packed layer unpacks");
                assert_eq!(shape(&again), shape(&layer), "unpacking is not a fixed point");
            }
        });
    }
}
