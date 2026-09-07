// SPDX-License-Identifier: Apache-2.0
//! Skerry.
//!
//! Parses the little there is to parse, then hands over to `sk-app`. Everything
//! else lives in a crate, because a binary that grows logic is a binary nothing
//! can test.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Instant;

use sk_app::{App, Bench, Capture, Options};

/// Opens the window and runs it until it closes.
///
/// Separate from `main` because the other two modes are one expression each and
/// this one is not: a display server going away is not the user closing the
/// window, and the difference is worth a line on the way out.
fn window(options: Options) -> Result<bool, sk_app::Problem> {
    App::new(options).map(|mut app| {
        if let Some(failure) = app.run() {
            eprintln!("skerry: the display server went away — {failure}");
            return false;
        }
        true
    })
}

/// Which overlay a `--capture` is a capture of.
///
/// One value rather than nine `let mut`s and nine arms, because they are one
/// question — and because the tenth is going to be added by somebody who has
/// not read this function.
#[derive(Default)]
struct Overlays {
    keys: bool,
    menu: bool,
    row_menu: bool,
    clash: bool,
    erase: bool,
    trash_question: bool,
    progress: bool,
    stopped: bool,
    indexing: bool,
    grid: bool,
    columns: bool,
    grouped: bool,
    grouped_size: bool,
    grouped_kind: bool,
    search: bool,
    carrying: bool,
    path_entry: bool,
    palette: bool,
    filter: bool,
    crumbs: bool,
    crumbs_empty: bool,
    properties: bool,
    open_with: bool,
    settings: bool,
    composing: bool,
    split: bool,
    tab_drop: bool,
    rename: bool,
    checked: bool,
    collapsed: bool,
    history: bool,
    look_selected: bool,
    look_source: bool,
    look_editing: bool,
    pinned: bool,
    tree: bool,
    coloured: bool,
}

impl Overlays {
    /// Which grouping the flags asked for. The first one given wins: they are
    /// three values of one setting rather than three settings.
    fn grouping(&self) -> sk_app::Grouping {
        use sk_app::Grouping;
        [
            (self.grouped, Grouping::Time),
            (self.grouped_size, Grouping::Size),
            (self.grouped_kind, Grouping::Kind),
        ]
        .into_iter()
        .find_map(|(wanted, grouping)| wanted.then_some(grouping))
        .unwrap_or(Grouping::None)
    }

    /// Whether `argument` was one of these, and sets it if so.
    fn take(&mut self, argument: &str) -> bool {
        let slot = match argument {
            "--keys" => &mut self.keys,
            "--menu" => &mut self.menu,
            "--menu-row" => &mut self.row_menu,
            "--clash" => &mut self.clash,
            "--erase" => &mut self.erase,
            "--trash-question" => &mut self.trash_question,
            "--progress" => &mut self.progress,
            "--stopped" => &mut self.stopped,
            "--indexing" => &mut self.indexing,
            "--grid" => &mut self.grid,
            "--look-selected" => &mut self.look_selected,
            "--look-source" => &mut self.look_source,
            "--look-editing" => &mut self.look_editing,
            "--pinned" => &mut self.pinned,
            "--tree" => &mut self.tree,
            "--coloured" => &mut self.coloured,
            "--columns" => &mut self.columns,
            "--grouped" => &mut self.grouped,
            "--grouped-size" => &mut self.grouped_size,
            "--grouped-kind" => &mut self.grouped_kind,
            "--checked" => &mut self.checked,
            "--collapsed" => &mut self.collapsed,
            "--history" => &mut self.history,
            "--search" => &mut self.search,
            "--carrying" => &mut self.carrying,
            "--path-entry" => &mut self.path_entry,
            "--palette" => &mut self.palette,
            "--filter" => &mut self.filter,
            "--crumbs" => &mut self.crumbs,
            "--crumbs-empty" => &mut self.crumbs_empty,
            "--properties" => &mut self.properties,
            "--open-with" => &mut self.open_with,
            "--settings" => &mut self.settings,
            "--composing" => &mut self.composing,
            "--split" => &mut self.split,
            "--tab-drop" => &mut self.tab_drop,
            "--rename" => &mut self.rename,
            _ => return false,
        };
        *slot = true;
        true
    }
}

/// What a `--capture` is a capture of, apart from which overlay is up.
struct Shape {
    file: PathBuf,
    path: PathBuf,
    logical: (u32, u32),
    scale_120: u32,
    dark: bool,
    stats: bool,
    own_frame: bool,
    sidebar: bool,
    /// How wide the sidebar is, where `--aside` said.
    aside: Option<f32>,
    /// Which page or frame `--look-page` asked for, or zero for the first.
    look_page: u32,
    /// How much `--look-zoom` magnified the picture, or one.
    look_zoom: f32,
    /// What `--look-find` asked the page's find bar to look for.
    look_find: String,
    /// Which folder inside an archive `--look-inside` named.
    look_inside: String,
    /// A subdirectory of `path` to capture, and where a column view's trail
    /// starts. See the `--at` line in the usage.
    at: Option<String>,
    /// The name of a file to open a quick look at.
    looking: Option<String>,
}

/// Renders one frame to a file.
///
/// Split out of `main` because the two halves are one struct literal each and
/// `main` is at the hundred-line limit — the same reason `Overlays` exists.
fn render_one(shape: &Shape, overlays: &Overlays) -> Result<(), sk_app::Problem> {
    sk_app::capture(&Capture {
        file: shape.file.clone(),
        path: shape.path.clone(),
        logical: shape.logical,
        scale_120: shape.scale_120,
        dark: shape.dark,
        keys: overlays.keys,
        menu: overlays.menu,
        row_menu: overlays.row_menu,
        clash: overlays.clash,
        erase: overlays.erase,
        trash_question: overlays.trash_question,
        progress: overlays.progress,
        stopped: overlays.stopped,
        indexing: overlays.indexing,
        grid: overlays.grid,
        columns: overlays.columns,
        at: shape.at.clone(),
        grouped: overlays.grouped,
        grouped_size: overlays.grouped_size,
        grouped_kind: overlays.grouped_kind,
        checked: overlays.checked,
        look_selected: overlays.look_selected,
        look_source: overlays.look_source,
        look_editing: overlays.look_editing,
        collapsed: overlays.collapsed,
        history: overlays.history,
        looking: shape.looking.clone(),
        search: overlays.search,
        carrying: overlays.carrying,
        path_entry: overlays.path_entry,
        palette: overlays.palette,
        filter: overlays.filter,
        crumbs: overlays.crumbs,
        crumbs_empty: overlays.crumbs_empty,
        properties: overlays.properties,
        open_with: overlays.open_with,
        settings: overlays.settings,
        composing: overlays.composing,
        split: overlays.split,
        tab_drop: overlays.tab_drop,
        rename: overlays.rename,
        stats: shape.stats,
        own_frame: shape.own_frame,
        sidebar: shape.sidebar,
        aside: shape.aside,
        look_page: shape.look_page,
        look_zoom: shape.look_zoom,
        look_find: shape.look_find.clone(),
        look_inside: shape.look_inside.clone(),
        pinned: overlays.pinned,
        tree: overlays.tree,
        coloured: overlays.coloured,
    })
}

/// The path an argument names.
///
/// **A `file://` URI is one of them.** A desktop launching a file manager from
/// its `.desktop` file passes `%U`, and every one of those is a URI — a
/// `PathBuf` of `file:///home/…` names a directory called `file:` in the working
/// directory, which is a listing that fails for a reason nobody can read.
fn named(argument: &str) -> PathBuf {
    sk_plat::decode_uri(argument).unwrap_or_else(|| PathBuf::from(argument))
}

// One arm per flag, and every one of them is two lines: the flag and what it
// sets. Splitting the parse in half would put half the flags somewhere a person
// looking for a flag does not look, which is the opposite of what this file is
// for.
#[expect(clippy::too_many_lines, reason = "one arm per flag, and the flags belong together")]
fn main() -> std::process::ExitCode {
    // Taken first, so the cold-start figure includes argument parsing, the
    // allocator waking up, and every dynamic library the loader pulls in.
    let started = Instant::now();

    let mut path = None;
    let mut dark = true;
    let mut stats = false;
    let mut overlays = Overlays::default();
    let mut own_frame = false;
    let mut no_sidebar = false;
    let mut aside = None;
    let mut look_page = 0;
    let mut look_zoom = 1.0;
    let mut look_find = String::new();
    let mut look_inside = String::new();
    let mut capture = None;
    let mut bench = None;
    let mut logical = (1100, 720);
    // `None` until `--scale` says otherwise, so the window can tell "asked for
    // 100%" from "not asked at all" — different things where the server states
    // one, and the only way to run at a fractional scale under a bare X server.
    let mut forced_scale = None;
    let mut at: Option<String> = None;
    let mut looking: Option<String> = None;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--light" => dark = false,
            "--stats" => stats = true,
            "--own-frame" => own_frame = true,
            "--no-sidebar" => no_sidebar = true,
            "--capture" => match arguments.next() {
                Some(file) => capture = Some(PathBuf::from(file)),
                None => return usage_error("--capture needs a file"),
            },
            "--bench" => match arguments.next().as_deref().map(str::parse) {
                Some(Ok(frames)) => bench = Some(frames),
                _ => return usage_error("--bench needs a frame count"),
            },
            "--size" => match arguments.next().as_deref().and_then(parse_size) {
                Some(size) => logical = size,
                None => return usage_error("--size needs WIDTHxHEIGHT"),
            },
            "--look-inside" => match arguments.next() {
                Some(path) => look_inside = path,
                None => return usage_error("--look-inside needs a folder inside the archive"),
            },
            "--look-find" => match arguments.next() {
                Some(text) => look_find = text,
                None => return usage_error("--look-find needs something to look for"),
            },
            "--look-zoom" => {
                match arguments.next().as_deref().and_then(|it| it.parse::<f32>().ok()) {
                    Some(zoom) if zoom >= 1.0 => look_zoom = zoom,
                    _ => return usage_error("--look-zoom needs a factor of one or more"),
                }
            }
            "--look-page" => {
                match arguments.next().as_deref().and_then(|it| it.parse::<u32>().ok()) {
                    Some(page) if page > 0 => look_page = page,
                    _ => return usage_error("--look-page needs a page or frame number"),
                }
            }
            "--aside" => match arguments.next().as_deref().and_then(|it| it.parse::<f32>().ok()) {
                Some(width) if width > 0.0 => aside = Some(width),
                _ => return usage_error("--aside needs a width in logical pixels"),
            },
            "--scale" => match arguments.next().as_deref().and_then(parse_scale) {
                Some(scale) => forced_scale = Some(scale),
                None => return usage_error("--scale needs a percentage, such as 125"),
            },
            "--look" | "--at" => match arguments.next() {
                // Two flags, one shape: a name that follows. `--at` says which
                // directory to capture and `--look` which file in it to open.
                Some(name) if argument == "--look" => looking = Some(name),
                Some(name) => at = Some(name),
                None => return usage_error("--at and --look each need a name"),
            },
            "--version" | "-V" => {
                // **What a bug report has to be able to say.** The crate's
                // version and the commit it was built from, because a version
                // alone cannot tell two builds of an alpha apart — and the
                // commit is stamped at build time by `build.rs`, which reads it
                // from git and falls back to the version when there is no git.
                println!("{}", version()); // skerry-allow: log-prefix — this is the answer
                return std::process::ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                // The answer to `--help`, which is the program's output rather
                // than a line of its log.
                println!("{USAGE}"); // skerry-allow: log-prefix — this is the answer
                return std::process::ExitCode::SUCCESS;
            }
            other if overlays.take(other) => {}
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option `{other}`"));
            }
            other => path = Some(named(other)),
        }
    }

    let named_path = path.clone();
    let path = path.or_else(home).unwrap_or_else(|| PathBuf::from("/"));
    // `.` and `../..` are legitimate arguments and illegitimate things to show in
    // a path bar. Resolved here rather than in the pane, because a pane's path is
    // the answer to "where am I" and should never have needed resolving.
    let path = std::fs::canonicalize(&path).unwrap_or(path);

    let scale_120 = forced_scale.unwrap_or(120);
    let outcome = match (capture, bench) {
        (Some(file), _) => render_one(
            &Shape {
                file,
                path,
                logical,
                scale_120,
                dark,
                stats,
                own_frame,
                sidebar: !no_sidebar,
                aside,
                look_page,
                look_inside,
                look_zoom,
                look_find,
                at,
                looking,
            },
            &overlays,
        )
        .map(|()| true),
        (None, Some(frames)) => {
            let grouping = overlays.grouping();
            sk_app::bench(&Bench { path, logical, scale_120, frames, grouping }).map(|()| true)
        }
        (None, None) => {
            // Before the window, which is the only place it can be: ADR 0023.
            let (standing, bus) = sk_app::take_the_name(named_path.as_deref());
            if standing == sk_app::Standing::Second {
                return std::process::ExitCode::SUCCESS;
            }
            window(Options {
                bus,
                // A named directory is an explicit request for one directory,
                // so the saved layout stays saved and is not opened over it.
                restore: named_path.is_none(),
                path,
                // `Some` only where a flag said so: the absence of `--light` is
                // not an instruction to be dark, it is the absence of one.
                dark: (!dark).then_some(false),
                sidebar: no_sidebar.then_some(false),
                stats,
                own_frame,
                scale_120: forced_scale,
                started,
            })
        }
    };

    match outcome {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(problem) => {
            // Both halves, to the two places they belong: the message a person
            // can act on, and the chain that explains it to us.
            eprintln!("skerry: {}", problem.message());
            // The second line of the message above: the debug chain, indented
            // under the sentence a person can act on.
            eprintln!("  {problem}"); // skerry-allow: log-prefix — continues the line above
            std::process::ExitCode::FAILURE
        }
    }
}

fn usage_error(message: &str) -> std::process::ExitCode {
    eprintln!("skerry: {message}\n\n{USAGE}");
    std::process::ExitCode::from(2)
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once(['x', 'X'])?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// A percentage, converted to the numerator over 120 that a compositor sends.
///
/// 125% is 150/120 exactly, which is the whole reason the scale is kept as an
/// integer rather than as a float.
fn parse_scale(text: &str) -> Option<u32> {
    let percent: u32 = text.trim_end_matches('%').parse().ok()?;
    (50..=400).contains(&percent).then(|| percent * 120 / 100)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The version and the commit, for `--version` and for a bug report.
///
/// `SKERRY_COMMIT` is stamped by `build.rs` at compile time; a build from a
/// tarball with no git history gets `unknown`, which is honest and still tells
/// a reader that the version beside it is the whole of what is known.
fn version() -> String {
    format!("skerry {} ({})", env!("CARGO_PKG_VERSION"), env!("SKERRY_COMMIT"))
}

const USAGE: &str = "\
skerry [OPTIONS] [PATH]

  PATH             the directory to open, defaulting to $HOME
  --light          use the light palette
  --stats          show the frame statistics overlay
  --keys           show the keyboard overlay
  --menu           show the context menu, for a golden frame of it
  --menu-row       show the menu a secondary click on a row opens
  --clash          show the panel that asks about a name already taken
  --stopped        show the strip after a stop: what is owed, and Pick up
  --search         show the search over everything under this folder
  --carrying       show the card that follows a drag of files
  --path-entry     show the path bar as a field, for a golden frame of it
  --palette        show the command palette, for a golden frame of it
  --filter         show the find popup, for a golden frame of it
  --crumbs         show a path-bar chevron's sibling menu, for a golden frame
  --crumbs-empty   show that menu over a folder with no folders in it
  --properties     show the Properties panel, for a golden frame of it
  --open-with      show the Open With list, for a golden frame of it
  --composing      show a rename with an input method composing in it
  --split          split the window, for a golden frame of two panes
  --rename         show a name being typed, for a golden frame of it
  --look-selected  select all of a text preview's text, for a frame of it
  --look-source    show a markdown preview as its source rather than rendered
  --look-editing   put a text preview into edit mode, for a frame of the caret
  --look-page <n>  which page or animation frame the quick look is on
  --look-zoom <f>  how much the quick look's picture is magnified
  --look-find <s>  open the page's find bar looking for this
  --look-inside <p> stand in this folder inside the archive being looked at
  --aside <px>     how wide the sidebar is, which decides what a volume's row fits
  --pinned         give the sidebar's sample a Pinned section, for a frame of it
  --tree           open one of the sidebar's places, for a frame of the tree
  --coloured       give two folders a colour, for a frame of the marks
  --no-sidebar     start with the list of places closed. F9 toggles it
  --own-frame      draw the window frame here rather than asking the compositor
                   to. Always on where the compositor offers no decorations,
                   which is GNOME.
  -h, --help       this
  -V, --version    the version and the commit it was built from

With no PATH, the window comes back the way it was left — the tabs, the
splits, the sort orders and the scroll offsets — from
$XDG_STATE_HOME/skerry/session. Naming a PATH opens that directory instead and
leaves the saved layout alone.

Rendering one frame without a display server, for a golden-frame test or for
looking at:

  --capture FILE   render one frame to FILE as a PNG and exit
  --bench FRAMES   render a scripted scroll and report p50 / p99 frame cost
  --size WxH       logical size for the capture (default 1100x720)
  --scale PERCENT  scale for the capture: 100, 125, 150, 200 (default 100)
  --at REL         capture a subdirectory of PATH, and start a column view's
                   trail at PATH — so a frame is a function of the directory
                   it was pointed at and not of what is above it

Once it is open, F1 lists the keyboard.";
