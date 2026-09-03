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
/// **`None` is a page with no selection, not an error.** A machine with `mutool`
/// and no poppler renders pages and cannot say where their words are; a scanned
/// document has no words at all. Both answer the same way, and the panel's three
/// text commands stay dimmed — [06-portability.md](../../../docs/06-portability.md).
pub fn of(path: &Path, page: u32) -> Option<Words> {
    if !on_path("pdftotext") {
        return None;
    }
    let page = page.max(1).to_string();
    let done = Command::new("pdftotext")
        .args(["-bbox-layout", "-f", &page, "-l", &page])
        .arg(path)
        .arg("-")
        .output()
        .ok()?;
    if !done.status.success() {
        return None;
    }
    let layer = parse(&String::from_utf8_lossy(&done.stdout));
    layer.filter(|it| !it.words.is_empty())
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

    use super::{parse, unescape};

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
}
