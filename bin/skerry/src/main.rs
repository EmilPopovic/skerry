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
fn window(options: &Options) -> Result<bool, sk_app::Problem> {
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
    progress: bool,
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
    composing: bool,
    split: bool,
    tab_drop: bool,
    rename: bool,
    checked: bool,
    collapsed: bool,
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
            "--progress" => &mut self.progress,
            "--indexing" => &mut self.indexing,
            "--grid" => &mut self.grid,
            "--columns" => &mut self.columns,
            "--grouped" => &mut self.grouped,
            "--grouped-size" => &mut self.grouped_size,
            "--grouped-kind" => &mut self.grouped_kind,
            "--checked" => &mut self.checked,
            "--collapsed" => &mut self.collapsed,
            "--search" => &mut self.search,
            "--carrying" => &mut self.carrying,
            "--path-entry" => &mut self.path_entry,
            "--palette" => &mut self.palette,
            "--filter" => &mut self.filter,
            "--crumbs" => &mut self.crumbs,
            "--crumbs-empty" => &mut self.crumbs_empty,
            "--properties" => &mut self.properties,
            "--open-with" => &mut self.open_with,
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
    /// A subdirectory of `path` to capture, and where a column view's trail
    /// starts. See the `--at` line in the usage.
    at: Option<String>,
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
        progress: overlays.progress,
        indexing: overlays.indexing,
        grid: overlays.grid,
        columns: overlays.columns,
        at: shape.at.clone(),
        grouped: overlays.grouped,
        grouped_size: overlays.grouped_size,
        grouped_kind: overlays.grouped_kind,
        checked: overlays.checked,
        collapsed: overlays.collapsed,
        search: overlays.search,
        carrying: overlays.carrying,
        path_entry: overlays.path_entry,
        palette: overlays.palette,
        filter: overlays.filter,
        crumbs: overlays.crumbs,
        crumbs_empty: overlays.crumbs_empty,
        properties: overlays.properties,
        open_with: overlays.open_with,
        composing: overlays.composing,
        split: overlays.split,
        tab_drop: overlays.tab_drop,
        rename: overlays.rename,
        stats: shape.stats,
        own_frame: shape.own_frame,
        sidebar: shape.sidebar,
    })
}

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
    let mut capture = None;
    let mut bench = None;
    let mut logical = (1100, 720);
    // `None` until `--scale` says otherwise, so the window can tell "asked for
    // 100%" from "not asked at all" — different things where the server states
    // one, and the only way to run at a fractional scale under a bare X server.
    let mut forced_scale = None;
    let mut at: Option<String> = None;

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
            "--scale" => match arguments.next().as_deref().and_then(parse_scale) {
                Some(scale) => forced_scale = Some(scale),
                None => return usage_error("--scale needs a percentage, such as 125"),
            },
            "--at" => match arguments.next() {
                Some(relative) => at = Some(relative),
                None => return usage_error("--at needs a path relative to the directory"),
            },
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
            other => path = Some(PathBuf::from(other)),
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
                at,
            },
            &overlays,
        )
        .map(|()| true),
        (None, Some(frames)) => {
            let grouping = overlays.grouping();
            sk_app::bench(&Bench { path, logical, scale_120, frames, grouping }).map(|()| true)
        }
        (None, None) => {
            window(&Options {
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

const USAGE: &str = "\
skerry [OPTIONS] [PATH]

  PATH             the directory to open, defaulting to $HOME
  --light          use the light palette
  --stats          show the frame statistics overlay
  --keys           show the keyboard overlay
  --menu           show the context menu, for a golden frame of it
  --menu-row       show the menu a secondary click on a row opens
  --clash          show the panel that asks about a name already taken
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
  --no-sidebar     start with the list of places closed. F9 toggles it
  --own-frame      draw the window frame here rather than asking the compositor
                   to. Always on where the compositor offers no decorations,
                   which is GNOME.
  -h, --help       this

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
