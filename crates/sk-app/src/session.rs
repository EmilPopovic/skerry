//! What the window was doing last time, and the file it is kept in.
//!
//! [ADR 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md)
//! decides the file: where it lives, its grammar, that it is written whole and
//! renamed into place, and that a version this build does not know is ignored in
//! full rather than half-read. What is here is that decision's implementation and
//! nothing else — the policy about *when* to write is in `App`, because that is
//! where the frames are.
//!
//! The grammar, in full:
//!
//! ```text
//! skerry 1
//! theme dark            dark | light
//! sidebar on            on | off
//! hidden off            on | off
//! zoom 2                an index into App::ZOOM
//! window 1100x720       logical pixels
//! split beside 0.5      beside | below, then a ratio
//!   leaf 1              the index of the showing tab
//!     tab /home name+ 0     a sort, a scroll offset, and the path last
//! ```
//!
//! A `split` is followed by its two children in order, which is what makes the
//! tree readable without indentation carrying meaning — the indentation is for a
//! person, and a parser that depended on it would fail silently the first time
//! something reformatted the file.
//!
//! **The path is last on its line and is never escaped.** A newline is the one
//! byte that cannot be in a path, so a line holds any path there is; every field
//! before it is a token from a closed set.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sk_model::{Sort, SortColumn};

use crate::panes::Axis;

/// The version this build writes and the only one it reads.
const VERSION: u32 = 1;

/// How deep a tree may be before the file is refused.
///
/// A person cannot build sixty-four nested splits by hand — each halves a pane —
/// so this is not a limit anybody meets. It is here because the parser recurses,
/// and a file is a thing that can arrive corrupt.
const MAX_DEPTH: u32 = 64;

/// Everything restored on relaunch.
#[derive(Clone, PartialEq, Debug)]
pub struct Session {
    /// Whether the dark palette was in use.
    pub dark: bool,
    /// Whether the list of places was open.
    pub sidebar: bool,
    /// Whether dotfiles were shown.
    pub hidden: bool,
    /// Which zoom step, as an index.
    pub zoom: usize,
    /// The window's logical size.
    pub window: (u32, u32),
    /// The pane tree.
    pub tree: Node,
}

/// One node of the saved tree.
#[derive(Clone, PartialEq, Debug)]
pub enum Node {
    /// A tab strip, and which of its tabs was showing.
    Leaf {
        /// The showing tab's index.
        active: usize,
        /// At least one.
        tabs: Vec<Tab>,
    },
    /// Two children and the line between them.
    Split {
        /// Which way it divides.
        axis: Axis,
        /// Where the divider sits, 0 to 1.
        ratio: f32,
        /// Left, or above.
        first: Box<Node>,
        /// Right, or below.
        second: Box<Node>,
    },
}

/// One tab: where it was, in what order, and how far down.
#[derive(Clone, PartialEq, Debug)]
pub struct Tab {
    /// The directory it was showing.
    pub path: PathBuf,
    /// The order it was in.
    pub sort: Sort,
    /// How it was laid out.
    pub mode: crate::cells::Mode,
    /// How it was cut into headings.
    pub grouping: crate::groups::Grouping,
    /// How far down the list it was, in logical pixels.
    pub scroll: f32,
}

/// Where the file lives.
///
/// `$XDG_STATE_HOME/skerry/session`, and `~/.local/state` where the variable is
/// unset — which is the basedir specification's own fallback. `None` when there
/// is no home either, which is a session that cannot be saved and is not an
/// error worth telling anybody about.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("skerry").join("session"))
}

/// Reads the file, or nothing if there is not one this build understands.
///
/// **Runs before the frame loop starts**, which is the one place a filesystem
/// call is not a rule 1 violation: there is no frame to block yet, and the
/// alternative is opening on `$HOME` and jumping to the restored tree a moment
/// later. One `openat` and one read of a file a few hundred bytes long.
#[must_use]
pub fn load() -> Option<Session> {
    let file = path()?;
    let text = std::fs::read_to_string(file).ok()?;
    parse(&text)
}

/// Writes `text` where [`path`] says, whole and atomically.
///
/// **Runs on a job.** Three steps, and each one is why rule 6 can claim to
/// survive a power cut: written to a neighbouring name, `fsync`ed so the bytes
/// are on the device rather than in the page cache, then `rename`d — which is
/// atomic within a directory, so a reader sees the whole old file or the whole
/// new one and never a torn one.
pub fn write(text: &str) -> bool {
    let Some(file) = path() else { return false };
    let Some(directory) = file.parent() else { return false };
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    let staging = file.with_extension("new");
    let Ok(handle) = std::fs::File::create(&staging) else { return false };
    {
        use std::io::Write as _;
        let mut handle = handle;
        if handle.write_all(text.as_bytes()).is_err() || handle.sync_all().is_err() {
            let _ = std::fs::remove_file(&staging);
            return false;
        }
    }
    if std::fs::rename(&staging, &file).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    true
}

/// The file's text for a session.
#[must_use]
pub fn render(session: &Session) -> String {
    let mut out = format!("skerry {VERSION}\n");
    out.push_str(if session.dark { "theme dark\n" } else { "theme light\n" });
    out.push_str(if session.sidebar { "sidebar on\n" } else { "sidebar off\n" });
    out.push_str(if session.hidden { "hidden on\n" } else { "hidden off\n" });
    let _ = writeln!(out, "zoom {}", session.zoom);
    let _ = writeln!(out, "window {}x{}", session.window.0, session.window.1);
    write_node(&session.tree, 0, &mut out);
    out
}

fn write_node(node: &Node, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match node {
        Node::Leaf { active, tabs } => {
            let _ = writeln!(out, "{pad}leaf {active}");
            for tab in tabs {
                let _ = writeln!(
                    out,
                    "{pad}  tab {} {} {} {} {}",
                    sort_token(tab.sort),
                    tab.mode.token(),
                    tab.grouping.token(),
                    tab.scroll.round() as i64,
                    tab.path.display()
                );
            }
        }
        Node::Split { axis, ratio, first, second } => {
            let axis = match axis {
                Axis::Beside => "beside",
                Axis::Below => "below",
            };
            let _ = writeln!(out, "{pad}split {axis} {ratio:.4}");
            write_node(first, depth + 1, out);
            write_node(second, depth + 1, out);
        }
    }
}

/// A sort as one token: the column, and `+` or `-` for the direction.
fn sort_token(sort: Sort) -> String {
    let column = match sort.column {
        SortColumn::Name => "name",
        SortColumn::Size => "size",
        SortColumn::Modified => "modified",
    };
    format!("{column}{}", if sort.descending { '-' } else { '+' })
}

fn read_sort(token: &str) -> Option<Sort> {
    let (column, direction) = token.split_at(token.len().checked_sub(1)?);
    let column = match column {
        "name" => SortColumn::Name,
        "size" => SortColumn::Size,
        "modified" => SortColumn::Modified,
        _ => return None,
    };
    let descending = match direction {
        "+" => false,
        "-" => true,
        _ => return None,
    };
    Some(Sort { column, descending, dirs_first: true, ..Sort::default() })
}

/// Reads a session, or nothing.
///
/// **Nothing rather than a partial one.** A state file holds nothing that cannot
/// be reconstructed by opening a folder again, so a half-understood layout is a
/// worse outcome than a fresh one — and an error message about a file the person
/// has never heard of is worse than both. A *line* this build does not recognise
/// is skipped, so a newer build's extra settings do not stop an older one
/// restoring the layout it does understand.
#[must_use]
pub fn parse(text: &str) -> Option<Session> {
    // **Trimmed at the start only.** The indentation is decoration and has to go;
    // the *end* is a path, and a directory whose name ends in a space is legal
    // and is exactly the kind of name a file manager has to handle. A lone `\r`
    // goes too, so a file that has been through a CRLF round trip still reads.
    let mut lines = text
        .lines()
        .map(|line| line.trim_end_matches('\r').trim_start())
        .filter(|line| !line.is_empty());
    let header = lines.next()?;
    let (name, version) = header.split_once(' ')?;
    if name != "skerry" || version.parse::<u32>().ok()? != VERSION {
        return None;
    }

    let mut session = Session {
        dark: true,
        sidebar: true,
        hidden: false,
        zoom: crate::app::ZOOM_DEFAULT,
        window: (1100, 720),
        tree: Node::Leaf { active: 0, tabs: Vec::new() },
    };
    let rest: Vec<&str> = lines.collect();
    let mut at = 0;
    while at < rest.len() {
        let line = rest[at];
        let (word, value) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "theme" => session.dark = value != "light",
            "sidebar" => session.sidebar = value == "on",
            "hidden" => session.hidden = value == "on",
            "zoom" => session.zoom = value.parse().unwrap_or(crate::app::ZOOM_DEFAULT),
            "window" => {
                if let Some(size) = read_size(value) {
                    session.window = size;
                }
            }
            "split" | "leaf" => break,
            // Skipped, not refused: a field this build has never heard of is a
            // newer build's, and it must not cost this one the layout.
            _ => {}
        }
        at += 1;
    }

    let (tree, next) = read_node(&rest, at, 0)?;
    // Trailing lines are fine — another build's, or a second root somebody
    // pasted in — but the tree has to have been the first thing after the
    // settings, or what was read is not what was written.
    let _ = next;
    session.tree = tree;
    Some(session)
}

fn read_size(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// Reads one node and everything under it. Returns it and the line after it.
fn read_node(lines: &[&str], at: usize, depth: u32) -> Option<(Node, usize)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let line = lines.get(at)?;
    let (word, value) = line.split_once(' ').unwrap_or((line, ""));
    match word {
        "leaf" => {
            let active = value.parse().unwrap_or(0);
            let mut tabs = Vec::new();
            let mut next = at + 1;
            while let Some(line) = lines.get(next) {
                let Some(rest) = line.strip_prefix("tab ") else { break };
                if let Some(tab) = read_tab(rest) {
                    tabs.push(tab);
                }
                next += 1;
            }
            // A leaf with no tab it could read is not a leaf: the invariant the
            // tree keeps is that every leaf holds at least one.
            (!tabs.is_empty())
                .then(|| (Node::Leaf { active: active.min(tabs.len() - 1), tabs }, next))
        }
        "split" => {
            let mut fields = value.split_whitespace();
            let axis = match fields.next()? {
                "beside" => Axis::Beside,
                "below" => Axis::Below,
                _ => return None,
            };
            let ratio: f32 = fields.next()?.parse().ok()?;
            let (first, next) = read_node(lines, at + 1, depth + 1)?;
            let (second, next) = read_node(lines, next, depth + 1)?;
            Some((
                Node::Split {
                    axis,
                    ratio: ratio.clamp(0.05, 0.95),
                    first: Box::new(first),
                    second: Box::new(second),
                },
                next,
            ))
        }
        _ => None,
    }
}

/// One `tab` line, in either arity: `name+ grid 340 /home/emil/notes`, or the
/// three-token `name+ 340 /home/emil/notes` an older build wrote. The path is the
/// rest of the line, so it may hold spaces.
///
/// **Both forms parse, so adding the mode cost nobody their layout.** [ADR
/// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)
/// put the view mode on this line, and the alternative — bumping the version —
/// discards the whole file, which is somebody's window arrangement and the one
/// thing [ADR 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md)
/// exists to keep. The two are told apart with no ambiguity at all: the field
/// after the sort is either a mode word or the scroll offset, and a mode word
/// never parses as a number.
fn read_tab(rest: &str) -> Option<Tab> {
    let (sort, rest) = rest.split_once(' ')?;
    // **Optional tokens, taken in the order the format grew them.** Each is
    // consumed only where it is actually there, so every arity this file has ever
    // been written in still parses: sort and scroll, or with a mode, or with a
    // mode and a grouping. A mode word and a grouping word never parse as a
    // number, which is what makes that unambiguous.
    let mut rest = rest;
    let mut mode = crate::cells::Mode::default();
    let mut grouping = crate::groups::Grouping::default();
    if let Some((token, tail)) = rest.split_once(' ')
        && let Some(found) = crate::cells::Mode::from_token(token)
    {
        mode = found;
        rest = tail;
    }
    if let Some((token, tail)) = rest.split_once(' ')
        && let Some(found) = crate::groups::Grouping::from_token(token)
    {
        grouping = found;
        rest = tail;
    }
    let (scroll, path) = rest.split_once(' ')?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    Some(Tab {
        sort: read_sort(sort)?,
        mode,
        grouping,
        scroll: scroll.parse::<f32>().ok()?.max(0.0),
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Node, Session, Tab, parse, render};
    use crate::panes::Axis;
    use sk_model::{Sort, SortColumn};
    use std::path::PathBuf;

    fn tab(path: &str, column: SortColumn, scroll: f32) -> Tab {
        Tab {
            path: PathBuf::from(path),
            sort: Sort { column, descending: column == SortColumn::Size, ..Sort::default() },
            mode: crate::cells::Mode::default(),
            grouping: crate::groups::Grouping::default(),
            scroll,
        }
    }

    /// A file written before the view mode existed still restores its layout.
    ///
    /// The reason ADR 0013 put the mode on the tab line instead of bumping the
    /// version: a bump discards the file, and the file is somebody's arrangement
    /// of windows.
    #[test]
    fn a_tab_line_without_a_mode_still_parses_and_defaults() {
        let old = "skerry 1\nleaf 0\ntab name+ 120 /tmp/notes\n";
        let session = parse(old).expect("the three-field form was refused");
        let Node::Leaf { tabs, .. } = &session.tree else { panic!("not a leaf") };
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].path, PathBuf::from("/tmp/notes"));
        assert!((tabs[0].scroll - 120.0).abs() < 0.01);
        assert_eq!(tabs[0].mode, crate::cells::Mode::Details, "the default is Details");
    }

    /// Every arity the file has been written in still parses.
    ///
    /// Three tokens, four, and five — the format grew a mode and then a grouping,
    /// and a bump would have discarded somebody's window arrangement each time.
    #[test]
    fn every_arity_the_format_has_ever_had_still_loads() {
        let cases = [
            ("skerry 1\nleaf 0\ntab name+ 120 /tmp/a\n", "details", "flat"),
            ("skerry 1\nleaf 0\ntab name+ grid 120 /tmp/a\n", "grid", "flat"),
            ("skerry 1\nleaf 0\ntab name+ grid by-time 120 /tmp/a\n", "grid", "by-time"),
            ("skerry 1\nleaf 0\ntab name+ by-time 120 /tmp/a\n", "details", "by-time"),
        ];
        for (text, mode, grouping) in cases {
            let session = parse(text).unwrap_or_else(|| panic!("refused {text:?}"));
            let Node::Leaf { tabs, .. } = &session.tree else { panic!("not a leaf") };
            assert_eq!(tabs[0].mode.token(), mode, "{text:?}");
            assert_eq!(tabs[0].grouping.token(), grouping, "{text:?}");
            assert!((tabs[0].scroll - 120.0).abs() < 0.01, "{text:?}");
            assert_eq!(tabs[0].path, PathBuf::from("/tmp/a"), "{text:?}");
        }
    }

    /// And the four-field form carries the mode both ways.
    #[test]
    fn a_mode_survives_being_written_and_read_back() {
        let written = "skerry 1\nleaf 0\ntab name+ grid 40 /tmp/pictures\n";
        let session = parse(written).expect("the four-field form was refused");
        let Node::Leaf { tabs, .. } = &session.tree else { panic!("not a leaf") };
        assert_eq!(tabs[0].mode, crate::cells::Mode::Grid);
        assert!((tabs[0].scroll - 40.0).abs() < 0.01);
        // Rendered again, it comes out in the four-field form and re-parses.
        let again = render(&session);
        assert!(again.contains("tab name+ grid flat 40 /tmp/pictures"), "{again}");
        let round = parse(&again).expect("what we wrote we cannot read");
        let Node::Leaf { tabs, .. } = &round.tree else { panic!("not a leaf") };
        assert_eq!(tabs[0].mode, crate::cells::Mode::Grid);
    }

    /// A path with a space in it still lands, in both arities.
    #[test]
    fn a_path_with_spaces_survives_either_arity() {
        for text in [
            "skerry 1\nleaf 0\ntab name+ 0 /tmp/my notes\n",
            "skerry 1\nleaf 0\ntab name+ grid 0 /tmp/my notes\n",
        ] {
            let session = parse(text).unwrap_or_else(|| panic!("refused {text:?}"));
            let Node::Leaf { tabs, .. } = &session.tree else { panic!("not a leaf") };
            assert_eq!(tabs[0].path, PathBuf::from("/tmp/my notes"), "{text:?}");
        }
    }

    fn nested() -> Session {
        Session {
            dark: false,
            sidebar: false,
            hidden: true,
            zoom: 4,
            window: (1440, 900),
            tree: Node::Split {
                axis: Axis::Below,
                ratio: 0.375,
                first: Box::new(Node::Leaf {
                    active: 1,
                    tabs: vec![
                        tab("/home/emil/notes", SortColumn::Name, 0.0),
                        tab("/home/emil/Downloads", SortColumn::Size, 340.0),
                    ],
                }),
                second: Box::new(Node::Split {
                    axis: Axis::Beside,
                    ratio: 0.5,
                    first: Box::new(Node::Leaf {
                        active: 0,
                        tabs: vec![tab("/", SortColumn::Modified, 0.0)],
                    }),
                    second: Box::new(Node::Leaf {
                        active: 0,
                        tabs: vec![tab("/usr/share", SortColumn::Name, 12.0)],
                    }),
                }),
            },
        }
    }

    /// A tree of three leaves at two depths, out and back unchanged.
    #[test]
    fn a_nested_tree_survives_being_written_and_read() {
        let before = nested();
        let text = render(&before);
        let after = parse(&text).expect("the file this build just wrote was refused");
        assert_eq!(after, before, "the round trip changed it:\n{text}");
    }

    /// The one byte a path cannot contain is the one that separates the lines,
    /// which is why the path is last and unescaped.
    #[test]
    fn a_path_with_spaces_and_punctuation_in_it_comes_back_whole() {
        let odd = "/home/emil/My Documents/report (final) #2/  trailing  ";
        let session = Session {
            tree: Node::Leaf { active: 0, tabs: vec![tab(odd, SortColumn::Name, 0.0)] },
            ..nested()
        };
        let read = parse(&render(&session)).expect("refused");
        let Node::Leaf { tabs, .. } = &read.tree else { panic!("not a leaf") };
        assert_eq!(tabs[0].path, PathBuf::from(odd));
    }

    /// A version this build does not know is ignored in full, not half-read.
    #[test]
    fn a_file_from_another_version_is_ignored_rather_than_guessed_at() {
        let mut text = render(&nested());
        text = text.replacen("skerry 1", "skerry 2", 1);
        assert!(parse(&text).is_none());
        assert!(parse("").is_none());
        assert!(parse("not a skerry file at all\n").is_none());
    }

    /// A setting a newer build wrote must not cost an older one the layout.
    #[test]
    fn an_unknown_setting_is_skipped_and_the_tree_still_arrives() {
        let text = render(&nested()).replacen("zoom 4\n", "zoom 4\nwallpaper yes\n", 1);
        let read = parse(&text).expect("one unknown line refused the whole file");
        assert_eq!(read.tree, nested().tree);
    }

    /// Two shapes of damage, and neither may produce a tree.
    #[test]
    fn a_truncated_or_impossible_tree_is_refused() {
        // A split with one child.
        let orphan = "skerry 1\nsplit beside 0.5\nleaf 0\ntab name+ 0 /tmp\n";
        assert!(parse(orphan).is_none(), "a split with one child parsed");
        // A leaf with no tab under it.
        let empty = "skerry 1\nleaf 0\n";
        assert!(parse(empty).is_none(), "a leaf with no tabs parsed");
        // A tab whose path is relative, which nothing here ever writes.
        let relative = "skerry 1\nleaf 0\ntab name+ 0 notes\n";
        assert!(parse(relative).is_none(), "a relative path parsed");
    }

    /// A showing tab past the end of the strip is what a hand-edited file gives,
    /// and what the tree's own invariant forbids.
    #[test]
    fn a_showing_tab_that_is_not_there_is_clamped_to_one_that_is() {
        let text = "skerry 1\nleaf 9\ntab name+ 0 /tmp\n";
        let read = parse(text).expect("refused");
        let Node::Leaf { active, tabs } = &read.tree else { panic!("not a leaf") };
        assert_eq!(*active, 0);
        assert_eq!(tabs.len(), 1);
    }
}
