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
/// let status = t!("status.counts", total, selected);     // String
/// ```
#[macro_export]
macro_rules! t {
    ($key:literal) => { $crate::i18n::lookup($key) };
    ($key:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format($key, &[$(&$arg as &dyn ::std::fmt::Display),+])
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

    #[test]
    fn the_macro_resolves_both_forms() {
        assert_eq!(t!("app.name"), "Skerry");
        assert_eq!(t!("status.counts", 3, 1), "3 items, 1 selected");
    }
}
