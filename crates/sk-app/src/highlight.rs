//! Which parts of a line are comments, keywords, literals and numbers.
//!
//! **The seam [`crate::quicklook::Tone`] was written for.** The panel already
//! draws each line as runs carrying a tone and colours each by its role, so a
//! highlighter is this file and nothing else: no change to how text is stored,
//! scrolled, selected or edited.
//!
//! # A scanner, not a parser
//!
//! What a reader wants out of forty lines at thirteen pixels is the *shape* of
//! the code: comments receding, strings and numbers separated from the
//! identifiers between them, and the words the language reserves standing out.
//! That is four roles, and four roles need a scanner over characters rather than
//! a grammar — which is the difference between this file and a language server.
//!
//! What it therefore does not know: that a name is a type, that a call is a call,
//! or that a `{` matches. Those are the distinctions a reader stops making at
//! this size, and each is a grammar per language.
//!
//! # What carries across a line, and what does not
//!
//! A block comment does — `/*` on one line and `*/` on another is one comment,
//! so a [`Mode`] travels from each line to the next. A **string does not**: an
//! unterminated quote colours to the end of its line and the next line starts
//! again as code, which is what every scanner of this size does and is right far
//! more often than it is wrong — a `"` in prose inside a comment would otherwise
//! tint the rest of the file.
//!
//! So a triple-quoted Python string and a Rust raw string spanning lines are
//! coloured only on their first line. Both are a state machine's worth of work
//! for a case that is rare in the forty lines a preview shows.

use crate::quicklook::{Span, Tone};

/// Which language's rules a file is read by.
///
/// **By extension, because that is what the file says about itself.** A shebang
/// is the other answer and a worse one here: the preview reads at most four
/// hundred lines and the extension is on screen in the title bar, so a `.py`
/// with a `#!/bin/sh` line is a file whose name is wrong rather than a case to
/// resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Language {
    /// No highlighting at all: prose, a log, a name nothing claims.
    #[default]
    Plain,
    Rust,
    /// C, C++, and the languages that borrowed its punctuation — Java, C#, Go,
    /// JavaScript, TypeScript, Swift, Kotlin.
    CLike,
    Python,
    /// A shell, and the configuration formats that comment with `#`.
    Shell,
}

impl Language {
    /// Which language a file name names, if any.
    pub(crate) fn of(name: &str) -> Self {
        // A name with no dot in it can still be a language's: `Makefile` and
        // `Dockerfile` are the two a directory actually holds.
        let stem = name.rsplit('/').next().unwrap_or(name);
        let lowered = stem.to_lowercase();
        if matches!(
            lowered.as_str(),
            "makefile" | "dockerfile" | "cmakelists.txt" | ".bashrc" | ".zshrc" | ".profile"
        ) {
            return Self::Shell;
        }
        match lowered.rsplit_once('.').map(|(_, extension)| extension) {
            Some("rs") => Self::Rust,
            Some(
                "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "go" | "js"
                | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "swift" | "kt" | "kts" | "scala" | "dart"
                | "php" | "glsl" | "wgsl" | "m" | "mm",
            ) => Self::CLike,
            Some("py" | "pyi" | "pyw") => Self::Python,
            Some(
                "sh" | "bash" | "zsh" | "fish" | "rb" | "pl" | "toml" | "yaml" | "yml" | "ini"
                | "cfg" | "conf" | "service" | "desktop" | "gitignore" | "env" | "mk",
            ) => Self::Shell,
            _ => Self::Plain,
        }
    }

    /// Whether this language has anything to say about a line at all.
    pub(crate) fn highlights(self) -> bool {
        self != Self::Plain
    }

    /// The two characters a line comment starts with, or one and a space.
    fn line_comment(self) -> &'static str {
        match self {
            Self::Rust | Self::CLike => "//",
            Self::Python | Self::Shell => "#",
            Self::Plain => "\0\0",
        }
    }

    /// Whether `/* … */` is a comment in it.
    fn blocks(self) -> bool {
        matches!(self, Self::Rust | Self::CLike)
    }

    /// Whether a single quote opens a string rather than a character.
    ///
    /// **The distinction that keeps a Rust lifetime from turning green.** `'a`
    /// in `&'a str` is not a literal and never closes, so in Rust and the
    /// C-likes a single quote opens a *character* — at most a few bytes and an
    /// escape — while in Python and a shell it opens a string like `"` does.
    fn single_quotes_a_string(self) -> bool {
        matches!(self, Self::Python | Self::Shell)
    }

    /// The words this language reserves.
    ///
    /// **A table per language, not a union across them.** A union highlights
    /// `fn` in C, where it is somebody's function pointer, and a word coloured as
    /// a keyword where it is an identifier is a wrong answer on screen rather
    /// than a missing one.
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::CLike => &[
                "auto",
                "bool",
                "break",
                "case",
                "catch",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "double",
                "else",
                "enum",
                "export",
                "extends",
                "extern",
                "false",
                "final",
                "float",
                "for",
                "func",
                "function",
                "goto",
                "if",
                "import",
                "inline",
                "int",
                "interface",
                "let",
                "long",
                "namespace",
                "new",
                "null",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "template",
                "this",
                "throw",
                "true",
                "try",
                "typedef",
                "typeof",
                "union",
                "unsigned",
                "var",
                "virtual",
                "void",
                "volatile",
                "while",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "false", "fi", "for",
                "function", "if", "in", "local", "readonly", "return", "set", "then", "true",
                "unset", "until", "while",
            ],
            Self::Plain => &[],
        }
    }
}

/// What a line begins in, carried from the line before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Mode {
    /// Ordinary code.
    #[default]
    Code,
    /// Inside a `/* … */` that began on an earlier line.
    Block,
}

/// Cuts every line into runs carrying a tone.
///
/// **The whole file, every time.** A preview reads at most four hundred lines —
/// `quicklook::LINES` — so re-scanning all of them after a keystroke is tens of
/// microseconds, and it is the only way a block comment can be right: a `*/`
/// typed on line 12 changes the colour of lines 13 to 400, which a per-line
/// re-tone cannot know.
pub(crate) fn all(lines: &[String], language: Language) -> Vec<Vec<Span>> {
    let mut mode = Mode::Code;
    lines
        .iter()
        .map(|line| {
            let (spans, next) = line_spans(line, language, mode);
            mode = next;
            spans
        })
        .collect()
}

/// One line's runs, and what the line after it begins in.
///
/// Every byte of the line is covered exactly once, in order, which is what the
/// drawing slices by — a gap would be text nobody sees and an overlap would be
/// text drawn twice.
fn line_spans(line: &str, language: Language, mode: Mode) -> (Vec<Span>, Mode) {
    if !language.highlights() || line.is_empty() {
        return (vec![Span { from: 0, to: line.len(), tone: Tone::Plain }], mode);
    }
    let bytes = line.as_bytes();
    let mut spans: Vec<Span> = Vec::new();
    let mut mode = mode;
    let mut at = 0;
    let mut plain_from = 0;
    // A run of ordinary code is closed when something else starts, so that the
    // spans come out in order with no gaps.
    let flush = |spans: &mut Vec<Span>, from: usize, to: usize| {
        if to > from {
            spans.push(Span { from, to, tone: Tone::Plain });
        }
    };
    while at < bytes.len() {
        if mode == Mode::Block {
            let end = line[at..].find("*/").map_or(bytes.len(), |found| at + found + 2);
            spans.push(Span { from: at, to: end, tone: Tone::Comment });
            mode = if end == bytes.len() && !line[at..].contains("*/") {
                Mode::Block
            } else {
                Mode::Code
            };
            at = end;
            plain_from = at;
            continue;
        }
        let rest = &line[at..];
        if rest.starts_with(language.line_comment()) {
            flush(&mut spans, plain_from, at);
            spans.push(Span { from: at, to: bytes.len(), tone: Tone::Comment });
            return (spans, Mode::Code);
        }
        if language.blocks() && rest.starts_with("/*") {
            flush(&mut spans, plain_from, at);
            mode = Mode::Block;
            continue;
        }
        let byte = bytes[at];
        if byte == b'"' || (byte == b'\'' && language.single_quotes_a_string()) {
            flush(&mut spans, plain_from, at);
            let end = closing(bytes, at, byte);
            spans.push(Span { from: at, to: end, tone: Tone::Literal });
            at = end;
            plain_from = at;
            continue;
        }
        if byte == b'\'' && !language.single_quotes_a_string() {
            // A character literal is `'x'` or `'\n'`; anything longer is a
            // lifetime or a stray quote and stays as code.
            if let Some(end) = character(bytes, at) {
                flush(&mut spans, plain_from, at);
                spans.push(Span { from: at, to: end, tone: Tone::Literal });
                at = end;
                plain_from = at;
                continue;
            }
            at += 1;
            continue;
        }
        if byte.is_ascii_digit() && starts_a_word(bytes, at) {
            flush(&mut spans, plain_from, at);
            let end = number(bytes, at);
            spans.push(Span { from: at, to: end, tone: Tone::Number });
            at = end;
            plain_from = at;
            continue;
        }
        if is_word(byte) && starts_a_word(bytes, at) {
            let end = word(bytes, at);
            if language.keywords().contains(&&line[at..end]) {
                flush(&mut spans, plain_from, at);
                spans.push(Span { from: at, to: end, tone: Tone::Keyword });
                plain_from = end;
            }
            at = end;
            continue;
        }
        at += 1;
    }
    flush(&mut spans, plain_from, bytes.len());
    if spans.is_empty() {
        spans.push(Span { from: 0, to: bytes.len(), tone: Tone::Plain });
    }
    (spans, mode)
}

/// Where a quoted run ends, honouring backslash escapes.
///
/// The end of the line where nothing closes it, which is the documented
/// single-line rule: a string is not carried to the next line.
fn closing(bytes: &[u8], from: usize, quote: u8) -> usize {
    let mut at = from + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            byte if byte == quote => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Where a character literal ends, or nothing if this quote does not open one.
fn character(bytes: &[u8], from: usize) -> Option<usize> {
    let escaped = bytes.get(from + 1) == Some(&b'\\');
    let end = if escaped { from + 3 } else { from + 2 };
    (bytes.get(end) == Some(&b'\'')).then_some(end + 1)
}

/// Where a number ends: digits, and what a number may hold between them.
fn number(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        let byte = bytes[at];
        let inside = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.';
        if !inside {
            break;
        }
        at += 1;
    }
    at
}

/// Where a word ends.
fn word(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() && is_word(bytes[at]) {
        at += 1;
    }
    at
}

/// Whether a byte can be part of a word.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether a word or a number *starts* here rather than continuing one.
///
/// **So `x2` is not a number and `let_go` is not `let`.** A keyword is a whole
/// word, which means the byte before it has to be one that cannot be part of
/// one — the check a table lookup cannot make for itself.
fn starts_a_word(bytes: &[u8], at: usize) -> bool {
    at == 0 || !is_word(bytes[at - 1])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Language, all};
    use crate::quicklook::Tone;

    /// The tones of one line, as `(text, tone)` pairs.
    fn toned(line: &str, language: Language) -> Vec<(String, Tone)> {
        let lines = vec![line.to_owned()];
        all(&lines, language)[0]
            .iter()
            .map(|span| (line[span.from..span.to].to_owned(), span.tone))
            .collect()
    }

    /// **Every line is covered by its runs exactly once**, whatever the language:
    /// the drawing slices a line by its spans, so a gap is text nobody sees and
    /// an overlap is text drawn twice.
    #[test]
    fn every_byte_of_every_line_is_covered_exactly_once() {
        let lines: Vec<String> = [
            "fn main() { let x = 42; }",
            "/* a block",
            "   still a block */ let y = \"s\";",
            "# a hash comment",
            "print('it', 3.14)  # trailing",
            "",
            "  ",
            "let s = \"a // b\"; // real",
            "let c = 'x'; let r: &'a str = \"\";",
            "no_keywords_here(x2, y_3)",
            "0x1f + 1_000 + 2.5e3",
        ]
        .iter()
        .map(|it| (*it).to_owned())
        .collect();
        for language in
            [Language::Plain, Language::Rust, Language::CLike, Language::Python, Language::Shell]
        {
            let spans = all(&lines, language);
            assert_eq!(spans.len(), lines.len());
            for (line, runs) in lines.iter().zip(&spans) {
                let mut at = 0;
                for run in runs {
                    assert_eq!(run.from, at, "a gap or an overlap in {line:?} for {language:?}");
                    assert!(run.to >= run.from);
                    assert!(line.is_char_boundary(run.from) && line.is_char_boundary(run.to));
                    at = run.to;
                }
                assert_eq!(at, line.len(), "{line:?} for {language:?} stops short");
            }
        }
    }

    /// A keyword is a whole word, a string is a string, and a number is a number.
    #[test]
    fn the_four_roles_are_what_they_say() {
        let runs = toned("let x = 42; // why", Language::Rust);
        assert_eq!(runs[0], ("let".to_owned(), Tone::Keyword));
        assert!(runs.iter().any(|(text, tone)| text == "42" && *tone == Tone::Number));
        assert!(runs.iter().any(|(text, tone)| text == "// why" && *tone == Tone::Comment));

        // **A keyword only when it is the whole word.** `let_go` and `x2` are the
        // two shapes that catch a table lookup with no boundary check.
        let runs = toned("let_go(x2)", Language::Rust);
        assert!(runs.iter().all(|(_, tone)| *tone == Tone::Plain), "{runs:?}");

        // A comment marker inside a string is not a comment.
        let runs = toned("let s = \"a // b\"; // real", Language::Rust);
        assert!(runs.iter().any(|(text, tone)| text == "\"a // b\"" && *tone == Tone::Literal));
        assert!(runs.iter().any(|(text, tone)| text == "// real" && *tone == Tone::Comment));

        // And a quote inside a comment does not tint the rest of the line.
        let runs = toned("// it's fine", Language::Rust);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, Tone::Comment);
    }

    /// A lifetime is not a string, and a character literal is.
    #[test]
    fn a_rust_lifetime_stays_code_and_a_character_does_not() {
        let runs = toned("fn f<'a>(s: &'a str) -> char { 'x' }", Language::Rust);
        let literals: Vec<&String> =
            runs.iter().filter(|(_, tone)| *tone == Tone::Literal).map(|(text, _)| text).collect();
        assert_eq!(literals, [&"'x'".to_owned()], "{runs:?}");

        // In a shell or Python the same quote opens a string, because neither has
        // a lifetime and both quote with it.
        let runs = toned("echo 'it is'", Language::Shell);
        assert!(runs.iter().any(|(text, tone)| text == "'it is'" && *tone == Tone::Literal));
    }

    /// A block comment carries to the lines after it, and stops where it closes.
    #[test]
    fn a_block_comment_crosses_lines_and_ends_where_it_ends() {
        let lines: Vec<String> = ["let a = 1; /* here", "still", "*/ let b = 2;", "let c = 3;"]
            .iter()
            .map(|it| (*it).to_owned())
            .collect();
        let spans = all(&lines, Language::Rust);
        // The first line: code, then the comment to its end.
        assert_eq!(spans[0].last().map(|it| it.tone), Some(Tone::Comment));
        // The second: all comment.
        assert_eq!(spans[1].len(), 1);
        assert_eq!(spans[1][0].tone, Tone::Comment);
        // The third: the comment closes and `let` is a keyword again.
        assert_eq!(spans[2][0].tone, Tone::Comment);
        assert!(spans[2].iter().any(|it| it.tone == Tone::Keyword), "{:?}", spans[2]);
        // The fourth: ordinary code, which is the assertion that the mode was
        // put back rather than carried.
        assert!(spans[3].iter().any(|it| it.tone == Tone::Keyword));
        assert!(spans[3].iter().all(|it| it.tone != Tone::Comment));
    }

    /// The language comes from the name, and an unknown name highlights nothing.
    #[test]
    fn the_language_is_the_extension_and_two_names_without_one() {
        assert_eq!(Language::of("main.rs"), Language::Rust);
        assert_eq!(Language::of("a/b/main.c"), Language::CLike);
        assert_eq!(Language::of("Widget.tsx"), Language::CLike);
        assert_eq!(Language::of("setup.py"), Language::Python);
        assert_eq!(Language::of("Makefile"), Language::Shell);
        assert_eq!(Language::of(".bashrc"), Language::Shell);
        assert_eq!(Language::of("skerry.toml"), Language::Shell);
        assert_eq!(Language::of("notes.txt"), Language::Plain);
        assert_eq!(Language::of("README"), Language::Plain);
        assert!(!Language::Plain.highlights());
        // A plain file is one run per line, which is what it was before any of
        // this existed.
        let lines = vec!["let x = 42; // why".to_owned()];
        assert_eq!(all(&lines, Language::Plain)[0].len(), 1);
    }
}
