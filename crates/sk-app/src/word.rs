// SPDX-License-Identifier: MIT OR Apache-2.0
//! The run of text a double click takes, wherever text can be selected.
//!
//! Three surfaces need this and each of them used to have nothing: a field being
//! typed into, a file's text in the preview panel, and a page's words — which
//! already had it, because a PDF arrives pre-segmented and a word box is the unit
//! the layer carries ([ADR
//! 0024](../../../docs/adr/0024-a-page-arrives-with-its-words.md)). For the other
//! two the segmentation is ours, and it is one rule so that a double click means
//! the same thing in a rename as it does in a preview.
//!
//! **A run of one kind of character, not "letters up to a space".** Letters and
//! digits and `_` are one kind, whitespace is another, and everything left over —
//! brackets, operators, a path's separators — is a third. So a double click on
//! `holiday_2024` takes all of it, on the `.` before `jpeg` takes the dot, and in
//! the middle of an indent takes the indent. That is what a double click does in
//! every editor and every browser, and matching it is the whole specification.
//!
//! **This is deliberately not [`crate::field::is_separator`]**, which is the rule
//! `Ctrl ←` follows inside a name. The two answer different questions: that one is
//! *how many presses does it take to cross this filename*, where `holiday-2024`
//! being two words saves a press, and this one is *what am I pointing at*, where
//! it would take half of what the pointer is on.

/// What one character is, for the purpose of the run around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A letter, a digit, or `_` — one word of prose, code or a filename.
    Word,
    /// Whitespace of any kind.
    Space,
    /// Everything else: punctuation, brackets, operators, separators.
    Mark,
}

/// Which of the three a character is.
///
/// `is_alphanumeric` rather than `is_ascii_alphanumeric`, because `größe` and
/// `日本語` are words to the person double-clicking them.
fn kind_of(letter: char) -> Kind {
    if letter.is_alphanumeric() || letter == '_' {
        Kind::Word
    } else if letter.is_whitespace() {
        Kind::Space
    } else {
        Kind::Mark
    }
}

/// The run of one kind of character around `at`, as byte offsets into `text`.
///
/// `at` is a caret position — between two characters — so the two neighbours
/// decide which run is meant. **A word behind the pointer wins.** Clicking the
/// right-hand half of the `o` in `hello world` lands the caret after it, and a
/// rule that read forward from there would select the space; every text view
/// resolves the tie the same way, towards the word.
///
/// Both ends are character boundaries, and so is anything derived from them: a
/// caret offset that lands inside `é` is snapped back rather than sliced.
pub(crate) fn around(text: &str, at: usize) -> (usize, usize) {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let behind = text[..at].chars().next_back().map(kind_of);
    let ahead = text[at..].chars().next().map(kind_of);
    let kind = match (behind, ahead) {
        (Some(Kind::Word), _) | (None, Some(Kind::Word)) => Kind::Word,
        (_, Some(kind)) | (Some(kind), None) => kind,
        (None, None) => return (0, 0),
    };
    let from = text[..at]
        .char_indices()
        .rev()
        .find(|(_, letter)| kind_of(*letter) != kind)
        .map_or(0, |(index, letter)| index + letter.len_utf8());
    let to = text[at..]
        .char_indices()
        .find(|(_, letter)| kind_of(*letter) != kind)
        .map_or(text.len(), |(index, _)| at + index);
    (from, to)
}

#[cfg(test)]
mod tests {
    use super::around;

    #[test]
    fn a_word_is_letters_digits_and_underscores() {
        let text = "let holiday_2024 = 7;";
        assert_eq!(&text[around(text, 6).0..around(text, 6).1], "holiday_2024");
        // Every offset inside the word, and both of its edges, give the same run.
        for at in 4..=16 {
            assert_eq!(around(text, at), (4, 16), "at {at}");
        }
    }

    /// The tie at a boundary goes to the word, which is what a click on the last
    /// half of a letter means.
    #[test]
    fn a_word_behind_the_pointer_wins_the_boundary() {
        let text = "hello world";
        assert_eq!(around(text, 5), (0, 5));
        assert_eq!(around(text, 6), (6, 11));
    }

    /// A click on the space between two words takes the space, not either word.
    #[test]
    fn a_run_of_spaces_is_a_run() {
        let text = "foo    bar";
        assert_eq!(around(text, 5), (3, 7));
    }

    /// And a run of punctuation is one too — `);` is what a click on it takes.
    #[test]
    fn a_run_of_marks_is_a_run() {
        let text = "call(x);";
        assert_eq!(around(text, 7), (6, 8));
        assert_eq!(around(text, 4), (0, 4));
    }

    /// Past the last character there is nothing ahead, so the run behind is meant.
    #[test]
    fn a_click_past_the_end_takes_the_last_run() {
        let text = "hello world";
        assert_eq!(around(text, text.len()), (6, 11));
        assert_eq!(around(text, 900), (6, 11));
    }

    /// Nothing at all is an empty run rather than a panic.
    #[test]
    fn an_empty_line_has_an_empty_run() {
        assert_eq!(around("", 0), (0, 0));
        assert_eq!(around("", 7), (0, 0));
    }

    /// A two-byte letter is part of a word and never split — the defect that
    /// took the window down in `highlight.rs` was this arithmetic, one module
    /// over.
    #[test]
    fn a_multibyte_letter_is_part_of_the_word() {
        let text = "let Žofia = größe;";
        let (from, to) = around(text, 5);
        assert_eq!(&text[from..to], "Žofia");
        let (from, to) = around(text, 15);
        assert_eq!(&text[from..to], "größe");
        // An offset inside `Ž` is snapped back rather than sliced.
        let (from, to) = around(text, 5);
        assert_eq!((from, to), around(text, 4));
        let cjk = "日本語 x";
        let (from, to) = around(cjk, 3);
        assert_eq!(&cjk[from..to], "日本語");
    }

    /// Every offset in a line gives a run whose ends are character boundaries.
    #[test]
    fn every_offset_gives_a_run_on_boundaries() {
        for text in ["", " ", "a", "日本語 = \"Žofia\";  # größe", "\t\tif (a_b == 1) {"] {
            for at in 0..=text.len() + 2 {
                let (from, to) = around(text, at);
                assert!(from <= to && to <= text.len(), "{text:?} at {at}");
                assert!(text.is_char_boundary(from) && text.is_char_boundary(to), "{text:?} {at}");
            }
        }
    }
}
