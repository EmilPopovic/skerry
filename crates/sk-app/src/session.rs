// SPDX-License-Identifier: Apache-2.0
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
//! sidebar on 220        on | off, then how wide it has been dragged
//! history on            on | off — whether the visible history is expanded
//! hidden off            on | off
//! view details          details | grid — what a folder with no record gets
//! confirm off           on | off — whether Move to Trash asks first
//! icons desktop         marks | desktop | a theme's own name
//! touched theme hidden  the settings this window changed, in table order
//! zoom 2                an index into App::ZOOM
//! window 1100x720       logical pixels
//! columns 520 96 128    name, size and modified widths, in logical pixels
//! expanded /home/emil   a place opened in the sidebar's tree, one line each
//! page 7 /home/a.pdf   which page a document was last left on, one line each
//! split beside 0.5      beside | below, then a ratio
//!   leaf 1              the index of the showing tab
//!     tab name+ 0 /home     a sort, a scroll offset, and the path last
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

/// One acknowledged snapshot and at most one writer in flight.
#[derive(Default)]
pub struct WriteBack {
    file: Option<PathBuf>,
    acknowledged: String,
    flight: Option<std::sync::mpsc::Receiver<(String, bool)>>,
}

impl WriteBack {
    /// Save every checkpoint to the exclusively held window slot.
    pub fn at(file: PathBuf) -> Self {
        Self { file: Some(file), ..Self::default() }
    }

    /// Drains acknowledgement, optionally waiting during process exit.
    pub fn ready(&mut self, wait: bool) -> bool {
        use std::sync::mpsc::TryRecvError;
        let Some(receiver) = self.flight.take() else { return true };
        let answer = if wait {
            receiver.recv().map_err(|_| TryRecvError::Disconnected)
        } else {
            receiver.try_recv()
        };
        match answer {
            Ok((text, true)) => self.acknowledged = text,
            Err(TryRecvError::Empty) => {
                self.flight = Some(receiver);
                return false;
            }
            Ok((_, false)) | Err(TryRecvError::Disconnected) => {}
        }
        true
    }

    /// The most recent successful write, never merely the most recent request.
    pub fn acknowledged(&self) -> &str {
        &self.acknowledged
    }

    /// Starts a save if no earlier save is still running.
    pub fn start(&mut self, text: String, pool: &sk_util::Pool, wake: &sk_util::WakeHandle) {
        let file = self.file.clone().or_else(path);
        self.start_with(text, pool, wake, move |text| {
            file.is_some_and(|file| sk_fs::atomically(&file, text))
        });
    }

    fn start_with(
        &mut self,
        text: String,
        pool: &sk_util::Pool,
        wake: &sk_util::WakeHandle,
        save: impl FnOnce(&str) -> bool + Send + 'static,
    ) {
        if self.flight.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        self.flight = Some(receiver);
        let wake = wake.clone();
        pool.spawn(sk_util::Lane::Control, sk_util::Cancel::new(), move |_| {
            let saved = save(&text);
            let _ = sender.send((text, saved));
            wake.wake();
        });
    }

    /// Finishes older work before publishing the final snapshot on exit.
    pub fn finish(&mut self, text: String) {
        self.ready(true);
        let file = self.file.clone().or_else(path);
        if text != self.acknowledged && file.is_some_and(|file| sk_fs::atomically(&file, &text)) {
            self.acknowledged = text;
        }
    }
}

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
    /// How wide the list of places was dragged to, in logical pixels.
    ///
    /// **A second token on the `sidebar` line, not a line of its own**, and an
    /// older file that has only `on` restores the default — the same per-field
    /// tolerance the `columns` line has, for the same reason.
    pub aside: f32,
    /// Whether the history strip is expanded.
    ///
    /// **[Rule 6](../../../docs/03-principles.md), which is about the layout and
    /// not about the stack.** The entries themselves are not written down — they
    /// name paths whose inverses may no longer be possible after a restart, and an
    /// undo stack that survives a process is an offer to revert a change the
    /// filesystem has moved on from. What survives is whether the panel was open.
    pub history: bool,
    /// Whether dotfiles were shown.
    pub hidden: bool,
    /// What a folder with no record of its own is laid out as.
    pub layout: crate::views::Layout,
    /// Whether Move to Trash asks first.
    pub confirm_trash: bool,
    /// Where a file's mark came from — ADR 0042.
    pub icons: crate::settings::Icons,
    /// Which settings this window changed, and so may override the config file.
    ///
    /// **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md).**
    /// A file with no `touched` line reads as nothing touched, so an older
    /// session restores the config's values — which is the behaviour a first
    /// upgrade should have, and the reason this is a line rather than a version
    /// bump.
    pub touched: crate::settings::Marks,
    /// Which zoom step, as an index.
    pub zoom: usize,
    /// The window's logical size.
    pub window: (u32, u32),
    /// How wide the list's columns were dragged to.
    ///
    /// **A line an older file does not have, and no version bump for it.** A bump
    /// discards the file and the file is somebody's window arrangement, which is
    /// the whole thing [ADR
    /// 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md)
    /// exists to preserve. A file without the line restores the defaults, which
    /// is exactly what a person who has never dragged a column expects.
    pub columns: crate::layout::Columns,
    /// The places opened in the sidebar's tree.
    ///
    /// **One line each, and the path is last**, which is the grammar's own rule:
    /// a newline is the one byte a path cannot hold. A place whose directory has
    /// gone comes back as a line that opens nothing, which costs one read and no
    /// row.
    pub expanded: Vec<PathBuf>,
    /// Which page each document was last left on, most recent first.
    ///
    /// **A reading position is part of how the window was left.** Reopening a
    /// four-hundred-page manual at page one is the same failure as reopening a
    /// window at the home folder, and [rule
    /// 6](../../../docs/03-principles.md) is about not losing what a person set
    /// up. Bounded — see [`MOST_PAGES`] — and one line each, with the path last
    /// for the grammar's own reason.
    pub pages: Vec<(PathBuf, u32)>,
    /// The pane tree.
    pub tree: Node,
}

/// How many documents' reading positions to keep.
///
/// **Thirty-two, and the oldest goes.** The list is written on every change like
/// the rest of the file, so its length is what a save costs; thirty-two is more
/// documents than a person has open in a week and four lines of text.
pub const MOST_PAGES: usize = 32;

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

/// A session slot exclusively owned by this window, acquired before display startup.
pub fn claim(independent: bool) -> std::io::Result<Option<(PathBuf, std::fs::File)>> {
    let Some(base) = path() else { return Ok(None) };
    claim_at(&base, independent).map(Some)
}

fn claim_at(base: &Path, independent: bool) -> std::io::Result<(PathBuf, std::fs::File)> {
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for slot in usize::from(independent)..64 {
        let file = if slot == 0 {
            base.to_path_buf()
        } else {
            base.with_file_name(format!("session-window-{slot}"))
        };
        let lock = file.with_extension("lock");
        match sk_fs::lock_file(&lock) {
            Ok(held) => return Ok((file, held)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "all window session slots are in use"))
}

/// Retain panic diagnostics without attempting to recover inconsistent UI state.
pub fn install_panic_log(identity: &'static str) {
    use std::io::Write as _;
    let Some(file) = path().map(|path| path.with_file_name("panic.log")) else { return };
    let output = match sk_fs::diagnostic_file(&file) {
        Ok(output) => std::sync::Mutex::new(output),
        Err(error) => {
            eprintln!("skerry: cannot open panic diagnostics {}: {error}", file.display());
            return;
        }
    };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut output) = output.try_lock() {
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_secs());
            let _ =
                writeln!(output, "{identity} pid={} unix={seconds}: {info}", std::process::id());
            let _ = output.flush();
        }
        previous(info);
    }));
}

/// The three column widths on a `columns` line, defaulting any that is missing
/// or unreadable.
///
/// **Per field, not per line.** A line that has grown a fourth number in a later
/// build still yields three, and one truncated to two yields two and a default —
/// where refusing the line whole would throw away a width somebody dragged
/// because of a field they never touched.
fn read_columns(value: &str) -> crate::layout::Columns {
    let mut widths = value.split_whitespace();
    let default = crate::layout::Columns::default();
    let mut next = |fallback: f32| widths.next().and_then(|it| it.parse().ok()).unwrap_or(fallback);
    crate::layout::Columns {
        name: next(default.name),
        size: next(default.size),
        modified: next(default.modified),
    }
    .clamped()
}

/// The file's text for a session.
#[must_use]
pub fn render(session: &Session) -> String {
    let mut out = format!("skerry {VERSION}\n");
    out.push_str(if session.dark { "theme dark\n" } else { "theme light\n" });
    let _ = writeln!(
        out,
        "sidebar {} {}",
        if session.sidebar { "on" } else { "off" },
        session.aside.round()
    );
    out.push_str(if session.history { "history on\n" } else { "history off\n" });
    out.push_str(if session.hidden { "hidden on\n" } else { "hidden off\n" });
    let _ = writeln!(out, "view {}", session.layout.token());
    out.push_str(if session.confirm_trash { "confirm on\n" } else { "confirm off\n" });
    let _ = writeln!(out, "icons {}", session.icons.token());
    // **Written even when empty**, so a window that has un-touched nothing still
    // says so and the line is a fact rather than an absence to be inferred.
    let _ = writeln!(out, "touched {}", session.touched.names().join(" "));
    let _ = writeln!(out, "zoom {}", session.zoom);
    let _ = writeln!(out, "window {}x{}", session.window.0, session.window.1);
    // Rounded, because a width comes from a pointer position and the file is a
    // thing a person reads. Half a logical pixel of a column boundary is not a
    // decision anybody made.
    let _ = writeln!(
        out,
        "columns {} {} {}",
        session.columns.name.round(),
        session.columns.size.round(),
        session.columns.modified.round()
    );
    for path in &session.expanded {
        let _ = writeln!(out, "expanded {}", path.display());
    }
    for (path, page) in session.pages.iter().take(MOST_PAGES) {
        let _ = writeln!(out, "page {page} {}", path.display());
    }
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
                    "{pad}  tab {} {} {}",
                    sort_token(tab.sort),
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
        aside: crate::sidebar::DEFAULT_WIDTH,
        history: false,
        hidden: false,
        layout: crate::views::Layout::Details,
        confirm_trash: false,
        icons: crate::settings::Icons::default(),
        touched: crate::settings::Marks::default(),
        zoom: crate::app::ZOOM_DEFAULT,
        window: (1100, 720),
        columns: crate::layout::Columns::default(),
        expanded: Vec::new(),
        pages: Vec::new(),
        tree: Node::Leaf { active: 0, tabs: Vec::new() },
    };
    let rest: Vec<&str> = lines.collect();
    let mut at = 0;
    while at < rest.len() {
        let line = rest[at];
        let (word, value) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "theme" => session.dark = value != "light",
            "sidebar" => {
                let mut words = value.split_whitespace();
                session.sidebar = words.next() == Some("on");
                if let Some(width) = words.next().and_then(|it| it.parse::<f32>().ok()) {
                    session.aside = width.clamp(crate::sidebar::NARROWEST, crate::sidebar::WIDEST);
                }
            }
            "history" => session.history = value == "on",
            "hidden" => session.hidden = value == "on",
            "view" => {
                if let Some(layout) = crate::views::Layout::from_token(value) {
                    session.layout = layout;
                }
            }
            "confirm" => session.confirm_trash = value == "on",
            // A word this build does not know leaves the default, which is the
            // per-field tolerance the `columns` and `sidebar` lines already have.
            "icons" => {
                let mut values =
                    crate::settings::Values { icons: session.icons.clone(), ..Default::default() };
                if values.take(crate::settings::Setting::Icons, value) {
                    session.icons = values.icons;
                }
            }
            "touched" => session.touched.read(value),
            "zoom" => session.zoom = value.parse().unwrap_or(crate::app::ZOOM_DEFAULT),
            "columns" => session.columns = read_columns(value),
            "expanded" if !value.is_empty() => session.expanded.push(PathBuf::from(value)),
            // `page 7 /home/emil/manual.pdf` — the number first, so the path
            // keeps the rest of the line and needs no quoting.
            "page" => {
                if let Some((number, path)) = value.split_once(' ')
                    && let Ok(page) = number.parse::<u32>()
                    && !path.is_empty()
                    && session.pages.len() < MOST_PAGES
                {
                    session.pages.push((PathBuf::from(path), page.max(1)));
                }
            }
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
    // **A view mode and a grouping were written here for two commits**, and are
    // skipped rather than refused: the layout of a folder belongs to the folder
    // now — see [`crate::views`] — and two files that could disagree about it is
    // the thing that made the setting feel unreliable. A file written by those
    // builds still restores its tabs; only the tokens they carried are ignored.
    let mut rest = rest;
    while let Some((token, tail)) = rest.split_once(' ')
        && (crate::cells::Mode::from_token(token).is_some()
            || crate::groups::Grouping::from_token(token).is_some())
    {
        rest = tail;
    }
    let (scroll, path) = rest.split_once(' ')?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    Some(Tab {
        sort: read_sort(sort)?,
        scroll: scroll.parse::<f32>().ok()?.max(0.0),
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{MOST_PAGES, Node, Session, Tab, parse, render};
    use crate::panes::Axis;
    use sk_model::{Sort, SortColumn};
    use std::path::PathBuf;

    fn tab(path: &str, column: SortColumn, scroll: f32) -> Tab {
        Tab {
            path: PathBuf::from(path),
            sort: Sort { column, descending: column == SortColumn::Size, ..Sort::default() },
            scroll,
        }
    }

    /// Every arity this file has ever been written in still restores its tabs.
    ///
    /// The view mode and the grouping were written on this line for two commits
    /// and belong to the folder now, so those tokens are skipped rather than
    /// refused — a person who ran those builds keeps their splits and their tabs.
    #[test]
    fn simultaneous_windows_own_distinct_reusable_checkpoint_slots() {
        let root =
            std::env::temp_dir().join(format!("skerry-session-slots-{}", std::process::id()));
        let base = root.join("session");
        let (main, first_lock) = super::claim_at(&base, false).unwrap();
        let (second, second_lock) = super::claim_at(&base, true).unwrap();
        let (third, third_lock) = super::claim_at(&base, false).unwrap();
        assert_eq!(main, base);
        assert_ne!(main, second);
        assert_ne!(second, third);
        std::fs::write(&main, b"first window").unwrap();
        std::fs::write(&second, b"second window").unwrap();
        drop(second_lock);
        let (reused, reused_lock) = super::claim_at(&base, true).unwrap();
        assert_eq!(reused, second);
        assert_eq!(std::fs::read(&reused).unwrap(), b"second window");
        assert_eq!(std::fs::read(&main).unwrap(), b"first window");
        drop((first_lock, third_lock, reused_lock));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_arity_the_format_has_ever_had_still_restores_its_tabs() {
        let cases = [
            "skerry 1\nleaf 0\ntab name+ 120 /tmp/my notes\n",
            "skerry 1\nleaf 0\ntab name+ grid 120 /tmp/my notes\n",
            "skerry 1\nleaf 0\ntab name+ grid by-time 120 /tmp/my notes\n",
            "skerry 1\nleaf 0\ntab name+ by-time 120 /tmp/my notes\n",
        ];
        for text in cases {
            let session = parse(text).unwrap_or_else(|| panic!("refused {text:?}"));
            let Node::Leaf { tabs, .. } = &session.tree else { panic!("not a leaf") };
            assert_eq!(tabs.len(), 1, "{text:?}");
            assert!((tabs[0].scroll - 120.0).abs() < 0.01, "{text:?}");
            // The path is the rest of the line, so a space in it survives.
            assert_eq!(tabs[0].path, PathBuf::from("/tmp/my notes"), "{text:?}");
        }
        // And what this build writes is the three-token form.
        let session = parse(cases[2]).expect("refused");
        assert!(render(&session).contains("tab name+ 120 /tmp/my notes"), "{}", render(&session));
    }

    /// A reading position survives the round trip, and an old file has none.
    #[test]
    fn a_page_a_document_was_left_on_comes_back() {
        let mut session = nested();
        session.pages = vec![
            (PathBuf::from("/tmp/manual.pdf"), 47),
            (PathBuf::from("/tmp/a book with spaces.pdf"), 2),
        ];
        let text = render(&session);
        assert!(text.contains("page 47 /tmp/manual.pdf"), "{text}");
        let back = parse(&text).expect("read back");
        assert_eq!(back.pages, session.pages);

        // A file written before this line existed restores nothing, which is a
        // document that opens at page one — the behaviour it had.
        let older: String =
            text.lines().filter(|line| !line.starts_with("page ")).collect::<Vec<_>>().join("\n");
        assert!(parse(&format!("{older}\n")).expect("read back").pages.is_empty());
    }

    /// The list is bounded, so a session file cannot grow without end.
    #[test]
    fn only_so_many_reading_positions_are_kept() {
        let mut session = nested();
        session.pages =
            (0..MOST_PAGES + 8).map(|n| (PathBuf::from(format!("/tmp/{n}.pdf")), 3)).collect();
        let text = render(&session);
        assert_eq!(text.lines().filter(|line| line.starts_with("page ")).count(), MOST_PAGES);
        assert_eq!(parse(&text).expect("read back").pages.len(), MOST_PAGES);
    }

    fn nested() -> Session {
        Session {
            aside: crate::sidebar::DEFAULT_WIDTH,
            expanded: Vec::new(),
            pages: Vec::new(),
            dark: false,
            sidebar: false,
            history: false,
            hidden: true,
            layout: crate::views::Layout::Details,
            confirm_trash: false,
            icons: crate::settings::Icons::Named("Adwaita".to_owned()),
            touched: crate::settings::Marks::default(),
            zoom: 4,
            window: (1440, 900),
            columns: crate::layout::Columns::default(),
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

    /// **A file written before column widths existed is still that person's
    /// window.**
    ///
    /// The alternative was a version bump, which discards the file whole — the
    /// one thing ADR 0011 exists to prevent — so the line is additive and its
    /// absence means the defaults.
    #[test]
    fn a_file_with_no_columns_line_restores_the_defaults() {
        let older = "skerry 1\n\
             theme dark\n\
             sidebar on\n\
             hidden off\n\
             zoom 2\n\
             window 1100x720\n\
             leaf 0\n\
             tab name+ 0 /home/emil\n";
        let session = parse(older).expect("an older file is not a broken one");
        assert_eq!(session.columns, crate::layout::Columns::default());
        assert_eq!(session.window, (1100, 720), "and the rest of it still read");
        // The same rule for the sidebar's width, which is a *second token* on a
        // line an older file already has — so `sidebar on` has to keep meaning
        // "open, at whatever width this build ships".
        assert!(session.sidebar, "the line stopped being read when it grew a field");
        assert!((session.aside - crate::sidebar::DEFAULT_WIDTH).abs() < f32::EPSILON);
    }

    /// The width out and the same width back, and one the file should not be
    /// able to hold held inside the range rather than refusing the file with it.
    #[test]
    fn the_sidebars_width_survives_a_round_trip_and_a_corrupt_one_is_clamped() {
        let mut session = nested();
        session.aside = 331.0;
        session.sidebar = true;
        let text = render(&session);
        assert!(text.contains("sidebar on 331"), "{text}");
        assert!((parse(&text).expect("round trip").aside - 331.0).abs() < f32::EPSILON);

        let broken = text.replace("sidebar on 331", "sidebar on 4000");
        let read = parse(&broken).expect("a bad width is not a bad file");
        assert!((read.aside - crate::sidebar::WIDEST).abs() < f32::EPSILON);
        assert!(read.sidebar, "and the token before it still read");
        let nonsense = text.replace("sidebar on 331", "sidebar off nonsense");
        let read = parse(&nonsense).expect("a bad width is not a bad file");
        assert!(!read.sidebar);
        assert!((read.aside - crate::sidebar::DEFAULT_WIDTH).abs() < f32::EPSILON);
    }

    /// Three widths out and the same three back.
    ///
    /// And a width the file should not be able to hold is held inside the range
    /// rather than refused along with the tab tree that shared the file with it.
    #[test]
    fn the_column_widths_survive_a_round_trip_and_a_corrupt_one_is_clamped() {
        let mut session = nested();
        session.columns = crate::layout::Columns { name: 331.0, size: 72.0, modified: 150.0 };
        let text = render(&session);
        assert!(text.contains("columns 331 72 150"), "{text}");
        assert_eq!(parse(&text).expect("round trip").columns, session.columns);

        let broken = text.replace("columns 331 72 150", "columns 0 nonsense");
        let read = parse(&broken).expect("a bad line is not a bad file");
        assert!((read.columns.name - crate::layout::Columns::LEAST).abs() < f32::EPSILON);
        let default = crate::layout::Columns::default();
        assert!((read.columns.size - default.size).abs() < f32::EPSILON, "unreadable took default");
        assert!((read.columns.modified - default.modified).abs() < f32::EPSILON, "missing field");
        assert_eq!(read.tree, session.tree, "and the tree on the same file survived it");
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
    #[test]
    fn writes_are_serial_and_failed_snapshots_remain_unacknowledged() {
        let pool = sk_util::Pool::with_width(2);
        let wake = sk_util::Waker::new().unwrap();
        let mut writer = super::WriteBack::default();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        writer.start_with("old".into(), &pool, &wake.handle(), move |_| {
            started.send(()).unwrap();
            wait.recv().unwrap();
            true
        });
        ready.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(!writer.ready(false));
        writer.start_with("must coalesce".into(), &pool, &wake.handle(), |_| {
            panic!("concurrent save")
        });
        assert_eq!(writer.acknowledged(), "");
        release.send(()).unwrap();
        assert!(writer.ready(true));
        assert_eq!(writer.acknowledged(), "old");
        writer.start_with("new".into(), &pool, &wake.handle(), |_| false);
        assert!(writer.ready(true));
        assert_eq!(writer.acknowledged(), "old");
        writer.start_with("new".into(), &pool, &wake.handle(), |_| true);
        assert!(writer.ready(true));
        assert_eq!(writer.acknowledged(), "new");
    }
}
