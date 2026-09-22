// SPDX-License-Identifier: Apache-2.0
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

/// What one captured frame is of. One value, because these five arrive together
/// and `warm_and_draw` is at the argument limit without them.
struct Drawing<'a> {
    request: &'a Capture,
    sample: &'a Sample<'a>,
    panes: &'a crate::panes::Panes,
    layout: &'a Layout,
    now: i64,
}

/// Draws until the glyph atlas owes nothing, and returns the last frame's figures.
///
/// **The first pass rasterizes and the second draws from a warm atlas.** A frame is
/// repeated only while texels are still queued, so a capture is never an image with
/// holes in the text.
fn warm_and_draw(
    session: &mut crate::headless::Session,
    drawing: &Drawing<'_>,
    list: &mut DrawList,
    cache: &mut LayoutCache,
    frames: &mut Frames,
    theme: &sk_ui::Theme,
) -> FrameStats {
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
            frames,
            stats,
            cache.len(),
            drawing.request.scale_120,
        );
        {
            let mut painter =
                Painter { list, fonts: &mut session.fonts, cache, theme, scale: session.scale };
            let chrome = sample_chrome(drawing.request, drawing.sample);
            view::draw(
                &mut painter,
                drawing.panes,
                drawing.layout,
                drawing.now,
                &chrome,
                &mut diagnostics,
                &mut crate::view::Hits::default(),
            );
        }
        stats = session.renderer.draw(list, &mut session.fonts, theme.palette.background);
    }
    stats
}

/// The text half of a sample: the wrap, the selection and edit mode.
///
/// **Its own function because `sample_quick_look` is at the hundred-line limit**,
/// and this is the half that has nothing to do with a child: a text file is read
/// in this process, and everything below the early return is a round trip.
/// Measures text with the capture's own faces, for the markdown layout.
///
/// The window has one of these too — one measurer per owner of a font stack, and
/// both convert logical to device in the same place.
struct Measurer<'a> {
    cache: &'a mut LayoutCache,
    fonts: &'a mut sk_text::Fonts,
    scale: f32,
}

impl crate::mdlayout::Measure for Measurer<'_> {
    fn width_of(&mut self, text: &str, weight: sk_text::Weight, size: f32) -> f32 {
        sk_ui::advance_of(self.cache, self.fonts, text, weight, size * self.scale) / self.scale
    }
}

/// Which row `--look` names, and what it can be shown as.
fn looked_at(
    request: &Capture,
    pane: &Pane,
) -> Option<(crate::previews::Shows, std::path::PathBuf, i64)> {
    let wanted = request.looking.as_deref()?;
    let types = sk_desktop::MimeDatabase::load();
    let row = (0..pane.listing.rows())
        .filter_map(|row| pane.listing.at(row))
        .find(|id| pane.listing.name(*id) == wanted)?;
    let kind = sk_desktop::kind_of(&pane.listing, row, Some(&types));
    let shows = crate::previews::shows(wanted, kind, Some(&types))?;
    Some((shows, pane.path_of(row), pane.listing.mtime(row)))
}

/// The layout the window does before it draws — `App::lay_the_markdown_out`.
///
/// **Repeated here on purpose.** A capture builds its own sample and never runs
/// the frame loop, so a step that lives there has to be done here as well or the
/// frame is of a state the program never has — the trap `12-measurement.md`
/// records against the text preview's own wrap.
fn lay_the_markdown_out(
    look: &mut crate::quicklook::QuickLook,
    where_: (crate::layout::Area, &Theme, &mut LayoutCache),
    fonts: &mut sk_text::Fonts,
    scale: f32,
) {
    let (inner, theme, cache) = where_;
    if !look.rendered || look.blocks.is_empty() {
        return;
    }
    let size = theme.metrics.text_size;
    let room = (inner.width - crate::view::MARKDOWN_PAD * 2.0).max(size);
    let mut shaper = Measurer { cache, fonts, scale };
    look.laid = Some(crate::mdlayout::lay_out(&look.blocks, room, size, scale, &mut shaper));
}

fn sample_text(
    request: &Capture,
    session: &mut Session,
    look: &mut crate::quicklook::QuickLook,
    where_: (crate::layout::Area, &Theme, &mut LayoutCache),
    scale: f32,
) {
    let (inner, theme, cache) = where_;
    // **Cut to the panel's width, the same way a window does it.** The rows are
    // the wrap and the wrap is what is drawn, so a capture that skipped this
    // would render long lines elided — a frame of a state the program no longer
    // has. `App::fit_the_look` is the window's half.
    if let Some(text) = look.text.as_mut() {
        let cells = crate::view::text_cells(
            text.lines.len(),
            (inner, &theme.metrics),
            (cache, &mut session.fonts, scale),
        );
        text.wrap(cells);
    }
    lay_the_markdown_out(look, (inner, theme, cache), &mut session.fonts, scale);
    if request.look_selected {
        look.select_all();
    }
    if request.look_editing {
        look.start_editing();
        look.put_caret(crate::quicklook::Spot { line: 2, byte: 4 });
        // **With an edit outstanding**, because that is the state whose strip is
        // worth a frame: Done beside Discard, which is the only way out that
        // writes nothing and therefore the one that has to be visible.
        look.unsaved = true;
    }
}

/// The history strip's height, the same way `App::history_height` works it out.
fn history_height(request: &Capture, metrics: &sk_ui::Metrics) -> f32 {
    if !request.history {
        return 0.0;
    }
    metrics.header_height
        + (SAMPLE_EDITS.min(crate::view::HISTORY_ROWS) as f32) * metrics.row_height
}

/// Waits for an archive's listing, and stands where `--look-inside` said.
///
/// **The same wait `sample_text` makes, for the same reason.** A listing comes
/// back on a job and a capture has one frame, so a frame taken without waiting
/// is the panel's "Reading the archive…" every time — a reference that says
/// nothing about the list it exists to show.
fn sample_archive(
    request: &Capture,
    look: &mut crate::quicklook::QuickLook,
    deadline: std::time::Instant,
) {
    while !look.drain_listing() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    if !request.look_inside.is_empty()
        && let Some(archive) = look.inside.as_mut()
    {
        archive.go_to(&request.look_inside);
    }
}

/// Opens a quick look and waits for it, so the one frame that gets taken has it.
///
/// The window does this over frames — `App::settle_quick_look` asks and a later
/// frame draws — and a capture has one frame, so it waits here. The same bargain
/// `settle_thumbnails` makes and the same deadline.
fn sample_quick_look(
    request: &Capture,
    session: &mut Session,
    pane: &Pane,
    where_: (&Layout, &Theme, &mut LayoutCache),
) -> Option<crate::quicklook::QuickLook> {
    let (layout, theme, cache) = where_;
    let (shows, path, mtime) = looked_at(request, pane)?;
    let mut look = crate::quicklook::QuickLook::open(path.clone(), mtime, shows, &session.jobs);

    // The same box the panel will be drawn into, in device pixels — the one
    // number that crosses the logical/device line on purpose, because a page
    // rendered at logical pixels on a 200% screen is a page at half the
    // resolution of the screen showing it.
    let inner = crate::view::quick_look_area(layout);
    let scale = f32::from(u16::try_from(request.scale_120).unwrap_or(120)) / 120.0;
    let within = ((inner.width * scale) as u32, (inner.height * scale) as u32);
    let deadline = std::time::Instant::now() + READ_DEADLINE;
    if shows == crate::previews::Shows::Archive {
        sample_archive(request, &mut look, deadline);
        return Some(look);
    }
    if shows == crate::previews::Shows::Text {
        while !look.drain() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        if request.look_source {
            look.rendered = false;
        }
        sample_text(request, session, &mut look, (inner, theme, cache), scale);
        return Some(look);
    }
    // **The frame or page asked for, set before the first request.** A picture
    // with frames is asked for by page like a document is, and `--look-page` is
    // how a frame of the *third* frame exists at all.
    if request.look_page > 0 {
        look.page = request.look_page;
    }
    if request.look_zoom > 1.0 {
        // Panned as well as zoomed, and off centre on purpose: a frame of a
        // picture zoomed about its own middle would look the same whether the pan
        // arithmetic worked or not. **Clamped by the same function the window
        // clamps with**, once the picture has arrived and its size is known — the
        // first render of this frame panned a picture that fitted the panel, which
        // is a state the window does not have.
        look.zoom = request.look_zoom;
        look.pan = (inner.width / 4.0, -inner.height / 6.0);
    }
    let mut previews = sk_preview::Previews::new();
    let helper = match shows {
        crate::previews::Shows::Page => sk_preview::Kind::Document,
        _ => sk_preview::Kind::Image,
    };
    // **Asks again for as long as the look still wants something**, which is what
    // makes this the window's answer and not a coarser one. A document is fetched
    // twice — a third-size draft that reaches the screen in a third of the time,
    // then the full size — and a frame blessed on the draft would be a reference
    // that only matches when the machine is slow.
    // **Which shot has actually been answered**, because "there is an image" is
    // not "there is nothing outstanding": the first version broke out of this loop
    // in the round *after* asking for the full size, so every document frame was
    // blessed on the third-size draft.
    let mut served = None;
    let mut minted = u64::MAX;
    while std::time::Instant::now() < deadline {
        if look.wants(within) {
            let shot = look.shot(within);
            look.asked = Some(shot);
            previews.want(sk_preview::Want {
                kind: helper,
                ask: sk_preview::Ask::Preview,
                path: path.clone(),
                mtime,
                within: shot.within,
                page: shot.page,
                ahead: false,
            });
        } else if look.image.is_some() && served == look.asked {
            break;
        }
        previews.pump(&session.jobs.pool, &session.waker.handle());
        for answer in previews.drain() {
            // **The count as well as the pixels.** A frame that took only the
            // image drew the page control saying `1` on a document 66 pages long,
            // which is the panel's "unknown" shape standing in for a known answer.
            if answer.pages > 0 {
                look.pages = answer.pages;
            }
            if answer.delay_ms > 0 {
                look.delay_ms = answer.delay_ms;
            }
            // The page's words, so a frame of a selection over one exists at all
            // — ADR 0024, and the frame is what says the transform from points to
            // pixels is the right one.
            if !answer.words.is_empty() {
                look.page_text = crate::pagetext::Page::of(&answer.words);
            }
            if answer.refusal == sk_preview::Refusal::None && !answer.pixels.is_empty() {
                // **A key per answer, counting down from the top of the
                // range.** It was one fixed key, on the grounds that a capture
                // draws exactly one preview — which stopped being true when a
                // document began arriving as a draft and then a full render:
                // the second `hold` was refused as a key already held, and the
                // frame was blessed on the draft, upscaled and soft. Counting
                // down keeps these clear of `Thumbs`, which counts up.
                minted -= 1;
                let key = sk_gpu::ImageKey(minted);
                session.renderer.hold_preview(key, answer.size, &answer.pixels);
                look.took(key, answer.size, answer.key.page);
                look.pan = crate::view::clamped_pan(
                    inner,
                    answer.size,
                    look.drafted,
                    (look.zoom, look.pan),
                );
                served = Some(crate::quicklook::Shot {
                    within: answer.key.within,
                    page: answer.key.page,
                });
            } else {
                look.refusal = Some(answer.refusal);
                break;
            }
        }
        if look.refusal.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    // **After the page has arrived, because the find is over its words.** A bar
    // with a query and no matches is a different frame from a bar with both, and
    // the one worth having is the one that found something.
    if !request.look_find.is_empty() && look.page_text.is_some() {
        look.toggle_finding();
        if let Some(field) = look.finding.as_mut() {
            field.insert(&request.look_find);
        }
        look.find_again(0);
    }
    // **After the page has arrived, because the selection is over its words.**
    // `--look-selected` is one flag for both kinds of text: over a file it
    // selects the lines, and over a page it selects everything the tool found.
    if request.look_selected {
        look.select_all();
    }
    Some(look)
}

/// An undo stack for the frame, with a sample history on it where one was asked
/// for.
fn sample_edits(request: &Capture, session: &crate::headless::Session) -> crate::edit::Edits {
    let mut edits = crate::edit::Edits::new(session.waker.handle());
    if request.history {
        sample_history(&mut edits, &request.path);
    }
    edits
}

/// How many edits the sample history holds, and how many rows it therefore draws.
const SAMPLE_EDITS: usize = 5;

/// Puts a history on the stack, with one of it undone.
///
/// **The interesting state, not the easy one.** A history with nothing undone shows
/// four rows the same colour and says nothing about what the panel is for; one step
/// back from the end draws the present marker with a future under it in grey, which
/// is the whole of the design in one frame.
///
/// The kinds are chosen to cover every `Change::summary` shape a person can reach:
/// a folder, a rename, a batch that reports a count, and a trash.
fn sample_history(edits: &mut crate::edit::Edits, root: &std::path::Path) {
    use sk_fs::ops::Change;
    let at = |name: &str| root.join(name);
    edits.history.record(Change::Created { path: at("build") });
    edits.history.record(Change::Renamed { from: at("old.txt"), to: at("étude.txt") });
    edits.history.record(Change::Batch(vec![
        Change::Copied { source: at("a"), destination: at("img2.png") },
        Change::Copied { source: at("b"), destination: at("img10.png") },
        Change::Copied { source: at("c"), destination: at("report.pdf") },
    ]));
    edits.history.record(Change::Trashed {
        original: at("Makefile"),
        stored: at("Makefile"),
        info: at("Makefile.trashinfo"),
    });
    edits.history.record(Change::Renamed { from: at("draft"), to: at("holiday.jpeg") });
    // One step back, so the last row is a future rather than the present.
    let _ = edits.history.undo(|_| true);
}

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
    /// Whether to draw the menu a secondary click on a row opens instead.
    pub row_menu: bool,
    /// Whether to draw that menu with its Colour submenu open beside it.
    pub menu_colours: bool,
    /// Whether to draw that menu with its Open with submenu open, listing two
    /// applications the capture states rather than a job found.
    pub menu_open_with: bool,
    /// Whether to draw the panel that asks about a name already taken.
    pub clash: bool,
    /// Whether to draw the question a permanent deletion asks.
    pub erase: bool,
    /// Whether to draw the same panel asking about a Move to Trash instead —
    /// the `confirm_trash` setting, which is off unless somebody turns it on.
    pub trash_question: bool,
    /// Whether to draw the status strip mid-operation: the bar behind its own
    /// text, with Pause and Stop in it.
    pub progress: bool,
    /// Whether to draw the strip after an operation was stopped: what it still
    /// owes, and the button that picks it up.
    pub stopped: bool,
    /// Whether to draw the strip mid-index-build, which is the same bar over a
    /// different operation — and the frame that would catch the two drifting
    /// apart.
    pub indexing: bool,
    /// Which icon theme to draw the marks from, or nothing for our own.
    ///
    /// **A capture's third source, and the reason it is a flag** — [ADR
    /// 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).
    /// A window asks the desktop what it is using; a frame cannot, because a
    /// golden frame of "whatever this machine had installed" is a frame of the
    /// build box. So a capture names the theme, and the release gate is one frame
    /// per source over one directory.
    pub icons: Option<String>,
    /// Whether to lay the pane out as a grid instead of a details list.
    pub grid: bool,
    /// Whether to lay it out as a run of columns, one per level of the path.
    pub columns: bool,
    /// A subdirectory of `path` to open instead of it, and the depth the trail is
    /// capped to.
    ///
    /// **So a frame of a column view is a function of the fixture.** The real
    /// ancestors of any fixture reach `/tmp` and then `/`, and what is in `/tmp`
    /// is not the fixture's business — see [`crate::trail::settled`].
    pub at: Option<String>,
    /// Whether to cut the pane into time headings, which is orthogonal to `grid`.
    pub grouped: bool,
    /// The same, banded on size instead: folders, then gigantic down to empty.
    pub grouped_size: bool,
    /// The same, banded on `sk_desktop::Category` — the taxonomy that also
    /// colours each row's mark, so this is the frame where a heading disagreeing
    /// with the marks under it would be visible.
    pub grouped_kind: bool,
    /// Whether to hover a row that is *selected*, so the check box is filled.
    ///
    /// **Because `pose` cannot be both.** It hovers row seven and selects rows two
    /// to four, so the box under the pointer is empty in every frame that has one
    /// and the state that says "this one is chosen" was in none of them. Row zero,
    /// which is the one row every fixture has — a trail's own column holds three.
    pub checked: bool,
    /// Whether to collapse one time band, which is filtering by group.
    ///
    /// Yesterday's, so the frame holds an open band, a shut one with its count
    /// beside it, and two more open below — which is the only way to see that a
    /// collapse takes the rows and leaves the heading.
    pub collapsed: bool,
    /// Whether that look's text is all selected, for a frame of the highlight.
    ///
    /// A flag rather than two coordinates: what a frame has to show is that a
    /// selection is drawn behind the runs and across a blank line, and the whole
    /// file shows that in one picture.
    pub look_selected: bool,
    /// Whether a markdown preview shows its source rather than the rendering.
    ///
    /// **Rendered is the default and this is the frame of the other half** — ADR
    /// 0029. Not a remembered mode: the flag exists so both states have a
    /// picture, which is what a toggle owes.
    pub look_source: bool,
    /// Whether that preview is in edit mode, with a caret in it.
    pub look_editing: bool,
    /// Which quick look to draw, by the name of the file it is of.
    ///
    /// A name rather than a flag, because the three kinds — a picture, a page and
    /// text — are three frames and the fixture holds one of each.
    pub looking: Option<String>,
    /// Whether to draw the visible history, expanded, with edits in it.
    pub history: bool,
    /// Whether to draw the subtree search, with results in it.
    pub search: bool,
    /// Whether to draw the card that follows a drag of files.
    pub carrying: bool,
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
    /// Whether to draw the settings panel.
    pub settings: bool,
    /// Whether to draw the rename field with an input method composing in it.
    pub composing: bool,
    /// Whether to split the window, with two tabs in the lower half.
    pub split: bool,
    /// Whether to mark the second of those tabs as the one a drag of files would
    /// land in. Nothing without `split`: a leaf with one tab draws no strip.
    pub tab_drop: bool,
    /// Whether to draw that menu over a folder that holds no folders.
    pub crumbs_empty: bool,
    /// Whether to draw the find popup, with a query narrowing the list.
    pub filter: bool,
    /// Whether to draw a name being typed over a row, with its stem selected.
    pub rename: bool,
    /// Whether to draw the sidebar.
    pub sidebar: bool,
    /// Whether the sidebar's sample carries a Pinned section.
    ///
    /// Off by default so that adding the section did not re-bless every frame
    /// the sidebar is in, which is all of them.
    pub pinned: bool,
    /// Whether one of its places is open, with what is under it.
    pub tree: bool,
    /// What the page's find bar is looking for, where a frame wants one open.
    pub look_find: String,
    /// How much the quick look's picture is magnified, where a frame wants it
    /// zoomed.
    ///
    /// **Because a zoom is not only a bigger drawing.** It asks the child for a
    /// bigger picture and clips what comes back to the panel, and a capture that
    /// could only render the fitted case would render neither half.
    pub look_zoom: f32,
    /// Which page or frame the quick look is on, where a frame wants one other
    /// than the first.
    ///
    /// **Because an animation's third frame is a different picture from its
    /// first**, and a capture that could only ever ask for frame one would render
    /// the one case where composing frames in order cannot go wrong.
    pub look_page: u32,
    /// The folder *inside* an archive the panel should be standing in.
    ///
    /// **A path rather than a count of Enter presses**, because that is what the
    /// panel's own state is: `Inside::at` names one level, and a frame that got
    /// there by pressing keys would be a frame of the key handling.
    pub look_inside: String,
    /// How wide the sidebar is, where a frame wants one other than the default.
    ///
    /// **Because the width decides what a volume's row can say.** The figure, the
    /// gauge and the name compete for one row and the figure gives up its word
    /// first, so the default width and a dragged-open one are two different
    /// frames — and only one of them was ever rendered.
    pub aside: Option<f32>,
    /// Whether two of the listing's folders carry a colour somebody chose.
    pub coloured: bool,
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
            // `index.forget` among them: a fresh window has no index, so a
            // frame that drew that row bright would be a frame of a state this
            // menu is never opened in.
            !matches!(id, "edit.undo" | "edit.redo" | "file.paste" | "file.cut" | "index.forget")
        })?;
    menu.step(true);
    menu.step(true);
    Some(menu)
}

/// The menu a secondary click on a row opens, over a folder.
///
/// **Over a folder, not a file.** Open and Open in a new tab are the two rows
/// whose enabling depends on what the pointer is over, and a frame taken over a
/// file would show them both dimmed and prove nothing about either. Paste is
/// dimmed because nothing has been copied and Copy the text because a folder has
/// none — a fresh window with the pointer on a folder, which is the state this
/// menu is most often opened in.
fn sample_row_menu(here: &crate::panes::PaneLayout) -> Option<crate::menu::Menu> {
    let at = sk_plat::Point::new(here.name.x + 60.0, here.body.y + 40.0);
    let mut menu =
        crate::menu::build(crate::menu::ON_ROW, &crate::commands::registry(), at, |id| {
            !matches!(id, "file.paste" | "file.copy_content")
        })?;
    menu.step(true);
    Some(menu)
}

/// Cuts the sample pane into time headings.
///
/// **The fixture's files all share one mtime**, so no set of edges can band them
/// and the first version of this frame drew one heading over everything. The
/// times are spread here rather than on disk: every other frame reads the same
/// fixture, and a Modified column that moved would re-bless all of them.
///
/// The edges are the product's, asked about the fixture's newest file rather than
/// about now, so the frame means the same thing tomorrow: every offset below is a
/// whole number of days from a local midnight, and which band each falls in is the
/// same arithmetic on any day of any year.
fn group_the_sample(
    pane: &mut Pane,
    grouping: crate::groups::Grouping,
    types: &sk_desktop::MimeDatabase,
    collapse: bool,
) {
    const DAY: i64 = 86_400;
    pane.grouping = grouping;
    if grouping != crate::groups::Grouping::Time {
        // Size and kind band on what the fixture already is, so there is nothing
        // to arrange: the sizes are truncated to fixed byte counts and the names
        // are the adversarial set, which between them reach four size bands and
        // five kinds.
        pane.sort.bands = grouping.banding(0);
        pane.resort_now(Some(types));
        return;
    }
    let base = (0..pane.listing.rows())
        .filter_map(|row| pane.listing.at(row))
        .map(|id| pane.listing.mtime(id))
        .max()
        .unwrap_or_default();
    // Four bands, and one day inside each: today, yesterday, and the two windows.
    let spread = [0i64, -1, -3, -10];
    let ids: Vec<_> = (0..pane.listing.rows()).filter_map(|row| pane.listing.at(row)).collect();
    for (at, id) in ids.iter().enumerate() {
        let size = pane.listing.size(*id);
        let days = spread[at % spread.len()];
        pane.listing.set_meta(*id, size, base + days * DAY, sk_model::Flags::NONE);
    }
    // **The product's own edges, asked about the fixture's newest file.** Cutting
    // them here by hand drew four headings too, and never called `day_bands` — so
    // the frame agreed with this function rather than with the program, and three
    // of them recorded a heading that named the wrong window as correct.
    pane.sort.bands = grouping.banding(base);
    pane.resort_now(None);
    if collapse {
        // Band two is yesterday. `reband` first, because `toggle_band` refuses a
        // pane whose runs have not been worked out — an ungrouped pane has no band
        // to collapse, and one that has not been banded yet looks the same.
        pane.reband();
        pane.toggle_band(2);
    }
}

/// Which grouping the flags asked for, if any.
///
/// One at a time: the three flags are three values of one setting, and a frame
/// banded twice would be a frame of whichever call ran last.
/// Whichever layout the flags asked for.
fn mode_the_sample(
    panes: &mut crate::panes::Panes,
    request: &Capture,
    session: &crate::headless::Session,
    width: f32,
) {
    if request.grid {
        panes.focused_mut().mode = crate::cells::Mode::Grid;
    }
    if request.columns {
        // As deep as `--at` went, so the shallowest column is the directory the
        // harness was pointed at and nothing above it is on screen.
        let most = request.at.as_deref().map_or(0, |at| Path::new(at).components().count());
        column_the_sample(panes.focused_mut(), session, width, most);
    }
}

/// Puts the sample pane into column mode with every ancestor already read.
fn column_the_sample(
    pane: &mut crate::pane::Pane,
    session: &crate::headless::Session,
    width: f32,
    most: usize,
) {
    pane.mode = crate::cells::Mode::Columns;
    let path = pane.path.clone();
    pane.trail = crate::trail::settled(&path, most, |at| session.read_fully(at).0.listing);
    let levels = pane.trail.len();
    let crate::pane::Pane { across, revealed, .. } = pane;
    crate::trail::follow(across, revealed, levels, width);
}

fn requested_grouping(request: &Capture) -> Option<crate::groups::Grouping> {
    [
        (request.grouped, crate::groups::Grouping::Time),
        (request.grouped_size, crate::groups::Grouping::Size),
        (request.grouped_kind, crate::groups::Grouping::Kind),
    ]
    .into_iter()
    .find_map(|(wanted, grouping)| wanted.then_some(grouping))
}

/// Whichever of the two context menus was asked for, with a submenu open if
/// one was.
fn sample_context_menu(
    request: &Capture,
    here: &crate::panes::PaneLayout,
) -> Option<crate::menu::Menu> {
    let over_a_row = request.row_menu || request.menu_colours || request.menu_open_with;
    let mut menu = match (request.menu, over_a_row) {
        (_, true) => sample_row_menu(here),
        (true, false) => sample_menu(here),
        (false, false) => None,
    }?;
    if request.menu_colours {
        // Remove the colour is dimmed, because a folder that has no colour has
        // nothing to take away — the state the submenu opens in over a fresh
        // folder, and the one row of it that draws no swatch.
        expand_sample(&mut menu, crate::menu::COLOUR.id, |id| id != "view.colour_none");
    } else if request.menu_open_with {
        // Two applications, stated: what a job would find depends on the
        // machine, and a frame that depended on the machine would not be a
        // reference.
        let path = std::path::PathBuf::from("/tmp/skerry-golden");
        let stated = |id: &str, name: &str| sk_desktop::DesktopEntry {
            id: id.to_owned(),
            path: std::path::PathBuf::from(format!("/usr/share/applications/{id}")),
            name: name.to_owned(),
            exec: "true %f".to_owned(),
            terminal: false,
            mime_types: vec!["inode/directory".to_owned()],
            no_display: false,
            try_exec: None,
        };
        menu.asks_about(path.clone());
        menu.answer(crate::openers::Handlers {
            path,
            mime: "inode/directory".to_owned(),
            rows: vec![
                stated("org.gnome.Nautilus.desktop", "Files"),
                stated("org.gnome.Console.desktop", "Terminal"),
            ],
        });
        expand_sample(&mut menu, crate::menu::OPEN_WITH, |_| true);
    }
    Some(menu)
}

/// Opens the submenu under the row called `owner` and lights its first row,
/// which is the state a key leaves it in.
fn expand_sample(menu: &mut crate::menu::Menu, owner: &str, enabled: impl Fn(&str) -> bool) {
    let Some(row) = menu.items.iter().position(|item| item.id == owner) else { return };
    if menu.expand(row, &crate::commands::registry(), enabled, None)
        && let Some(sub) = menu.sub.as_mut()
    {
        sub.menu.step(true);
    }
}

/// The question a paste onto a taken name asks.
///
/// Stated rather than provoked: a capture has no queue and no pool, and the
/// panel's whole content is two files' facts and four answers. The two files are
/// the fixture's own, so the sizes and the dates in the frame are the ones every
/// other frame already shows.
fn sample_clash(path: &std::path::Path) -> crate::edit::Clash {
    let existing = path.join("report.pdf");
    let source = path.join("build").join("report.pdf");
    crate::edit::Clash {
        theirs: sk_fs::facts::describe(&existing),
        // Not on disk — the fixture holds one `report.pdf`, not two — so this
        // states what the other one would be. A frame of "Cannot be read" in
        // the arriving column would be a frame of the empty case.
        ours: sk_fs::facts::describe(&existing).map(|mut facts| {
            facts.path.clone_from(&source);
            facts.size = 4_512_000;
            facts.modified = FIXED_MTIME - 60 * 60 * 24 * 400;
            facts
        }),
        source,
        into: path.to_path_buf(),
        existing,
        moving: false,
        rest: 3,
    }
}

/// The subtree search, with a query and results from more than one folder.
///
/// Stated rather than walked: a capture has no pool to run a walk on, and what
/// the frame is of is a *list* — a name, the folder it is in, and a summary that
/// says the walk is still going. The paths are the fixture's own, so the frame
/// is the same on every machine.
fn sample_search(path: &std::path::Path) -> crate::search::Search {
    let mut search = crate::search::Search::new(crate::search::Scope::Here(path.to_path_buf()));
    search.query = crate::field::Field::new("re".to_owned());
    "re".clone_into(&mut search.asked);
    let hit = |where_: &str, name: &str, size: u64, directory: bool| sk_index::Hit {
        path: path.join(where_).join(name),
        size,
        modified: FIXED_MTIME,
        directory,
    };
    search.found = sk_index::Found {
        hits: vec![
            hit(".", "report.pdf", 1_048_576, false),
            hit("build", "release", 4096, true),
            hit("build/release", "core.rs", 8_192, false),
            hit("documents", "read-me.md", 640, false),
        ],
        scanned: 18_402,
        complete: false,
        partial: false,
        capped: false,
    };
    search.active = 2;
    search
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
    settings: Option<(crate::settings::Panel, crate::settings::Values, crate::settings::Marks)>,
    /// What the frame's `icons` value resolved to — ADR 0042's three sources.
    icons: crate::icons::Source,
    clash: Option<crate::edit::Clash>,
    search: Option<crate::search::Search>,
    carrying: Option<crate::view::Carrying>,
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
fn sample_crumb_menu(here: &crate::panes::PaneLayout, empty: bool) -> crate::crumbs::CrumbMenu {
    let rows = if empty {
        Vec::new()
    } else {
        ["fixture", "rendered", "مستندات"].map(str::to_owned).to_vec()
    };
    crate::crumbs::CrumbMenu::new(
        2,
        std::path::PathBuf::from("/tmp/skerry-golden"),
        here.path.x + 180.0,
        rows,
        (!empty).then_some("fixture"),
    )
}

/// Splits the sample tree, and puts a second tab in the new half.
///
/// **The lower half shows a different directory**, and it is the tab beside the
/// one the leaf opened with. Every pane draws its own path bar now, so a frame
/// where both halves held the same folder would look identical whether each bar
/// read its own pane or both read the focused one — which is the mistake this
/// frame exists to catch. `fixture/documents` is part of what the fixture
/// guarantees on every machine, so it costs the capture nothing.
fn split_the_sample(panes: &mut crate::panes::Panes, session: &Session, request: &Capture) {
    let another = |path: &std::path::Path| {
        let (mut pane, _) = session.read_fully(path);
        pane.selection.set_cursor(pane.listing.at(0));
        settle_folder_totals(&mut pane, session);
        pane
    };
    let id = panes.focus();
    panes.split_with(id, crate::panes::Axis::Below, false, another(&request.path));
    let into = panes.focus();
    panes.put_tab(into, 1, another(&request.path.join("documents")));
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
        original: None,
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
    crate::openers::Openers::new(crate::openers::Handlers {
        path: std::path::PathBuf::from("/tmp/skerry-golden/fixture/archive.tar.gz"),
        mime: "application/gzip".to_owned(),
        rows: vec![
            entry("Archive Manager", "org.gnome.FileRoller"),
            entry("Ark", "org.kde.ark"),
            entry("Text Editor", "org.gnome.TextEditor"),
        ],
    })
}

/// The settings panel, with one value from each of the three sources.
///
/// **Not all six at their defaults.** The third column is what the frame exists
/// to check, and a panel where every row says *default* would look identical to
/// one where the column had been dropped.
fn sample_settings() -> (crate::settings::Panel, crate::settings::Values, crate::settings::Marks) {
    use crate::settings::Setting;
    let mut panel = crate::settings::Panel::new();
    panel.active = 1;
    panel.handler = Some(false);
    let values = crate::settings::Values {
        dark: true,
        hidden: true,
        sidebar: true,
        aside: 240.0,
        layout: crate::views::Layout::Grid,
        confirm_trash: false,
        icons: crate::settings::Icons::Marks,
    };
    let mut marks = crate::settings::Marks::default();
    marks.name(Setting::Hidden);
    marks.name(Setting::View);
    marks.touch(Setting::View);
    marks.name(Setting::SidebarWidth);
    marks.name(Setting::Icons);
    (panel, values, marks)
}

/// The instant the fixture's files are stamped with, which `tools/golden` sets
/// and this states so the panel's three dates are the same in every run.
const FIXED_MTIME: i64 = 981_158_400;

fn sample_overlays(
    request: &Capture,
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
        menu: sample_context_menu(request, here),
        crumbs: (request.crumbs || request.crumbs_empty)
            .then(|| sample_crumb_menu(here, request.crumbs_empty)),
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
        settings: request.settings.then(sample_settings),
        // **Our own marks, on every machine.** A frame that showed a loaded theme
        // would be a frame of whatever the build box happened to have installed,
        // and ADR 0042's point is that this state is a choice rather than a
        // fallback — so it is the one the golden frame keeps honest.
        icons: crate::icons::Source::Ours,
        clash: request.clash.then(|| sample_clash(&request.path)),
        search: request.search.then(|| sample_search(&request.path)),
        // Stated, because a capture has no pointer: the card follows one, and
        // this puts it where a pointer resting over the list would be.
        carrying: request.carrying.then(|| crate::view::Carrying {
            at: sk_plat::Point::new(here.name.x + 90.0, here.body.y + 96.0),
            name: "quarterly-report.pdf".to_owned(),
            count: 3,
            directory: false,
            copying: false,
        }),
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

/// Loads the named theme and rasterises the marks it resolves, synchronously.
///
/// **What `App::settle_icons` does over frames, in one pass** — the same shape
/// [`settle_thumbnails`] below has and for the same reason: the answer has to be
/// on the GPU before the one frame that gets taken.
///
/// Returns what the theme resolved to, so [`report`] can print it — a frame that
/// silently drew no icon is a frame of the geometric marks, and blessing one of
/// those as if it showed the feature is the mistake
/// [12-measurement.md](../../../docs/12-measurement.md) keeps recording.
fn settle_icons(
    request: &Capture,
    pane: &mut Pane,
    session: &mut Session,
    types: &sk_desktop::MimeDatabase,
    at: (&crate::panes::PaneLayout, &sk_ui::Theme),
) -> String {
    let (here, theme) = at;
    let Some(name) = request.icons.as_deref() else { return "ours".to_owned() };
    let source = match sk_desktop::icons::Theme::load(name) {
        Ok(theme) => crate::icons::Source::Ready(theme),
        Err(why) => crate::icons::Source::Failed { theme: name.to_owned(), why },
    };
    let Some(found) = source.theme() else { return source.token() };

    // The mark's own box, which is what decides the band — through the same
    // `mark_of` the drawing calls, so a capture cannot disagree with the frame it
    // is a capture of about where a mark is.
    let cells = crate::groups::Bands::new(
        pane.mode,
        crate::view::table_of(here, pane),
        theme.metrics.row_height,
        &pane.bands,
        pane.listing.rows(),
        pane.closed,
    );
    let scale = f32::from(u16::try_from(request.scale_120).unwrap_or(120)) / 120.0;

    let mut previews = sk_preview::Previews::new();
    let mut names = crate::icons::Names::default();
    let mut asked: Vec<(sk_model::EntryId, sk_gpu::ImageKey)> = Vec::new();
    let mut wanted = 0usize;
    for row in 0..pane.listing.rows().min(MEASURED) {
        let Some(id) = pane.listing.at(row) else { continue };
        if pane.thumbs.contains_key(&id) {
            continue;
        }
        let side = crate::view::mark_of(pane.mode, here, cells.at(row, 0.0), &theme.metrics).width;
        let directory = pane.listing.flags(id).has(sk_model::Flags::DIR);
        let wants = crate::icons::names_for(directory, types.by_name(pane.listing.name(id)));
        let nominal = side.round().max(1.0) as u32;
        // Rounded up, for the reason `App::settle_icons` gives.
        let step = scale.ceil().max(1.0) as u32;
        let Some(file) = wants.iter().find_map(|it| found.find(it, nominal, step)) else {
            continue;
        };
        let band = sk_gpu::icon_band((side * scale).round().max(1.0) as u32);
        let (_, image) = names.name(&file, band);
        asked.push((id, image));
        previews.want(crate::icons::wanted(file, band));
        wanted += 1;
    }

    let deadline = std::time::Instant::now() + READ_DEADLINE;
    let mut landed = 0usize;
    while landed < wanted && std::time::Instant::now() < deadline {
        previews.pump(&session.jobs.pool, &session.waker.handle());
        for answer in previews.drain() {
            landed += 1;
            if answer.refusal != sk_preview::Refusal::None {
                continue;
            }
            if let Some(image) = names.look_up(&answer.key) {
                session.renderer.hold_icon(image, answer.size.0, answer.size.1, &answer.pixels);
            }
        }
        std::thread::yield_now();
    }
    for (id, image) in asked {
        if session.renderer.holds_icon(image) {
            pane.icons.insert(id, image);
        }
    }
    source.token()
}

/// Fetches every visible thumbnail and waits for it, so the frame is reproducible.
///
/// **A capture does synchronously what the window does over frames.** In the
/// window, `App::settle_thumbnails` asks and a later frame draws whatever has
/// arrived; here the answer has to be in the atlas before the one frame that gets
/// taken. Bounded by the same deadline as the read, for the same reason — a
/// capture of a directory of ten thousand photographs is not a reason to decode
/// them all.
///
/// What landed is counted by the atlas, and `report` prints that count: a golden
/// frame that silently drew no pictures is a golden frame of the fallback marks,
/// and blessing one of those as if it showed the feature is exactly the mistake
/// [12-measurement.md](../../../docs/12-measurement.md) keeps recording.
fn settle_thumbnails(
    pane: &mut Pane,
    session: &mut Session,
    types: Option<&sk_desktop::MimeDatabase>,
    thumbs: &mut crate::previews::Thumbs,
) {
    let Some(types) = types else { return };
    let mut previews = sk_preview::Previews::new();
    let mut asked: Vec<(sk_model::EntryId, sk_gpu::ImageKey)> = Vec::new();
    let mut wanted = 0usize;
    for row in 0..pane.listing.rows().min(MEASURED) {
        let Some(id) = pane.listing.at(row) else { continue };
        let kind = sk_desktop::kind_of(&pane.listing, id, Some(types));
        if crate::previews::shows(pane.listing.name(id), kind, Some(types))
            != Some(crate::previews::Shows::Picture)
        {
            continue;
        }
        let path = pane.path_of(id);
        let mtime = pane.listing.mtime(id);
        let (_, image) = thumbs.name(&path, mtime);
        asked.push((id, image));
        previews.want(crate::previews::thumbnail(path, mtime));
        wanted += 1;
    }
    if wanted == 0 {
        return;
    }

    let deadline = std::time::Instant::now() + READ_DEADLINE;
    let mut landed = 0usize;
    while landed < wanted && std::time::Instant::now() < deadline {
        previews.pump(&session.jobs.pool, &session.waker.handle());
        for answer in previews.drain() {
            landed += 1;
            if answer.refusal != sk_preview::Refusal::None {
                continue;
            }
            if let Some(image) = thumbs.look_up(&answer.key) {
                session.renderer.hold_image(image, answer.size.0, answer.size.1, &answer.pixels);
            }
        }
        std::thread::yield_now();
    }
    // **Only what the atlas actually holds** — the same rule
    // `App::settle_thumbnails` follows, and the reason it is a rule: the first
    // version of this recorded every key it asked about, so a file that came back
    // `Undecodable` drew `painter.image` for a picture that was never there and
    // its kind mark went with it. Three golden frames caught it as two rows whose
    // marks had vanished; `holiday 🏖️.jpeg` and `img10.png` in the shared fixture
    // are named like pictures and filled with junk, which is exactly that case.
    for (id, image) in asked {
        if session.renderer.holds_image(image) {
            pane.thumbs.insert(id, image);
        }
    }
}

/// Puts the list into the state a golden frame is worth taking of.
///
/// A selection, a cursor and a hover, at fixed rows: a frame with none of them
/// covers none of the three shapes a row can have.
fn pose(pane: &mut Pane, checked: bool, coloured: bool) {
    // **Two folders with a colour somebody chose**, which is the only way a frame
    // shows what `crate::colours` does: the records live in a file this capture
    // does not read, and the drawing takes them from the pane.
    if coloured {
        for (row, colour) in [(0, crate::colours::Colour::Red), (1, crate::colours::Colour::Green)]
        {
            if let Some(id) = pane.listing.at(row) {
                pane.colours.push((id, colour));
            }
        }
    }
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
    // Row zero instead, selected: the one row every fixture has, including a
    // trail's own column, which holds three.
    if let Some(id) = pane.listing.at(0).filter(|_| checked) {
        pane.selection.select(id);
        pane.selection.set_cursor(Some(id));
        pane.hover = Some(0);
    }
}

/// Everything a captured frame's chrome borrows.
///
/// One value rather than seven arguments, because they are gathered together and
/// used together, and because the alternative is a function signature nobody can
/// read.
struct Sample<'a> {
    overlays: &'a Overlays,
    places: &'a [crate::sidebar::Row],
    types: &'a sk_desktop::MimeDatabase,
    keys: Option<&'a [(String, String)]>,
    hint: &'a str,
    leaves: &'a [crate::panes::PaneLayout],
    dividers: &'a [crate::panes::Divider],
    drop_tab: Option<(crate::panes::PaneId, usize)>,
    erasing: Option<(&'a [String], usize, bool, bool)>,
    /// The undo stack the history strip lists, with a sample on it.
    edits: &'a crate::edit::Edits,
    /// The quick look, if this frame is of one.
    look: Option<&'a crate::quicklook::QuickLook>,
}

/// The chrome of the frame this capture was asked for.
fn sample_chrome<'a>(request: &Capture, sample: &Sample<'a>) -> Chrome<'a> {
    Chrome {
        keys: sample.keys,
        stats: request.stats,
        quick_look: sample.look,
        hovered_look: None,
        edits: Some(sample.edits),
        history: (request.history, 0.0),
        // The second row, so the frame shows the hover as well as the present.
        hovered_edit: request.history.then_some(1),
        typed: None,
        band: None,
        watching: true,
        preview_missing: false,
        watch_failure: None,
        // A capture has no queue, so the sentence a copy would be writing is
        // stated — the same trade `sample_properties` makes, and for the same
        // reason: a frame has to be the same on every machine.
        notice: request
            .progress
            .then_some("Copying 1,204 of 1,776 · 42 MB/s · about 14 s left")
            // **With the queue's own clause**, because "index every filesystem"
            // is three walks and the line that says which one is the difference
            // between a build in progress and a build that looks stuck.
            .or(request
                .indexing
                .then_some("Indexing… 214,880 of 563,605 entries · filesystem 1 of 3"))
            .or(request.stopped.then_some("Copying stopped — 572 of 1,776 left")),
        // The pointer is on a row, so nothing in the frame is hovered
        // — the hover shapes are covered by the GUI scenarios, which can
        // actually move a pointer.
        hovered_button: None,
        hovered_crumb: None,
        hovered_separator: None,
        crumb_menu: sample.overlays.crumbs.as_ref(),
        hovered_tool: None,
        hovered_mode: None,
        places: sample.places,
        hovered_place: None,
        aside_grabbed: false,
        place_drop: request.pinned.then_some(1),
        drop_row: None,
        drop_tab: sample.drop_tab,
        drop_pane: None,
        answering: 1,
        for_the_rest: true,
        // Two thirds through and not paused, which is the state that draws every
        // part of this: a filled bar, its head, the sentence at full strength and
        // both controls.
        progress: request
            .progress
            .then_some(0.66)
            .or(request.indexing.then_some(214_880.0 / 563_605.0)),
        paused: false,
        pausable: request.progress,
        resumable: request.stopped,
        naming: sample.overlays.naming.as_ref(),
        // What a freshly opened directory looks like: nowhere to go
        // back to, nowhere forward, a parent and a reload. Hardcoded
        // because a capture has no history — and it is the state worth
        // having a golden frame of, since it is the one with two of the
        // four dimmed.
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
        settings: sample.overlays.settings.as_ref().map(|(panel, values, marks)| {
            crate::view::Settings { panel, values, marks: *marks, icons: &sample.overlays.icons }
        }),
        carrying: sample.overlays.carrying.as_ref(),
        search: sample.overlays.search.as_ref(),
        clash: sample.overlays.clash.as_ref(),
        erasing: sample.erasing,
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
    // `--at` moves the frame into a subdirectory before anything is read, so the
    // capture is *of* that directory rather than of one the pane navigated to
    // afterwards — a navigation would leave `view_owed` set and a `Back` in the
    // history, neither of which belongs in a still frame.
    let where_ = match request.at.as_deref() {
        Some(at) => request.path.join(at),
        None => request.path.clone(),
    };
    let (mut pane, _) = session.read_fully(&where_);

    settle_folder_totals(&mut pane, &session);
    pose(&mut pane, request.checked, request.coloured);

    let types = sk_desktop::MimeDatabase::load();
    let mut thumbs = crate::previews::Thumbs::default();
    settle_thumbnails(&mut pane, &mut session, Some(&types), &mut thumbs);

    // The overlay reads the command registry, so a capture of it builds one.
    let keys_rows = request.keys.then(|| {
        let registry = crate::commands::registry();
        crate::commands::overlay_rows(&registry)
    });

    // Read here rather than left as `None`: the mark at the head of a row is
    // coloured from it, so a golden frame without it is a golden frame of grey.
    // A fixed set rather than this machine's: the sidebar is part of the frame,
    // and a reference image that embeds somebody's `user-dirs.dirs` and mount
    // table is one nobody else can reproduce.
    let places = if request.sidebar {
        crate::sidebar::sample(&request.path, request.pinned, request.tree)
    } else {
        Vec::new()
    };
    let theme = if request.dark { Theme::dark() } else { Theme::light() };
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let frame = if request.own_frame {
        Frame::Own { edges: true, minimize: true, maximize: true }
    } else {
        Frame::None
    };
    let layout = Layout::compute_with(
        logical,
        &theme.metrics,
        frame,
        match (request.sidebar, request.aside) {
            (false, _) => 0.0,
            (true, Some(width)) => width,
            (true, None) => crate::sidebar::DEFAULT_WIDTH,
        },
        history_height(request, &theme.metrics),
    );
    // One leaf: a capture is a frame of one directory, and a split is its own
    // frame rather than a variant of every other.
    let crate::panes::Single { mut panes, mut leaves, mut dividers, mut here } =
        crate::panes::single(pane, layout.content, &theme.metrics);
    mode_the_sample(&mut panes, request, &session, here.body.width);
    if let Some(grouping) = requested_grouping(request) {
        group_the_sample(panes.focused_mut(), grouping, &types, request.collapsed);
    }
    if request.split {
        // **Stacked, and two tabs in the lower half.** A `Below` split is the
        // arrangement that puts a second column header halfway down the window,
        // which is where a rule drawn from the window's left edge instead of the
        // header's own crosses the sidebar — a line nothing else would catch.
        split_the_sample(&mut panes, &session, request);
        let widths = crate::layout::Columns::default();
        panes.layout(layout.content, &theme.metrics, widths, &mut leaves, &mut dividers);
        here = leaves.first().copied().unwrap_or_default();
    }
    // The tab `split_the_sample` left *behind* in the lower half, which is the
    // only tab in this frame that is not the one its leaf is showing — and being
    // the one that is not showing is the whole of what this mark means.
    // The lower leaf's *first* tab, which is not the one it is showing: a drop on
    // a tab lands in the directory that tab holds, and a mark on the tab already
    // on screen could not tell the two apart.
    let drop_tab = (request.split && request.tab_drop).then(|| (panes.focus(), 0));
    // **After the panes are laid out**, because the band an icon comes from is
    // decided by the mark's own box and that box is a function of the layout.
    let icons = settle_icons(request, panes.focused_mut(), &mut session, &types, (&here, &theme));
    // Three names and a total of nine, so the frame holds both the list and the
    // "and 6 more" line the list stops at.
    let doomed: Vec<String> = ["quarterly-report.pdf", "archive.tar.gz", "notes"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    let overlays = sample_overlays(request, &here, panes.focused_mut(), &session.jobs);

    // Frames until the atlas owes nothing. The first rasterizes; the second draws
    // from a warm atlas. A frame is repeated only while texels are still queued,
    // so a capture is never an image with holes in the text.
    let keyboard_hint = crate::commands::hint(&crate::commands::registry());
    let edits = sample_edits(request, &session);
    let look =
        sample_quick_look(request, &mut session, panes.focused(), (&layout, &theme, &mut cache));
    let sample = Sample {
        look: look.as_ref(),
        edits: &edits,
        overlays: &overlays,
        places: &places,
        types: &types,
        keys: keys_rows.as_deref(),
        hint: &keyboard_hint,
        leaves: &leaves,
        dividers: &dividers,
        drop_tab,
        erasing: (request.erase || request.trash_question).then_some((
            doomed.as_slice(),
            9,
            false,
            request.erase,
        )),
    };
    let stats = warm_and_draw(
        &mut session,
        &Drawing { request, sample: &sample, panes: &panes, layout: &layout, now },
        &mut list,
        &mut cache,
        &mut frames,
        &theme,
    );

    let pixels = session.renderer.capture().ok_or_else(|| {
        Problem::plain(ProblemKind::NoGpu, "the offscreen target could not be read back")
    })?;
    write(&request.file, session.physical, &pixels)?;
    report(request, &session, panes.focused(), (stats, &icons));
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
fn report(request: &Capture, session: &Session, pane: &Pane, what: (FrameStats, &str)) {
    let (stats, icons) = what;
    eprintln!(
        "skerry: wrote {} — {}×{} at {}%, {} entries, {} quads, {} draws, {} glyphs, \
{} thumbnails, {} pages holding {} KiB, icons {} in {} slots",
        request.file.display(),
        session.physical.0,
        session.physical.1,
        request.scale_120 * 100 / 120,
        pane.listing.rows(),
        stats.quads,
        stats.draws,
        session.renderer.atlas_stats().0,
        // **Said out loud, because a frame that drew none is a frame of the
        // fallback marks** — and blessing one of those as if it showed the feature
        // is exactly the mistake docs/12 keeps recording.
        session.renderer.image_stats().0,
        session.renderer.preview_stats().0,
        session.renderer.preview_stats().1 / 1024,
        // **Where the marks came from and how many arrived.** A frame under a
        // theme that drew no icon is a frame of the geometric marks wearing the
        // theme's name, which is the one way this gate could pass by accident.
        icons,
        session.renderer.icon_stats().0,
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
