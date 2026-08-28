//! Skerry.
//!
//! Parses the little there is to parse, then hands over to `sk-app`. Everything
//! else lives in a crate, because a binary that grows logic is a binary nothing
//! can test.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Instant;

use sk_app::{App, Bench, Capture, Options};

fn main() -> std::process::ExitCode {
    // Taken first, so the cold-start figure includes argument parsing, the
    // allocator waking up, and every dynamic library the loader pulls in.
    let started = Instant::now();

    let mut path = None;
    let mut dark = true;
    let mut stats = false;
    let mut keys = false;
    let mut menu = false;
    let mut path_entry = false;
    let mut palette = false;
    let mut filter = false;
    let mut rename = false;
    let mut own_frame = false;
    let mut no_sidebar = false;
    let mut capture = None;
    let mut bench = None;
    let mut logical = (1100, 720);
    // `None` until `--scale` says otherwise, so the window can tell "asked for
    // 100%" from "not asked at all" — different things where the server states
    // one, and the only way to run at a fractional scale under a bare X server.
    let mut forced_scale = None;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--light" => dark = false,
            "--stats" => stats = true,
            "--keys" => keys = true,
            "--menu" => menu = true,
            "--path-entry" => path_entry = true,
            "--palette" => palette = true,
            "--filter" => filter = true,
            "--rename" => rename = true,
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
            "--help" | "-h" => {
                println!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option `{other}`"));
            }
            other => path = Some(PathBuf::from(other)),
        }
    }

    let path = path.or_else(home).unwrap_or_else(|| PathBuf::from("/"));
    // `.` and `../..` are legitimate arguments and illegitimate things to show in
    // a path bar. Resolved here rather than in the pane, because a pane's path is
    // the answer to "where am I" and should never have needed resolving.
    let path = std::fs::canonicalize(&path).unwrap_or(path);

    let scale_120 = forced_scale.unwrap_or(120);
    let outcome = match (capture, bench) {
        (Some(file), _) => sk_app::capture(&Capture {
            file,
            path,
            logical,
            scale_120,
            dark,
            keys,
            menu,
            path_entry,
            palette,
            filter,
            rename,
            stats,
            own_frame,
            sidebar: !no_sidebar,
        })
        .map(|()| true),
        (None, Some(frames)) => {
            sk_app::bench(&Bench { path, logical, scale_120, frames }).map(|()| true)
        }
        (None, None) => {
            App::new(Options { path, dark, stats, own_frame, scale_120: forced_scale, started })
                .map(|mut app| {
                    // A compositor that goes away is not the user closing the window, and
                    // the difference is worth a line on the way out.
                    if let Some(failure) = app.run() {
                        eprintln!("skerry: the display server went away — {failure}");
                        return false;
                    }
                    true
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
            eprintln!("  {problem}");
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
  --path-entry     show the path bar as a field, for a golden frame of it
  --palette        show the command palette, for a golden frame of it
  --filter         show the find popup, for a golden frame of it
  --rename         show a name being typed, for a golden frame of it
  --no-sidebar     start with the list of places closed. F9 toggles it
  --own-frame      draw the window frame here rather than asking the compositor
                   to. Always on where the compositor offers no decorations,
                   which is GNOME.
  -h, --help       this

Rendering one frame without a display server, for a golden-frame test or for
looking at:

  --capture FILE   render one frame to FILE as a PNG and exit
  --bench FRAMES   render a scripted scroll and report p50 / p99 frame cost
  --size WxH       logical size for the capture (default 1100x720)
  --scale PERCENT  scale for the capture: 100, 125, 150, 200 (default 100)

Once it is open, F1 lists the keyboard.";
