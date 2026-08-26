//! Spike D, arm 2 — the same capability list, through `winit` 0.30 and nothing
//! else.
//!
//! Two kinds of result live here. What `winit` *can* do is run and reported with
//! the value it produced. What it *cannot* do is reported as the absence of an
//! API, and that absence is checked mechanically against the crate's own source
//! rather than asserted, because "I could not find it" and "it is not there" are
//! different claims.
//!
//!     cargo run --release            run the window and report
//!     cargo run --release -- api     the API audit alone, no window

use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Every capability the [Wayland arm](../d-platform-and-dnd/README.md) covers,
/// with what `winit` offers for it. `reachable` means through the public API
/// only: dropping to `raw_window_handle` and speaking the protocol directly is
/// not counted, because a raw-protocol escape hatch is the thing ADR 0002 says we
/// would end up fighting.
struct Capability {
    name: &'static str,
    reachable: bool,
    /// The API that provides it, or the reason there is none. Checked against the
    /// crate source by `api_audit`.
    api: &'static str,
    note: &'static str,
}

const CAPABILITIES: &[Capability] = &[
    Capability {
        name: "window + xdg_shell",
        reachable: true,
        api: "Window",
        note: "what the crate is for",
    },
    Capability {
        name: "fractional scale",
        reachable: true,
        api: "ScaleFactorChanged",
        note: "reported as an f64; the 120ths numerator the protocol sends is not exposed, so a \
               buffer size has to be derived from a float that cannot represent 1.25 exactly in \
               every intermediate",
    },
    Capability {
        name: "keyboard + xkb",
        reachable: true,
        api: "KeyEvent",
        note: "logical and physical keys, text, and modifiers; xkbcommon is used internally",
    },
    Capability {
        name: "IME / text-input-v3",
        reachable: true,
        api: "set_ime_allowed",
        note: "Ime::{Preedit, Commit} with a cursor range, plus set_ime_cursor_area",
    },
    Capability {
        name: "drag-and-drop target",
        reachable: true,
        api: "DroppedFile",
        note: "paths only. No MIME list, no action negotiation, and no way to refuse a drop or to \
               say which of several types is wanted — a file manager has to accept everything or \
               nothing",
    },
    Capability {
        name: "drag-and-drop source",
        reachable: false,
        api: "start_drag",
        note: "no API. This is the half a file manager uses most, and ADR 0002's revisit trigger \
               names it first",
    },
    Capability {
        name: "clipboard",
        reachable: false,
        api: "clipboard",
        note: "no API, by design. Reaching it means a second crate with its own connection to the \
               display server, which is a second platform layer",
    },
    Capability {
        name: "primary selection",
        reachable: false,
        api: "primary_selection",
        note: "no API",
    },
    Capability {
        name: "dnd actions (copy/move/link)",
        reachable: false,
        api: "DndAction",
        note: "no API; a target cannot express a preferred action",
    },
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `CARGO_PKG_VERSION` here is this spike's version, which is 0.0.0 and says
    // nothing. The version under test is the one in the registry.
    println!("Spike D arm 2 — {}", winit_version());
    println!("  session      {}", std::env::var("XDG_SESSION_TYPE").unwrap_or_default());
    println!();
    api_audit();
    if args.first().map(String::as_str) == Some("api") {
        return;
    }
    println!();
    run_window();
}

/// The registry directory name of the crate we compiled against.
fn winit_version() -> String {
    let home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
    let registry = std::path::Path::new(&home).join("registry/src");
    let Ok(entries) = std::fs::read_dir(&registry) else { return "winit (version unknown)".into() };
    for e in entries.flatten() {
        let Ok(crates) = std::fs::read_dir(e.path()) else { continue };
        for c in crates.flatten() {
            let name = c.file_name().to_string_lossy().to_string();
            if name.starts_with("winit-0.30") {
                return name;
            }
        }
    }
    "winit (not found in the registry)".into()
}

/// Check each claim against `winit`'s own source in the cargo registry. A claim
/// that "there is no API" is only worth recording if it has been looked for, and
/// the line that was found is printed so the classification can be disagreed
/// with.
///
/// A bare substring search is not enough: `clipboard` occurs in `winit` 0.30 —
/// in a `TODO` saying the clipboard has not been added. That is the difference
/// between an API and an intention, and the audit has to be able to tell them
/// apart.
fn api_audit() {
    let lines = winit_source();
    println!("  API audit — {} source lines scanned", lines.len());
    for c in CAPABILITIES {
        let hits: Vec<&(String, String)> = lines.iter().filter(|(_, l)| l.contains(c.api)).collect();
        let declared = hits.iter().any(|(_, l)| is_declaration(l, c.api));
        let verdict = match (c.reachable, declared) {
            (true, true) => "PASS  reachable, declared",
            (true, false) => "FAIL  claimed reachable, no declaration found",
            (false, false) => "PASS  not reachable, no declaration",
            (false, true) => "CHECK claimed unreachable but a declaration exists",
        };
        println!("  {verdict:<48} {:<30} {}", c.name, c.api);
        // One line of evidence, preferring a declaration over a mention.
        let evidence = hits
            .iter()
            .find(|(_, l)| is_declaration(l, c.api))
            .or_else(|| hits.first())
            .map(|(f, l)| format!("{f}: {}", l.trim()))
            .unwrap_or_else(|| "no occurrence anywhere in the crate".into());
        println!("        {}", truncate(&evidence, 120));
    }
    println!();
    println!("  Consequences, in the order ADR 0002's revisit trigger asks for them:");
    for c in CAPABILITIES.iter().filter(|c| !c.reachable) {
        println!("    {} — {}", c.name, c.note);
    }
}

/// A declaration rather than a mention: not a comment, and either a `pub` item or
/// something shaped like a variant of a public enum.
fn is_declaration(line: &str, symbol: &str) -> bool {
    let t = line.trim();
    if t.starts_with("//") || t.starts_with("/*") || t.starts_with("*") {
        return false;
    }
    if t.contains("pub fn ") || t.contains("pub enum ") || t.contains("pub struct ") {
        return true;
    }
    // `DroppedFile(PathBuf),` and friends.
    t.starts_with(symbol) && (t.contains('(') || t.contains('{') || t.ends_with(','))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

/// The registry copy of the crate we are compiled against, concatenated. Not
/// elegant; it is the difference between a table and a guess.
fn winit_source() -> Vec<(String, String)> {
    let home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
    let mut out = Vec::new();
    let registry = std::path::Path::new(&home).join("registry/src");
    let Ok(entries) = std::fs::read_dir(&registry) else { return out };
    for e in entries.flatten() {
        let Ok(crates) = std::fs::read_dir(e.path()) else { continue };
        for c in crates.flatten() {
            let name = c.file_name().to_string_lossy().to_string();
            if !name.starts_with("winit-0.30") {
                continue;
            }
            collect(&c.path().join("src"), &mut out);
        }
    }
    out
}

fn collect(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                let file = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                for (i, line) in s.lines().enumerate() {
                    out.push((format!("{file}:{}", i + 1), line.to_string()));
                }
            }
        }
    }
}

struct App {
    window: Option<Window>,
    started: Instant,
    scale: f64,
    scale_events: u32,
    inner: (u32, u32),
    keys: Vec<String>,
    preedit: Vec<String>,
    committed: String,
    hovered: Vec<String>,
    dropped: Vec<String>,
    hold: Duration,
}

fn run_window() {
    let hold = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(6));
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        window: None,
        started: Instant::now(),
        scale: 0.0,
        scale_events: 0,
        inner: (0, 0),
        keys: Vec::new(),
        preedit: Vec::new(),
        committed: String::new(),
        hovered: Vec::new(),
        dropped: Vec::new(),
        hold,
    };
    event_loop.run_app(&mut app).expect("run");
    app.report();
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("skerry spike D (winit)")
            .with_inner_size(winit::dpi::LogicalSize::new(480.0, 320.0));
        let window = el.create_window(attrs).expect("window");
        // The IME has to be asked for; without this no preedit ever arrives, which
        // is the same shape as our own arm's `enable()`.
        window.set_ime_allowed(true);
        window.set_ime_cursor_area(
            winit::dpi::LogicalPosition::new(16.0, 96.0),
            winit::dpi::LogicalSize::new(320.0, 24.0),
        );
        self.scale = window.scale_factor();
        self.inner = (window.inner_size().width, window.inner_size().height);
        self.window = Some(window);
        self.started = Instant::now();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor;
                self.scale_events += 1;
            }
            WindowEvent::Resized(size) => self.inner = (size.width, size.height),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state.is_pressed() {
                    self.keys.push(format!("{:?}", event.logical_key));
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, range)) => {
                if !text.is_empty() {
                    self.preedit.push(format!("{text:?} cursor {range:?}"));
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => self.committed.push_str(&text),
            WindowEvent::HoveredFile(p) => self.hovered.push(p.display().to_string()),
            WindowEvent::DroppedFile(p) => self.dropped.push(p.display().to_string()),
            _ => {}
        }
        if self.started.elapsed() > self.hold {
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() && self.started.elapsed() > self.hold {
            el.exit();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl App {
    fn report(&self) {
        println!("  Runtime, over {:?}:", self.hold);
        println!(
            "    scale        {:.4} after {} ScaleFactorChanged events; inner size {}x{} physical",
            self.scale, self.scale_events, self.inner.0, self.inner.1
        );
        println!("    keys         {:?}", self.keys);
        println!("    preedit      {:?}", self.preedit);
        println!("    committed    {:?}", self.committed);
        println!("    hovered      {:?}", self.hovered);
        println!("    dropped      {:?}", self.dropped);
        println!();
        // The one number that decides the revisit trigger.
        let missing = CAPABILITIES.iter().filter(|c| !c.reachable).count();
        println!(
            "  {} of {} capabilities are unreachable through the public API",
            missing,
            CAPABILITIES.len()
        );
    }
}
