//! Subsequence matching with a score, for the palette over the registry.
//!
//! Rule 4 is that a feature nobody can find has no users. The registry has been
//! the single list since before there was a window; the palette is the surface
//! that makes it findable, and this is the ranking underneath it.
//!
//! Written here rather than taken from a crate: it is forty lines, it has to
//! score two fields of a [`crate::registry::Described`] against one query, and
//! the alternative is a dependency whose row in
//! [07-dependencies.md](../../../docs/07-dependencies.md) would have to argue
//! that forty lines are worth carrying.

/// How well `query` matches `candidate`, or `None` if it does not match at all.
///
/// A match means every character of `query` appears in `candidate`, in order,
/// case-insensitively — the rule every palette a person has used follows, so `nf`
/// finds New folder and `hid` finds Show hidden files.
///
/// Higher is better. Every candidate that matches at all matches every
/// character, so the score is about *where* the matches land:
///
/// * **A word start** — the first character, or one after a space, a hyphen, an
///   underscore, a dot, a slash or a colon — is the strongest signal, because it
///   is how initials work.
/// * **A run** of adjacent characters, which is how a prefix wins.
/// * **A shorter candidate**, so two commands that both contain the query are
///   ranked by how much of them the query is.
///
/// **The best alignment, not the first one.** A left-to-right greedy walk takes
/// the earliest occurrence of each character, which is usually the wrong one:
/// `hid` against *Show hidden files* greedily matches the `h` inside *Show*,
/// scores no word-start bonus at all, and loses to *Show the sidebar* on length.
/// So this is a small dynamic program over query positions against candidate
/// positions — `O(query × candidate)` over strings of a few dozen characters
/// each, which is nothing next to the frame it runs in.
#[must_use]
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let wanted: Vec<char> = query.chars().map(lower).collect();
    let target: Vec<char> = candidate.chars().map(lower).collect();
    if wanted.len() > target.len() {
        return None;
    }
    let boundaries = word_starts(candidate);

    // `best[j]` is the best score for the query characters matched so far, with
    // the last of them landing exactly at candidate position `j`; `None` where
    // that is not reachable. One row per query character, kept as two rows.
    let mut previous: Vec<Option<i32>> = vec![None; target.len()];
    let mut current: Vec<Option<i32>> = vec![None; target.len()];

    for (step, character) in wanted.iter().enumerate() {
        current.fill(None);
        // The best score reachable by the previous query character at any
        // position strictly before the one under consideration, carried along so
        // the inner loop stays constant time.
        let mut carried: Option<i32> = None;
        for position in 0..target.len() {
            if position > 0
                && let Some(before) = previous[position - 1]
            {
                carried = Some(carried.map_or(before, |it: i32| it.max(before)));
            }
            if target[position] != *character {
                continue;
            }
            let landing = if boundaries[position] { BOUNDARY } else { 0 };
            current[position] = if step == 0 {
                Some(landing)
            } else {
                // Adjacent to the previous match, or anywhere before it. Adjacent
                // earns the run bonus, which is what makes a prefix win.
                let adjacent = position
                    .checked_sub(1)
                    .and_then(|at| previous[at])
                    .map(|score| score + landing + RUN);
                let apart = carried.map(|score| score + landing);
                match (adjacent, apart) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                }
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }

    let best = previous.iter().flatten().copied().max()?;
    let length = i32::try_from(target.len()).unwrap_or(i32::MAX);
    Some(best - length)
}

/// For a match at the start of a word, which is what makes initials rank.
const BOUNDARY: i32 = 12;
/// For a match adjacent to the one before it, which is what makes a prefix rank.
const RUN: i32 = 8;

fn lower(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

/// Which positions start a word: the first, and any after a separator.
fn word_starts(candidate: &str) -> Vec<bool> {
    let mut out = Vec::with_capacity(candidate.len());
    let mut previous = None;
    for character in candidate.chars() {
        out.push(match previous {
            None => true,
            Some(before) => matches!(before, ' ' | '-' | '_' | '.' | '/' | ':'),
        });
        previous = Some(character);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::score;

    /// The order a palette has to produce, written as the ranking rather than as
    /// numbers: what matters is which command is offered first, and a score is
    /// only a means to it.
    fn ranked(query: &str, candidates: &[&str]) -> Vec<String> {
        let mut hits: Vec<(i32, &str)> =
            candidates.iter().filter_map(|it| score(query, it).map(|s| (s, *it))).collect();
        // Descending score, then the candidate itself, so a tie is not arbitrary.
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        hits.into_iter().map(|(_, it)| it.to_owned()).collect()
    }

    #[test]
    fn every_character_has_to_appear_in_order() {
        assert!(score("nf", "New folder").is_some());
        // In order, and there is no `n` after the `f`.
        assert!(score("fn", "New folder").is_none());
        assert!(score("nfx", "New folder").is_none());
        assert!(score("zz", "New folder").is_none());
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn case_is_not_part_of_the_match() {
        assert_eq!(score("NF", "New folder"), score("nf", "new FOLDER"));
    }

    /// Initials are how a palette is actually used, so a match at word starts has
    /// to beat the same letters found in the middle of words.
    #[test]
    fn initials_outrank_letters_buried_in_words() {
        let best = ranked("nf", &["New folder", "Undo the last change"]);
        assert_eq!(best.first().map(String::as_str), Some("New folder"));
    }

    /// A prefix is the strongest signal there is: somebody typing `cop` wants
    /// Copy, not something that merely contains those three letters.
    #[test]
    fn a_prefix_outranks_a_scattered_match() {
        let best = ranked("copy", &["Copy", "Copy the path", "Close the pane, only yesterday"]);
        assert_eq!(best.first().map(String::as_str), Some("Copy"));
    }

    #[test]
    fn a_shorter_candidate_wins_a_tie() {
        let best = ranked("copy", &["Copy the path", "Copy"]);
        assert_eq!(best, vec!["Copy", "Copy the path"]);
    }

    /// A compact match beats one scattered over a longer name.
    #[test]
    fn a_compact_match_beats_a_scattered_one() {
        let tight = score("newf", "New folder").expect("matches");
        let scattered = score("newf", "Never mind what else, forget it").expect("matches");
        assert!(tight > scattered, "tight {tight} scattered {scattered}");
    }

    /// The defect that replaced the greedy walk with an alignment, kept as the
    /// case rather than as a comment. Greedily, `hid` takes the `h` inside *Show*,
    /// earns no word-start bonus, and loses to a longer name on length alone.
    #[test]
    fn the_best_alignment_wins_and_not_the_first_one() {
        let best = ranked("hid", &["Show hidden files", "Show the sidebar"]);
        assert_eq!(best.first().map(String::as_str), Some("Show hidden files"));
    }

    /// Initials across three words, which is the query a palette gets most.
    #[test]
    fn initials_across_words_find_the_command() {
        let best = ranked("ctp", &["Copy the path as text", "Clear selection", "Close the window"]);
        assert_eq!(best.first().map(String::as_str), Some("Copy the path as text"));
    }
}
