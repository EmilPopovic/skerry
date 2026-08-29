//! One frame, rendered with no display server and written to a PNG.
//!
//! Two jobs, and the second is the reason it lives in the product rather than in
//! a test:
//!
//! * **The golden-frame harness.** `docs/04-architecture.md` wants offscreen
//!   frames diffed against references at 100/125/150/200% on llvmpipe, so CI
//!   needs no GPU. This renders them.
//! * **Looking at the thing.** A renderer that can only be judged by running it
//!   on a machine with a screen is a renderer nobody reviews. This makes a frame
//!   a file.
//!
//! The frame is deliberately not empty: a fixed set of rows is selected, one
//! carries the cursor and one is hovered, and every visible folder is measured —
//! so a capture exercises the selection shape, the focus ring, the hover tint and
//! the size column rather than just the text.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sk_gpu::{DrawList, FrameStats, capture as png};
use sk_model::{Problem, ProblemKind};
use sk_plat::Size;
use sk_text::LayoutCache;
use sk_ui::{Painter, Theme};

use crate::frames::Frames;
use crate::headless::{READ_DEADLINE, Session};
use crate::layout::{Frame, Layout};
use crate::pane::Pane;
use crate::view::{self, Chrome};

/// Rows selected in a capture, and which of them carries the cursor.
const SELECTED: std::ops::Range<usize> = 2..5;
const CURSOR: usize = 3;
const HOVERED: usize = 7;

/// Rows a capture will measure folder totals for.
const MEASURED: usize = 120;

/// How many frames a capture may draw before giving up on a warm atlas.
const MAX_PASSES: usize = 8;

/// What to capture.
pub struct Capture {
    /// Where the PNG goes.
    pub file: PathBuf,
    /// The directory to read.
    pub path: PathBuf,
    /// Logical size.
    pub logical: (u32, u32),
    /// The scale as a numerator over 120, exactly as a compositor sends it.
    pub scale_120: u32,
    /// Whether to use the dark palette.
    pub dark: bool,
    /// Whether to draw the keyboard overlay.
    pub keys: bool,
    /// Whether to draw the statistics overlay.
    pub stats: bool,
    /// Whether to draw the context menu.
    pub menu: bool,
    /// Whether to draw the path bar as a field being typed into.
    pub path_entry: bool,
    /// Whether to draw the command palette, with a query typed into it.
    pub palette: bool,
    /// Whether to draw the sibling menu one of the path bar's chevrons opens.
    pub crumbs: bool,
    /// Whether to draw the Properties panel.
    pub properties: bool,
    /// Whether to draw the Open With list.
    pub open_with: bool,
    /// Whether to draw the rename field with an input method composing in it.
    pub composing: bool,
    /// Whether to split the window, with two tabs in the lower half.
    pub split: bool,
    /// Whether to draw that menu over a folder that holds no folders.
    pub crumbs_empty: bool,
    /// Whether to draw the find popup, with a query narrowing the list.
    pub filter: bool,
    /// Whether to draw a name being typed over a row, with its stem selected.
    pub rename: bool,
    /// Whether to draw the sidebar.
    pub sidebar: bool,
    /// Whether to draw the window frame the program draws for itself.
    ///
    /// A capture has no compositor to ask, so it is told. Without this the
    /// frame — the one GNOME requires, with its three buttons and its resize
    /// band — has no golden coverage at all.
    pub own_frame: bool,
}

/// The background menu, in the state a fresh window would have it in.
///
/// Nothing has been done, so Undo and Redo are dimmed, and nothing has been
/// copied, so Paste is. Stated rather than asked because a capture has no `App`
/// for the registry to ask — the same trade the navigation buttons' `tools`
/// makes, for the same reason. Highlighted, because that is the state a pointer
/// holds a menu in for as long as it is open.
fn sample_menu(here: &crate::panes::PaneLayout) -> Option<crate::menu::Menu> {
    let at = sk_plat::Point::new(here.body.x + 140.0, here.body.y + 96.0);
    let mut menu =
        crate::menu::build(crate::menu::ON_BACKGROUND, &crate::commands::registry(), at, |id| {
            !matches!(id, "edit.undo" | "edit.redo" | "file.paste" | "file.cut")
        })?;
    menu.step(true);
    menu.step(true);
    Some(menu)
}

/// The palette with a query in it, narrowed but not to one row.
///
/// `co` rather than nothing and rather than something exact: an empty query is a
/// frame of the whole registry, which is the keyboard overlay's job, and a query
/// matching one command would not show the ranking, the dimmed rows or the
/// highlight together. Dimmed the way a fresh window dims them — nothing done,
/// nothing copied — which is the same set `sample_menu` states and for the same
/// reason.
fn sample_palette() -> crate::palette::Palette {
    let commands = crate::commands::registry();
    let mut palette = crate::palette::open(&commands, |id| {
        !matches!(id, "edit.undo" | "edit.redo" | "file.paste_location")
    });
    palette.query = crate::field::Field::new("co".to_owned());
    palette.refilter();
    palette
}

/// The path bar as a field, with the caret five sixths of the way along it.
///
/// **Not at either end, and not in the middle.** The caret's position is the
/// shaped width of the text before it, so a caret at offset zero or at the end
/// would be in the right place even if that measurement were wrong; and far
/// enough along that the narrow frame has to scroll the text to show it, which
/// is the arithmetic that keeps a caret from being drawn outside its own box.
fn sample_path_entry(path: &std::path::Path) -> crate::field::Field {
    let mut field = crate::field::Field::new(path.to_string_lossy().into_owned());
    field.jump(false);
    for _ in 0..field.text.chars().count() * 5 / 6 {
        field.step(true);
    }
    field
}

/// Every overlay a capture was asked for, built once.
///
/// One struct rather than four locals because `run` is at the hundred-line limit
/// and the four of them are the same thing: a thing the window would be showing,
/// stated rather than reached.
struct Overlays {
    /// The sibling menu a path-bar chevron opens.
    crumbs: Option<crate::crumbs::CrumbMenu>,
    menu: Option<crate::menu::Menu>,
    path_entry: Option<crate::field::Field>,
    palette: Option<crate::palette::Palette>,
    naming: Option<crate::edit::Naming>,
    properties: Option<crate::properties::Properties>,
    openers: Option<crate::openers::Openers>,
}

/// A rename in progress over the row whose name has two dots in it.
///
/// `archive.tar.gz` is the case the stem rule is about: the selection has to reach
/// the **last** dot, so what is offered is `archive.tar` and `.gz` is kept. A name
/// with one extension would look the same whichever dot the rule used.
fn sample_naming(pane: &mut Pane) -> Option<crate::edit::Naming> {
    // Two dots where there are two to be had, because a stem selection that stops
    // at the *last* dot is what this frame is for. Any extension otherwise: a
    // filtered view does not hold `archive.tar.gz`, and asking for it anyway
    // rendered a focus ring where the field was supposed to be.
    fn has_extension(name: &str) -> bool {
        name.rfind('.').is_some_and(|dot| dot > 0)
    }
    let name_at = |row: &usize| pane.listing.at(*row).map(|id| pane.listing.name(id));
    let mut rows = 0..pane.listing.rows();
    let row = rows
        .clone()
        .find(|row| name_at(row).is_some_and(|name| name.contains(".tar.")))
        .or_else(|| rows.find(|row| name_at(row).is_some_and(has_extension)))?;
    let id = pane.listing.at(row)?;
    let name = pane.listing.name(id).to_owned();
    // The cursor goes here too, because a rename always happens on the focused
    // row — and the frame is only about the *second* box if the row that would
    // have drawn one is the row being renamed.
    pane.selection.set_cursor(Some(id));
    Some(crate::edit::Naming {
        row,
        path: pane.path.join(&name),
        field: crate::field::Field::naming(name),
    })
}

/// The sibling menu, as the chevron *before* the last component opens it.
///
/// So the rows are the leaf's own siblings and one of them is the leaf — which
/// is the state a person meets, and the only one where the "you are here" mark
/// is worth a frame at all. A menu anchored at one depth listing another's
/// children would be a frame that agreed with nothing.
fn sample_crumb_menu(layout: &Layout, empty: bool) -> crate::crumbs::CrumbMenu {
    let rows = if empty {
        Vec::new()
    } else {
        ["fixture", "rendered", "مستندات"].map(str::to_owned).to_vec()
    };
    crate::crumbs::CrumbMenu::new(
        2,
        std::path::PathBuf::from("/tmp/skerry-golden"),
        layout.path.x + 180.0,
        rows,
        (!empty).then_some("fixture"),
    )
}

/// Splits the sample tree, and puts a second tab in the new half.
///
/// The second and third panes read the same directory: a capture's frame has to
/// be the same on every machine, and a second directory would be a second thing
/// for the fixture to guarantee.
fn split_the_sample(panes: &mut crate::panes::Panes, session: &Session, request: &Capture) {
    let another = || {
        let (mut pane, _) = session.read_fully(&request.path);
        pane.selection.set_cursor(pane.listing.at(0));
        settle_folder_totals(&mut pane, session);
        pane
    };
    let id = panes.focus();
    panes.split_with(id, crate::panes::Axis::Below, false, another());
    let into = panes.focus();
    panes.put_tab(into, 1, another());
}

/// The Properties panel over one file, with every fact stated.
///
/// **Stated and not read.** A capture that ran `statx` on the fixture would put
/// this machine's uid, this machine's filesystem and this machine's block size
/// into a reference image, and the frame would differ on every other machine —
/// which is the same trade `sample_menu` makes with its dimming, for the same
/// reason.
fn sample_properties() -> crate::properties::Properties {
    let mut panel = crate::properties::Properties::new(vec![std::path::PathBuf::from(
        "/tmp/skerry-golden/fixture/archive.tar.gz",
    )]);
    panel.facts.push(sk_fs::facts::Facts {
        path: std::path::PathBuf::from("/tmp/skerry-golden/fixture/archive.tar.gz"),
        size: 12_582_912,
        on_disk: 12_587_008,
        mode: 0o644,
        modified: FIXED_MTIME,
        accessed: FIXED_MTIME,
        created: Some(FIXED_MTIME),
        uid: 1000,
        gid: 1000,
        links: 1,
        kind: sk_fs::facts::Kind::File,
        target: None,
        filesystem: Some("ext2/3/4"),
    });
    panel
}

/// The Open With list, over a type three applications claim.
///
/// **Stated, like every other sample here.** The real list is whatever
/// `mimeapps.list` says on the machine running the capture, which is a different
/// list on every machine and a different reference image on each.
fn sample_openers() -> crate::openers::Openers {
    let entry = |name: &str, binary: &str| sk_desktop::DesktopEntry {
        id: format!("{binary}.desktop"),
        path: std::path::PathBuf::from("/usr/share/applications"),
        name: name.to_owned(),
        exec: format!("{binary} %f"),
        terminal: false,
        mime_types: vec!["application/gzip".to_owned()],
        no_display: false,
        try_exec: None,
    };
    crate::openers::Openers::new(
        std::path::PathBuf::from("/tmp/skerry-golden/fixture/archive.tar.gz"),
        "application/gzip".to_owned(),
        vec![
            entry("Archive Manager", "org.gnome.FileRoller"),
            entry("Ark", "org.kde.ark"),
            entry("Text Editor", "org.gnome.TextEditor"),
        ],
    )
}

/// The instant the fixture's files are stamped with, which `tools/golden` sets
/// and this states so the panel's three dates are the same in every run.
const FIXED_MTIME: i64 = 981_158_400;

fn sample_overlays(
    request: &Capture,
    layout: &Layout,
    here: &crate::panes::PaneLayout,
    pane: &mut Pane,
    jobs: &crate::pane::Jobs,
) -> Overlays {
    // A query that matches some of the fixture and not all of it, so the frame
    // carries the count as well as the field.
    if request.filter {
        pane.set_filter("im".to_owned(), jobs);
    }
    Overlays {
        menu: request.menu.then(|| sample_menu(here)).flatten(),
        crumbs: (request.crumbs || request.crumbs_empty)
            .then(|| sample_crumb_menu(layout, request.crumbs_empty)),
        path_entry: request.path_entry.then(|| sample_path_entry(&request.path)),
        palette: request.palette.then(sample_palette),
        naming: (request.rename || request.composing).then(|| sample_naming(pane)).flatten().map(
            |mut naming| {
                // Three characters of Japanese being composed, underlined and
                // not yet part of the name — which is the one frame that would
                // catch the rule drawing over the text instead of under it.
                if request.composing {
                    naming.field.preedit = Some(("にほん".to_owned(), Some((9, 9))));
                }
                naming
            },
        ),
        properties: request.properties.then(sample_properties),
        openers: request.open_with.then(sample_openers),
    }
}

/// Walks every visible folder to completion, so the frame is reproducible.
///
/// Bounded by the same deadline as the read, because a capture of `/` is not a
/// reason to walk the whole disk.
fn settle_folder_totals(pane: &mut Pane, session: &Session) {
    let mut measurer = crate::sizes::Measurer::new(session.waker.handle());
    let rows = 0..pane.listing.rows().min(MEASURED);
    let deadline = std::time::Instant::now() + READ_DEADLINE;
    loop {
        // `update` applies what is known and claims what is not, so a pass that
        // claims nothing and has nothing running is a pass with nothing left.
        measurer.update(pane, rows.clone(), &session.jobs);
        measurer.drain();
        if measurer.stats().1 == 0 || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::yield_now();
    }
    // **One more pass after the loop ends.** `drain` records a total into the
    // cache and `update` is what copies it into the listing, so the last walk to
    // finish was recorded and never applied — and the frame drew a dash for a
    // folder whose size was known. It depended on which side of the last `drain`
    // the walk landed, which made the golden frames flicker between two images
    // and is exactly the kind of thing they exist to catch.
    measurer.update(pane, rows, &session.jobs);
    measurer.stop();
}

/// Puts the list into the state a golden frame is worth taking of.
///
/// A selection, a cursor and a hover, at fixed rows: a frame with none of them
/// covers none of the three shapes a row can have.
fn pose(pane: &mut Pane) {
    for row in SELECTED {
        if let Some(id) = pane.listing.at(row) {
            pane.selection.toggle(id);
        }
    }
    if let Some(id) = pane.listing.at(CURSOR) {
        pane.selection.set_cursor(Some(id));
    }
    if pane.listing.rows() > HOVERED {
        pane.hover = Some(HOVERED);
    }
}

/// Everything a captured frame's chrome borrows.
///
/// One value rather than seven arguments, because they are gathered together and
/// used together, and because the alternative is a function signature nobody can
/// read.
struct Sample<'a> {
    overlays: &'a Overlays,
    places: &'a [crate::sidebar::Section],
    types: &'a sk_desktop::MimeDatabase,
    keys: Option<&'a [(String, String)]>,
    hint: &'a str,
    leaves: &'a [crate::panes::PaneLayout],
    dividers: &'a [crate::panes::Divider],
}

/// The chrome of the frame this capture was asked for.
fn sample_chrome<'a>(request: &Capture, sample: &Sample<'a>) -> Chrome<'a> {
    Chrome {
        keys: sample.keys,
        stats: request.stats,
        typed: None,
        band: None,
        watching: true,
        notice: None,
        // The pointer is on a row, so nothing in the frame is hovered
        // — the hover shapes are covered by the GUI scenarios, which can
        // actually move a pointer.
        hovered_button: None,
        hovered_crumb: None,
        hovered_separator: None,
        crumb_menu: sample.overlays.crumbs.as_ref(),
        hovered_tool: None,
        places: sample.places,
        hovered_place: None,
        drop_row: None,
        naming: sample.overlays.naming.as_ref(),
        // What a freshly opened directory looks like: nowhere to go
        // back to, nowhere forward, a parent and a reload. Hardcoded
        // because a capture has no history — and it is the state worth
        // having a golden frame of, since it is the one with two of the
        // four dimmed.
        tools: [false, false, true, true],
        // **The filter is applied; the box only sometimes has the
        // keyboard.** `--filter --rename` is the state a rename started
        // from a search is in, and the frame that would catch its panel
        // going back to accent while the caret is somewhere else.
        filtering: request.filter && !request.rename,
        types: Some(sample.types),
        window: sk_plat::WindowState {
            activated: true,
            can_maximize: true,
            can_minimize: true,
            ..sk_plat::WindowState::default()
        },
        menu: sample.overlays.menu.as_ref(),
        path_entry: sample.overlays.path_entry.as_ref(),
        palette: sample.overlays.palette.as_ref(),
        properties: sample.overlays.properties.as_ref(),
        openers: sample.overlays.openers.as_ref(),
        hint: sample.hint,
        leaves: sample.leaves,
        dividers: sample.dividers,
        hovered_divider: None,
        hovered_tab: None,
        hovered_plus: false,
        tab_drop: None,
    }
}

/// Renders one frame and writes it.
pub fn run(request: &Capture) -> Result<(), Problem> {
    let mut session = Session::open(request.logical, request.scale_120)?;
    let (mut pane, _) = session.read_fully(&request.path);

    settle_folder_totals(&mut pane, &session);
    pose(&mut pane);

    // The overlay reads the command registry, so a capture of it builds one.
    let keys_rows = request.keys.then(|| {
        let registry = crate::commands::registry();
        crate::commands::overlay_rows(&registry)
    });

    // Read here rather than left as `None`: the mark at the head of a row is
    // coloured from it, so a golden frame without it is a golden frame of grey.
    let types = sk_desktop::MimeDatabase::load();
    // A fixed set rather than this machine's: the sidebar is part of the frame,
    // and a reference image that embeds somebody's `user-dirs.dirs` and mount
    // table is one nobody else can reproduce.
    let places = if request.sidebar { crate::sidebar::sample(&request.path) } else { Vec::new() };
    let theme = if request.dark { Theme::dark() } else { Theme::light() };
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let frame = if request.own_frame {
        Frame::Own { edges: true, minimize: true, maximize: true }
    } else {
        Frame::None
    };
    let layout = Layout::compute_with(logical, &theme.metrics, frame, request.sidebar);
    // One leaf: a capture is a frame of one directory, and a split is its own
    // frame rather than a variant of every other.
    let crate::panes::Single { mut panes, mut leaves, mut dividers, mut here } =
        crate::panes::single(pane, layout.content, &theme.metrics);
    if request.split {
        // **Stacked, and two tabs in the lower half.** A `Below` split is the
        // arrangement that puts a second column header halfway down the window,
        // which is where a rule drawn from the window's left edge instead of the
        // header's own crosses the sidebar — a line nothing else would catch.
        split_the_sample(&mut panes, &session, request);
        panes.layout(layout.content, &theme.metrics, &mut leaves, &mut dividers);
        here = leaves.first().copied().unwrap_or_default();
    }
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    let overlays = sample_overlays(request, &layout, &here, panes.focused_mut(), &session.jobs);

    // Frames until the atlas owes nothing. The first rasterizes; the second draws
    // from a warm atlas. A frame is repeated only while texels are still queued,
    // so a capture is never an image with holes in the text.
    let keyboard_hint = crate::commands::hint(&crate::commands::registry());
    let sample = Sample {
        overlays: &overlays,
        places: &places,
        types: &types,
        keys: keys_rows.as_deref(),
        hint: &keyboard_hint,
        leaves: &leaves,
        dividers: &dividers,
    };
    let mut stats = FrameStats::default();
    for pass in 0..MAX_PASSES {
        if pass >= 2 && session.renderer.pending_uploads() == 0 {
            break;
        }
        list.begin(session.physical);
        let mut diagnostics = crate::headless::offscreen(
            &session.renderer,
            session.pool_stats(),
            session.physical,
            &mut frames,
            stats,
            cache.len(),
            request.scale_120,
        );
        {
            let mut painter = Painter {
                list: &mut list,
                fonts: &mut session.fonts,
                cache: &mut cache,
                theme: &theme,
                scale: session.scale,
            };
            let chrome = sample_chrome(request, &sample);
            view::draw(
                &mut painter,
                &panes,
                &layout,
                now,
                &chrome,
                &mut diagnostics,
                &mut crate::view::Hits::default(),
            );
        }
        stats = session.renderer.draw(&mut list, &mut session.fonts, theme.palette.background);
    }

    let pixels = session.renderer.capture().ok_or_else(|| {
        Problem::plain(ProblemKind::NoGpu, "the offscreen target could not be read back")
    })?;
    write(&request.file, session.physical, &pixels)?;
    report(request, &session, panes.focused(), stats);
    Ok(())
}

/// What the capture contained, so a golden frame is comparable to the run that
/// produced it without opening the file.
///
/// The face chain is on its own line because it is the input that decides most
/// of the pixels and the only one that comes from outside the repository: the
/// fixture holds Hebrew, Arabic and an emoji, so a machine with a different set
/// of fonts installed renders a different frame from the same commit. A harness
/// comparing against a stored reference has to be able to say so.
fn report(request: &Capture, session: &Session, pane: &Pane, stats: FrameStats) {
    eprintln!(
        "skerry: wrote {} — {}×{} at {}%, {} entries, {} quads, {} draws, {} glyphs",
        request.file.display(),
        session.physical.0,
        session.physical.1,
        request.scale_120 * 100 / 120,
        pane.listing.rows(),
        stats.quads,
        stats.draws,
        session.renderer.atlas_stats().0,
    );
    for path in session.fonts.paths() {
        eprintln!("skerry: face {}", path.display());
    }
}

fn write(file: &Path, physical: (u32, u32), pixels: &[u8]) -> Result<(), Problem> {
    png::write_png(file, physical.0, physical.1, pixels).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, file.display().to_string(), error.to_string())
    })
}
