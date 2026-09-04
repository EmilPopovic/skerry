// SPDX-License-Identifier: MIT OR Apache-2.0
//! Every user-visible string, resolved by key from an embedded catalogue.
//!
//! A hundred lines instead of a localization framework, for the reason
//! [docs/07-dependencies.md](../../../docs/07-dependencies.md) gives: the
//! framework is a dependency and a build step, and this is a lookup. The
//! discipline it buys is worth more than the mechanism — a key cannot be
//! assembled by concatenating two sentences, so the catalogue cannot contain a
//! string that is untranslatable by construction.
//!
//! The catalogue is parsed once, on the first lookup, into a map. Parsing at
//! first use rather than at build time keeps the catalogue a text file a
//! translator can edit; the cost is a single pass over a few kilobytes, which
//! happens before the first frame and is charged to start-up, not to a frame.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Looks up a message by key, falling back to the key itself.
///
/// The fallback is deliberate and visible: a missing key renders as
/// `status.counts` on screen, which is a bug report a user can read out loud.
/// It is not an empty string, which is a bug report nobody notices.
#[must_use]
pub fn lookup(key: &'static str) -> &'static str {
    catalogue().get(key).copied().unwrap_or(key)
}

/// Looks up the form of `key` that agrees with `count`, and interpolates `args`.
///
/// **[ADR 0016](../../../docs/adr/0016-plural-forms-are-bracketed-keys-in-the-catalogue.md).**
/// The forms live under bracketed keys and the catalogue names the rule that picks
/// between them:
///
/// ```text
/// plural.rule = one-other
/// properties.items[one]   = {0} item
/// properties.items[other] = {0} items
/// ```
///
/// **`count` is its own argument and not `args[0]`.** Which number governs
/// agreement is per string: `{0} items` agrees with the first and `{0} of {1} items
/// match` agrees with the *second*, so a mechanism that read it off the arguments
/// would be right by coincidence in half the catalogue.
///
/// Falls back to `[other]` and then to the bare key, so a translation missing a
/// form reads as a slightly wrong sentence rather than as `properties.items[few]`.
#[must_use]
pub fn plural(key: &'static str, count: u64, args: &[&dyn std::fmt::Display]) -> String {
    interpolate(form(key, count), args)
}

/// The line for `key` in the category `count` falls in.
fn form(key: &'static str, count: u64) -> &'static str {
    let categories = catalogue();
    let wanted = category(categories.get("plural.rule").copied().unwrap_or(ENGLISH), count);
    // Two allocations at most, on a path that runs once per counted string per
    // frame — measured against the shaping that follows it, which is orders more.
    for category in [wanted, "other"] {
        if let Some(line) = categories.get(format!("{key}[{category}]").as_str()) {
            return line;
        }
    }
    lookup(key)
}

/// The rule every catalogue is assumed to use when it names none.
const ENGLISH: &str = "one-other";

/// Which CLDR category `count` falls in, under the rule named `rule`.
///
/// **One rule, and the seam is the name rather than a table.** Arms for languages
/// with no catalogue would be code with no test; a rule this build does not
/// implement is caught by `the_catalogue_names_a_plural_rule_this_build_knows`
/// rather than by a silent fall through, because falling through to `other` would
/// put `1 items` back on screen and call it localization.
fn category(rule: &str, count: u64) -> &'static str {
    match rule {
        "one-other" if count == 1 => "one",
        _ => "other",
    }
}

/// Whether `rule` is one [`category`] actually implements.
#[must_use]
pub fn knows_plural_rule(rule: &str) -> bool {
    matches!(rule, "one-other")
}

/// Looks up a message and substitutes `{0}`, `{1}`, … from `args`.
///
/// Positional rather than in-order, because a translation reorders clauses: the
/// German for "3 items, 1 selected" puts the count last.
#[must_use]
pub fn format(key: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    interpolate(lookup(key), args)
}

fn interpolate(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push('{');
            rest = after;
            break;
        };
        let body = &after[..close];
        if let Some(arg) = body.parse::<usize>().ok().and_then(|index| args.get(index)) {
            let _ = write!(out, "{arg}");
        } else {
            // A placeholder with no argument is left as written, so the gap is
            // visible on screen rather than silently closed.
            out.push('{');
            out.push_str(body);
            out.push('}');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn catalogue() -> &'static HashMap<&'static str, &'static str> {
    static PARSED: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    PARSED.get_or_init(|| parse(include_str!("../catalogue/en.txt")))
}

fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim(), value.trim());
        }
    }
    map
}

/// Resolves a catalogue key, with optional positional arguments.
///
/// ```ignore
/// let header = t!("column.name");                       // &'static str
/// let asking = t!("status.loading", total);              // String
/// // A key with plural forms needs `tp!` — `t!` on one draws the key.
/// ```
#[macro_export]
macro_rules! t {
    ($key:literal) => { $crate::i18n::lookup($key) };
    ($key:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format($key, &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

/// Resolves the form of a catalogue key that agrees with a count.
///
/// The count is given twice on purpose — once to choose the form and once as an
/// argument — because those are two jobs and a string may not interpolate the
/// number it agrees with at all. See [ADR
/// 0016](../../../docs/adr/0016-plural-forms-are-bracketed-keys-in-the-catalogue.md).
///
/// ```ignore
/// let items = tp!("properties.items", n, n);          // "1 item" / "9 items"
/// let matched = tp!("status.filtered", total, shown, total, query);
/// ```
#[macro_export]
macro_rules! tp {
    ($key:literal, $count:expr) => { $crate::i18n::plural($key, $count as u64, &[]) };
    ($key:literal, $count:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::plural(
            $key,
            $count as u64,
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{catalogue, interpolate, lookup};

    #[test]
    fn the_catalogue_parses_and_has_no_empty_values() {
        let map = catalogue();
        assert!(map.len() > 10, "parsed {} keys", map.len());
        for (key, value) in map {
            assert!(!value.is_empty(), "`{key}` has no text");
            assert!(!key.contains(' '), "`{key}` is not a key");
        }
    }

    /// A key with no line in the catalogue shows as the key. That is the fallback
    /// a translator's incomplete file needs, and it is the reason
    /// `tools/check-invariants.py` fails the build on a key that is not there —
    /// nothing at run time will.
    #[test]
    fn a_missing_key_renders_as_itself() {
        assert_eq!(lookup("nothing.here"), "nothing.here"); // skerry-allow: missing-string
    }

    #[test]
    fn placeholders_are_positional_and_may_be_reordered() {
        let args: [&dyn std::fmt::Display; 2] = [&"a", &7];
        assert_eq!(interpolate("{0} then {1}", &args), "a then 7");
        assert_eq!(interpolate("{1} first, {0} after", &args), "7 first, a after");
        assert_eq!(interpolate("{0} twice {0}", &args), "a twice a");
    }

    #[test]
    fn a_placeholder_with_no_argument_stays_visible() {
        let args: [&dyn std::fmt::Display; 1] = [&1];
        assert_eq!(interpolate("{0} and {4}", &args), "1 and {4}");
        assert_eq!(interpolate("unclosed {0", &args), "unclosed {0");
    }

    /// **The rule the catalogue names has to be one `category` implements.**
    ///
    /// Not a fall through to `other`, which would put `1 items` back on screen and
    /// call it localization — see [ADR
    /// 0016](../../../docs/adr/0016-plural-forms-are-bracketed-keys-in-the-catalogue.md).
    #[test]
    fn the_catalogue_names_a_plural_rule_this_build_knows() {
        let named = catalogue().get("plural.rule").copied().unwrap_or(super::ENGLISH);
        assert!(super::knows_plural_rule(named), "no rule called `{named}` is implemented");
    }

    /// Every plural key has an `[other]` form, and no bracket holds a category
    /// that is not one of CLDR's six.
    ///
    /// **The `[other]` half is the load-bearing one.** It is what a count in a
    /// category this catalogue has no line for falls back to, so a key with only
    /// `[one]` renders as the key itself for every count but 1 — which is the
    /// failure mode a translation in progress hits first.
    #[test]
    fn every_plural_key_has_an_other_form_and_a_real_category() {
        const CLDR: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];
        let mut bracketed: Vec<(&str, &str)> = Vec::new();
        for key in catalogue().keys() {
            if let Some((stem, rest)) = key.split_once('[') {
                let category =
                    rest.strip_suffix(']').unwrap_or_else(|| panic!("`{key}` has no closing ]"));
                assert!(CLDR.contains(&category), "`{key}` names no CLDR category");
                bracketed.push((stem, category));
            }
        }
        assert!(bracketed.len() >= 12, "only {} plural forms parsed", bracketed.len());
        let mut stems: Vec<&str> = bracketed.iter().map(|(stem, _)| *stem).collect();
        stems.sort_unstable();
        stems.dedup();
        for stem in stems {
            assert!(
                bracketed.iter().any(|(at, category)| *at == stem && *category == "other"),
                "`{stem}` has plural forms but no [other]"
            );
            // And no bare line beside them: two spellings of one key is two
            // answers, and whichever `lookup` finds first is the one on screen.
            assert!(catalogue().get(stem).is_none(), "`{stem}` has plural forms *and* a bare line");
        }
    }

    /// One and not-one, through the macro a call site uses.
    #[test]
    fn a_count_of_one_agrees_with_its_noun() {
        assert_eq!(tp!("properties.items", 1, 1), "1 item");
        assert_eq!(tp!("properties.items", 9, 9), "9 items");
        assert_eq!(tp!("properties.items", 0, 0), "0 items");
        // The status line, where the count that governs is not the only argument.
        assert_eq!(tp!("status.counts", 1, 1, 1), "1 item, 1 selected");
        assert_eq!(tp!("status.counts", 3, 3, 1), "3 items, 1 selected");
        // And where it is the *second* argument: "1 of 1 item matches".
        assert_eq!(tp!("status.filtered", 1, 1, 1, "a"), "1 of 1 item matches “a”");
        assert_eq!(tp!("status.filtered", 9, 1, 9, "a"), "1 of 9 items match “a”");
    }

    /// A key with no forms at all falls back to its bare line, and a key with
    /// forms falls back to `[other]` for a category the catalogue does not carry.
    #[test]
    fn a_plural_of_a_key_with_no_forms_is_the_key() {
        // `search.running` has no noun to agree with and no bracketed line.
        assert_eq!(super::form("search.running", 1), lookup("search.running"));
        // `two` is a real category with no line here, so `[other]` answers.
        assert_eq!(super::form("properties.items", 2), "{0} items");
        // And a key in no catalogue at all is still itself, which is the visible
        // fallback `a_missing_key_renders_as_itself` is about.
        assert_eq!(super::form("nothing.here", 1), "nothing.here"); // skerry-allow: missing-string
    }

    #[test]
    fn the_macro_resolves_both_forms() {
        assert_eq!(t!("app.name"), "Skerry");
        assert_eq!(t!("status.loading", 3), "Reading 3…");
    }
}
