// SPDX-License-Identifier: Apache-2.0
//! The Details view and the window chrome.
//!
//! One immediate-mode pass over the **visible rows only**. At a million entries
//! the row loop below runs about forty times, which is the whole reason a
//! million entries costs the same as a hundred.

use sk_desktop::{Category, MimeDatabase};
use sk_gpu::{Color, FrameStats};
use sk_model::{Flags, SortColumn, i18n};
use sk_plat::{Point, WindowState};
use sk_text::Weight;
use sk_ui::{Align, Direction, Painter, RunKey, TextStyle, format};

use crate::cells::Mode;

use crate::edit::Naming;
use crate::field::Field;
use crate::frames::{BUDGET_US, Frames};
use crate::layout::{Area, Button, Layout, Tool};
use crate::menu::Menu;
use crate::palette::Palette;
use crate::pane::Pane;
use crate::panes::{Divider, PaneId, PaneLayout, Panes, SplitId};
use crate::sidebar::Mark;

/// Which column a row cache key is for.
const COLUMN_NAME: u8 = 0;
const COLUMN_SIZE: u8 = 1;
const COLUMN_MODIFIED: u8 = 2;

/// What the overlays are showing.
pub struct Chrome<'a> {
    /// The keyboard list: what to press, and what it does. `None` hides it.
    ///
    /// The rows come from the command registry rather than from a table beside
    /// it, so the string on screen cannot describe a key that is not bound.
    pub keys: Option<&'a [(String, String)]>,
    /// The frame statistics.
    pub stats: bool,
    /// The file being looked at, if `Space` has been pressed on one.
    pub quick_look: Option<&'a crate::quicklook::QuickLook>,
    /// The undo stack, whose entries the history strip lists.
    ///
    /// `None` where there is no stack to draw — a bench, whose frame is measured
    /// with no panel open because a panel nobody opened is the wrong frame to
    /// measure.
    pub edits: Option<&'a crate::edit::Edits>,
    /// Whether the history strip is showing its list, and how far it is scrolled.
    ///
    /// The offset rather than the [`sk_ui::Scroll`], so a caller with no scroll of
    /// its own needs no borrow it would have to keep alive.
    pub history: (bool, f32),
    /// The history row under the pointer.
    pub hovered_edit: Option<usize>,
    /// The type-ahead buffer, while it is live.
    pub typed: Option<&'a str>,
    /// The rubber-band rectangle, while one is being dragged.
    pub band: Option<(Point, Point)>,
    /// Something to tell the person, which replaces the counts while it is up.
    pub notice: Option<&'a str>,
    /// Whether changes to this directory are being watched.
    ///
    /// `false` on a filesystem inotify does not cover — every NFS mount — and the
    /// status line says so, because a listing that is a snapshot should not look
    /// like one that is live.
    pub watching: bool,
    /// The window button under the pointer, if the window draws its own frame.
    pub hovered_button: Option<Button>,
    /// The path component under the pointer: whose bar, and how deep.
    pub hovered_crumb: Option<(PaneId, usize)>,
    /// The navigation button under the pointer, and which pane's bar it is in.
    pub hovered_tool: Option<(PaneId, Tool)>,
    /// The switcher button under the pointer, and the pane whose bar it is in.
    pub hovered_mode: Option<(PaneId, Mode)>,
    /// The quick look's control under the pointer.
    pub hovered_look: Option<LookControl>,
    /// The sidebar's rows as they are drawn, once the job that reads them has
    /// finished.
    ///
    /// A slice rather than the model, because the view needs the arrangement and
    /// not the channel the arrangement arrived on — which is what lets a capture
    /// build one synchronously and have a golden frame of it. Flattened, with the
    /// heading carried by the row that starts a section: a heading and a row are
    /// the same kind of thing to a hit test.
    pub places: &'a [crate::sidebar::Row],
    /// The sidebar row under the pointer.
    pub hovered_place: Option<usize>,
    /// Whether the sidebar's divider is under the pointer or being dragged.
    ///
    /// **Drawn, because eight logical pixels of invisible grab band is a feature
    /// only somebody who was told about it can use** — which is [rule
    /// 4](../../../docs/03-principles.md), and the same argument the column
    /// boundaries already won.
    pub aside_grabbed: bool,
    /// Where a pinned place being dragged would land, as a gap between rows.
    pub place_drop: Option<usize>,
    /// The row a drag of files would drop into, while one is in flight.
    pub drop_row: Option<(PaneId, usize)>,
    /// The tab it would drop into instead, marked on the tab itself: a drop on
    /// a tab lands in the directory that tab is holding, not the one its leaf
    /// happens to be showing.
    pub drop_tab: Option<(PaneId, usize)>,
    /// The pane whose whole list would take a drop, ringed rather than a row
    /// highlighted, because the target is the directory it is showing.
    pub drop_pane: Option<PaneId>,
    /// What a drag of files is carrying, while one is over this window.
    pub carrying: Option<&'a Carrying>,
    /// The subtree search, while it is open.
    pub search: Option<&'a crate::search::Search>,
    /// The name-taken question, while one is open.
    pub clash: Option<&'a crate::edit::Clash>,
    /// What a permanent deletion is asking about: the names it would erase, how
    /// many there are in total, and whether the keyboard is on Delete.
    pub erasing: Option<(&'a [String], usize, bool)>,
    /// Which answer it is on.
    pub answering: usize,
    /// Whether the answer would cover the rest of the queue.
    pub for_the_rest: bool,
    /// How far along a running operation is, once that is knowable.
    pub progress: Option<f32>,
    /// Whether that operation is on the pause latch, so the button can say
    /// *Resume* rather than offering to pause something already paused.
    pub paused: bool,
    /// Whether a stopped operation is waiting to be picked up.
    ///
    /// Its own field rather than a third state of `progress`: a stopped queue has
    /// no fraction — nothing is running — and the button that picks it up has to
    /// be there anyway, which is [rule
    /// 7](../../../docs/03-principles.md#7-honest-progress)'s "resumable" made
    /// pressable.
    pub resumable: bool,
    /// Whether the operation on the strip is one a latch can hold.
    ///
    /// An index build is cancellable and not pausable — it is seconds long — and
    /// a Pause drawn on the strip that did nothing would be worse than none.
    pub pausable: bool,
    /// The breadcrumb separator under the pointer: whose bar, and how deep.
    pub hovered_separator: Option<(PaneId, usize)>,
    /// The sibling menu a separator has opened, while one is open.
    pub crumb_menu: Option<&'a crate::crumbs::CrumbMenu>,
    /// The name being typed, if one is.
    pub naming: Option<&'a Naming>,
    /// Whether the filter field has the keyboard.
    pub filtering: bool,
    /// The filename half of shared-mime-info, once the job that reads it is
    /// done. `None` for the first few milliseconds, and every row is grey.
    pub types: Option<&'a MimeDatabase>,
    /// What the compositor says the window is, for the frame the window draws.
    pub window: WindowState,
    /// The context menu, while one is open.
    pub menu: Option<&'a Menu>,
    /// The path being typed, which replaces the breadcrumbs while it is up.
    pub path_entry: Option<&'a Field>,
    /// The command palette, while it is open.
    pub palette: Option<&'a Palette>,
    /// The Properties panel, while it is open.
    pub properties: Option<&'a crate::properties::Properties>,
    /// The Open With list, while it is open.
    pub openers: Option<&'a crate::openers::Openers>,
    /// The keyboard hint in the corner of the path bar, from the registry.
    pub hint: &'a str,
    /// Where each leaf of the pane tree is, in layout order.
    pub leaves: &'a [PaneLayout],
    /// The dividers between them.
    pub dividers: &'a [Divider],
    /// The divider under the pointer, or being dragged.
    pub hovered_divider: Option<SplitId>,
    /// The tab under the pointer: which leaf, and which of its tabs.
    pub hovered_tab: Option<(PaneId, usize)>,
    /// Whether the pointer is on the `+` of the leaf it is in.
    pub hovered_plus: bool,
    /// Where a tab being dragged would land, while one is.
    pub tab_drop: Option<crate::panes::Drop>,
}

/// One of a quick look's four controls.
///
/// **Four buttons rather than none**, because [rule
/// 5](../../../docs/03-principles.md) is that every command is reachable by
/// pointer alone and the panel covers the list a pointer would otherwise use. Two
/// pairs rather than one, because they step different things: the file being
/// looked at, and the page inside it — and the page numbers between the second
/// pair are what stop the two pairs reading as the same control twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookControl {
    /// The previous file with a preview.
    PreviousFile,
    /// The next one.
    NextFile,
    /// The page before this one.
    PreviousPage,
    /// The page after it.
    NextPage,
    /// Turns editing on, or off and saves.
    Edit,
    /// Puts the file back the way it was, throwing the edit away.
    Discard,
    /// Plays the animation, or stops it where it is.
    Play,
}

/// Hit regions this frame produced, for the next event to test against.
///
/// A breadcrumb's rectangle is a function of the shaped width of a directory
/// name, so it is known where the text is drawn and nowhere else. Recording it
/// as the frame is built is the immediate-mode answer; the alternative is
/// measuring every component twice, once to draw and once to hit-test, and the
/// two measurements drifting apart at 125%.
#[derive(Default)]
pub struct Hits {
    /// One region per path component of every pane's bar, root first within each
    /// pane. `depth` is the number of components to keep when it is clicked.
    ///
    /// A component that did not fit is still recorded, with a zero-width
    /// rectangle: the depth of a crumb has to be its index in the path whether
    /// the bar had room to draw it or not.
    pub crumbs: Vec<BarHit>,
    /// One rectangle per *drawn* row of a separator's menu, in draw order — so
    /// the index a click reports is an offset from the menu's scroll.
    pub crumb_menu: Vec<Area>,
    /// One region per **separator**, `depth` naming the component on its left.
    ///
    /// A separator is a target of its own — clicking one lists the directories
    /// inside the component to its left, which are the siblings of the component
    /// to its right. There is one after the last component too: its list is the
    /// children of where you already are.
    pub separators: Vec<BarHit>,
    /// One rectangle per sidebar row, in the flattened order the model uses, so
    /// the index a click reports is an index into it.
    pub places: Vec<Area>,
    /// One per sidebar row, in the same order: the box that opens and closes it.
    pub twisties: Vec<Area>,
    /// The quick look's controls, while one is open and while it has them.
    ///
    /// **A list rather than four fields**, and empty rather than absent where the
    /// thing on screen has no pages: a control that is not drawn must not be
    /// clickable, and the one place that knows whether it was drawn is the code
    /// that drew it.
    pub look: Vec<(LookControl, Area)>,
    /// The history strip's header, which collapses the list when pressed.
    ///
    /// `None` where there is no history and therefore no strip at all.
    pub history_header: Option<Area>,
    /// One rectangle per *drawn* history row, with its index in the timeline.
    ///
    /// The index travels with the rectangle rather than being the position in this
    /// vector, because the list is scrolled and clipped: only the rows on screen
    /// are recorded, and the first of them is not row zero.
    pub history_rows: Vec<(usize, Area)>,
    /// One rectangle per open context-menu row, in the menu's own order.
    ///
    /// The menu's width is the width of its widest shaped row, so this is the
    /// same story as the breadcrumbs': the rectangle is known where the text is
    /// measured, and the next event tests against it.
    pub menu: Vec<Area>,
    /// One rectangle per *drawn* palette result, in draw order — so the index a
    /// click reports is an offset from the palette's scroll rather than an index
    /// into the whole result list.
    pub palette: Vec<Area>,
    /// The same, for the Open With list's drawn rows.
    pub openers: Vec<Area>,
    /// One rectangle per *drawn* search result, in draw order — so the index a
    /// click reports is an offset from the list's scroll.
    pub search: Vec<Area>,
    /// The search's query field, so a click into it can place the caret.
    ///
    /// Narrower than the field's row: the scope switcher sits at the right end of
    /// it, and a press on a switcher that also placed a caret would be two
    /// things at once.
    pub search_field: Option<Area>,
    /// The scope switcher's two buttons: everywhere, and whether each is the
    /// scope in force.
    pub search_scope: Vec<(bool, Area)>,
    /// The label that puts every result into the pane, while there are results.
    pub search_all: Option<Area>,
    /// One rectangle per answer to an open name-taken question, in panel order.
    pub answers: Vec<Area>,
    /// Its "for the rest of them" toggle, when there is a rest.
    pub for_the_rest: Option<Area>,
    /// The strip's Pick up button, while a stopped operation is owed.
    pub pick_up: Option<Area>,
    /// The palette's query field, so a click into it can place the caret.
    pub palette_field: Option<Area>,
    /// The name field's box, while a rename or a new folder is being typed.
    ///
    /// **Recorded by the frame that drew it**, because only the frame knows where
    /// the row ended up: a heading above it, a grid cell, a scroll. A press used
    /// to be tested against `name_field(layout, metrics, row, offset)` instead —
    /// `body.y + row * row_height`, which agrees with the drawn box in an
    /// ungrouped details list and nowhere else, so a double click on a grouped
    /// list's name committed the rename and opened the file behind it.
    pub name_field: Option<Area>,
    /// The two buttons of the delete-for-good question, while it is up.
    pub erase_cancel: Option<Area>,
    pub erase_delete: Option<Area>,
    /// Pause and Cancel in the status strip, while an operation is running.
    pub pause: Option<Area>,
    pub cancel: Option<Area>,
    /// Every panel drawn this frame, in draw order.
    ///
    /// **What the pointer cannot see through.** A panel over the list used to
    /// leave the row beneath it hovering — the highlight tracked the pointer
    /// across a question it was supposed to be reading. Recorded by
    /// [`panel_shape`] rather than by each panel, so a panel added later cannot
    /// forget; a card that *follows* the pointer uses [`floating_shape`] and is
    /// deliberately not here, because a drag has to see the rows under it.
    pub panels: Vec<Area>,
}

/// One target in one pane's own path bar.
///
/// **Which pane, not just where.** Every leaf draws its own breadcrumbs, so a
/// rectangle alone cannot say whose path a click is asking about — and a click
/// in the other half of a split has to move the keyboard there before it
/// navigates, which needs the leaf's name.
#[derive(Clone, Copy, Debug, Default)]
pub struct BarHit {
    /// The leaf whose bar it is in.
    pub leaf: PaneId,
    /// How many path components the click keeps.
    pub depth: usize,
    /// Where it was drawn, or empty for a component that did not fit.
    pub area: Area,
}

/// Numbers for the statistics overlay and the status line.
pub struct Diagnostics<'a> {
    /// Frame history.
    pub frames: &'a mut Frames,
    /// What the last frame cost.
    pub last: FrameStats,
    /// Glyphs cached, mask utilization, colour utilization.
    pub atlas: (usize, f32, f32),
    /// Shaped runs held, hits, misses.
    pub shaping: (usize, u64, u64),
    /// Pool width, jobs finished, jobs that panicked.
    pub pool: (usize, u64, u64),
    /// The GPU adapter's name.
    pub adapter: &'a str,
    /// Which backend it is on.
    pub backend: &'a str,
    /// The surface size in device pixels.
    pub physical: (u32, u32),
    /// The compositor's scale, over 120.
    pub scale_120: u32,
    /// Which platform capabilities were found at run time.
    ///
    /// [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md) says
    /// capabilities are probed and never compiled in. A probe nobody can see is
    /// a probe nobody can debug, so the answer is on screen.
    pub capabilities: &'a str,
}

/// Draws one frame.
pub fn draw(
    painter: &mut Painter<'_>,
    panes: &Panes,
    layout: &Layout,
    now: i64,
    chrome: &Chrome<'_>,
    diagnostics: &mut Diagnostics<'_>,
    hits: &mut Hits,
) {
    let focus = panes.focus();
    let pane = panes.focused();
    hits.panels.clear();
    sidebar(painter, pane, layout, chrome, hits);
    title_bar(painter, pane, layout, chrome);
    window_buttons(painter, layout, chrome);
    // Cleared once for the window and filled by each pane in turn, because a
    // click has to find a crumb in whichever bar it landed in.
    hits.crumbs.clear();
    hits.separators.clear();
    // Cleared for the same reason the look's controls are: a rectangle that
    // outlived the field would be a box the pointer could still find with a
    // committed rename behind it.
    hits.name_field = None;
    for leaf in chrome.leaves {
        let Some(tabs) = panes.leaf(leaf.id) else { continue };
        let has_keyboard = leaf.id == focus;
        tab_strip(painter, tabs, leaf, chrome, has_keyboard);
        nav_bar(painter, tabs.showing(), leaf, chrome, hits, has_keyboard);
        header(painter, tabs.showing(), leaf);
        body(painter, tabs.showing(), leaf, now, chrome, has_keyboard, hits);
    }
    // **Over the pane, and after every pane is drawn.** A drop that lands in the
    // directory a pane is showing has no row to point at, so what is marked is
    // the list itself. Drawn here rather than inside `body` because a ring
    // inside the clip would lose the edge it is a ring around.
    if let Some(target) = chrome.drop_pane
        && let Some(leaf) = chrome.leaves.iter().find(|leaf| leaf.id == target)
    {
        let theme = *painter.theme;
        let shape = painter.rect(leaf.body.x, leaf.body.y, leaf.body.width, leaf.body.height);
        painter.list.rect(shape, theme.palette.accent.with_alpha(28), 0.0);
        let ring = painter.px(2.0).round().max(1.0);
        painter.list.stroke(shape, theme.palette.accent, 0.0, ring);
    }
    // After the panes, so a divider is a line over two edges rather than a gap
    // between them, and before the status strip, which spans the window.
    for divider in chrome.dividers {
        divider_line(painter, *divider, chrome.hovered_divider == Some(divider.id));
    }
    // Where a dragged tab would land, over everything it would land on.
    if let Some(drop) = chrome.tab_drop
        && let Some(leaf) = chrome.leaves.iter().find(|leaf| leaf.id == drop.leaf())
    {
        let count = panes.leaf(leaf.id).map_or(0, |tabs| tabs.panes.len());
        let area = drop.overlay(leaf, count);
        let theme = *painter.theme;
        let shape = painter.rect(area.x, area.y, area.width, area.height);
        painter.list.rect(shape, theme.palette.accent.with_alpha(70), painter.px(4.0));
        let ring = painter.px(1.0).round().max(1.0);
        painter.list.stroke(shape, theme.palette.accent, painter.px(4.0), ring);
    }
    // Every leaf drew its own frame; the window's own chrome belongs to the one
    // with the keyboard, because that is the pane the path bar and the counts
    // are describing.
    let here = focused_leaf(chrome, focus);
    status(painter, pane, layout, chrome, diagnostics, hits);
    if let Some(edits) = chrome.edits {
        let (open, offset) = chrome.history;
        history_at(painter, edits, layout.history, open, offset, chrome.hovered_edit, hits);
    }
    if chrome.filtering || !pane.filter.is_empty() {
        find_popup(painter, pane, here, chrome.filtering, hits);
    }
    if chrome.stats {
        stats_overlay(hits, painter, pane, here, diagnostics);
    }
    // **Cleared whether one is drawn or not.** A control's rectangle outliving the
    // panel it was in would be a button the pointer could still find with nothing
    // on screen under it.
    hits.look.clear();
    if let Some(look) = chrome.quick_look {
        quick_look(painter, layout, look, hits, chrome.hovered_look);
    }
    if let Some(rows) = chrome.keys {
        keys_overlay(painter, layout, rows, hits);
    }
    if let Some(menu) = chrome.crumb_menu {
        crumb_menu(painter, layout, here, menu, hits);
    }
    if let Some(palette) = chrome.palette {
        command_palette(painter, layout, palette, hits);
    }
    if let Some(search) = chrome.search {
        search_panel(painter, layout, search, hits);
    } else {
        hits.search.clear();
        hits.search_field = None;
        hits.search_scope.clear();
    }
    if let Some(panel) = chrome.properties {
        properties(hits, painter, layout, panel, now);
    }
    if let Some(list) = chrome.openers {
        open_with(painter, layout, list, hits);
    }
    // Over both of those: it is the one overlay with no way to dismiss it, so
    // anything drawn on top of it would be drawn on top of the only thing on
    // screen that can be acted on.
    if let Some(clash) = chrome.clash {
        clash_panel(painter, layout, clash, chrome.answering, chrome.for_the_rest, now, hits);
    } else {
        hits.answers.clear();
        hits.for_the_rest = None;
    }
    // The same place, for the same reason: a question with no dismissal must be
    // the thing on top. The two cannot be open at once — one is a queue asking
    // about a name, the other is a queue that has not started.
    if let Some((names, total, on_delete)) = chrome.erasing {
        erase_panel(painter, layout, names, total, on_delete, hits);
    } else {
        hits.erase_cancel = None;
        hits.erase_delete = None;
    }
    if let Some(typed) = chrome.typed {
        type_ahead(painter, here, typed);
    }
    // Over every panel, because it is attached to the pointer and the pointer is
    // over whatever it is over — a card that slid under a panel would look like
    // the drag had been dropped.
    if let Some(carrying) = chrome.carrying {
        drag_card(painter, layout, carrying);
    }
    // Last, so it is over everything: a menu under the statistics overlay is a
    // menu whose rows cannot be read or aimed at.
    match chrome.menu {
        Some(menu) => context_menu(painter, layout, menu, hits),
        None => hits.menu.clear(),
    }
}

/// One string, from a logical `x` across a logical `width`.
///
/// The shorthand exists because a text call has seven inputs and five of them
/// are the same at every site; spelling out the rectangle each time is what made
/// the first version of this file unreadable.
fn line(
    painter: &mut Painter<'_>,
    text: &str,
    style: TextStyle,
    x: f32,
    width: f32,
    baseline: f32,
) -> f32 {
    let box_rect = painter.rect(x, 0.0, width, 0.0);
    painter.label(text, style, box_rect, baseline)
}

/// The strip the window draws for itself: the buttons' ground, and a title.
///
/// Only where the compositor draws no frame. Everywhere else this is zero height
/// and nothing is drawn at all, because a strip holding neither a path nor a
/// button is 44 logical pixels of list given away for a background colour.
///
/// The title is the focused pane's folder, which is what a title bar is for and
/// what the task switcher already shows — the *path* belongs to each pane's own
/// bar, and repeating one of them up here would say which pane is privileged.
fn title_bar(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    let strip = layout.title;
    if strip.height <= 0.0 {
        return;
    }
    fill(painter, strip, theme.palette.chrome);
    painter.hairline(0.0, strip.bottom() - 1.0, strip.width, theme.palette.divider);
    let metrics = theme.metrics;
    let ink =
        if chrome.window.activated { theme.palette.dim } else { theme.palette.dim.with_alpha(140) };
    let baseline = painter.baseline(strip.y, strip.height, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, ink).centred();
    let room = layout.drag.width.max(1.0);
    line(painter, crate::panes::title_of(&pane.path), style, 0.0, room, baseline);
}

/// One pane's own path bar: four buttons, then where that pane is.
///
/// Every leaf draws one, so each half of a split says where *it* is. The hit
/// regions carry the leaf's name for the same reason ([`BarHit`]): a click has
/// to move the keyboard into the pane whose bar it landed in before it navigates.
///
/// The keyboard hint goes in the pane that has the keyboard and nowhere else —
/// four copies of "press F1" is four times the furniture and three times the
/// lie.
fn nav_bar(
    painter: &mut Painter<'_>,
    pane: &Pane,
    leaf: &PaneLayout,
    chrome: &Chrome<'_>,
    hits: &mut Hits,
    has_keyboard: bool,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    if leaf.bar.height <= 0.0 {
        return;
    }
    fill(painter, leaf.bar, theme.palette.chrome);
    painter.hairline(leaf.bar.x, leaf.bar.bottom() - 1.0, leaf.bar.width, theme.palette.divider);
    tool_buttons(painter, pane, leaf, chrome);
    mode_buttons(painter, pane, leaf, chrome);

    let baseline = painter.baseline(leaf.bar.y, leaf.bar.height, metrics.path_size);
    let names = crumb_names(pane);
    let widths = crumb_widths(painter, &names, metrics.path_size);

    // **The hint gives way to the directory's name, and to nothing else.**
    // `first_crumb` never elides the leaf — a bar reading `/ … ›` with the current
    // directory absent is worse than a truncated name — so the leaf is the one
    // component that can overrun, and what it overran into was `F1 keys`. The
    // switcher taking sixty pixels off this region is what made it visible; the
    // collision was reachable before it, at any window narrow enough.
    //
    // Dropping the hint is the right way round. A person who cannot see where they
    // are has lost more than a person who cannot see which key lists the others,
    // and `F1` works whether or not it is advertised.
    let hint = text_width(painter, chrome.hint, metrics.header_size, false);
    let needed = widths.first().copied().unwrap_or_default()
        + crumb_width(painter, i18n::lookup("chrome.crumb_elided"), metrics.path_size, false)
        + CRUMB_PAD
        + widths.last().copied().unwrap_or_default();
    let entering = chrome.path_entry.is_some();
    let spare = leaf.path.width - metrics.gutter * 2.0 - 12.0 - hint;
    let hint_width = if has_keyboard && (entering || spare >= needed) {
        let style = TextStyle::new(metrics.header_size, theme.palette.dim.with_alpha(150)).right();
        line(painter, chrome.hint, style, leaf.path.x, leaf.path.width - metrics.gutter, baseline)
            / painter.scale
    } else {
        0.0
    };

    if has_keyboard && let Some(field) = chrome.path_entry {
        // No breadcrumbs and no hit regions for them: the bar is a text field
        // now, and a click in it belongs to the caret.
        path_entry(painter, leaf, field, baseline);
        return;
    }
    let available = leaf.path.width - metrics.gutter * 2.0 - hint_width - 12.0;

    let first = first_crumb(painter, &names, &widths, available, metrics.path_size);
    let elided = first > 1;
    let mut cursor = leaf.path.x + metrics.gutter;

    // Clipped to its own region **minus the hint's**, so a path that will not fit
    // cannot draw over the keyboard hint or into the pane beside it.
    //
    // `first_crumb` fits the elided components inside `available`, which already
    // reserves the hint — but it deliberately never elides the leaf, because a bar
    // reading `/ … ›` with the current directory absent is worse than a truncated
    // name. So the leaf is the one component that can overrun, and this is what it
    // overruns *into*: without the subtraction it drew straight through `F1 keys`,
    // which is what sixty pixels of switcher off the same region made visible.
    //
    // The hint's own gutter inset comes off as well: it is drawn right-aligned at
    // `path.right() - gutter`, so subtracting only its width leaves the clip a
    // gutter's worth *past* where the hint begins — which was six pixels, and six
    // pixels was the whole of the gap between a directory's name and `F1 keys`.
    let region = painter.rect(
        leaf.path.x,
        leaf.path.y,
        (leaf.path.width - hint_width - metrics.gutter - 6.0).max(0.0),
        leaf.path.height,
    );
    painter.list.push_clip(region);
    for (index, name) in names.iter().enumerate() {
        // A component that did not fit still gets its regions, empty: the depth
        // of a crumb is its index in the path however narrow the bar was.
        if index > 0 && index < first {
            hits.crumbs.push(BarHit { leaf: leaf.id, depth: index, area: Area::default() });
            hits.separators.push(BarHit { leaf: leaf.id, depth: index, area: Area::default() });
            continue;
        }
        let last = index + 1 == names.len();
        let width = widths[index];
        let area = Area { x: cursor, y: leaf.path.y, width, height: leaf.path.height };
        hits.crumbs.push(BarHit { leaf: leaf.id, depth: index, area });

        let text_cell = width - SEPARATOR;
        if chrome.hovered_crumb == Some((leaf.id, index)) && text_cell > 0.0 {
            let shape = painter.rect(area.x, area.y + 6.0, text_cell, area.height - 12.0);
            painter.list.rect(shape, theme.palette.hover, painter.px(metrics.radius));
        }
        if text_cell > 0.0 {
            let style = if last {
                TextStyle::new(metrics.path_size, theme.palette.text).bold()
            } else {
                TextStyle::new(metrics.path_size, theme.palette.dim)
            };
            line(painter, name, style, cursor + CRUMB_PAD, text_cell - CRUMB_PAD, baseline);
        }
        cursor += text_cell;

        // The separator, which is a target of its own.
        let cell = Area { x: cursor, y: leaf.path.y, width: SEPARATOR, height: leaf.path.height };
        hits.separators.push(BarHit { leaf: leaf.id, depth: index, area: cell });
        let opened = has_keyboard && chrome.crumb_menu.map(|it| it.depth) == Some(index);
        let hovered = chrome.hovered_separator == Some((leaf.id, index));
        if hovered || opened {
            let shape = painter.rect(cell.x, cell.y + 6.0, cell.width, cell.height - 12.0);
            painter.list.rect(shape, theme.palette.hover, painter.px(metrics.radius));
        }
        let mark = if index == 0 {
            i18n::lookup("chrome.crumb_root")
        } else {
            i18n::lookup("chrome.crumb_separator")
        };
        let shade = if hovered { theme.palette.text } else { theme.palette.dim.with_alpha(150) };
        let style = TextStyle::new(metrics.path_size, shade).centred();
        line(painter, mark, style, cell.x, cell.width, baseline);
        cursor += SEPARATOR;

        if index == 0 && elided {
            // The ellipsis stands for the components that did not fit, so it
            // takes a separator of its own: `/ … › fixture` reads as root, some
            // things, fixture, where `/ … fixture` reads as one odd name.
            let style = TextStyle::new(metrics.path_size, theme.palette.dim.with_alpha(150));
            let mark = i18n::lookup("chrome.crumb_elided");
            cursor +=
                line(painter, mark, style, cursor, available, baseline) / painter.scale + CRUMB_PAD;
            let style = TextStyle::new(metrics.path_size, theme.palette.dim.with_alpha(150));
            let mark = i18n::lookup("chrome.crumb_separator");
            let cell = Area { x: cursor, y: leaf.path.y, width: SEPARATOR, ..leaf.path };
            line(painter, mark, style.centred(), cell.x, cell.width, baseline);
            cursor += SEPARATOR;
        }
    }
    painter.list.pop_clip();
}

/// The path being typed, across the bar the breadcrumbs were on.
fn path_entry(painter: &mut Painter<'_>, leaf: &PaneLayout, field: &Field, baseline: f32) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let area = path_field(leaf, &metrics);
    let box_rect = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.rect(box_rect, theme.palette.background, painter.px(metrics.radius));
    painter.list.stroke(
        box_rect,
        theme.palette.accent,
        painter.px(metrics.radius),
        painter.snap(1.0).max(1.0),
    );

    let style = TextStyle::new(metrics.path_size, theme.palette.text);
    field_text(painter, area, field.into(), style, PATH_INSET, baseline);
}

/// Where the path field goes, for the draw and for the hit test.
///
/// The focused pane's bar, because only one path is ever being typed: the field
/// belongs to whichever pane has the keyboard.
pub fn path_field(leaf: &PaneLayout, metrics: &sk_ui::Metrics) -> Area {
    Area {
        x: leaf.path.x + metrics.gutter * 0.5,
        y: leaf.path.y + 6.0,
        width: (leaf.path.width - metrics.gutter).max(0.0),
        height: (leaf.path.height - 12.0).max(0.0),
    }
}

/// The text inset inside the path field, shared by the caret, the label and the
/// hit test that turns a click into a caret position.
pub const PATH_INSET: f32 = 8.0;

/// Room for the chevron between two components, in logical pixels.
const SEPARATOR: f32 = SEPARATOR_TARGET;

/// Space each side of a breadcrumb's text, so the hover shape is not tight
/// against the glyphs and the target is bigger than the word.
const CRUMB_PAD: f32 = 6.0;

/// How much of the separator's width is a click target.
///
/// The glyph is narrow and the thing it opens is a menu, so the target is the
/// whole cell rather than the ink: a chevron a person has to hit within two
/// pixels is a chevron nobody discovers.
const SEPARATOR_TARGET: f32 = 18.0;

/// The path's components, root first, with `/` as its own first component.
///
/// `/` and `/home/emil` split differently and both have to read correctly: the
/// root has no name of its own, so it is drawn as the separator it is.
fn crumb_names(pane: &Pane) -> Vec<String> {
    let mut names = vec![i18n::lookup("chrome.crumb_root").to_owned()];
    names.extend(
        pane.path
            .components()
            .filter_map(|part| match part {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .filter(|name| !name.is_empty()),
    );
    names
}

/// The first component to draw, once the bar has been fitted to its width.
///
/// **From the right**, because the leaf answers "where am I" and everything to
/// its left is context: a path too long for the bar loses its middle and keeps
/// its end, which is the half a person is reading. The root stays, because `/`
/// is one glyph and dropping it makes an absolute path look relative.
fn first_crumb(
    painter: &mut Painter<'_>,
    names: &[String],
    widths: &[f32],
    available: f32,
    size: f32,
) -> usize {
    let root = widths.first().copied().unwrap_or_default();
    let mut first = 1;
    // The elision mark has a width of its own and the fitting never counted it,
    // so an elided bar overflowed by exactly that much. Reserved up front: it
    // costs one component's worth of room in the case where there is already not
    // enough, which is the case it exists for.
    let mut total =
        root + crumb_width(painter, i18n::lookup("chrome.crumb_elided"), size, false) + CRUMB_PAD;
    for index in (1..names.len()).rev() {
        if total + widths[index] > available && index > 1 {
            first = index + 1;
            break;
        }
        total += widths[index];
    }
    // **Never past the leaf.** A leaf wider than the whole bar used to set this
    // to `names.len()`, and the draw loop then skipped every index from one —
    // including the leaf. The bar read `/ … ›` with the current directory absent
    // and nothing clickable, at window widths the toplevel's own 320 px minimum
    // can reach. It is drawn and clipped instead.
    first.min(names.len().saturating_sub(1)).max(1)
}

/// How wide each component's cell is, separator included.
///
/// Every component carries the separator that *follows* it, the last one
/// included: its chevron lists the children of where you already are, which is
/// how Windows 11 reads and is the one that saves the most steps. The root draws
/// no name of its own — it is all separator, because `/` is the separator.
fn crumb_widths(painter: &mut Painter<'_>, names: &[String], size: f32) -> Vec<f32> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let text = if index == 0 {
                0.0
            } else {
                crumb_width(painter, name, size, index + 1 == names.len()) + CRUMB_PAD * 2.0
            };
            text + SEPARATOR
        })
        .collect()
}

/// How wide a breadcrumb's text is, in logical pixels.
fn crumb_width(painter: &mut Painter<'_>, name: &str, size: f32, bold: bool) -> f32 {
    text_width(painter, name, size, bold)
}

/// How wide a string is when shaped at `size`, in logical pixels.
///
/// Shaped and not estimated from a character count: a component whose width is
/// guessed is a component that overlaps its neighbour in one script and leaves a
/// hole in another. The run comes out of the same cache the draw will use, so
/// measuring first costs a hash lookup.
fn text_width(painter: &mut Painter<'_>, text: &str, size: f32, bold: bool) -> f32 {
    let px_size = painter.px(size);
    let weight = if bold { Weight::Bold } else { Weight::Regular };
    sk_ui::advance_of(painter.cache, painter.fonts, text, weight, px_size) / painter.scale
}

/// How far a field's text is shifted left so the caret stays inside its box.
///
/// **Derived from the caret, not remembered.** A field here is a filename or a
/// path, open for a few seconds: shifting just enough that the caret is inside
/// the box needs no state, cannot get out of step with the text, and is the same
/// arithmetic on the draw and on the hit test. The cost is that walking left from
/// the end slides the text right rather than moving the caret, which is unusual
/// and legible; the alternative was a caret drawn past the end of the box, or
/// clipped away entirely, which is what `Ctrl+L` did on any path longer than the
/// bar.
#[must_use]
pub fn field_scroll(caret: f32, room: f32) -> f32 {
    (caret + CARET_PAD - room).max(0.0)
}

/// How much of the box is kept to the right of the caret.
const CARET_PAD: f32 = 12.0;

/// The context menu, over everything else.
///
/// **Width from the widest row, never a constant.** Every string here is
/// translated and a chord's display name is longer in some languages than the
/// command's; a fixed width ellipsizes Move to Trash in exactly the locale
/// nobody testing it reads.
fn context_menu(painter: &mut Painter<'_>, layout: &Layout, menu: &Menu, hits: &mut Hits) {
    use crate::menu::{DIVIDER, INSET, SPACING, STEP, WIDTH};

    let theme = *painter.theme;
    let metrics = theme.metrics;
    // What fits in the window, which for the background menu is not all of it.
    let (first, drawn) = menu.window(layout.window.height - crate::menu::MARGIN * 2.0);
    let widest = drawn
        .iter()
        .map(|item| {
            let name = text_width(painter, &item.name, metrics.text_size, false);
            let keys = text_width(painter, &item.keys, metrics.header_size, true);
            name + if keys > 0.0 { SPACING + keys } else { 0.0 }
        })
        .fold(0.0f32, f32::max);
    let width = (widest + INSET * 2.0).clamp(WIDTH.0, WIDTH.1);
    let area = crate::menu::place(menu.at, width, crate::menu::height(drawn), layout.window);
    panel_shape(painter, area, hits);

    // One rectangle per row, drawn or not, so a hit is an index into the menu's
    // own rows: a row that is scrolled out gets an empty rectangle, which
    // `contains` answers `false` for.
    hits.menu.clear();
    hits.menu.resize(menu.items.len(), Area::default());
    let tops: Vec<f32> = crate::menu::row_tops(drawn, area).collect();
    for (offset, (item, top)) in drawn.iter().zip(tops).enumerate() {
        let index = first + offset;
        hits.menu[index] = Area { x: area.x, y: top, width: area.width, height: STEP };
        if item.starts_group && offset > 0 {
            let y = top - DIVIDER / 2.0;
            painter.hairline(area.x + INSET, y, area.width - INSET * 2.0, theme.palette.divider);
        }
        let highlighted = menu.active == Some(index);
        if highlighted {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let (name_colour, keys_colour) = match (item.enabled, highlighted) {
            (false, _) => (theme.palette.dim.with_alpha(110), theme.palette.dim.with_alpha(80)),
            (true, true) => (theme.palette.selected_text, theme.palette.selected_text),
            (true, false) => (theme.palette.text, theme.palette.dim),
        };
        let baseline = painter.baseline(top, STEP, metrics.text_size);
        let mut inner = area.width - INSET * 2.0;
        let keys_width = text_width(painter, &item.keys, metrics.header_size, true);
        let name_style = TextStyle::new(metrics.text_size, name_colour);
        let mut x = area.x + INSET;
        // **A colour's row shows the colour.** Seven rows of colour names with no
        // colours on them is a menu somebody has to try rather than read, which
        // is the difference between this and every other row here.
        if let Some(colour) = crate::colours::of_command(item.id) {
            let dot = 10.0;
            let ink = colour.ink(&theme.palette);
            let ink = if item.enabled { ink } else { ink.with_alpha(90) };
            let box_ = painter.rect(x, top + (STEP - dot) / 2.0, dot, dot);
            painter.list.rect(box_, ink, painter.px(dot / 2.0));
            x += dot + 8.0;
            inner -= dot + 8.0;
        }
        line(painter, &item.name, name_style, x, inner - keys_width - SPACING * 0.5, baseline);
        let keys_style = TextStyle::new(metrics.header_size, keys_colour).right();
        line(painter, &item.keys, keys_style, x, inner, baseline);
    }
    // A menu that has more above or below says so. Without it a scrolled menu
    // looks like a complete one that is missing commands.
    let centre = area.x + area.width / 2.0;
    if first > 0 {
        painter.triangle(centre - 3.5, area.y + 1.0, 7.0, 4.0, theme.palette.dim, Direction::Up);
    }
    if first + drawn.len() < menu.items.len() {
        let y = area.bottom() - 5.0;
        painter.triangle(centre - 3.5, y, 7.0, 4.0, theme.palette.dim, Direction::Down);
    }
}

/// The command palette: a query field over the results it narrows.
///
/// Centred horizontally and near the top, which is where every palette a person
/// has used puts it, and a fixed height so the list does not jump as a query
/// narrows.
/// The sibling menu hanging from a breadcrumb separator.
///
/// Anchored under the chevron it belongs to, and pushed back inside the window
/// where the chevron is near the right edge: a menu that runs off the screen is
/// a menu whose bottom rows nobody can reach.
fn crumb_menu(
    painter: &mut Painter<'_>,
    layout: &Layout,
    here: &PaneLayout,
    menu: &crate::crumbs::CrumbMenu,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let (first, drawn) = menu.window();
    let nothing = i18n::lookup("chrome.crumb_empty");

    let widest = if drawn.is_empty() {
        text_width(painter, nothing, metrics.path_size, false)
    } else {
        drawn
            .iter()
            .map(|name| text_width(painter, name, metrics.path_size, false))
            .fold(0.0_f32, f32::max)
            + CRUMB_MENU_MARK
    };
    let width = (widest + CRUMB_MENU_INSET * 2.0)
        .clamp(CRUMB_MENU_WIDTH.0, CRUMB_MENU_WIDTH.1.min(layout.window.width - 16.0));
    // A menu with nothing in it is still one row tall. Sized by its rows alone it
    // is eight pixels of panel with the message hanging out of both ends.
    let height = drawn.len().max(1) as f32 * CRUMB_MENU_STEP + CRUMB_MENU_PAD * 2.0;
    let area = Area {
        x: (menu.at - CRUMB_MENU_INSET).clamp(8.0, (layout.window.width - width - 8.0).max(8.0)),
        // Under the bar it was opened from, which is the focused pane's: opening
        // one puts the keyboard in that pane, and nothing can move it while the
        // menu is up.
        y: here.bar.bottom(),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    hits.crumb_menu.clear();
    for (offset, name) in drawn.iter().enumerate() {
        let top = area.y + CRUMB_MENU_PAD + offset as f32 * CRUMB_MENU_STEP;
        let row = Area { x: area.x, y: top, width: area.width, height: CRUMB_MENU_STEP };
        hits.crumb_menu.push(row);
        let highlighted = menu.active == Some(first + offset);
        if highlighted {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, CRUMB_MENU_STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let baseline = painter.baseline(top, CRUMB_MENU_STEP, metrics.path_size);
        // The way you came, marked. Without it a menu of forty siblings gives no
        // clue which of them the path already goes through.
        if menu.current == Some(first + offset) {
            let mark = TextStyle::new(
                metrics.path_size,
                if highlighted { theme.palette.selected_text } else { theme.palette.accent },
            );
            line(
                painter,
                i18n::lookup("chrome.crumb_here"),
                mark,
                area.x + CRUMB_MENU_INSET,
                CRUMB_MENU_MARK,
                baseline,
            );
        }
        let colour = if highlighted { theme.palette.selected_text } else { theme.palette.text };
        let style = TextStyle::new(metrics.path_size, colour);
        line(
            painter,
            name,
            style,
            area.x + CRUMB_MENU_INSET + CRUMB_MENU_MARK,
            area.width - CRUMB_MENU_INSET * 2.0 - CRUMB_MENU_MARK,
            baseline,
        );
    }
    if drawn.is_empty() {
        let baseline = painter.baseline(area.y, area.height, metrics.path_size);
        let style = TextStyle::new(metrics.path_size, theme.palette.dim);
        line(
            painter,
            nothing,
            style,
            area.x + CRUMB_MENU_INSET,
            area.width - CRUMB_MENU_INSET * 2.0,
            baseline,
        );
    }
}

/// One row of a separator's menu.
const CRUMB_MENU_STEP: f32 = 26.0;
/// Space above and below the rows.
const CRUMB_MENU_PAD: f32 = 4.0;
/// Text inset inside the menu.
const CRUMB_MENU_INSET: f32 = 10.0;
/// The column the "you are here" mark sits in.
const CRUMB_MENU_MARK: f32 = 16.0;
/// How wide the menu may be, whatever its widest name is.
const CRUMB_MENU_WIDTH: (f32, f32) = (160.0, 420.0);

/// How much of the window a quick look takes.
///
/// **Most of it, with a margin.** A quick look is the thing being looked at; a
/// panel that took half the window would be a picture too small to have been
/// worth opening. The margin is what keeps the list visible behind it, so it is
/// still obvious what is being looked at and where it came from.
const QUICK_LOOK_MARGIN: f32 = 48.0;

/// The strip at the top of the panel that holds its title.
const QUICK_LOOK_TITLE: f32 = 34.0;

/// The rectangle a quick look's *content* goes in, in logical pixels.
///
/// **One function, because the app asks the same question the drawing does.** The
/// picture is rendered at this size — the child is told the box — so a panel drawn
/// to a different rectangle than the one that was asked for would be a picture
/// scaled twice.
pub(crate) fn quick_look_area(layout: &Layout) -> Area {
    let outer = Area {
        x: QUICK_LOOK_MARGIN,
        y: QUICK_LOOK_MARGIN,
        width: (layout.window.width - QUICK_LOOK_MARGIN * 2.0).max(64.0),
        height: (layout.window.height - QUICK_LOOK_MARGIN * 2.0).max(64.0),
    };
    Area {
        x: outer.x,
        y: outer.y + QUICK_LOOK_TITLE,
        width: outer.width,
        height: (outer.height - QUICK_LOOK_TITLE).max(1.0),
    }
}

/// The strip at the top of a quick look, where its title and controls are.
pub(crate) fn quick_look_strip(layout: &Layout) -> Area {
    let inner = quick_look_area(layout);
    Area { x: inner.x, y: inner.y - QUICK_LOOK_TITLE, width: inner.width, height: QUICK_LOOK_TITLE }
}

/// The quick look panel: a title, and the file under it.
fn quick_look(
    painter: &mut Painter<'_>,
    layout: &Layout,
    look: &crate::quicklook::QuickLook,
    hits: &mut Hits,
    hovered: Option<LookControl>,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let inner = quick_look_area(layout);
    let outer = Area {
        x: inner.x,
        y: inner.y - QUICK_LOOK_TITLE,
        width: inner.width,
        height: inner.height + QUICK_LOOK_TITLE,
    };
    panel_shape(painter, outer, hits);

    // **The strip's own top, not six pixels into it.** `baseline` centres the text
    // in the box it is given, so a top pushed down by six with the full height
    // still passed centres it six pixels low — and the strip is 34 tall, where six
    // is visible.
    let baseline = painter.baseline(outer.y, QUICK_LOOK_TITLE, metrics.path_size);
    let strip = Area { height: QUICK_LOOK_TITLE, ..outer };
    let name_from = quick_look_controls(painter, strip, look, hits, (baseline, hovered));
    let title = look.title();
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(painter, &title, style, name_from, (strip.right() - 8.0 - name_from).max(1.0), baseline);
    painter.hairline(outer.x, inner.y - 1.0, outer.width, theme.palette.divider);

    if let Some((key, size)) = look.image {
        // **The page's own rectangle, and it is what the words are drawn onto.**
        // A selection over a page is boxes in the page's points, and this is the
        // one place that knows where those points landed — the same function the
        // zoom and the pan go through, so a highlight cannot disagree with the
        // page it is over.
        // **Fitted here, and this is the only place that fits it.** The child was
        // asked for a box and answered with what fitted inside it, so the size that
        // came back is already right — all that is left is to centre it.
        //
        // **Except when it is zoomed**, where the picture is larger than the
        // panel and has to be clipped to it: a preview is a quad and a quad that
        // hangs over the panel's edge is drawn over the list behind it.
        let box_ = fitted_box(inner, size, look.coarse, (look.zoom, look.pan));
        if look.zoomed() {
            let clip = painter.rect(inner.x, inner.y, inner.width, inner.height);
            painter.list.push_clip(clip);
            let shape = painter.rect(box_.x, box_.y, box_.width, box_.height);
            painter.preview(key, shape, theme.palette.text);
            page_selection(painter, look, box_);
            painter.list.pop_clip();
            page_find(painter, look, inner, hits);
            return;
        }
        painter.preview(key, fitted(painter, inner, size, look.coarse), theme.palette.text);
        page_selection(painter, look, box_);
        page_find(painter, look, inner, hits);
        return;
    }
    if let Some((key, aspect)) = look.standby {
        // **The thumbnail, drawn where the picture will be.** Already on the GPU —
        // the row it belongs to was on screen — so it is the only free thing in
        // the whole path, and it answers the question a person has while waiting:
        // *which* picture is this.
        //
        // Fitted from the thumbnail's own aspect, which is the picture's, so the
        // rectangle is the one the picture will occupy: fitting a shape of a given
        // aspect into a box has one answer whatever size the shape is. Soft for
        // the sixty milliseconds it is up, and in the right place, which is the
        // trade a stand-in exists to make.
        //
        // **Unless the thumbnail says the picture is small.** A raster thumbnail
        // is never enlarged into its 128-pixel slot — `box_fit` — so one that
        // came back strictly inside the slot *is* the source, at its own size,
        // and the finished preview will be drawn at that size rather than at the
        // panel's. Enlarging the stand-in there is the one case where it lands
        // somewhere the picture will not: a 64-pixel icon shown eleven times too
        // big for sixty milliseconds. An SVG fills its slot by construction — a
        // vector has no natural size — so this never fires for one, which is
        // right, because an SVG preview *does* fill the panel.
        painter.standby(key, fitted(painter, inner, aspect, grows(aspect)), theme.palette.text);
        return;
    }
    if let Some(text) = &look.text {
        quick_look_text(painter, look, text, inner);
        return;
    }
    // Neither yet. **A refusal always says something**, its own sentence where it
    // has one and a catch-all where it has not: three of them mean "nothing to
    // say", which is right when it decides whether the panel *opens* and wrong
    // once it is open — an empty panel is indistinguishable from a program that
    // has stopped. Where nothing has been refused, nothing is drawn: the draft
    // arrives in sixty milliseconds and a spinner for that is worse than nothing —
    // [rule 7](../../../docs/03-principles.md).
    let Some(message) = look.said() else { return };
    let baseline = painter.baseline(inner.y + inner.height / 2.0, 0.0, metrics.path_size);
    // Centred, because it is a statement about the whole panel rather than an
    // entry in it — the same reason the Properties panel centres its title.
    let style = TextStyle::new(metrics.path_size, theme.palette.dim).centred();
    line(painter, i18n::lookup(message), style, inner.x + 14.0, inner.width - 28.0, baseline);
}

/// The strip's controls: the file pair on the left, the page pair on the right.
///
/// Returns where the name may start, which is past the file pair — the name is
/// laid out around the controls rather than the other way round, because the
/// controls are a fixed size and a name is whatever it is.
fn quick_look_controls(
    painter: &mut Painter<'_>,
    strip: Area,
    look: &crate::quicklook::QuickLook,
    hits: &mut Hits,
    text: (f32, Option<LookControl>),
) -> f32 {
    let (baseline, hovered) = text;
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let side = QUICK_LOOK_TITLE;
    let mut at = strip.x + 4.0;
    for (control, direction) in
        [(LookControl::PreviousFile, Direction::Left), (LookControl::NextFile, Direction::Right)]
    {
        let area = Area { x: at, y: strip.y, width: side, height: side };
        look_button(painter, area, (control, hovered), direction, hits);
        at += side;
    }

    // **The Edit control, only where there is text.** A picture and a page are
    // not editable, so the button that says a preview can be typed into is drawn
    // exactly where that is true — rule 4, which is that a feature reachable only
    // by knowing the chord is a feature most people never find.
    let mut right = strip.right() - 8.0;
    if look.text.is_some() {
        let key = if look.editing { "look.done" } else { "look.edit" };
        let label = i18n::lookup(key);
        let width = text_width(painter, label, metrics.header_size, false) + 16.0;
        let area = Area { x: right - width, y: strip.y, width, height: side };
        let ink = if look.editing { theme.palette.accent } else { theme.palette.dim };
        if hovered == Some(LookControl::Edit) {
            let shape = painter.rect(area.x, area.y + 5.0, area.width, area.height - 10.0);
            painter.list.rect(shape, theme.palette.button_hover, painter.px(metrics.radius));
        }
        let style = TextStyle::new(metrics.header_size, ink).centred();
        line(painter, label, style, area.x, area.width, baseline);
        hits.look.push((LookControl::Edit, area));
        right = area.x - 4.0;
    }

    // **Discard, beside Done, and only while there is something to discard.**
    // Leaving edit mode writes what was typed — which is what makes not asking
    // safe — so the way out that writes nothing has to be *visible*, not a chord
    // in the overlay: rule 4, and the same argument that put Edit here.
    if look.unsaved {
        let label = i18n::lookup("look.discard");
        let width = text_width(painter, label, metrics.header_size, false) + 16.0;
        let area = Area { x: right - width, y: strip.y, width, height: side };
        if hovered == Some(LookControl::Discard) {
            let shape = painter.rect(area.x, area.y + 5.0, area.width, area.height - 10.0);
            painter.list.rect(shape, theme.palette.button_hover, painter.px(metrics.radius));
        }
        let style = TextStyle::new(metrics.header_size, theme.palette.dim).centred();
        line(painter, label, style, area.x, area.width, baseline);
        hits.look.push((LookControl::Discard, area));
    }

    // **What the zoom is, where it is not one.** A picture that is magnified and
    // does not say so is a picture somebody will believe is the file's own size —
    // and the same figure is what says a pinch did anything at all on a machine
    // where the gesture is absent.
    if look.zoomed() {
        let label = format!("{:.0}%", look.zoom * 100.0);
        let width = text_width(painter, &label, metrics.header_size, false) + 4.0;
        let style = TextStyle::new(metrics.header_size, theme.palette.dim).right();
        line(painter, &label, style, right - width, width, baseline);
        right -= width + 8.0;
    }

    // **Play and pause, only where there is an animation.** A GIF plays when it
    // opens, so this is the control that stops it — and the one that starts it
    // again after a frame arrow has stepped somewhere, which is the gesture that
    // pauses. Drawn as the two shapes everything with a play button draws, rather
    // than as a glyph: a triangle and two bars are in no font's coverage table.
    if look.animated() {
        let side = QUICK_LOOK_TITLE;
        let area = Area { x: right - side, y: strip.y, width: side, height: side };
        if hovered == Some(LookControl::Play) {
            let shape = painter.rect(area.x + 3.0, area.y + 5.0, side - 6.0, side - 10.0);
            painter.list.rect(shape, theme.palette.button_hover, painter.px(metrics.radius));
        }
        let ink = theme.palette.accent;
        let middle = area.y + (side - 10.0) / 2.0;
        if look.playing {
            for offset in [0.0, 6.0] {
                let bar = painter.rect(area.x + 9.0 + offset, middle, 3.0, 10.0);
                painter.list.rect(bar, ink, 0.0);
            }
        } else {
            painter.triangle(area.x + 10.0, middle, 8.0, 10.0, ink, Direction::Right);
        }
        hits.look.push((LookControl::Play, area));
        right = area.x - 2.0;
    }

    // **The page pair only where there are pages**, drawn from the right edge
    // inwards so the whole group stays put as the numbers widen from `1` to
    // `1000`.
    if look.paged() {
        let label = look.page_label();
        let style = TextStyle::new(metrics.path_size, theme.palette.dim);
        // **An estimate, and deliberately a generous one.** There is no measure
        // that does not also draw — `Painter::label` returns an advance *after*
        // putting glyphs in the list — so the room is taken from the character
        // count. Over-estimating leaves a pixel or two of air between the numbers
        // and the arrow on their right; under-estimating would elide a page
        // number, so the error is spent in the harmless direction.
        let room = label.chars().count() as f32 * metrics.path_size * 0.62;
        let width = side * 2.0 + room + 12.0;
        let from = (right - width).max(at);
        let back = Area { x: from, y: strip.y, width: side, height: side };
        look_button(painter, back, (LookControl::PreviousPage, hovered), Direction::Left, hits);
        line(painter, &label, style, from + side + 6.0, room + 2.0, baseline);
        let forward = Area { x: from + side + room + 12.0, y: strip.y, width: side, height: side };
        look_button(painter, forward, (LookControl::NextPage, hovered), Direction::Right, hits);
    }
    at + 6.0
}

/// One of them: a hover pill, an arrow, and a rectangle the pointer can find.
fn look_button(
    painter: &mut Painter<'_>,
    area: Area,
    which: (LookControl, Option<LookControl>),
    direction: Direction,
    hits: &mut Hits,
) {
    let (control, hovered) = which;
    let theme = *painter.theme;
    if hovered == Some(control) {
        let shape = painter.rect(area.x + 3.0, area.y + 5.0, area.width - 6.0, area.height - 10.0);
        painter.list.rect(shape, theme.palette.button_hover, painter.px(theme.metrics.radius));
    }
    let centre = (area.x + area.width / 2.0, area.y + area.height / 2.0);
    arrow(painter, centre, theme.palette.dim, direction);
    hits.look.push((control, area));
}

/// A picture's rectangle, centred in `inner` with its aspect kept.
///
/// **Never larger than it came back at, unless it stands in for one that will
/// be.** A finished render was asked for a box and answered with what fitted
/// inside it, so enlarging it would be a picture scaled twice — once by a box
/// filter and once by a sampler. A draft or a thumbnail is the opposite case: it
/// has to occupy the rectangle its replacement will, or the panel jumps when the
/// replacement lands — and since both preserve the same aspect, that rectangle is
/// the same one.
///
/// The one case this gets wrong is a picture *smaller* than the panel: its
/// stand-in is drawn at the panel's size and its replacement at its own, so it
/// shrinks. Getting that right needs the source's dimensions, which are not on
/// the wire, and it lasts the sixty milliseconds an image takes.
/// Whether a thumbnail of this size may be enlarged to stand in for the picture.
///
/// A raster thumbnail is never enlarged into its slot, so one that came back
/// strictly inside the slot is the source at its own size — and the picture will
/// be drawn at that size. One that touches the slot's edge came from something at
/// least that big, and so did the picture.
fn grows(aspect: (u32, u32)) -> bool {
    aspect.0 >= sk_gpu::THUMBNAIL_SLOT || aspect.1 >= sk_gpu::THUMBNAIL_SLOT
}

fn fitted(painter: &Painter<'_>, inner: Area, size: (u32, u32), standing_in: bool) -> sk_gpu::Rect {
    let box_ = fitted_box(inner, size, standing_in, (1.0, (0.0, 0.0)));
    painter.rect(box_.x, box_.y, box_.width, box_.height)
}

/// A pan that keeps a zoomed picture's edges no further in than the panel's.
///
/// **One home for it, because both the window and a capture pan.** A capture
/// sets the pan directly to render a frame of a dragged picture, and a pan the
/// window would have clamped away is a reference of a state the program cannot
/// reach — which is the defect this frame was written to catch and caught, in
/// itself, on the first render.
pub(crate) fn clamped_pan(
    inner: Area,
    size: (u32, u32),
    standing_in: bool,
    zoom: (f32, (f32, f32)),
) -> (f32, f32) {
    let (magnify, pan) = zoom;
    let drawn = fitted_box(inner, size, standing_in, (magnify, (0.0, 0.0)));
    // Half the overhang in each direction: at that pan the picture's edge is
    // exactly at the panel's, and a margin of window inside a panel the picture
    // is bigger than is the thing this prevents.
    let slack = |drawn: f32, room: f32| ((drawn - room) / 2.0).max(0.0);
    let (across, down) = (slack(drawn.width, inner.width), slack(drawn.height, inner.height));
    (pan.0.clamp(-across, across), pan.1.clamp(-down, down))
}

/// Where a picture goes in the panel, in logical pixels.
///
/// **One function for the drawing and the pointer.** A pan is clamped against
/// the drawn rectangle and the drawn rectangle is computed from the pan, so a
/// second copy of this arithmetic is a picture that can be dragged to somewhere
/// it is not drawn.
pub(crate) fn fitted_box(
    inner: Area,
    size: (u32, u32),
    standing_in: bool,
    zoom: (f32, (f32, f32)),
) -> Area {
    let (magnify, pan) = zoom;
    let fit = (inner.width / size.0.max(1) as f32).min(inner.height / size.1.max(1) as f32);
    let scale = if standing_in { fit } else { fit.min(1.0) };
    let (width, height) = (size.0 as f32 * scale * magnify, size.1 as f32 * scale * magnify);
    Area {
        x: inner.x + (inner.width - width) / 2.0 + pan.0,
        y: inner.y + (inner.height - height) / 2.0 + pan.1,
        width,
        height,
    }
}

/// The inset between the text and the panel's edges.
const TEXT_PAD: f32 = 14.0;

/// The gap between the line numbers and the text they belong to.
const TEXT_GUTTER_GAP: f32 = 10.0;

/// Where a text preview's parts sit, so the drawing and the pointer agree.
///
/// **One function, because a selection is the two of them having the same
/// answer.** A click is turned into a byte by measuring from `text_x`, and the
/// highlight is drawn from `text_x`; a second copy of that arithmetic is a
/// selection that lands one gutter to the left of where it was made.
#[derive(Clone, Copy)]
pub(crate) struct TextLayout {
    /// Where the line numbers' column ends.
    pub(crate) gutter: f32,
    /// Where a line's own text starts.
    pub(crate) text_x: f32,
    /// One line's pitch.
    pub(crate) step: f32,
}

/// The logical width of the widest line number a text of `lines` lines can show.
///
/// **From the count and not from the line on screen**, so the gutter does not
/// widen as a scroll passes line 100 and shift every line of the file sideways.
pub(crate) fn gutter_width(
    lines: usize,
    size: f32,
    cache: &mut sk_text::LayoutCache,
    fonts: &mut sk_text::Fonts,
    scale: f32,
) -> f32 {
    let widest = "0".repeat(lines.max(1).to_string().len());
    // **Plus two, because a box exactly one advance wide elides.** The shaped
    // advance is device pixels rounded to the glyph grid and the box is snapped
    // to it separately, so a number that fits by arithmetic came out as `1…`.
    sk_ui::advance_of(cache, fonts, &widest, sk_text::Weight::Mono, size * scale) / scale.max(0.01)
        + 2.0
}

/// Where the parts of a text preview go, given the gutter's measured width.
pub(crate) fn text_layout(inner: Area, metrics: &sk_ui::Metrics, digits: f32) -> TextLayout {
    let gutter = inner.x + TEXT_PAD + digits;
    TextLayout { gutter, text_x: gutter + TEXT_GUTTER_GAP, step: metrics.row_height }
}

/// How many monospaced characters fit across a text preview.
///
/// **The wrap width, measured where the text is drawn.** One cell, because the
/// face is monospaced and every advance in it is the same — which is the same
/// assumption the caret and the selection already make.
pub(crate) fn text_cells(
    lines: usize,
    where_: (Area, &sk_ui::Metrics),
    tools: (&mut sk_text::LayoutCache, &mut sk_text::Fonts, f32),
) -> usize {
    let (inner, metrics) = where_;
    let (cache, fonts, scale) = tools;
    let size = metrics.text_size;
    let digits = gutter_width(lines, size, cache, fonts, scale);
    let placed = text_layout(inner, metrics, digits);
    let cell =
        sk_ui::advance_of(cache, fonts, "0", sk_text::Weight::Mono, size * scale) / scale.max(0.01);
    let room = inner.right() - TEXT_PAD - placed.text_x;
    if cell < 0.5 || room < cell {
        return 0;
    }
    (room / cell).floor() as usize
}

/// Which line and byte a point lands on, for a press or a drag.
///
/// **Clamped rather than optional.** A drag that leaves the panel has to keep
/// extending the selection — that is what dragging past the edge means everywhere
/// — so a point above the first line is the start of the file and one below the
/// last is the end of it.
pub(crate) fn text_spot(
    look: &crate::quicklook::QuickLook,
    text: &crate::quicklook::Text,
    where_: (Area, &sk_ui::Metrics),
    at: (f32, f32),
    tools: (&mut sk_text::LayoutCache, &mut sk_text::Fonts, f32),
) -> crate::quicklook::Spot {
    let (inner, metrics) = where_;
    let (cache, fonts, scale) = tools;
    let size = metrics.text_size;
    let digits = gutter_width(text.lines.len(), size, cache, fonts, scale);
    let placed = text_layout(inner, metrics, digits);
    let at_row = ((at.1 - inner.y + look.scroll.offset()) / placed.step).floor();
    let index = (at_row.max(0.0) as usize).min(text.rows().len().saturating_sub(1));
    // **The row's own slice, not the whole line.** A wrapped line is several rows
    // and a click on the third of them is about the characters drawn there; before
    // this, every row of a paragraph answered with a byte counted from the line's
    // start, so a click below the first row landed at the end of the text.
    let row = text.rows().get(index).copied();
    let body =
        row.and_then(|row| text.lines.get(row.line)?.get(row.from..row.to)).unwrap_or_default();
    // The colour is not part of a measurement; the weight is, and it has to be
    // the face the text was drawn in or the answer is a different font's.
    let style = mono(TextStyle::new(size, sk_gpu::Color::rgba(0, 0, 0, 0)));
    let want = (at.0 - placed.text_x).max(0.0) * scale;
    let byte = sk_ui::caret_offset(fonts, body, style, size * scale, want);
    let cell = body.get(..byte).map_or(0, |slice| slice.chars().count());
    text.spot_at(index, cell)
}

/// The text of a file, line by line, each line by its tones.
///
/// **Drawn as runs, which is the whole of the syntax-highlighting seam.** Every
/// line is already cut into `Span`s carrying a `Tone`, and this colours each run by
/// its tone; today there is one run per line and one tone. A highlighter replaces
/// `quicklook::highlight` and nothing here.
///
/// **In the mono face, with its lines numbered.** A file's own text is the one
/// place in this program where the columns *are* the content: an indented block, a
/// table of numbers and a diff all stop meaning anything in a proportional face.
fn quick_look_text(
    painter: &mut Painter<'_>,
    look: &crate::quicklook::QuickLook,
    text: &crate::quicklook::Text,
    inner: Area,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let digits = {
        let Painter { cache, fonts, scale, .. } = painter;
        gutter_width(text.lines.len(), metrics.text_size, cache, fonts, *scale)
    };
    let placed = text_layout(inner, &metrics, digits);
    let step = placed.step;
    let first = (look.scroll.offset() / step).floor().max(0.0) as usize;
    let room = (inner.height / step).ceil() as usize + 1;
    // **Drawn by row, and a row is what wrapping made of a line.** The gutter
    // numbers the first row of each line and leaves the continuations blank,
    // which is what every editor with soft wrap does — a line number repeated
    // down six rows says the file has six line 12s.
    let rows = text.rows();
    let clip = painter.rect(inner.x, inner.y, inner.width, inner.height);
    painter.list.push_clip(clip);
    // The gutter's own rule, so the numbers read as a column rather than as the
    // first word of every line. A vertical rule, which `hairline` does not draw:
    // that one is a horizontal line of a given width.
    let rule = painter.rect(
        placed.gutter + TEXT_GUTTER_GAP / 2.0,
        inner.y,
        painter.logical(1.0),
        inner.height,
    );
    painter.list.rect(rule, theme.palette.divider, 0.0);
    for (index, row) in rows.iter().enumerate().skip(first).take(room) {
        let Some(whole) = text.lines.get(row.line) else { continue };
        let top = inner.y + index as f32 * step - look.scroll.offset();
        let baseline = painter.baseline(top, step, metrics.text_size);
        text_selection(painter, look, (index, whole), (placed, top, inner));
        text_caret(painter, look, (index, whole), (placed, top, inner));
        if row.from == 0 {
            let number = (row.line + 1).to_string();
            let numbers = TextStyle::new(metrics.text_size, theme.palette.dim).right();
            let column = painter.rect(inner.x + TEXT_PAD, 0.0, digits, 0.0);
            painter.label(&number, mono(numbers), column, baseline);
        }
        let mut x = placed.text_x;
        for span in text.spans.get(row.line).map_or(&[][..], Vec::as_slice) {
            // The part of this run that is on this row, which is the whole of
            // what wrapping costs the highlighter: a span is still a slice of a
            // line, and a row is a window onto it.
            let (from, to) = (span.from.max(row.from), span.to.min(row.to));
            let Some(run) = whole.get(from..to).filter(|it| !it.is_empty()) else { continue };
            // **The theme decides what a role looks like**, which is the whole
            // reason a `Tone` is a role: a light theme is not a second table of
            // hexadecimal in the highlighter.
            let code = theme.palette.code;
            let colour = match span.tone {
                crate::quicklook::Tone::Plain => theme.palette.text,
                crate::quicklook::Tone::Comment => code.comment,
                crate::quicklook::Tone::Keyword => code.keyword,
                crate::quicklook::Tone::Literal => code.literal,
                crate::quicklook::Tone::Number => code.number,
            };
            let style = mono(TextStyle::new(metrics.text_size, colour));
            let room = (inner.right() - TEXT_PAD - x).max(1.0);
            let drawn = line(painter, run, style, x, room, baseline);
            x += painter.logical(drawn);
        }
    }
    painter.list.pop_clip();
    if text.truncated {
        let baseline = painter.baseline(inner.bottom() - step, step, metrics.text_size);
        let style = TextStyle::new(metrics.text_size, theme.palette.dim);
        line(painter, i18n::lookup("preview.more"), style, placed.text_x, inner.width, baseline);
    }
}

/// The highlight behind what is selected on a page.
///
/// **Over the page rather than behind it**, which is the opposite of the text
/// preview's rule and is forced: a page is an opaque image, so a highlight under
/// it would not be seen at all. Translucent, so the words stay readable through
/// it — the same accent at the same alpha a text selection uses, and the reason
/// it reads as a selection rather than as a redaction.
fn page_selection(painter: &mut Painter<'_>, look: &crate::quicklook::QuickLook, page: Area) {
    let Some(text) = look.page_text.as_ref() else { return };
    let (across, down) = text.size();
    if across <= 0.0 || down <= 0.0 {
        return;
    }
    let theme = *painter.theme;
    let scale = (page.width / across, page.height / down);
    // **The matches a find has not walked to yet, fainter.** `3 of 7` is a
    // number; seven marks on the page is where they are — and the seventh is
    // often the one somebody was looking for.
    if let Some(query) = look.finding.as_ref().map(|field| field.text.as_str()) {
        for (left, top, width, height) in text.other_matches(query) {
            let shape = painter.rect(
                page.x + left * scale.0,
                page.y + top * scale.1,
                (width * scale.0).max(1.0),
                (height * scale.1).max(1.0),
            );
            painter.list.rect(shape, theme.palette.accent.with_alpha(40), 0.0);
        }
    }
    for (left, top, width, height) in text.highlights() {
        let shape = painter.rect(
            page.x + left * scale.0,
            page.y + top * scale.1,
            (width * scale.0).max(1.0),
            (height * scale.1).max(1.0),
        );
        painter.list.rect(shape, theme.palette.accent.with_alpha(90), 0.0);
    }
}

/// The find bar, along the bottom of the panel.
///
/// **At the bottom, because the strip is full and a browser puts it there.** The
/// panel's own strip already carries the file arrows, the page pair, the play
/// control, the zoom and Edit; a find field in it would be a sixth thing
/// competing for a 34-pixel row.
fn page_find(
    painter: &mut Painter<'_>,
    look: &crate::quicklook::QuickLook,
    inner: Area,
    hits: &mut Hits,
) {
    let Some(field) = look.finding.as_ref() else { return };
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let height = PAGE_FIND_HEIGHT;
    let area = Area { x: inner.x, y: inner.bottom() - height, width: inner.width, height };
    let ground = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.rect(ground, theme.palette.chrome, 0.0);
    painter.hairline(area.x, area.y, area.width, theme.palette.divider);
    let baseline = painter.baseline(area.y, height, metrics.path_size);
    let said = look.found_label();
    let width = text_width(painter, &said, metrics.header_size, false) + 6.0;
    if !said.is_empty() {
        let ink = if look.found.0 == 0 { theme.palette.danger } else { theme.palette.dim };
        let style = TextStyle::new(metrics.header_size, ink).right();
        line(painter, &said, style, area.right() - 12.0 - width, width, baseline);
    }
    let room = Area { width: area.width - 24.0 - width, ..area };
    if field.text.is_empty() {
        let prompt = TextStyle::new(metrics.path_size, theme.palette.dim);
        line(
            painter,
            i18n::lookup("look.find_prompt"),
            prompt,
            area.x + 12.0,
            room.width,
            baseline,
        );
    }
    let style = TextStyle::new(metrics.path_size, theme.palette.text);
    field_text(painter, room, field.into(), style, 12.0, baseline);
    // The bar is a panel as far as the pointer is concerned: a press in it must
    // not reach the list behind the look.
    hits.panels.push(area);
}

/// How tall the page's find bar is.
const PAGE_FIND_HEIGHT: f32 = 34.0;

/// Which place on the page a point in the panel lands on.
///
/// **The inverse of the transform that drew it**, in the same file for the reason
/// [`text_spot`] is: a hit test computed anywhere else is a second copy of the
/// page's geometry, and the two drift apart the first time a zoom is involved.
pub(crate) fn page_place(
    look: &crate::quicklook::QuickLook,
    inner: Area,
    at: (f32, f32),
) -> Option<crate::pagetext::Place> {
    let text = look.page_text.as_ref()?;
    let (_, size) = look.image?;
    let (across, down) = text.size();
    if across <= 0.0 || down <= 0.0 {
        return None;
    }
    let page = fitted_box(inner, size, look.coarse, (look.zoom, look.pan));
    if page.width <= 0.0 || page.height <= 0.0 {
        return None;
    }
    let point = ((at.0 - page.x) * across / page.width, (at.1 - page.y) * down / page.height);
    Some(text.place_at(point))
}

/// Whether a point in the panel is over a word of the page.
///
/// **What decides between selecting and panning.** A press on a word starts a
/// selection and a press on the paper moves the page — which needs no modifier
/// and is what a hand expects: you grab the margin to move paper and the text to
/// take a line out of it.
pub(crate) fn page_word_at(
    look: &crate::quicklook::QuickLook,
    inner: Area,
    at: (f32, f32),
) -> bool {
    let Some(text) = look.page_text.as_ref() else { return false };
    let Some((_, size)) = look.image else { return false };
    let (across, down) = text.size();
    let page = fitted_box(inner, size, look.coarse, (look.zoom, look.pan));
    if across <= 0.0 || down <= 0.0 || page.width <= 0.0 {
        return false;
    }
    let point = ((at.0 - page.x) * across / page.width, (at.1 - page.y) * down / page.height);
    text.covers(point)
}

/// The same style in the mono face.
fn mono(style: TextStyle) -> TextStyle {
    TextStyle { weight: sk_text::Weight::Mono, ..style }
}

/// The highlight behind one line's selected part, where it has one.
///
/// **A line inside the selection but not at either end is highlighted whole**,
/// including a strip past its last character, because a selection that stopped at
/// each line's ragged right edge would not read as one region.
fn text_selection(
    painter: &mut Painter<'_>,
    look: &crate::quicklook::QuickLook,
    line: (usize, &str),
    where_: (TextLayout, f32, Area),
) {
    let (index, body) = line;
    let (placed, top, inner) = where_;
    let Some(chosen) = look.chosen.filter(|it| !it.is_empty()) else { return };
    let Some(text) = look.text.as_ref() else { return };
    let Some(row) = text.rows().get(index).copied() else { return };
    let (from, to) = chosen.ordered();
    if row.line < from.line || row.line > to.line {
        return;
    }
    let theme = *painter.theme;
    let size = theme.metrics.text_size;
    // **Clamped to this row**, because the selection is in line-and-byte and the
    // row is a window onto that line: a paragraph selected whole highlights each
    // of its rows from its own start to its own end.
    let line_start = if row.line == from.line { from.byte } else { 0 };
    let line_end = if row.line == to.line { to.byte } else { body.len() };
    // A row the selection does not reach draws nothing. One it does draws even
    // where the intersection is empty — an empty line inside the region, whose
    // stub is what keeps the region from having a notch in it.
    if line_start > row.to || line_end < row.from {
        return;
    }
    let start = line_start.clamp(row.from, row.to);
    let end = line_end.clamp(row.from, row.to);
    let continues = row.line < to.line || row.to < line_end;
    if start >= end && !continues {
        return;
    }
    let (left, right) = {
        let Painter { fonts, scale, .. } = painter;
        let px = size * *scale;
        let weight = sk_text::Weight::Mono;
        // Measured across the row's own slice, since that is what is drawn at
        // `text_x`: an advance counted from the line's start puts the highlight
        // off the right edge of every row but the first.
        let drawn = body.get(row.from..row.to).unwrap_or_default();
        let (head, tail) = (start - row.from, end - row.from);
        (
            sk_ui::advance_to(fonts, drawn, weight, px, head) / scale.max(0.01),
            sk_ui::advance_to(fonts, drawn, weight, px, tail) / scale.max(0.01),
        )
    };
    // Past the end of a line that continues into the next one, so the region has
    // no notch in it where a short line was.
    let tail = if continues { size * 0.5 } else { 0.0 };
    let x = placed.text_x + left;
    let width = (right - left + tail).max(1.0).min(inner.right() - x);
    let shape = painter.rect(x, top, width, placed.step);
    painter.list.rect(shape, theme.palette.accent.with_alpha(80), 0.0);
}

/// The caret, while the text is being edited.
///
/// **Only while editing.** A preview is for reading and a bar blinking in it
/// would be a promise that typing does something — which it does not until
/// somebody says so. Not blinking either: an animation is a wake-up with an
/// owner, and a caret that blinks is a window that never sleeps
/// ([12-measurement.md](../../../docs/12-measurement.md)).
fn text_caret(
    painter: &mut Painter<'_>,
    look: &crate::quicklook::QuickLook,
    line: (usize, &str),
    where_: (TextLayout, f32, Area),
) {
    if !look.editing {
        return;
    }
    let (index, body) = line;
    let (placed, top, inner) = where_;
    let at = look.caret();
    let Some(text) = look.text.as_ref() else { return };
    if text.row_of(at) != index {
        return;
    }
    let theme = *painter.theme;
    let size = theme.metrics.text_size;
    let left = {
        let row = text.rows().get(index).copied().unwrap_or(crate::quicklook::Row {
            line: at.line,
            from: 0,
            to: body.len(),
        });
        let drawn = body.get(row.from..row.to).unwrap_or_default();
        let byte = at.byte.clamp(row.from, row.to) - row.from;
        let Painter { fonts, scale, .. } = painter;
        let px = size * *scale;
        sk_ui::advance_to(fonts, drawn, sk_text::Weight::Mono, px, byte) / scale.max(0.01)
    };
    let x = (placed.text_x + left).min(inner.right() - TEXT_PAD);
    let bar = painter.rect(x, top + 2.0, painter.logical(2.0), placed.step - 4.0);
    painter.list.rect(bar, theme.palette.accent, 0.0);
}

/// The Properties panel: a title, and a label-and-value row per fact.
///
/// Centred, because it is a statement rather than a thing to aim at — nothing in
/// it is clickable, and a panel anchored to a row would move when the row did.
fn properties(
    hits: &mut Hits,
    painter: &mut Painter<'_>,
    layout: &Layout,
    panel: &crate::properties::Properties,
    now: i64,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let rows = panel.rows(now);
    let title = panel.title();

    let mut widest = text_width(painter, &title, metrics.path_size, true);
    for row in &rows {
        let label = text_width(painter, &row.label, metrics.text_size, false);
        let value = text_width(painter, &row.value, metrics.text_size, false);
        widest = widest.max(label + PROPERTIES_GAP + value);
    }
    let width = (widest + PROPERTIES_INSET * 2.0)
        .clamp(PROPERTIES_WIDTH.0.min(layout.window.width), PROPERTIES_WIDTH.1);
    let height = PROPERTIES_TITLE + rows.len() as f32 * PROPERTIES_STEP + PROPERTIES_INSET;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: ((layout.window.height - height) / 2.0).max(8.0),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    let baseline = painter.baseline(area.y + 4.0, PROPERTIES_TITLE, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(
        painter,
        &title,
        style,
        area.x + PROPERTIES_INSET,
        area.width - PROPERTIES_INSET * 2.0,
        baseline,
    );
    painter.hairline(
        area.x + PROPERTIES_INSET,
        area.y + PROPERTIES_TITLE - 1.0,
        area.width - PROPERTIES_INSET * 2.0,
        theme.palette.divider,
    );

    for (index, row) in rows.iter().enumerate() {
        let top = area.y + PROPERTIES_TITLE + index as f32 * PROPERTIES_STEP;
        let baseline = painter.baseline(top, PROPERTIES_STEP, metrics.text_size);
        let label = TextStyle::new(metrics.text_size, theme.palette.dim);
        let column = (area.width - PROPERTIES_INSET * 2.0) * 0.38;
        line(painter, &row.label, label, area.x + PROPERTIES_INSET, column, baseline);
        let value = TextStyle::new(metrics.text_size, theme.palette.text);
        line(
            painter,
            &row.value,
            value,
            area.x + PROPERTIES_INSET + column + PROPERTIES_GAP,
            area.width - PROPERTIES_INSET * 2.0 - column - PROPERTIES_GAP,
            baseline,
        );
    }
}

/// Open With: the applications this machine says can open the file.
///
/// Centred like the Properties panel and for the same reason — it is about the
/// file rather than about a place on screen — and with the type it resolved to
/// under the title, because "why are these the choices" is the question a person
/// asks when the list surprises them.
fn open_with(
    painter: &mut Painter<'_>,
    layout: &Layout,
    list: &crate::openers::Openers,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let (first, drawn) = list.window();
    let title = crate::panes::title_of(&list.path).to_owned();

    let mut widest = text_width(painter, &title, metrics.path_size, true);
    widest = widest.max(text_width(painter, &list.mime, metrics.header_size, false));
    for entry in drawn {
        widest = widest.max(text_width(painter, &entry.name, metrics.text_size, false));
    }
    let width = (widest + PROPERTIES_INSET * 2.0)
        .clamp(PROPERTIES_WIDTH.0.min(layout.window.width), PROPERTIES_WIDTH.1);
    let height = OPENERS_TITLE + drawn.len() as f32 * OPENERS_STEP + PROPERTIES_INSET;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: ((layout.window.height - height) / 2.0).max(8.0),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    let baseline = painter.baseline(area.y + 4.0, PROPERTIES_TITLE - 12.0, metrics.path_size);
    let inner = area.width - PROPERTIES_INSET * 2.0;
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(painter, &title, style, area.x + PROPERTIES_INSET, inner, baseline);
    let under = painter.baseline(area.y + PROPERTIES_TITLE - 14.0, 16.0, metrics.header_size);
    let subtitle = TextStyle::new(metrics.header_size, theme.palette.dim);
    line(painter, &list.mime, subtitle, area.x + PROPERTIES_INSET, inner, under);
    painter.hairline(
        area.x + PROPERTIES_INSET,
        area.y + OPENERS_TITLE - 1.0,
        inner,
        theme.palette.divider,
    );

    hits.openers.clear();
    for (offset, entry) in drawn.iter().enumerate() {
        let top = area.y + OPENERS_TITLE + offset as f32 * OPENERS_STEP;
        hits.openers.push(Area { x: area.x, y: top, width: area.width, height: OPENERS_STEP });
        let highlighted = list.active == first + offset;
        if highlighted {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, OPENERS_STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let colour = if highlighted { theme.palette.selected_text } else { theme.palette.text };
        let baseline = painter.baseline(top, OPENERS_STEP, metrics.text_size);
        line(
            painter,
            &entry.name,
            TextStyle::new(metrics.text_size, colour),
            area.x + PROPERTIES_INSET,
            inner,
            baseline,
        );
    }
}

/// The name-taken question: which two files, and the four answers.
///
/// **Both files described, not just named.** "Replace or keep both" cannot be
/// answered about two identical strings. It can be answered about 4 MB from this
/// morning against 12 MB from last year, which is what the two columns are for
/// and why the facts are gathered on the job that found the clash.
/// The question a permanent deletion asks, and the only one in the program that
/// cannot be taken back by `Ctrl+Z`.
///
/// **Shaped like the name-taken question, because it is the same kind of thing:**
/// a decision with the facts in front of it and two real targets. It used to be a
/// sentence in the status strip, in the dim grey a row count is drawn in, armed
/// for exactly as long as that sentence happened to stay up.
///
/// **One left edge and no list of one.** The first version indented the names
/// eight pixels past the title and the warning, and then listed the single file
/// the title had just named — a panel with three left edges and a line that said
/// nothing. It is as wide as its widest line now, within bounds, rather than a
/// fixed 420 that left a short filename swimming.
fn erase_panel(
    painter: &mut Painter<'_>,
    layout: &Layout,
    names: &[String],
    total: usize,
    on_delete: bool,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let title = if total == 1 {
        i18n::format("erase.title_one", &[&names.first().cloned().unwrap_or_default()])
    } else {
        i18n::format("erase.title_many", &[&total])
    };
    let warning = i18n::lookup("erase.forever");
    // A single file is named by the title and listing it again says nothing.
    let listed: &[String] = if total == 1 { &[] } else { names };
    // Only where something *is* left out. With one file the title has named it
    // and `listed` is empty on purpose, which the arithmetic alone read as one
    // name unlisted — a panel saying "and 1 more" about the file in its title.
    let more = (!listed.is_empty() && total > listed.len())
        .then(|| i18n::format("erase.and_more", &[&(total - listed.len())]));

    let mut widest = text_width(painter, &title, metrics.path_size, false).max(text_width(
        painter,
        warning,
        metrics.text_size,
        false,
    ));
    for line_text in listed.iter().chain(more.iter()) {
        widest = widest.max(text_width(painter, line_text, metrics.text_size, false));
    }
    let buttons = ERASE_BUTTON.0 * 2.0 + ERASE_GAP;
    let width = (widest.max(buttons) + ERASE_PAD * 2.0)
        .clamp(ERASE_WIDTH.0, ERASE_WIDTH.1.min(layout.window.width - 16.0));

    let body = (listed.len() + usize::from(more.is_some())) as f32 * ERASE_LINE;
    // Half a pad between the list and the warning, and none where there is no
    // list: the colour says the warning is different, and the gap agrees without
    // opening a hole in a panel that has nothing above it.
    let apart = if listed.is_empty() { 0.0 } else { ERASE_GAP };
    let height = ERASE_PAD
        + ERASE_TITLE
        + body
        + apart
        + ERASE_LINE
        + ERASE_PAD
        + ERASE_BUTTON.1
        + ERASE_PAD;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: ((layout.window.height - height) / 2.0).max(8.0),
        width,
        height,
    };
    panel_shape(painter, area, hits);
    // One left edge for everything, and one right edge for the buttons.
    let left = area.x + ERASE_PAD;
    let inner = area.width - ERASE_PAD * 2.0;

    let mut top = area.y + ERASE_PAD;
    let baseline = painter.baseline(top, ERASE_TITLE, metrics.path_size);
    line(
        painter,
        &title,
        TextStyle::new(metrics.path_size, theme.palette.text),
        left,
        inner,
        baseline,
    );
    top += ERASE_TITLE;

    let dim = TextStyle::new(metrics.text_size, theme.palette.dim);
    for line_text in listed.iter().chain(more.iter()) {
        let baseline = painter.baseline(top, ERASE_LINE, metrics.text_size);
        line(painter, line_text, dim, left, inner, baseline);
        top += ERASE_LINE;
    }

    // **In the danger colour, and stated rather than implied.** Everything else
    // this program does to a file is on the undo stack; this is the sentence
    // that says this one is not.
    top += apart;
    let baseline = painter.baseline(top, ERASE_LINE, metrics.text_size);
    line(
        painter,
        warning,
        TextStyle::new(metrics.text_size, theme.palette.danger),
        left,
        inner,
        baseline,
    );
    top += ERASE_LINE + ERASE_PAD;

    let (w, h) = ERASE_BUTTON;
    let delete = Area { x: area.right() - ERASE_PAD - w, y: top, width: w, height: h };
    let cancel = Area { x: delete.x - w - ERASE_GAP, y: top, width: w, height: h };
    for (button, label, is_delete) in
        [(cancel, "erase.cancel", false), (delete, "erase.delete", true)]
    {
        let shape = painter.rect(button.x, button.y, button.width, button.height);
        let fill_colour = if is_delete { theme.palette.danger } else { theme.palette.chrome };
        painter.list.rect(shape, fill_colour, painter.px(metrics.radius));
        if is_delete == on_delete {
            let ring = painter.px(2.0).round().max(1.0);
            painter.list.stroke(shape, theme.palette.accent, painter.px(metrics.radius), ring);
        }
        let ink = if is_delete { theme.palette.selected_text } else { theme.palette.text };
        let baseline = painter.baseline(button.y, button.height, metrics.text_size);
        let label = i18n::lookup(label);
        let text = text_width(painter, label, metrics.text_size, false);
        let x = button.x + (button.width - text).max(0.0) / 2.0;
        line(painter, label, TextStyle::new(metrics.text_size, ink), x, button.width, baseline);
    }
    hits.erase_cancel = Some(cancel);
    hits.erase_delete = Some(delete);
}

fn clash_panel(
    painter: &mut Painter<'_>,
    layout: &Layout,
    clash: &crate::edit::Clash,
    on: usize,
    for_the_rest: bool,
    now: i64,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let name = crate::panes::title_of(&clash.existing).to_owned();
    let title = i18n::format("clash.title", &[&name]);
    let where_ = i18n::format(
        if clash.moving { "clash.into_move" } else { "clash.into_copy" },
        &[&clash.into.display()],
    );

    let offered = clash.answers();
    let rows = offered.len() as f32;
    let asks_the_rest = clash.rest > 0;
    let width = CLASH_WIDTH.min(layout.window.width - 16.0).max(240.0);
    let height = CLASH_TITLE
        + CLASH_FACTS
        + rows * CLASH_STEP
        + if asks_the_rest { CLASH_STEP } else { 0.0 }
        + PROPERTIES_INSET;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: ((layout.window.height - height) / 2.0).max(8.0),
        width,
        height,
    };
    panel_shape(painter, area, hits);
    let inner = area.width - PROPERTIES_INSET * 2.0;
    let left = area.x + PROPERTIES_INSET;

    let baseline = painter.baseline(area.y + 4.0, PROPERTIES_TITLE - 12.0, metrics.path_size);
    line(
        painter,
        &title,
        TextStyle::new(metrics.path_size, theme.palette.text).bold(),
        left,
        inner,
        baseline,
    );
    let under = painter.baseline(area.y + PROPERTIES_TITLE - 14.0, 16.0, metrics.header_size);
    let subtitle = TextStyle::new(metrics.header_size, theme.palette.dim);
    line(painter, &where_, subtitle, left, inner, under);
    painter.hairline(left, area.y + CLASH_TITLE - 1.0, inner, theme.palette.divider);

    clash_files(painter, clash, left, area.y + CLASH_TITLE + 6.0, inner, now);

    hits.answers.clear();
    let first = area.y + CLASH_TITLE + CLASH_FACTS;
    for (index, answer) in offered.iter().copied().enumerate() {
        let top = first + index as f32 * CLASH_STEP;
        hits.answers.push(Area { x: area.x, y: top, width: area.width, height: CLASH_STEP });
        if index == on {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, CLASH_STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let colour = if index == on { theme.palette.selected_text } else { theme.palette.text };
        let baseline = painter.baseline(top, CLASH_STEP, metrics.text_size);
        line(
            painter,
            i18n::lookup(answer.key()),
            TextStyle::new(metrics.text_size, colour),
            left,
            inner,
            baseline,
        );
    }

    hits.for_the_rest = asks_the_rest.then(|| {
        let top = first + rows * CLASH_STEP;
        let box_ = painter.rect(left, top + 6.0, 14.0, 14.0);
        painter.checkbox(box_, theme.palette.accent, theme.palette.dim, for_the_rest);
        let label = i18n::format("clash.for_the_rest", &[&clash.rest]);
        let baseline = painter.baseline(top, CLASH_STEP, metrics.text_size);
        line(painter, &label, subtitle, left + 22.0, inner - 22.0, baseline);
        Area { x: area.x, y: top, width: area.width, height: CLASH_STEP }
    });
}

/// The two files, side by side, so the difference is where the eye already is
/// rather than two paragraphs apart.
fn clash_files(
    painter: &mut Painter<'_>,
    clash: &crate::edit::Clash,
    left: f32,
    top: f32,
    inner: f32,
    now: i64,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let subtitle = TextStyle::new(metrics.header_size, theme.palette.dim);
    let column = (inner - PROPERTIES_GAP) / 2.0;
    for (at, (key, facts)) in
        [("clash.there", &clash.theirs), ("clash.arriving", &clash.ours)].into_iter().enumerate()
    {
        let x = left + at as f32 * (column + PROPERTIES_GAP);
        let head = painter.baseline(top, 16.0, metrics.header_size);
        line(painter, i18n::lookup(key), subtitle, x, column, head);
        let detail = facts.as_ref().map_or_else(
            || i18n::lookup("clash.unknown").to_owned(),
            |it| {
                format!(
                    "{}\u{2002}·\u{2002}{}",
                    format::size(it.size),
                    format::modified(it.modified, now)
                )
            },
        );
        let body = painter.baseline(top + 18.0, metrics.row_height, metrics.text_size);
        line(
            painter,
            &detail,
            TextStyle::new(metrics.text_size, theme.palette.text),
            x,
            column,
            body,
        );
    }
}

/// The question panel: its title strip, its two-column comparison, one answer
/// row, and how wide it is before the window narrows it.
/// How many of the doomed names the question lists before it stops.
///
/// Six, and then "and N more". A question a person has to scroll is a question
/// they stop reading, and the count in the title is the number that matters.
pub(crate) const ERASE_NAMES: usize = 6;
/// The narrowest and widest the question is drawn, before it takes the width of
/// its own longest line.
const ERASE_WIDTH: (f32, f32) = (300.0, 520.0);
const ERASE_BUTTON: (f32, f32) = (92.0, 30.0);
/// One inset, used on all four sides and between the two buttons, so the panel
/// has one left edge and one rhythm rather than three of each.
const ERASE_PAD: f32 = 18.0;
const ERASE_GAP: f32 = 10.0;
const ERASE_TITLE: f32 = 34.0;
const ERASE_LINE: f32 = 24.0;

const CLASH_TITLE: f32 = 54.0;
const CLASH_FACTS: f32 = 52.0;
const CLASH_STEP: f32 = 26.0;
const CLASH_WIDTH: f32 = 440.0;

/// How much smaller a tab's shape is than the space it is given, at each edge.
const TAB_INSET: f32 = 3.0;

/// The title and subtitle together, and one application row.
const OPENERS_TITLE: f32 = 54.0;
const OPENERS_STEP: f32 = 28.0;

/// The title strip's height, one row, the inset, and the least space between a
/// label and its value.
const PROPERTIES_TITLE: f32 = 38.0;
const PROPERTIES_STEP: f32 = 24.0;
const PROPERTIES_INSET: f32 = 16.0;
const PROPERTIES_GAP: f32 = 24.0;
/// How wide the panel may be. A path is the longest value in it and a narrow
/// panel would ellipsize exactly the one thing somebody opened it to read.
const PROPERTIES_WIDTH: (f32, f32) = (360.0, 640.0);

fn command_palette(painter: &mut Painter<'_>, layout: &Layout, palette: &Palette, hits: &mut Hits) {
    use crate::palette::{FIELD, INSET, SPACING, STEP, TOP, VISIBLE, WIDTH};

    let theme = *painter.theme;
    let metrics = theme.metrics;
    let (first, last) = palette.window();
    let width = (layout.window.width - 80.0).clamp(WIDTH.0.min(layout.window.width), WIDTH.1);
    let height = crate::palette::height(last - first);
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: TOP.min((layout.window.height - height).max(8.0)),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    let field_area = Area { x: area.x, y: area.y, width: area.width, height: FIELD };
    hits.palette_field = Some(field_area);
    let baseline = painter.baseline(area.y, FIELD, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text);
    if palette.query.text.is_empty() {
        // Behind the caret rather than instead of it: an empty field with no
        // caret looks like a field that is not taking input.
        let prompt = TextStyle::new(metrics.path_size, theme.palette.dim);
        let room = area.width - INSET * 2.0;
        line(painter, i18n::lookup("palette.prompt"), prompt, area.x + INSET, room, baseline);
    }
    field_text(painter, field_area, (&palette.query).into(), style, INSET, baseline);
    painter.hairline(area.x, area.y + FIELD, area.width, theme.palette.divider);

    hits.palette.clear();
    let tops: Vec<f32> = crate::palette::row_tops(last - first, area).collect();
    for (offset, top) in tops.into_iter().enumerate() {
        let Some(row) = palette.row(first + offset) else { continue };
        hits.palette.push(Area { x: area.x, y: top, width: area.width, height: STEP });
        let highlighted = palette.active == first + offset;
        if highlighted {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let (name_colour, keys_colour) = match (row.enabled, highlighted) {
            (false, _) => (theme.palette.dim.with_alpha(110), theme.palette.dim.with_alpha(80)),
            (true, true) => (theme.palette.selected_text, theme.palette.selected_text),
            (true, false) => (theme.palette.text, theme.palette.dim),
        };
        let baseline = painter.baseline(top, STEP, metrics.text_size);
        let inner = area.width - INSET * 2.0;
        let keys_width = text_width(painter, &row.keys, metrics.header_size, true);
        let name_style = TextStyle::new(metrics.text_size, name_colour);
        let x = area.x + INSET;
        line(painter, &row.name, name_style, x, inner - keys_width - SPACING * 0.5, baseline);
        let keys_style = TextStyle::new(metrics.header_size, keys_colour).right();
        line(painter, &row.keys, keys_style, x, inner, baseline);
    }
    // How much of the list is not on screen, because a palette that silently
    // shows eleven of fifty is a palette that looks like it has eleven commands.
    let hidden = palette.shown.len().saturating_sub(last - first);
    if hidden > 0 {
        let note = format!("+{hidden}");
        let style = TextStyle::new(metrics.header_size, theme.palette.dim).right();
        let baseline = painter.baseline(area.y, FIELD, metrics.header_size);
        line(painter, &note, style, area.x + INSET, area.width - INSET * 2.0, baseline);
    }
    let _ = VISIBLE;
}

/// The subtree search: a field, a summary, and results in other folders.
///
/// **Each row is a name and the folder it is in**, which is the difference
/// between this and the find popup: a result out of a hundred thousand entries
/// is not identifiable by its name alone, and *which* `Makefile` is the whole
/// question. The folder is shown relative to where the search started, so a deep
/// tree reads as `a/b/c` rather than as the same absolute prefix on every line.
fn search_panel(
    painter: &mut Painter<'_>,
    layout: &Layout,
    search: &crate::search::Search,
    hits: &mut Hits,
) {
    use crate::palette::{FIELD, INSET, STEP, TOP, WIDTH};

    let theme = *painter.theme;
    let metrics = theme.metrics;
    let (first, drawn) = search.window();
    let width = (layout.window.width - 80.0).clamp(WIDTH.0.min(layout.window.width), WIDTH.1);
    let rows = drawn.len().max(1) as f32;
    let height = FIELD + SEARCH_SUMMARY + rows * STEP + 6.0;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: TOP.min((layout.window.height - height).max(8.0)),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    // **The switcher first, because the field is what is left of the row.** Two
    // buttons with the one in force filled, which is the shape the pane bar's view
    // switcher already set — a mode reached only by a chord is a mode most people
    // never find, and that is [rule 4](../../../docs/03-principles.md).
    let taken = scope_switch(painter, area, search, hits);
    let field_area =
        Area { x: area.x, y: area.y, width: (area.width - taken).max(0.0), height: FIELD };
    hits.search_field = Some(field_area);
    let baseline = painter.baseline(area.y, FIELD, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text);
    if search.query.text.is_empty() {
        let prompt = TextStyle::new(metrics.path_size, theme.palette.dim);
        let label = match search.scope.under() {
            Some(path) => i18n::format(search.scope.prompt(), &[&crate::panes::title_of(path)]),
            None => i18n::lookup(search.scope.prompt()).to_owned(),
        };
        line(painter, &label, prompt, area.x + INSET, area.width - INSET * 2.0, baseline);
    }
    field_text(painter, field_area, (&search.query).into(), style, INSET, baseline);
    painter.hairline(area.x, area.y + FIELD, area.width, theme.palette.divider);

    let dim = TextStyle::new(metrics.header_size, theme.palette.dim);
    let head = painter.baseline(area.y + FIELD, SEARCH_SUMMARY, metrics.header_size);
    let mut inner = area.width - INSET * 2.0;
    // **The pointer's way to the same command the keyboard reaches with
    // `Ctrl+Enter`.** At the right end of the line that already says how many
    // results there are, because that is the sentence it answers: "all 47 of
    // them, here". Only where there is something to show — a label offering to
    // do nothing is worse than no label.
    hits.search_all = None;
    if !search.found.hits.is_empty() {
        let label = i18n::lookup("action.search_results");
        let width = text_width(painter, label, metrics.header_size, false).min(inner / 2.0);
        let button = Area {
            x: area.right() - INSET - width,
            y: area.y + FIELD,
            width,
            height: SEARCH_SUMMARY,
        };
        let accent = TextStyle::new(metrics.header_size, theme.palette.accent);
        line(painter, label, accent, button.x, width, head);
        hits.search_all = Some(button);
        inner -= width + INSET;
    }
    line(painter, &search.summary(), dim, area.x + INSET, inner, head);

    hits.search.clear();
    if drawn.is_empty() {
        if !search.asked.is_empty() && search.found.complete {
            let top = area.y + FIELD + SEARCH_SUMMARY;
            let baseline = painter.baseline(top, STEP, metrics.text_size);
            let style = TextStyle::new(metrics.text_size, theme.palette.dim);
            line(painter, i18n::lookup("search.none"), style, area.x + INSET, inner, baseline);
        }
        return;
    }
    for (offset, hit) in drawn.iter().enumerate() {
        let top = area.y + FIELD + SEARCH_SUMMARY + offset as f32 * STEP;
        hits.search.push(Area { x: area.x, y: top, width: area.width, height: STEP });
        let highlighted = search.active == first + offset;
        if highlighted {
            let pill = painter.rect(area.x + 4.0, top, area.width - 8.0, STEP);
            painter.list.rect(pill, theme.palette.accent, painter.px(5.0));
        }
        let (name_colour, where_colour) = if highlighted {
            (theme.palette.selected_text, theme.palette.selected_text)
        } else {
            (theme.palette.text, theme.palette.dim)
        };
        let baseline = painter.baseline(top, STEP, metrics.text_size);
        let name = crate::panes::title_of(&hit.path).to_owned();
        // Relative to where the search started, and absolute where it started
        // everywhere: a result three disks away has no useful prefix to strip.
        let where_ = hit
            .path
            .parent()
            .map(|parent| match search.scope.under() {
                Some(under) => parent.strip_prefix(under).map_or_else(
                    |_| parent.display().to_string(),
                    |relative| relative.display().to_string(),
                ),
                None => parent.display().to_string(),
            })
            .filter(|relative| !relative.is_empty())
            .unwrap_or_else(|| ".".to_owned());
        let where_width = text_width(painter, &where_, metrics.header_size, false);
        let x = area.x + INSET;
        let name_style = TextStyle::new(metrics.text_size, name_colour);
        line(painter, &name, name_style, x, inner - where_width - 16.0, baseline);
        let where_style = TextStyle::new(metrics.header_size, where_colour).right();
        line(painter, &where_, where_style, x, inner, baseline);
    }
}

/// The scope switcher at the right of the search's field. Returns its width.
fn scope_switch(
    painter: &mut Painter<'_>,
    area: Area,
    search: &crate::search::Search,
    hits: &mut Hits,
) -> f32 {
    use crate::palette::{FIELD, INSET};

    let theme = *painter.theme;
    let metrics = theme.metrics;
    hits.search_scope.clear();
    let height = 20.0;
    let top = area.y + (FIELD - height) / 2.0;
    let mut right = area.right() - INSET;
    // Right to left, so the pair reads left to right: here, then everywhere.
    for (everywhere, key) in [(true, "search.everywhere"), (false, "search.here")] {
        let label = i18n::lookup(key);
        let width = text_width(painter, label, metrics.header_size, false) + 16.0;
        let button = Area { x: right - width, y: top, width, height };
        let chosen = search.scope.is_everywhere() == everywhere;
        let ink = if chosen {
            painter.list.rect(
                painter.rect(button.x, button.y, button.width, button.height),
                theme.palette.accent,
                painter.px(metrics.radius),
            );
            theme.palette.selected_text
        } else {
            theme.palette.dim
        };
        let style = TextStyle::new(metrics.header_size, ink).centred();
        let baseline = painter.baseline(button.y, height, metrics.header_size);
        line(painter, label, style, button.x, button.width, baseline);
        hits.search_scope.push((everywhere, button));
        right = button.x - 4.0;
    }
    (area.right() - right).max(0.0)
}

/// The summary line between the field and the results.
const SEARCH_SUMMARY: f32 = 22.0;

/// What a drag of files is carrying, for the card under the pointer.
pub struct Carrying {
    /// Where the pointer is, in logical pixels.
    pub at: Point,
    /// The first file's name.
    pub name: String,
    /// How many there are.
    pub count: usize,
    /// Whether the first one is a folder, so the card draws the right icon.
    pub directory: bool,
}

/// The card that follows the pointer while files are being carried.
///
/// **Drawn here rather than handed to the display server.** Wayland's drag icon
/// is a second `wl_surface` with its own buffer and its own scale to keep right,
/// and X11's is a cursor; neither can show a name, and the two would look
/// different. This is one card, in the real renderer, at the real scale, with
/// the real font — and the case it is for is a drop into the other half of a
/// split, which never leaves this window.
///
/// Offset down and to the right of the hotspot, the way every drag image is:
/// under the pointer it would cover the row the drop is about to land on.
fn drag_card(painter: &mut Painter<'_>, layout: &Layout, carrying: &Carrying) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let badge = (carrying.count > 1).then(|| format!("+{}", carrying.count - 1));
    let badge_width =
        badge.as_ref().map_or(0.0, |it| text_width(painter, it, metrics.header_size, true) + 10.0);
    let name_width = text_width(painter, &carrying.name, metrics.text_size, false);
    let width = (CARD_ICON + CARD_INSET * 3.0 + name_width + badge_width).clamp(64.0, CARD_WIDEST);
    let area = Area {
        // Clamped inside the window: a card that runs off the right edge is a
        // card with the name cut off at exactly the moment it is being read.
        x: (carrying.at.x + CARD_OFFSET).min(layout.window.width - width - 4.0).max(4.0),
        y: (carrying.at.y + CARD_OFFSET).min(layout.window.height - CARD_HEIGHT - 4.0).max(4.0),
        width,
        height: CARD_HEIGHT,
    };
    // The panel's own shadow and fill, so a card over the list reads as being
    // above it rather than painted on it — and an accent ring, because it is
    // attached to the pointer and everything else with one is a target.
    floating_shape(painter, area);
    let shape = painter.rect(area.x, area.y, area.width, area.height);
    let ring = painter.px(1.0).round().max(1.0);
    painter.list.stroke(shape, theme.palette.accent, painter.px(10.0), ring);

    let icon = painter.rect(
        area.x + CARD_INSET,
        area.y + (CARD_HEIGHT - CARD_ICON) / 2.0,
        CARD_ICON,
        CARD_ICON,
    );
    let colour =
        if carrying.directory { theme.palette.kinds.folder } else { theme.palette.kinds.unknown };
    if carrying.directory {
        painter.folder(icon, colour);
    } else {
        painter.file(icon, colour);
    }

    let baseline = painter.baseline(area.y, CARD_HEIGHT, metrics.text_size);
    let x = area.x + CARD_INSET * 2.0 + CARD_ICON;
    let room = area.width - (x - area.x) - CARD_INSET - badge_width;
    let style = TextStyle::new(metrics.text_size, theme.palette.text);
    line(painter, &carrying.name, style, x, room, baseline);
    if let Some(badge) = badge {
        let style = TextStyle::new(metrics.header_size, theme.palette.dim).right();
        line(painter, &badge, style, x, area.width - (x - area.x) - CARD_INSET, baseline);
    }
}

/// The card: how far from the hotspot, how tall, its icon, its padding, and the
/// widest it goes before a long name is elided.
const CARD_OFFSET: f32 = 14.0;
const CARD_HEIGHT: f32 = 28.0;
const CARD_ICON: f32 = 16.0;
const CARD_INSET: f32 = 8.0;
const CARD_WIDEST: f32 = 320.0;

/// The filter, as a panel over the top-right of the list.
///
/// **A strip at the bottom of the window is the wrong place for a field.** It was
/// in the status line, sixteen pixels tall, at the far edge from where the eye is
/// — so a person typing could not see what they had typed.
///
/// **Bottom-right of the list, not top-right.** An editor puts this widget at the
/// top because the interesting line is wherever the caret is; a filtered list's
/// interesting rows are always the first few, and a panel there covers the one
/// thing the filter was typed to find. The bottom is empty exactly when the filter
/// has done its job.
///
/// The count comes with it, because "no matches" and "one match" are the two
/// answers a filter is asked for and neither is visible in the list itself.
fn find_popup(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    focused: bool,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let count = i18n::format(
        "find.count",
        &[&format::grouped(pane.listing.rows()), &format::grouped(pane.listing.len())],
    );
    let count_width = text_width(painter, &count, metrics.header_size, false);
    let typed = text_width(painter, &pane.filter, metrics.path_size, false);
    let room = (layout.body.width - FIND_MARGIN * 2.0).max(FIND_WIDTH.0);
    let width = (typed + count_width + FIND_INSET * 3.0)
        .clamp(FIND_WIDTH.0.min(room), FIND_WIDTH.1.min(room));
    let area = Area {
        x: layout.body.x + layout.body.width - width - FIND_MARGIN,
        y: (layout.body.y + layout.body.height - FIND_HEIGHT - FIND_MARGIN).max(layout.body.y),
        width,
        height: FIND_HEIGHT,
    };
    panel_shape(painter, area, hits);
    // An accent outline rather than a tinted strip somewhere else: the border is
    // on the thing that has the keyboard, which is what makes it a mode marker
    // rather than a decoration. Dim once the keyboard has gone elsewhere and the
    // filter is only still narrowing the list — the panel stays, because a list
    // showing 12 of 4,000 rows with nothing on screen saying so is a list that
    // looks broken.
    let border = if focused { theme.palette.accent } else { theme.palette.divider };
    let outline = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.stroke(outline, border, painter.px(metrics.radius), painter.snap(1.0).max(1.0));

    let baseline = painter.baseline(area.y, area.height, metrics.path_size);
    let counted = TextStyle::new(metrics.header_size, theme.palette.dim).right();
    line(painter, &count, counted, area.x + FIND_INSET, area.width - FIND_INSET * 2.0, baseline);

    // The field stops short of the count, so a long filter scrolls under its own
    // caret instead of running into the number.
    let field = Area { width: area.width - count_width - FIND_INSET, ..area };
    if pane.filter.is_empty() {
        let prompt = TextStyle::new(metrics.path_size, theme.palette.dim);
        let room = field.width - FIND_INSET * 2.0;
        line(painter, i18n::lookup("find.prompt"), prompt, field.x + FIND_INSET, room, baseline);
    }
    let text = if focused { theme.palette.text } else { theme.palette.dim };
    let style = TextStyle::new(metrics.path_size, text);
    let view = if focused {
        FieldView::plain(&pane.filter, pane.filter.len())
    } else {
        FieldView::unfocused(&pane.filter)
    };
    field_text(painter, field, view, style, FIND_INSET, baseline);
}

/// The narrowest and widest the find popup may be.
const FIND_WIDTH: (f32, f32) = (240.0, 460.0);
/// How tall it is: one line of text with room around it.
const FIND_HEIGHT: f32 = 38.0;
/// The text inset at each side.
const FIND_INSET: f32 = 12.0;
/// How far it sits from the list's bottom and right edges.
const FIND_MARGIN: f32 = 10.0;

/// The name being typed, drawn over the row it belongs to.
fn name_field_at(
    painter: &mut Painter<'_>,
    area: Area,
    naming: &Naming,
    baseline: f32,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    // **The box a press is tested against is this one.** See `Hits::name_field`.
    // The caller decides it, because the three views put a row in three different
    // places — and this is the only place it is written down.
    hits.name_field = Some(area);
    let box_rect = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.rect(box_rect, theme.palette.background, painter.px(metrics.radius));
    painter.list.stroke(
        box_rect,
        theme.palette.accent,
        painter.px(metrics.radius),
        painter.snap(1.0).max(1.0),
    );

    let style = TextStyle::new(metrics.text_size, theme.palette.text);
    field_text(painter, area, (&naming.field).into(), style, NAME_INSET, baseline);
}

/// The name field's box inside a column view's column.
///
/// **Not [`name_field`]'s.** That box spans the details list's name column and
/// half of its size column, and a column view has neither — so the field belongs
/// inside the column the row was drawn in, which is the rectangle it is given.
fn name_field_in(cell: Area, metrics: &sk_ui::Metrics) -> Area {
    Area {
        x: cell.x + metrics.gutter * 0.5,
        y: cell.y + 2.0,
        width: (cell.width - metrics.gutter).max(1.0),
        height: metrics.row_height - 4.0,
    }
}

/// The text and the caret inside a field's box.
///
/// One implementation for the two fields, because the caret's position and the
/// text's origin are the same number offset by the same scroll — and the two of
/// them living in two functions is how a caret ends up two characters from the
/// character it is between.
/// What a field looks like to the draw: its text, its caret, its selection.
///
/// A view rather than a [`Field`] because the filter is not one — it is a `String`
/// that only ever grows and shrinks at the end — and a struct rather than three
/// more parameters because eight is one more than a function should take.
#[derive(Clone, Copy)]
struct FieldView<'a> {
    text: &'a str,
    /// `None` when the field does not have the keyboard, and draws no caret.
    caret: Option<usize>,
    selection: Option<(usize, usize)>,
    /// What an input method is still composing, and where its caret is inside
    /// it. Drawn at the field's caret and underlined, because it is not text
    /// yet — the method may replace all of it with one character.
    preedit: Option<&'a (String, Option<(usize, usize)>)>,
}

impl<'a> From<&'a Field> for FieldView<'a> {
    fn from(field: &'a Field) -> Self {
        Self {
            text: &field.text,
            caret: Some(field.caret),
            selection: field.selection(),
            preedit: field.preedit.as_ref(),
        }
    }
}

impl<'a> FieldView<'a> {
    /// A field with no selection, for the filter.
    const fn plain(text: &'a str, caret: usize) -> Self {
        Self { text, caret: Some(caret), selection: None, preedit: None }
    }

    /// A field the keyboard has left, still showing what was typed into it.
    const fn unfocused(text: &'a str) -> Self {
        Self { text, caret: None, selection: None, preedit: None }
    }

    /// The string to draw, the caret's offset in it, and the preedit's span.
    ///
    /// A preedit is drawn *inside* the text at the caret rather than after it,
    /// because that is where the characters will land — a candidate shown at the
    /// end of the line is a candidate for a different sentence.
    fn shown(&self) -> (std::borrow::Cow<'a, str>, usize, Option<(usize, usize)>) {
        let caret = self.caret.unwrap_or(self.text.len());
        let Some((preedit, cursor)) = self.preedit else {
            return (std::borrow::Cow::Borrowed(self.text), caret, None);
        };
        let mut shown = String::with_capacity(self.text.len() + preedit.len());
        shown.push_str(self.text.get(..caret).unwrap_or(self.text));
        shown.push_str(preedit);
        shown.push_str(self.text.get(caret..).unwrap_or(""));
        // Inside the preedit where the method said, and at its end where it
        // said nothing — which is where a person typing expects to be.
        let inside = cursor.map_or(preedit.len(), |(begin, _)| begin);
        (std::borrow::Cow::Owned(shown), caret + inside, Some((caret, caret + preedit.len())))
    }
}

/// One line of text in a box, with a caret in it, clipped and scrolled.
///
/// One implementation for all four fields, because a second caret is a second
/// thing to get wrong — the first one was as tall as its box.
fn field_text(
    painter: &mut Painter<'_>,
    area: Area,
    field: FieldView<'_>,
    style: TextStyle,
    inset: f32,
    baseline: f32,
) {
    let (shown, caret, composing) = field.shown();
    let text: &str = &shown;
    let theme = *painter.theme;
    let room = area.width - inset * 2.0;
    // Shaping what is before the caret is the only measurement that survives a
    // ligature or a combining mark.
    let before = text_width(painter, text.get(..caret).unwrap_or(text), style.size, false);
    let scroll = field_scroll(before, room);
    let full = text_width(painter, text, style.size, false);

    // Clipped to the box, and drawn as wide as the text: `label` ellipsizes
    // whatever will not fit its own rectangle, and an ellipsis in a field being
    // typed into is a character that is not there.
    let box_rect = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.push_clip(box_rect);
    // Behind the text, and only as tall as the text: a selection block the height
    // of the box is the same defect the caret had.
    if let Some((start, end)) = field.selection {
        let from = text_width(painter, text.get(..start).unwrap_or(""), style.size, false);
        let to = text_width(painter, text.get(..end).unwrap_or(text), style.size, false);
        let block = painter.caret(area.x + inset - scroll + from, baseline, style.size);
        let width = painter.snap(to - from).max(1.0);
        let block = sk_gpu::Rect::new(block.x, block.y, width, block.height);
        painter.list.rect(block, theme.palette.accent.with_alpha(120), 0.0);
    }
    let inner = painter.rect(area.x + inset - scroll, 0.0, full.max(room) + scroll, 0.0);
    painter.label(text, style, inner, baseline);
    // A line under what is being composed: it is the convention every toolkit
    // uses for text that is not typed yet, and without it a preedit is
    // indistinguishable from the text it will replace.
    if let Some((start, end)) = composing {
        let from = text_width(painter, text.get(..start).unwrap_or(""), style.size, false);
        let to = text_width(painter, text.get(..end).unwrap_or(text), style.size, false);
        let x = painter.snap(area.x + inset - scroll + from);
        let width = painter.snap(to - from).max(1.0);
        let thickness = painter.px(1.0).round().max(1.0);
        // **`baseline` is already device pixels.** `snap` converts logical to
        // device, so snapping it again put the rule a quarter of the way down
        // the window at 125% and exactly under the text at 100% — which is the
        // trap CLAUDE.md names, and the reason this frame is rendered at 125%.
        let y = (baseline + painter.px(2.0)).round();
        let rule = sk_gpu::Rect::new(x, y, width, thickness);
        painter.list.rect(rule, theme.palette.accent, 0.0);
    }
    if field.caret.is_some() {
        let caret = painter.caret(area.x + inset - scroll + before, baseline, style.size);
        painter.list.rect(caret, theme.palette.accent, 0.0);
    }
    painter.list.pop_clip();
}

/// The text inset inside the name field, shared by the draw and the hit test.
pub const NAME_INSET: f32 = 6.0;

/// Where the name field goes for a row: the name column, at that row's height.
///
/// A free function because both the drawing and the hit test need it, and two
/// rectangles computed separately are two rectangles that agree until one of
/// them is edited.
#[must_use]
pub fn name_field(layout: &PaneLayout, metrics: &sk_ui::Metrics, top: f32) -> Area {
    Area {
        x: layout.name.x - 4.0,
        y: top + 2.0,
        // Half the size column past the name's own right edge, because a name
        // being typed is usually longer than the one it replaces. Taken from the
        // *drawn* size column rather than from a metric, so a size column
        // dragged narrow lends less and one dragged wide lends more.
        width: (layout.name.width + layout.size.width * 0.5)
            .min(layout.rows(crate::cells::Mode::Details).right() - layout.name.x + 4.0),
        height: metrics.row_height - 4.0,
    }
}

/// One colour per kind, and the palette is the only thing this decides.
///
/// What kind a file *is* comes from `sk_desktop::kind_of` — a hash lookup and,
/// for the few globs that are not `*.ext`, a short scan, which is what lets it
/// run once per visible row per frame. Grey until the databases arrive, which is
/// the first few milliseconds after start-up: a row that changes colour once is
/// better than a first frame that waits for `globs2`.
fn kind_colour(theme: &sk_ui::Theme, category: Category) -> Color {
    let kinds = theme.palette.kinds;
    match category {
        Category::Folder => kinds.folder,
        Category::Image => kinds.image,
        Category::Audio => kinds.audio,
        Category::Video => kinds.video,
        Category::Archive => kinds.archive,
        Category::Code => kinds.code,
        Category::Document => kinds.document,
        Category::Executable => kinds.executable,
        Category::Unknown => kinds.unknown,
    }
}

/// The list of places down the left.
///
/// Sections with a heading, rows with a mark and a label, one accent for where
/// you are. The order is the one Windows 11 and every Linux file manager use:
/// the directories you keep things in, the ones you bookmarked, then what is
/// mounted — because that is the order of how often they are clicked.
fn sidebar(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &Layout,
    chrome: &Chrome<'_>,
    hits: &mut Hits,
) {
    hits.places.clear();
    hits.twisties.clear();
    let area = layout.sidebar;
    if area.width <= 0.0 {
        return;
    }
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, area, theme.palette.sidebar);
    // The divider is on the sidebar's edge rather than the list's, so the list
    // owns its own left gutter and a row's hover shape does not touch it.
    //
    // Two pixels and the accent colour while a hand is on it, which is what the
    // column boundaries do: the line is the control, so it has to look like one
    // when it is about to be used.
    let (ink, thickness) = if chrome.aside_grabbed {
        (theme.palette.accent, 2.0)
    } else {
        (theme.palette.divider, 1.0)
    };
    let edge = painter.rect(area.right() - thickness, area.y, thickness, area.height);
    painter.list.rect(edge, ink, 0.0);

    let clip = painter.rect(area.x, area.y, area.width - 1.0, area.height);
    painter.list.push_clip(clip);
    let current = crate::sidebar::current(chrome.places, &pane.path);
    let heading = metrics.sidebar_heading + 12.0;
    let mut y = area.y + 6.0;
    // How many pinned rows have been drawn, and where the last of them ended:
    // the gap a reorder would drop into is counted in pinned rows, and "after
    // the last one" is a position no row can draw for itself.
    let mut pins = 0;
    let mut after_pins = area.y;
    for (index, row) in chrome.places.iter().enumerate() {
        if let Some(title) = row.heading {
            // A gap above every heading but the first, which is what makes three
            // lists rather than one list with words in it.
            if index > 0 {
                y += 8.0;
            }
            let baseline = painter.baseline(y, heading, metrics.sidebar_heading);
            let style = TextStyle::new(metrics.sidebar_heading, theme.palette.heading).bold();
            line(painter, i18n::lookup(title), style, area.x + 14.0, area.width - 20.0, baseline);
            y += heading;
        }
        if row.pinned {
            if chrome.place_drop == Some(pins) {
                insertion(painter, area, y);
            }
            pins += 1;
        }
        let box_ = Area { x: area.x, y, width: area.width - 1.0, height: metrics.sidebar_row };
        hits.places.push(box_);
        hits.twisties.push(twisty_box(box_, row.depth));
        place_row(painter, row, box_, current == Some(index), chrome.hovered_place == Some(index));
        y += metrics.sidebar_row;
        if row.pinned {
            after_pins = y;
        }
    }
    if pins > 0 && chrome.place_drop == Some(pins) {
        insertion(painter, area, after_pins);
    }
    painter.list.pop_clip();
}

/// Where the twisty of a row at `depth` is.
///
/// One indent step per level, and the same box whether or not there turns out to
/// be anything under the row: a directory that has to be read before its own
/// control can be drawn is a control that appears a frame after the row does.
fn twisty_box(row: Area, depth: usize) -> Area {
    Area { x: row.x + 4.0 + depth as f32 * INDENT, y: row.y, width: 16.0, height: row.height }
}

/// How far one level of the sidebar's tree is indented.
const INDENT: f32 = 13.0;

/// The line a dragged place would land on.
///
/// Between the rows rather than over one: a highlight on a row would say "into
/// this", which is what a drop on a folder means everywhere else in this window,
/// and the answer here is an order rather than a destination.
fn insertion(painter: &mut Painter<'_>, area: Area, y: f32) {
    let theme = *painter.theme;
    let line = painter.rect(area.x + 8.0, y - 1.0, area.width - 17.0, 2.0);
    painter.list.rect(line, theme.palette.accent, painter.px(1.0));
}

/// One row of the sidebar.
fn place_row(
    painter: &mut Painter<'_>,
    row: &crate::sidebar::Row,
    box_: Area,
    current: bool,
    hovered: bool,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let pill = painter.rect(box_.x + 6.0, box_.y + 1.0, box_.width - 12.0, box_.height - 2.0);
    if current {
        painter.list.rect(
            pill,
            theme.palette.accent.with_alpha(if theme.dark { 56 } else { 40 }),
            painter.px(metrics.radius),
        );
    } else if hovered {
        painter.list.rect(pill, theme.palette.hover, painter.px(metrics.radius));
    }

    // **The twisty is drawn, not implied.** A row that opens and gives no sign
    // that it does is a feature only somebody who was told about it can use,
    // which is rule 4 — the same argument the column boundaries won.
    let twisty = twisty_box(box_, row.depth);
    let ink = if current { theme.palette.selected_text } else { theme.palette.dim };
    // A triangle rather than a glyph, for the reason the reload button is one:
    // `▸` and `▾` are a font's decision and this machine's chain is missing
    // arrows already.
    let (width, height, at) =
        if row.open { (9.0, 5.0, Direction::Down) } else { (5.0, 9.0, Direction::Right) };
    painter.triangle(
        twisty.x + (twisty.width - width) / 2.0,
        twisty.y + (twisty.height - height) / 2.0,
        width,
        height,
        ink,
        at,
    );

    // **Logical pixels all the way to the draw call.** `Painter::rect` is where
    // the conversion happens, so a device rectangle's `right()` used as the next
    // shape's `x` is a position scaled twice — correct at 100% and, at 200%, a
    // label a hundred pixels from its own icon. Which is what the second golden
    // frame at 200% exists to catch.
    let mark_at = twisty.right() + 2.0;
    let mark = 15.0;
    let icon = painter.rect(mark_at, box_.y + (box_.height - mark) / 2.0, mark, mark);
    let kinds = theme.palette.kinds;
    match row.mark {
        Mark::Volume => painter.disk(icon, if row.remote { kinds.audio } else { kinds.unknown }),
        Mark::Place(_) => painter.folder(icon, kinds.folder),
    }

    let ink = if current { theme.palette.selected_text } else { theme.palette.text };
    // **A volume's label rides higher, because its gauge is underneath it.**
    // Three pixels of the row's twenty-six: the bar is two and the third is the
    // air above it. Every other row keeps the middle of its own height, so the
    // list's rhythm is unchanged and only the rows that have something to say
    // are two lines.
    let lift = if row.free.is_some() { GAUGE_SPACE } else { 0.0 };
    let baseline = painter.baseline(box_.y - lift, box_.height, metrics.text_size);
    let text_at = mark_at + mark + 7.0;
    // **The room left, at the right end of a volume's row.** It is the number a
    // person opens a file manager to find out, and the only place it fits is
    // where every other file manager puts it. Small and dim, because it is a
    // fact about the row rather than the row's name — and the name gives up the
    // space rather than overlapping it.
    let mut room = (box_.right() - 10.0 - text_at).max(0.0);
    if let Some((free, total)) = row.free {
        let text = i18n::format("sidebar.free", &[&format::size(free)]);
        let ink = if current { theme.palette.selected_text } else { theme.palette.dim };
        // **All of it or none of it.** `104 GiB fre…` is a number that has stopped
        // being one, so where the row is too narrow for both the figure goes and
        // the name keeps the width — a name can lose its tail and still be read.
        // A pixel of slack on every box: the measure and the draw agree to within
        // rounding at a fractional scale, and a box exactly as wide as the text
        // ellipsizes it — `104 GiB fre…`, which is what this looked like first.
        //
        // **Measured against the name, not against a floor.** The rule was 44
        // pixels for the label, which is a guess that fitted `Home` and elided
        // `Filesystem`; what a name needs is what it measures.
        let width = text_width(painter, &text, metrics.header_size, false) + 3.0;
        let name = text_width(painter, &row.label, metrics.text_size, false) + 3.0;
        if name + width <= room {
            let at = box_.right() - 10.0 - width;
            let style = TextStyle::new(metrics.header_size, ink).right();
            line(painter, &text, style, at, width, baseline);
            room = (room - width - 6.0).max(0.0);
        }
        // **The gauge spans the row under both of them**, which is the width the
        // figure and the name were competing with it for: a bar is a proportion
        // and a proportion is easier to read the longer it is.
        fullness(painter, (free, total), (text_at, box_.right() - 10.0), box_, current);
    }
    let box_rect = painter.rect(text_at, 0.0, room, 0.0);
    painter.label(&row.label, TextStyle::new(metrics.text_size, ink), box_rect, baseline);
}

/// How much of a row's height the gauge and its air take from the label.
///
/// **Three of the twenty-six.** The bar is two pixels and the third is the gap
/// above it; the label moves up by the same amount, so the two share the row
/// instead of overlapping.
const GAUGE_SPACE: f32 = 3.0;

/// How full a volume is, as a bar beside the figure.
///
/// **A figure is not a proportion.** "40 GB free" is nearly empty on a laptop's
/// disk and nearly full on a camera card, and the thing a person is actually
/// asking is whether the disk is about to run out — which is the proportion. The
/// two are drawn together because either one alone is half an answer.
///
/// **Red past nine tenths.** A disk with a tenth left is the state worth
/// noticing, and it is the same `danger` the panel that deletes for good uses:
/// one colour in this program means "this is the one to read twice".
fn fullness(
    painter: &mut Painter<'_>,
    room: (u64, u64),
    across: (f32, f32),
    box_: Area,
    current: bool,
) {
    let (free, total) = room;
    let (from, to) = across;
    let width = to - from;
    if total == 0 || width < 8.0 {
        return;
    }
    let theme = *painter.theme;
    let height = 2.0;
    // Along the row's bottom edge, inside the pill a hover or the current row
    // draws: a bar on the edge itself would be cut in half by the rounding.
    let top = box_.bottom() - height - 3.0;
    let radius = painter.px(height / 2.0);
    let track = if current { theme.palette.selected_text } else { theme.palette.text };
    painter.list.rect(
        painter.rect(from, top, width, height),
        track.with_alpha(if theme.dark { 48 } else { 40 }),
        radius,
    );
    // Saturating, because `statvfs` on some filesystems reports more available
    // than it reports in total — a reserve that is not ours to reason about, and
    // a bar past its own track is a bar that says nothing.
    let used = total.saturating_sub(free.min(total));
    #[expect(clippy::cast_precision_loss, reason = "a disk's size along a row")]
    let part = (used as f64 / total as f64) as f32;
    let filled = (width * part.clamp(0.0, 1.0)).max(2.0);
    let ink = if part > 0.9 { theme.palette.danger } else { theme.palette.accent };
    painter.list.rect(painter.rect(from, top, filled, height), ink, radius);
}

/// Back, forward, parent and reload, drawn from triangles and rectangles.
///
/// Not font glyphs: `↻` and `⟳` are both missing from this machine's chain and
/// render as a box, and a toolbar with a box in it is worse than one with no
/// reload button. The arrows are a triangle for the head and a bar for the shaft,
/// which is the same shape at every scale and needs nobody's font.
///
/// Dimmed from the command's own `enabled`, so a Back button with nowhere to go
/// looks like a dimmed menu item and not like a broken one.
fn tool_buttons(painter: &mut Painter<'_>, pane: &Pane, leaf: &PaneLayout, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    for (tool, area) in Tool::ALL.into_iter().zip(leaf.tools) {
        if area.width <= 0.0 {
            continue;
        }
        // The pane's own answer, not the focused pane's: an unfocused half of a
        // split has its own history, and a Back that looked live because the
        // *other* pane had somewhere to go would be a lie in both directions.
        let enabled = pane.allows(tool);
        let hovered = chrome.hovered_tool == Some((leaf.id, tool));
        if hovered && enabled {
            let shape =
                painter.rect(area.x + 3.0, area.y + 7.0, area.width - 6.0, area.height - 14.0);
            painter.list.rect(shape, theme.palette.button_hover, painter.px(theme.metrics.radius));
        }
        let ink = match (enabled, chrome.window.activated) {
            (false, _) => theme.palette.dim.with_alpha(70),
            (true, false) => theme.palette.dim,
            (true, true) => theme.palette.text,
        };
        let centre = (area.x + area.width / 2.0, area.y + area.height / 2.0);
        match tool {
            Tool::Back => arrow(painter, centre, ink, Direction::Left),
            Tool::Forward => arrow(painter, centre, ink, Direction::Right),
            Tool::Parent => arrow(painter, centre, ink, Direction::Up),
            Tool::Reload => reload(painter, centre, ink),
        }
    }
}

/// The view-mode switcher: one small mark per mode, the current one filled.
///
/// **The current mode is a filled pill, not a brighter mark.** A switcher whose
/// state is only the ink is a switcher a person has to compare two icons to read;
/// filling the one that is on says it at a glance, and it is the shape every
/// segmented control in every toolkit uses.
fn mode_buttons(painter: &mut Painter<'_>, pane: &Pane, leaf: &PaneLayout, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    for (mode, area) in Mode::ALL.into_iter().zip(leaf.modes) {
        if area.width <= 0.0 {
            continue;
        }
        let current = pane.mode == mode;
        let hovered = chrome.hovered_mode == Some((leaf.id, mode));
        if current || hovered {
            let shape =
                painter.rect(area.x + 2.0, area.y + 6.0, area.width - 4.0, area.height - 12.0);
            let fill = if current {
                theme.palette.button_hover
            } else {
                theme.palette.button_hover.with_alpha(110)
            };
            painter.list.rect(shape, fill, painter.px(theme.metrics.radius));
        }
        let ink = match (current, chrome.window.activated) {
            (true, _) => theme.palette.text,
            (false, true) => theme.palette.dim,
            (false, false) => theme.palette.dim.with_alpha(150),
        };
        let centre = (area.x + area.width / 2.0, area.y + area.height / 2.0);
        match mode {
            Mode::Grid => grid_mark(painter, centre, ink),
            Mode::Details => list_mark(painter, centre, ink),
            Mode::Columns => columns_mark(painter, centre, ink),
        }
    }
}

/// Four squares in a square, for the grid.
fn grid_mark(painter: &mut Painter<'_>, centre: (f32, f32), ink: Color) {
    /// One square, and the space between two of them.
    const CELL: f32 = 5.0;
    const GAP: f32 = 2.5;
    let (x, y) = centre;
    let span = CELL * 2.0 + GAP;
    for row in 0..2 {
        for column in 0..2 {
            let rect = painter.rect(
                x - span / 2.0 + column as f32 * (CELL + GAP),
                y - span / 2.0 + row as f32 * (CELL + GAP),
                CELL,
                CELL,
            );
            painter.list.rect(rect, ink, painter.px(1.0));
        }
    }
}

/// Three upright panes side by side, for the column view.
fn columns_mark(painter: &mut Painter<'_>, centre: (f32, f32), ink: Color) {
    /// The whole mark, and the rule between two panes.
    const SPAN: f32 = 13.0;
    const HEIGHT: f32 = 11.0;
    const WEIGHT: f32 = 1.5;
    let (x, y) = centre;
    let frame = painter.rect(x - SPAN / 2.0, y - HEIGHT / 2.0, SPAN, HEIGHT);
    painter.list.stroke(frame, ink, painter.px(1.5), painter.px(WEIGHT).max(1.0));
    // Two rules rather than three panes drawn: the frame is the outer two edges,
    // so the divisions are what is left to say.
    for third in 1..3 {
        let at = x - SPAN / 2.0 + SPAN * third as f32 / 3.0;
        painter.vline(at, y - HEIGHT / 2.0, HEIGHT, ink);
    }
}

/// Three rows of a dot and a line, for the details list.
fn list_mark(painter: &mut Painter<'_>, centre: (f32, f32), ink: Color) {
    /// Between one row's baseline and the next.
    const PITCH: f32 = 4.5;
    const DOT: f32 = 2.0;
    const RULE: f32 = 8.0;
    const WEIGHT: f32 = 1.5;
    let (x, y) = centre;
    let left = x - (DOT + 2.0 + RULE) / 2.0;
    for row in -1..2 {
        let top = y + row as f32 * PITCH - WEIGHT / 2.0;
        let dot = painter.rect(left, top - (DOT - WEIGHT) / 2.0, DOT, DOT);
        painter.list.rect(dot, ink, painter.px(1.0));
        let rule = painter.rect(left + DOT + 2.0, top, RULE, WEIGHT);
        painter.list.rect(rule, ink, 0.0);
    }
}

/// The head and the shaft of one arrow, centred on a point.
fn arrow(painter: &mut Painter<'_>, centre: (f32, f32), ink: Color, at: Direction) {
    /// Across the head, and how far it reaches from the shaft.
    const HEAD: f32 = 7.0;
    const REACH: f32 = 4.0;
    /// Tip to tail.
    const LENGTH: f32 = 11.0;
    const WEIGHT: f32 = 1.5;
    let (x, y) = centre;
    let half = LENGTH / 2.0;
    let bar = |painter: &mut Painter<'_>, x: f32, y: f32, w: f32, h: f32| {
        let rect = painter.rect(x, y, w, h);
        painter.list.rect(rect, ink, 0.0);
    };
    match at {
        Direction::Left => {
            painter.triangle(x - half, y - HEAD / 2.0, REACH, HEAD, ink, at);
            bar(painter, x - half + REACH - 0.5, y - WEIGHT / 2.0, LENGTH - REACH, WEIGHT);
        }
        Direction::Right => {
            painter.triangle(x + half - REACH, y - HEAD / 2.0, REACH, HEAD, ink, at);
            bar(painter, x - half, y - WEIGHT / 2.0, LENGTH - REACH + 0.5, WEIGHT);
        }
        Direction::Up | Direction::Down => {
            painter.triangle(x - HEAD / 2.0, y - half, HEAD, REACH, ink, at);
            bar(painter, x - WEIGHT / 2.0, y - half + REACH - 0.5, WEIGHT, LENGTH - REACH);
        }
    }
}

/// A circular arrow: an arc, and a head where the arc ends.
///
/// The arc is the ring **drawn around a hole** rather than a ring with the hole
/// painted over it. A clip is an intersection, so leaving a quadrant out means
/// two clipped passes — which is three lines more than painting the gap in the
/// background colour, and the reason to prefer it is that there is no background
/// colour: the button is chrome underneath and a translucent hover tint on top,
/// and an "erase" that has to know which was a mark that did not erase.
///
/// The head goes at twelve o'clock pointing right, because that is where a
/// clockwise arc ends and which way it is travelling when it gets there. The
/// first version put it outside the ring at half past one, where it read as a
/// circle with a flag on it.
fn reload(painter: &mut Painter<'_>, centre: (f32, f32), ink: Color) {
    const SIZE: f32 = 11.0;
    let (x, y) = centre;
    let half = SIZE / 2.0;
    let ring = painter.rect(x - half, y - half, SIZE, SIZE);
    let weight = painter.snap(1.5).max(1.0);
    for (left, top, width, height) in [
        // Everything left of centre, and the right side from a third of the way
        // up: together they are the ring minus a wedge from twelve o'clock to
        // about half past one, which is as much of a gap as the head needs.
        (x - half - 1.0, y - half - 1.0, half + 1.0, SIZE + 2.0),
        (x, y - half * 0.35, half + 1.0, SIZE),
    ] {
        let clip = painter.rect(left, top, width, height);
        painter.list.push_clip(clip);
        painter.list.stroke(ring, ink, painter.px(half), weight);
        painter.list.pop_clip();
    }
    // Base on the ring at twelve o'clock, apex to the right: where a clockwise
    // arc ends, and which way it is travelling when it gets there.
    painter.triangle(x - 1.5, y - half - 2.0, 5.0, 5.5, ink, Direction::Right);
}

/// Minimise, maximise and close, drawn rather than taken from an icon theme.
///
/// Three marks, all of them axis-aligned rectangles except the close cross,
/// which is the one shape a quad cannot make and so is the `×` the font already
/// has. An icon theme would be four hundred lines of `index.theme` parsing and a
/// dependency on the user having one installed, for three ten-pixel glyphs.
fn window_buttons(painter: &mut Painter<'_>, layout: &Layout, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    for (button, area) in Button::ALL.into_iter().zip(layout.buttons) {
        // The layout has already dropped the buttons the compositor does not
        // offer, so a zero-width slot is an absent action and not a narrow one.
        if area.width <= 0.0 {
            continue;
        }
        if chrome.hovered_button == Some(button) {
            let background = if button == Button::Close {
                theme.palette.close_hover
            } else {
                theme.palette.button_hover
            };
            fill(painter, area, background);
        }
        // Dimmed when the window is not the active one, the way every other
        // window on the desktop dims its frame.
        let mut ink = if chrome.window.activated { theme.palette.text } else { theme.palette.dim };
        if chrome.hovered_button == Some(button) && button == Button::Close {
            ink = theme.palette.selected_text;
        }
        let centre = (area.x + area.width / 2.0, area.y + area.height / 2.0);
        match button {
            Button::Minimize => {
                let bar = painter.rect(centre.0 - 5.0, centre.1, 10.0, 1.0);
                painter.list.rect(bar, ink, 0.0);
            }
            Button::Maximize => {
                let box_rect = painter.rect(centre.0 - 4.5, centre.1 - 4.5, 9.0, 9.0);
                // Two offset squares mean restore, one means maximise — the
                // convention every desktop uses, and the only way the button
                // says what it will do rather than what it is.
                if chrome.window.maximized {
                    let behind = painter.rect(centre.0 - 2.5, centre.1 - 6.5, 9.0, 9.0);
                    painter.list.stroke(behind, ink.with_alpha(120), 1.0, painter.snap(1.0));
                }
                painter.list.stroke(box_rect, ink, 1.0, painter.snap(1.0));
            }
            Button::Close => {
                let style = TextStyle::new(theme.metrics.path_size + 2.0, ink);
                let baseline = painter.baseline(area.y, area.height, style.size);
                let box_rect = painter.rect(area.x, 0.0, area.width, 0.0);
                painter.label(i18n::lookup("chrome.close"), style.centred(), box_rect, baseline);
            }
        }
    }
}

/// The leaf that has the keyboard, or the first one if the focus named none.
fn focused_leaf<'a>(chrome: &'a Chrome<'_>, focus: PaneId) -> &'a PaneLayout {
    chrome
        .leaves
        .iter()
        .find(|leaf| leaf.id == focus)
        .or_else(|| chrome.leaves.first())
        .unwrap_or(&EMPTY_LEAF)
}

/// Stands in for a frame drawn before the tree has been laid out, which is only
/// the very first one.
static EMPTY_LEAF: PaneLayout = PaneLayout {
    id: PaneId::FIRST,
    area: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    tabs: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    bar: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    tools: [Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; 4],
    modes: [Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; crate::cells::Mode::ALL.len()],
    path: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    header: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    body: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    icon: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    name: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    size: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
    modified: Area { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
};

/// The strip of tabs over one leaf. Nothing at all when the leaf has one tab.
fn tab_strip(
    painter: &mut Painter<'_>,
    tabs: &crate::panes::Tabs,
    layout: &PaneLayout,
    chrome: &Chrome<'_>,
    has_keyboard: bool,
) {
    if layout.tabs.height <= 0.0 {
        return;
    }
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.tabs, theme.palette.chrome);
    let clip = painter.rect(layout.tabs.x, layout.tabs.y, layout.tabs.width, layout.tabs.height);
    painter.list.push_clip(clip);
    let titles: Vec<&str> = tabs.titles().collect();
    for (index, area) in crate::panes::tab_areas(layout.tabs, titles.len()).enumerate() {
        let active = index == tabs.active;
        let hovered = chrome.hovered_tab == Some((layout.id, index));
        // The showing tab of the leaf with the keyboard is the background; every
        // other tab is a step off it. `row_background` is the same pair of tints
        // the list uses, so a current tab and a selected row agree.
        let background = theme.row_background(active && has_keyboard, active || hovered);
        // **Inset by the same amount top and bottom**, and the text centred in
        // the shape rather than in the strip: with three pixels off the top and
        // none off the bottom, every tab's label sat a pixel and a half above
        // the middle of its own pill.
        let shape = Area {
            x: area.x + 2.0,
            y: area.y + TAB_INSET,
            width: area.width - 4.0,
            height: area.height - TAB_INSET * 2.0,
        };
        let pill = painter.rect(shape.x, shape.y, shape.width, shape.height);
        painter.list.rect(pill, background, painter.px(metrics.radius));
        // Where a drop of files would land. The same accent fill a row takes,
        // because a tab and a row are the same promise — this is the directory
        // the files go into — and the ring means "the keyboard is here"
        // everywhere else in this window.
        let marked = chrome.drop_tab == Some((layout.id, index));
        if marked {
            painter.list.rect(
                pill,
                theme.palette.accent.with_alpha(90),
                painter.px(metrics.radius),
            );
        }
        // A marked tab reads at full strength whether or not it is the one
        // showing: the dim a background tab is drawn in is a tint away from the
        // chrome, and over an accent wash on the light palette it is a label
        // nobody can read.
        let colour = if active || marked { theme.palette.text } else { theme.palette.dim };
        let baseline = painter.baseline(shape.y, shape.height, metrics.text_size);
        let close = crate::panes::tab_close(area);
        let room = close.map_or(area.width - 20.0, |it| it.x - area.x - 16.0);
        line(
            painter,
            titles[index],
            TextStyle::new(metrics.text_size, colour),
            area.x + 10.0,
            room.max(0.0),
            baseline,
        );
        if let Some(close) = close {
            cross(painter, close, if active { theme.palette.text } else { theme.palette.dim });
        }
    }
    // The `+`, after the tabs and inside the same clip: a strip full of tabs
    // shrinks them rather than pushing it off the end.
    let plus = crate::panes::tab_plus(layout.tabs, titles.len());
    if chrome.hovered_plus && chrome.hovered_tab.is_none_or(|(id, _)| id != layout.id) {
        let pill = painter.rect(plus.x + 3.0, plus.y + 4.0, plus.width - 6.0, plus.height - 8.0);
        painter.list.rect(pill, theme.palette.hover, painter.px(metrics.radius));
    }
    let ink = if has_keyboard { theme.palette.text } else { theme.palette.dim };
    let centre = (plus.x + plus.width / 2.0, plus.y + plus.height / 2.0);
    let arm = 9.0;
    let thickness = painter.px(1.0).max(1.0).round();
    let across = painter.rect(centre.0 - arm / 2.0, centre.1, arm, 0.0);
    painter.list.rect(sk_gpu::Rect::new(across.x, across.y, across.width, thickness), ink, 0.0);
    let down = painter.rect(centre.0, centre.1 - arm / 2.0, 0.0, arm);
    painter.list.rect(sk_gpu::Rect::new(down.x, down.y, thickness, down.height), ink, 0.0);

    painter.list.pop_clip();
    painter.hairline(
        layout.tabs.x,
        layout.tabs.bottom() - 1.0,
        layout.tabs.width,
        theme.palette.divider,
    );
}

/// The × on a tab.
fn cross(painter: &mut Painter<'_>, area: Area, ink: Color) {
    let inset = 5.0;
    let thickness = painter.px(1.0).max(1.0).round();
    let (x, y) = (area.x + inset, area.y + inset);
    let side = area.width - inset * 2.0;
    for step in 0..=(side as usize) {
        let at = step as f32;
        let dot = painter.rect(x + at, y + at, 1.0, 1.0);
        painter.list.rect(dot, ink, 0.0);
        let dot = painter.rect(x + at, y + side - at, 1.0, 1.0);
        painter.list.rect(dot, ink, 0.0);
    }
    let _ = thickness;
}

/// The line between two panes.
fn divider_line(painter: &mut Painter<'_>, divider: Divider, hovered: bool) {
    let theme = *painter.theme;
    let ink = if hovered { theme.palette.accent } else { theme.palette.divider };
    match divider.axis {
        crate::panes::Axis::Beside => {
            let x = divider.area.x + divider.area.width / 2.0;
            let rect = painter.rect(x - 0.5, divider.area.y, 1.0, divider.area.height);
            painter.list.rect(rect, ink, 0.0);
        }
        crate::panes::Axis::Below => {
            let y = divider.area.y + divider.area.height / 2.0;
            let rect = painter.rect(divider.area.x, y - 0.5, divider.area.width, 1.0);
            painter.list.rect(rect, ink, 0.0);
        }
    }
}

/// Where the three sort labels are, and how each is aligned.
///
/// **A free function because the header draws them and the hit test finds them**,
/// and two rectangles computed separately are two rectangles that agree until one
/// of them is edited — which is the same reason [`name_field`] is one.
///
/// **A grid has no columns, so its header is not one.** The three labels stay —
/// they are how a pointer reaches the three sort commands, and the sort still
/// orders the cells — but they sit adjacently from the left rather than at the
/// Details column offsets, where `Size` stood over cells that have no size in
/// them and read as a column that was not there.
#[must_use]
pub fn sort_columns(
    layout: &PaneLayout,
    metrics: &sk_ui::Metrics,
    mode: Mode,
) -> [(SortColumn, Area, Align, f32); 3] {
    match mode {
        // **Only the name.** A trail's columns hold a name and a chevron; there is
        // no size and no date in one, so there is nothing for the other two
        // headers to sort and a header that offered them would sort by a column
        // that is not drawn.
        Mode::Columns => [
            (SortColumn::Name, layout.name, Align::Left, 0.0),
            (SortColumn::Size, Area::default(), Align::Right, 12.0),
            (SortColumn::Modified, Area::default(), Align::Left, 12.0),
        ],
        Mode::Details => [
            (SortColumn::Name, layout.name, Align::Left, 0.0),
            (SortColumn::Size, layout.size, Align::Right, 12.0),
            (SortColumn::Modified, layout.modified, Align::Left, 12.0),
        ],
        Mode::Grid => {
            let slot = 96.0_f32.min((layout.header.width - metrics.gutter) / 3.0).max(1.0);
            let strip = |nth: f32| Area {
                x: layout.name.x + nth * slot,
                y: layout.header.y,
                width: slot,
                height: layout.header.height,
            };
            [
                (SortColumn::Name, strip(0.0), Align::Left, 0.0),
                (SortColumn::Size, strip(1.0), Align::Left, 0.0),
                (SortColumn::Modified, strip(2.0), Align::Left, 0.0),
            ]
        }
    }
}

/// A rule down each column boundary in the header.
///
/// **What makes the boundary a thing to grab.** The band a press takes hold of is
/// eight logical pixels of nothing, and an invisible affordance is a feature only
/// somebody who was told about it can use — which is rule 4. It draws the same
/// three lines the drag reads, from the columns the frame laid out, so the line a
/// hand aims at is the line the press finds.
///
/// Details only: a grid's header is three labels in a strip at the left rather
/// than three columns, and there is nothing there to divide.
///
/// Inset top and bottom, and stopping short of the rule under the header, because
/// a boundary that met it on both ends would close a box around each label and
/// the header would read as three fields to type in.
///
/// **Between the columns and not around them.** A rule at the last column's right
/// edge closed the table off, and the scrollbar then had nowhere to go: the thumb
/// belongs against the table's edge, that rule marks a boundary a drag takes hold
/// of and may not be drawn over, and the two are the same ten pixels. The rule
/// lost, because what says where the table ends is the row fill, which ends there
/// on every row — and a boundary that only the third column has is the one a hand
/// reaches for least. It is still draggable; it is the fill's edge that shows it.
fn separators(painter: &mut Painter<'_>, pane: &Pane, layout: &PaneLayout, colour: sk_gpu::Color) {
    if pane.mode != Mode::Details {
        return;
    }
    let inset = 5.0;
    let height = (layout.header.height - inset * 2.0).max(1.0);
    for column in [layout.name, layout.size] {
        if column.width > 0.0 {
            painter.vline(column.right(), layout.header.y + inset, height, colour);
        }
    }
}

fn header(painter: &mut Painter<'_>, pane: &Pane, layout: &PaneLayout) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.header, theme.palette.chrome);
    // **From the header's own left edge, not the window's.** At `0.0` this ran
    // under the sidebar, which looked like a deliberate rule while there was one
    // pane at the top of the window — and became a line through the middle of
    // the sidebar the moment a split put a second header halfway down.
    painter.hairline(
        layout.header.x,
        layout.header.bottom() - 1.0,
        layout.header.width,
        theme.palette.divider,
    );

    let baseline = painter.baseline(layout.header.y, layout.header.height, metrics.header_size);
    let columns = sort_columns(layout, &metrics, pane.mode);
    separators(painter, pane, layout, theme.palette.divider);

    for (column, area, align, inset) in columns {
        if area.width <= 0.0 {
            continue;
        }
        let active = pane.sort.column == column;
        let color = if active { theme.palette.accent } else { theme.palette.dim };
        let style = TextStyle { weight: Weight::Bold, size: metrics.header_size, color, align };
        let (x, width) = match align {
            Align::Left => (area.x + inset, area.width - inset),
            Align::Right | Align::Center => (area.x, area.width - inset),
        };
        let label = i18n::lookup(column.label_key());
        let advance = line(painter, label, style, x, width, baseline);

        if active {
            // The direction is a bar at the edge the order runs from: below the
            // label for ascending, above it for descending. A glyph arrow would
            // be a font dependency for two pixels of meaning, and half the sans
            // fonts on a Linux box have no triangles in them.
            //
            // As wide as the *label*, not as wide as the column: a rule spanning
            // the whole name column reads as a text field, which is what the
            // first capture of this header looked like.
            let y = if pane.sort.descending {
                painter.snap(layout.header.y + 5.0)
            } else {
                painter.snap(layout.header.bottom() - 7.0)
            };
            let left = match align {
                Align::Left | Align::Center => painter.snap(x),
                Align::Right => painter.snap(x + width) - advance,
            };
            let bar = sk_gpu::Rect::new(left, y, advance, painter.px(2.0).max(1.0).round());
            painter.list.rect(bar, theme.palette.accent, painter.px(1.0));
        }
    }
}

/// The rectangle the pane's own entries occupy.
///
/// The whole table in Details, the body in a grid, and **the last column** in a
/// trail — which is the one thing about column mode that `PaneLayout` cannot
/// answer, because only the pane knows how many ancestors it has.
pub(crate) fn table_of(layout: &PaneLayout, pane: &Pane) -> Area {
    if pane.mode == Mode::Columns {
        return crate::trail::pane_area(layout.body, pane.trail.len(), pane.across.offset());
    }
    layout.rows(pane.mode)
}

/// How one row of a column is drawn.
#[derive(Clone, Copy, Default)]
struct CellState {
    selected: bool,
    cursor: bool,
    hovered: bool,
    /// Whether this is the entry whose folder is open in the column to the right.
    chosen: bool,
}

/// Where the message that stands in for an empty listing goes: its left edge and
/// how much room it has, both in logical pixels.
///
/// **It started at the window's gutter.** Ten pixels from the left of the
/// *window*, not of the pane — so with the sidebar open, which is the default,
/// "This folder is empty" and every read error were drawn underneath it and
/// nobody had ever seen one. A column view made it visible by drawing the message
/// across the ancestor columns instead of hiding it, which is how it was reported.
///
/// From the pane's own table, so it is under the rows it is standing in for in
/// every mode — the whole table in Details, the body in a grid, the last column of
/// a trail. It runs to the body's right edge rather than the table's, which in
/// column mode is the empty space beside the last column and is the only place a
/// sentence fits; there is no scrollbar to collide with, because a listing with
/// nothing in it has nothing to scroll.
fn empty_message(table: Area, body: Area, metrics: &sk_ui::Metrics) -> (f32, f32) {
    let x = table.x + metrics.gutter;
    ((x), (body.right() - metrics.gutter - x).max(1.0))
}

/// The mark in a column row, square.
const ICON: f32 = 16.0;

/// The most room a check box takes, square.
///
/// Sixteen, which is the whole of a row's mark and the corner of a cell's. A
/// pointer target smaller than this is one a hand misses; larger than this it
/// covers a grid mark somebody is trying to recognise.
const CHECK: f32 = 16.0;

/// Where an entry's mark is drawn, in logical pixels, in whichever mode.
///
/// **One home per expression, which is the whole reason this exists.** Each of the
/// three arms was written inline in the function that draws that mode, and the
/// check box needs the same three answers — so without this there would be two
/// copies of each, and the copy the pointer is tested against would drift from the
/// copy a person can see. `Mode::Columns` defers to [`column_parts`] rather than
/// repeating its gutter for the same reason.
///
/// `layout` is read by `Details` alone: it is the one mode whose mark sits in a
/// column of its own rather than at a fraction of the cell.
///
/// **A free function with no `Painter`, for the reason [`column_parts`] is one.**
/// Neither an [`Area`] nor [`sk_ui::Metrics`] can hold a device pixel, so this
/// arithmetic cannot mix the two the way `column_row`'s once did.
pub(crate) fn mark_of(
    mode: Mode,
    layout: &PaneLayout,
    cell: Area,
    metrics: &sk_ui::Metrics,
) -> Area {
    let (x, y, side) = match mode {
        Mode::Details => (
            layout.icon.x + (layout.icon.width - ICON) / 2.0,
            cell.y + (metrics.row_height - ICON) / 2.0,
            ICON,
        ),
        // **From the height, capped by the width.** The cell's height is fixed and
        // its width stretches to fill the pane, so a mark sized from the width grew
        // with the window while the cell it sits in did not — and in a wide pane it
        // drew straight through the name. The cap is what keeps it inside a narrow
        // one.
        Mode::Grid => {
            let side = (cell.height * 0.46).min(cell.width * 0.42);
            (cell.x + (cell.width - side) / 2.0, cell.y + cell.height * 0.34 - side / 2.0, side)
        }
        Mode::Columns => {
            (column_parts(cell, metrics).0, cell.y + (metrics.row_height - ICON) / 2.0, ICON)
        }
    };
    Area { x, y, width: side, height: side }
}

/// Where an entry's check box goes: the mark's own top-left square.
///
/// **One rule for three modes.** What it looks like follows from how big the mark
/// is rather than from a branch: a row's mark is sixteen pixels, so the box covers
/// it exactly and reads as a box where the mark was; a cell's mark is forty or
/// more, so the same square is a badge on its corner.
pub(crate) fn check_of(
    mode: Mode,
    layout: &PaneLayout,
    cell: Area,
    metrics: &sk_ui::Metrics,
) -> Area {
    let mark = mark_of(mode, layout, cell, metrics);
    let side = mark.width.min(CHECK);
    Area { width: side, height: side, ..mark }
}

/// Where a column row's three parts start, in logical pixels: the mark, the name,
/// and how wide the name's box is.
///
/// **A free function with no `Painter`, which is the point.** This arithmetic was
/// written inline and took `icon_box.right()` — a rectangle that has already been
/// through `Painter::rect` and is therefore in *device* pixels — as the left edge
/// of a name box measured against `area.right()`, which is logical. At 100% the
/// two are the same number and the row drew correctly; at 125% the name started a
/// quarter of a column too far right, and at 200% every column but the first had
/// a name box one pixel wide and drew a single character and an ellipsis.
///
/// It is a function taking an [`Area`] and [`Metrics`] because neither of those
/// can be a device pixel. The rule is CLAUDE.md's — everything above the
/// `Painter` is logical, and the conversion happens in exactly one place — and
/// this is what obeying it structurally looks like rather than by care.
fn column_parts(area: Area, metrics: &sk_ui::Metrics) -> (f32, f32, f32) {
    let icon_x = area.x + metrics.gutter;
    let text_x = icon_x + ICON + 6.0;
    let width = (area.right() - metrics.gutter - CHEVRON - text_x).max(1.0);
    (icon_x, text_x, width)
}

/// The room a folder's chevron takes at the right of a column row.
///
/// Reserved whether or not it is drawn, so a folder's name and a file's name elide
/// at the same width and the column does not look ragged.
const CHEVRON: f32 = 14.0;

/// One row of a trail column: a mark, a name, and a chevron for a folder.
///
/// **No size and no date, because a column has neither.** A trail level fetches no
/// metadata — ADR 0015 — so there is nothing to draw in those columns and nothing
/// that would be blank while it arrived.
fn column_row(
    painter: &mut Painter<'_>,
    listing: &sk_model::Listing,
    id: sk_model::EntryId,
    layout: &PaneLayout,
    cell: Area,
    state: CellState,
    types: Option<&sk_desktop::MimeDatabase>,
) {
    // The column's rectangle at this row's height. `area` is the column, `top` is
    // where this row starts in it, and every line below wants one or the other.
    let (area, top) = (cell, cell.y);
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let flags = listing.flags(id);
    let directory = flags.has(Flags::DIR);
    let broken = flags.has(Flags::BROKEN);

    let pill = painter.rect(
        area.x + metrics.gutter * 0.5,
        top + 1.0,
        area.width - metrics.gutter,
        metrics.row_height - 2.0,
    );
    // **The chosen entry keeps a fill while the keyboard is elsewhere.** It is
    // what says which of these rows the column to the right came from, so it
    // cannot be the hover's colour and it cannot go when focus does.
    let background = if state.chosen && !state.selected {
        theme.palette.button_hover
    } else {
        theme.row_background(state.selected, state.hovered)
    };
    if !background.is_invisible() {
        painter.list.rect(pill, background, painter.px(metrics.radius));
    }
    if state.cursor {
        let ring = painter.px(1.0).round().max(1.0);
        painter.list.stroke(pill, theme.palette.accent, painter.px(metrics.radius), ring);
    }

    let (_, text_x, width) = column_parts(area, &metrics);
    let mark = mark_of(Mode::Columns, layout, cell, &metrics);
    let icon_box = painter.rect(mark.x, mark.y, mark.width, mark.height);
    let ink = kind_colour_of(&theme, types, listing, id, directory, broken);
    if directory {
        painter.folder(icon_box, ink);
    } else {
        painter.file(icon_box, ink);
    }

    let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
    let colour = if broken { theme.palette.dim } else { theme.palette.text };
    let key = RunKey::cell(
        listing.version(),
        id.0,
        COLUMN_NAME,
        Weight::Regular,
        painter.px(metrics.text_size),
    );
    let box_ = painter.rect(text_x, top, width, metrics.row_height);
    painter.cell(key, listing.name(id), box_, baseline, colour, Align::Left);
    if directory {
        painter.triangle(
            area.right() - metrics.gutter - CHEVRON + 4.0,
            top + metrics.row_height / 2.0 - 4.0,
            6.0,
            8.0,
            theme.palette.dim,
            Direction::Right,
        );
    }
}

/// Every column left of the pane's own.
fn ancestor_columns(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    chrome: &Chrome<'_>,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let offset = pane.across.offset();
    for (level, area) in
        pane.trail.iter().zip(crate::trail::areas(layout.body, pane.trail.len(), offset))
    {
        if area.right() <= layout.body.x || area.x >= layout.body.right() {
            continue;
        }
        painter.vline(area.right(), area.y, area.height, theme.palette.divider);
        let rows = level.dir.listing.rows();
        let cells = crate::trail::cells_of(level, area, metrics.row_height);
        for row in cells.visible(level.scroll.offset()) {
            let Some(id) = level.dir.listing.at(row) else { continue };
            let top = cells.at(row, level.scroll.offset()).y;
            let state = CellState { chosen: level.chosen == Some(id), ..CellState::default() };
            let cell = Area { y: top, height: metrics.row_height, ..area };
            column_row(painter, &level.dir.listing, id, layout, cell, state, chrome.types);
        }
        if rows == 0 && !level.dir.loading {
            let message = level.dir.problem.as_ref().map_or_else(
                || i18n::lookup("status.empty").to_owned(),
                sk_model::Problem::message,
            );
            let baseline = painter.baseline(area.y + 8.0, metrics.row_height, metrics.header_size);
            let style = TextStyle::new(metrics.header_size, theme.palette.dim);
            let width = area.width - metrics.gutter * 2.0;
            line(painter, &message, style, area.x + metrics.gutter, width, baseline);
        }
    }
}

fn body(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    now: i64,
    chrome: &Chrome<'_>,
    has_keyboard: bool,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let clip = painter.rect(layout.body.x, layout.body.y, layout.body.width, layout.body.height);
    painter.list.push_clip(clip);

    let rows = pane.listing.rows();
    // **The ancestors first, so the pane's own column draws over their rule.** In
    // every other mode this is one list; here it is one list with a path's worth
    // of narrower ones to its left, and only the last of them is the pane.
    if pane.mode == Mode::Columns {
        ancestor_columns(painter, pane, layout, chrome);
    }
    // One geometry for both modes, per [ADR
    // 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md).
    // Details is a one-column grid, so the loop below is the same loop either way
    // and only the cell's *contents* differ.
    let cells = crate::groups::Bands::new(
        pane.mode,
        table_of(layout, pane),
        metrics.row_height,
        &pane.bands,
        rows,
        pane.closed,
    );
    for heading in cells.headings(pane.scroll.offset()) {
        heading_at(painter, pane.grouping, heading);
    }
    for row in cells.visible(pane.scroll.offset()) {
        let context = RowContext {
            types: chrome.types,
            naming: chrome.naming.filter(|_| has_keyboard),
            drop_row: chrome.drop_row.filter(|(id, _)| *id == layout.id).map(|(_, row)| row),
        };
        match pane.mode {
            Mode::Details => row_at(painter, pane, layout, now, row, context, hits),
            Mode::Grid => cell_at(painter, pane, layout, &cells, row, context, hits),
            Mode::Columns => {
                let area = table_of(layout, pane);
                let top = cells.at(row, pane.scroll.offset()).y;
                let Some(id) = pane.listing.at(row) else { continue };
                let state = CellState {
                    selected: pane.selection.contains(id),
                    cursor: pane.selection.cursor() == Some(id) && context.naming.is_none(),
                    hovered: pane.hover == Some(row),
                    chosen: false,
                };
                let cell = Area { y: top, height: metrics.row_height, ..area };
                column_row(painter, &pane.listing, id, layout, cell, state, chrome.types);
                // **The field, after the row it replaces.** A column view had
                // none: `F2` opened a rename that drew nothing, so the name was
                // typed blind — and a press had no box to land in, which is the
                // other half of the defect this records in `Hits::name_field`.
                if let Some(naming) = context.naming.filter(|naming| naming.row == row) {
                    let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
                    name_field_at(painter, name_field_in(cell, &metrics), naming, baseline, hits);
                }
            }
        }
        // **The check box, in one place for all three modes.** It is drawn after
        // the mark it sits over and from the same [`check_of`] a press is tested
        // against, so what a pointer hits is what a person sees — and [rule
        // 8](../../../docs/03-principles.md)'s "the same selection semantics in
        // every view" holds by construction rather than by three copies agreeing.
        //
        // **Only under the pointer.** A box on every selected row is a column of
        // furniture and puts the affordance where it is not needed; a box on a row
        // that is not hovered is a target nobody can see. Hover is the one state
        // where drawing it and taking a press are the same condition, which is why
        // `App::check_under` asks for it too.
        let renaming = context.naming.is_some_and(|naming| naming.row == row);
        if let Some(id) = pane.listing.at(row).filter(|_| pane.hover == Some(row) && !renaming) {
            let box_ = check_of(pane.mode, layout, cells.at(row, pane.scroll.offset()), &metrics);
            let rect = painter.rect(box_.x, box_.y, box_.width, box_.height);
            // **Its own ground, because it is drawn over the mark.** At sixteen
            // pixels the box is the mark's whole square and has to hide it; on a
            // grid cell it is a badge on the corner of one. Painting the pane's
            // background rather than the row's fill on purpose: a chip that is
            // clearly a separate object is a chip a hand aims at.
            let radius = painter.px(3.0);
            painter.list.rect(rect, theme.palette.background, radius);
            let on = pane.selection.contains(id);
            painter.checkbox(rect, theme.palette.accent, theme.palette.dim, on);
        }
    }

    if rows == 0 && !pane.loading {
        let message = pane
            .problem
            .as_ref()
            .map_or_else(|| i18n::lookup("status.empty").to_owned(), sk_model::Problem::message);
        let table = table_of(layout, pane);
        let (x, width) = empty_message(table, layout.body, &metrics);
        let baseline = painter.baseline(table.y + 36.0, metrics.row_height, metrics.path_size);
        let style = TextStyle::new(metrics.path_size, theme.palette.dim);
        line(painter, &message, style, x, width, baseline);
    }

    if let Some((from, to)) = chrome.band.filter(|_| has_keyboard) {
        let rect = painter.rect(
            from.x.min(to.x),
            from.y.min(to.y),
            (to.x - from.x).abs(),
            (to.y - from.y).abs(),
        );
        let ring = painter.px(1.0).round().max(1.0);
        painter.list.rect(rect, theme.palette.accent.with_alpha(40), painter.px(2.0));
        painter.list.stroke(rect, theme.palette.accent.with_alpha(180), painter.px(2.0), ring);
    }

    scrollbar(painter, pane, layout);
    painter.list.pop_clip();
}

/// What a row needs to know about the window around it.
///
/// One value rather than three more arguments: each is a thing *some* row is
/// drawn differently for, and they arrive together from the same `Chrome`.
#[derive(Clone, Copy)]
struct RowContext<'a> {
    /// The filename half of shared-mime-info, for the kind colour.
    types: Option<&'a MimeDatabase>,
    /// The name being typed, if one is — which suppresses every focus ring.
    naming: Option<&'a Naming>,
    /// Where a drag of files would land.
    drop_row: Option<usize>,
}

/// The disclosure at the head of a band's heading, square.
const DISCLOSE: f32 = 8.0;

/// One band's heading: a disclosure, a name, a count, and a rule.
///
/// **Not a row, and not furniture either.** It carries no selection, no hover and
/// no cursor ring, because it is not an entry and a click on one selects nothing —
/// which is what [`crate::groups::Bands::index_at`] answers. It *is* a control: a
/// press on it collapses the band, which is what the disclosure says and what
/// [`crate::groups::Bands::heading_under`] answers.
///
/// **The count is what makes collapsing honest.** A band with its rows hidden still
/// says how many it is holding, so nothing goes missing without a line on screen
/// saying how much — which is the difference between this and the name filter, and
/// the reason this one is allowed to keep working while a person navigates around
/// inside a folder.
fn heading_at(
    painter: &mut Painter<'_>,
    grouping: crate::groups::Grouping,
    heading: crate::groups::Heading,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let at = heading.at;
    let baseline = painter.baseline(at.y, at.height, metrics.header_size);
    let style = TextStyle {
        weight: Weight::Bold,
        size: metrics.header_size,
        color: theme.palette.heading,
        align: Align::Left,
    };

    // Right when it is shut and down when it is open, which is what a tree's
    // disclosure has always done — see `sk_gpu::Direction`, which exists because
    // four marks like this one would otherwise be four font glyphs.
    let facing = if heading.closed { Direction::Right } else { Direction::Down };
    painter.triangle(
        at.x + metrics.gutter,
        at.y + (at.height - DISCLOSE) / 2.0,
        DISCLOSE,
        DISCLOSE,
        theme.palette.dim,
        facing,
    );

    let text_x = at.x + metrics.gutter + DISCLOSE + 6.0;
    let room = (at.right() - metrics.gutter - text_x).max(1.0);
    // **`line` answers in device pixels**, because a shaped advance is measured in
    // them, and every coordinate here is logical. Crossing back once per run is
    // what `Painter::logical` is for: the rule below was placed without it, and
    // was a quarter of the label's width too far right at 125%.
    let label = i18n::lookup(grouping.label(heading.kind));
    let drawn = line(painter, label, style, text_x, room, baseline);
    let named = painter.logical(drawn);
    let counted = i18n::format("band.count", &[&format::grouped(heading.held)]);
    let dim = TextStyle::new(metrics.header_size, theme.palette.dim);
    let count_x = text_x + named + 6.0;
    let count_room = (at.right() - metrics.gutter - count_x).max(1.0);
    let drawn = line(painter, &counted, dim, count_x, count_room, baseline);
    let numbered = painter.logical(drawn);
    let advance = named + 6.0 + numbered;

    // A rule from the end of the text to the pane's edge, which is what says the
    // heading covers everything under it without drawing a box around it.
    //
    // **In logical pixels.** `hairline` takes logical and snaps for itself, while
    // `baseline` above returns *device* — so `snap(baseline - 4)` scaled an
    // already-scaled number and put the rule at 1.56× its y at 125%, correct at
    // 100% and wrong at every other scale. The heading's own rectangle is
    // logical, so its middle is the honest way to ask.
    let from = text_x + advance + metrics.gutter;
    let to = at.right() - metrics.gutter;
    if to > from {
        painter.hairline(from, at.y + at.height * 0.5, to - from, theme.palette.divider);
    }
}

/// One grid cell: its shape, a large mark, and the name under it.
///
/// The same three states a row draws — selected, cursor, drop target — because
/// [rule 8](../../../docs/03-principles.md) is that the selection semantics are
/// the same in every view, and a view where the cursor ring means something else
/// is a second thing to learn.
///
/// **No thumbnail yet.** The picture that belongs in the middle of this is M5's
/// and arrives through `sk-preview`; what is here is the kind mark at cell size,
/// which is the same colour the row draws at 16 pixels.
fn cell_at(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    cells: &crate::groups::Bands,
    row: usize,
    context: RowContext<'_>,
    hits: &mut Hits,
) {
    let RowContext { types, naming, drop_row } = context;
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let Some(id) = pane.listing.at(row) else { return };
    let box_ = cells.at(row, pane.scroll.offset());
    let flags = pane.listing.flags(id);
    let selected = pane.selection.contains(id);
    let directory = flags.has(Flags::DIR);
    let broken = flags.has(Flags::BROKEN);
    let ring = painter.px(1.0).round().max(1.0);

    // Inset, so two cells side by side have a gap between their fills rather
    // than one continuous band of selection colour.
    let inset = metrics.gutter * 0.5;
    let pill = painter.rect(
        box_.x + inset,
        box_.y + inset,
        (box_.width - inset * 2.0).max(1.0),
        (box_.height - inset * 2.0).max(1.0),
    );
    let background = theme.row_background(selected, pane.hover == Some(row));
    if !background.is_invisible() {
        painter.list.rect(pill, background, painter.px(metrics.radius));
    }
    if pane.selection.cursor() == Some(id) && naming.is_none() {
        painter.list.stroke(pill, theme.palette.accent, painter.px(metrics.radius), ring);
    }
    if drop_row == Some(row) {
        painter.list.rect(pill, theme.palette.accent.with_alpha(90), painter.px(metrics.radius));
    }

    let mark = mark_of(pane.mode, layout, box_, &metrics);
    mark_at(painter, pane, id, mark, MarkState { directory, broken }, types);

    let label = grid_label(box_, &metrics);
    let baseline = painter.baseline(label.y, label.height, metrics.text_size);
    if let Some(naming) = naming.filter(|naming| naming.row == row) {
        name_field_at(painter, name_field(layout, &metrics, label.y), naming, baseline, hits);
        return;
    }
    let name_box = painter.rect(label.x, 0.0, label.width, 0.0);
    painter.cell(
        RunKey::cell(
            pane.listing.version(),
            id.0,
            COLUMN_NAME,
            Weight::Regular,
            painter.px(metrics.text_size),
        ),
        pane.listing.name(id),
        name_box,
        baseline,
        theme.name_color(selected, directory, broken),
        Align::Center,
    );
}

/// Where a grid cell's name goes: a row-high band inside the *pill*, not the cell.
///
/// **Both insets from one figure**, because they used to be two. The pill is inset
/// from the cell by half the gutter, which scales with the zoom; the name box was
/// inset by a hard four logical pixels, which does not — so the name was *wider
/// than the shape behind it*, by one pixel at the default row height and by five
/// at the largest, and a long elided name crossed the selection's border on both
/// sides.
///
/// **And it is anchored to the pill's bottom** rather than to a fraction of the
/// cell's height, so no combination of the seven zoom steps and four scales can
/// put a descender through the bottom edge — which a fraction can only be checked
/// against one combination at a time.
fn grid_label(cell: Area, metrics: &sk_ui::Metrics) -> Area {
    let inset = metrics.gutter * 0.5;
    let height = metrics.row_height;
    Area {
        x: cell.x + inset * 2.0,
        y: (cell.y + cell.height - inset * 2.0 - height).max(cell.y),
        width: (cell.width - inset * 4.0).max(1.0),
        height,
    }
}

/// What a mark needs to know that the listing alone does not say twice.
#[derive(Clone, Copy)]
struct MarkState {
    directory: bool,
    broken: bool,
}

/// An entry's mark: its thumbnail where the atlas has one, its kind mark where
/// not.
///
/// **One or the other, never both.** `Pane::thumbs` holds only keys the atlas
/// actually has — `App::settle_thumbnails` checks before it records — so this needs
/// no fallback drawn underneath, and a picture with transparency in it shows the
/// row behind rather than a file glyph through the middle of it.
///
/// **Details and Grid only, and a trail is not an omission.** A column fetches no
/// metadata at all ([ADR
/// 0015](../../../docs/adr/0015-a-column-is-a-directory-and-a-choice.md)), so it has
/// no modification time — and without one there is no key, because a thumbnail is
/// keyed on when the file changed ([ADR
/// 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)).
/// A trail would have to `statx` every row to show a picture, which is the thing
/// that decision exists to avoid.
fn mark_at(
    painter: &mut Painter<'_>,
    pane: &Pane,
    id: sk_model::EntryId,
    mark: Area,
    state: MarkState,
    types: Option<&MimeDatabase>,
) {
    let theme = *painter.theme;
    let box_ = painter.rect(mark.x, mark.y, mark.width, mark.height);
    if let Some(image) = pane.thumbs.get(&id) {
        // The tint carries only its alpha, which is opaque here: a dimmed
        // thumbnail would be a second meaning for the colour a broken link uses.
        painter.image(*image, box_, theme.palette.text);
        return;
    }
    // **A colour somebody chose beats the colour of what the thing is.** A folder
    // is blue because it is a folder; one that is red is red because a person
    // said so, and the whole point of saying so is that it stops looking like the
    // others — see `crate::colours`.
    let colour = pane.colour_of(id).filter(|_| !state.broken).map_or_else(
        || kind_colour_of(&theme, types, &pane.listing, id, state.directory, state.broken),
        |chosen| chosen.ink(&theme.palette),
    );
    if state.directory {
        painter.folder(box_, colour);
    } else {
        painter.file(box_, colour);
    }
}

/// The colour of an entry's mark.
///
/// Shared by the row and the cell, because a file that is one colour at 16 pixels
/// and another at 48 is two files as far as anybody scanning the pane is
/// concerned.
fn kind_colour_of(
    theme: &sk_ui::Theme,
    types: Option<&MimeDatabase>,
    listing: &sk_model::Listing,
    id: sk_model::EntryId,
    directory: bool,
    broken: bool,
) -> Color {
    if broken {
        return theme.palette.pending;
    }
    if directory {
        return theme.palette.kinds.folder;
    }
    // **One taxonomy, two readers.** `sk_desktop::kind_of` decides what a file is
    // and a grouped pane's headings read the same function, so a row cannot be an
    // image to its mark and a document to the heading above it.
    kind_colour(theme, sk_desktop::kind_of(listing, id, types))
}

/// One row: its shape, its mark, and its three cells.
fn row_at(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    now: i64,
    row: usize,
    context: RowContext<'_>,
    hits: &mut Hits,
) {
    let RowContext { types, naming, drop_row } = context;
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let cursor = pane.selection.cursor();
    let ring = painter.px(1.0).round().max(1.0);
    let text_px = painter.px(metrics.text_size);
    let listing = pane.listing.version();

    {
        let Some(id) = pane.listing.at(row) else { return };
        let cells = crate::groups::Bands::new(
            pane.mode,
            layout.rows(pane.mode),
            metrics.row_height,
            &pane.bands,
            pane.listing.rows(),
            pane.closed,
        );
        let top = cells.at(row, pane.scroll.offset()).y;
        let flags = pane.listing.flags(id);
        let selected = pane.selection.contains(id);
        let directory = flags.has(Flags::DIR);
        let broken = flags.has(Flags::BROKEN);

        // Across the whole row, metadata columns included: a selection that
        // stops at the name column selects a name, and what was selected is a
        // file. It used to start at the *window's* left edge and run for the
        // list's width, which was invisible until the sidebar moved the list —
        // and then the pill began under the sidebar and stopped short of the
        // size column by exactly the sidebar's width.
        let table = layout.rows(pane.mode);
        let pill = painter.rect(
            table.x + metrics.gutter * 0.5,
            top + 1.0,
            table.width - metrics.gutter,
            metrics.row_height - 2.0,
        );
        let background = theme.row_background(selected, pane.hover == Some(row));
        if !background.is_invisible() {
            painter.list.rect(pill, background, painter.px(metrics.radius));
        }
        // **No focus ring anywhere while a name is being typed.** The ring means
        // the keyboard is on this row, and while a field is open the keyboard is
        // in the field — so on the row being renamed it drew a second accent box
        // around the field's own, and on any *other* row it claimed the focus was
        // somewhere it was not. The screenshot that prompted this had both: a
        // field on one row and a ring on another. Rows keep their selected fill,
        // so which ones are chosen is never in doubt.
        if cursor == Some(id) && naming.is_none() {
            painter.list.stroke(pill, theme.palette.accent, painter.px(metrics.radius), ring);
        }
        // Where a drag would land. A filled accent rather than another ring: the
        // ring means "the keyboard is here" everywhere else in this window, and a
        // second meaning for it during a drag is a second thing to learn.
        if drop_row == Some(row) {
            painter.list.rect(
                pill,
                theme.palette.accent.with_alpha(90),
                painter.px(metrics.radius),
            );
        }

        let mark = mark_of(pane.mode, layout, cells.at(row, pane.scroll.offset()), &metrics);
        mark_at(painter, pane, id, mark, MarkState { directory, broken }, types);

        let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
        if let Some(naming) = naming.filter(|naming| naming.row == row) {
            // The field replaces the name and the metadata columns both: they
            // describe the file this row *was*, and a size beside a name being
            // changed is a fact about something that is about to stop existing
            // under that name.
            name_field_at(painter, name_field(layout, &metrics, top), naming, baseline, hits);
            return;
        }
        let name_box = painter.rect(layout.name.x, 0.0, layout.name.width - 10.0, 0.0);
        painter.cell(
            RunKey::cell(listing, id.0, COLUMN_NAME, Weight::Regular, text_px),
            pane.listing.name(id),
            name_box,
            baseline,
            theme.name_color(selected, directory, broken),
            Align::Left,
        );

        metadata_cells(painter, pane, layout, now, id, baseline, selected, directory);
    }
}

/// The size and modified cells of one row.
///
/// Split out of [`row_at`] because the size cell has four cases now: a file, a
/// folder nobody has measured yet, a folder still being measured, and a folder
/// whose total is missing something.
#[expect(clippy::too_many_arguments, reason = "one row's worth of already-resolved state")]
fn metadata_cells(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    now: i64,
    id: sk_model::EntryId,
    baseline: f32,
    selected: bool,
    directory: bool,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let text_px = painter.px(metrics.text_size);
    let listing = pane.listing.version();
    let flags = pane.listing.flags(id);

    // A row whose metadata has not arrived shows nothing in the metadata
    // columns rather than a zero. Spike B's whole argument is that the name
    // arrives first; printing `0 B` for the milliseconds in between would
    // spend that advantage on a lie.
    let pending = flags.has(Flags::META_PENDING);
    let measured = pane.listing.recursive_size(id);
    if pending && measured.is_none() {
        return;
    }
    let meta_color = if selected { theme.palette.selected_text } else { theme.palette.dim };
    if layout.size.width > 0.0 {
        let (text, size_color) = match (directory, measured) {
            // A folder total that is still climbing is drawn dimmed: it is a
            // real number and a lower bound at the same time, and rule 7 says
            // progress is honest or it is a spinner.
            (true, Some((bytes, complete))) => {
                let text = if flags.has(Flags::SIZE_PARTIAL) {
                    i18n::format("size.atleast", &[&format::size(bytes)])
                } else {
                    format::size(bytes)
                };
                (text, if complete { meta_color } else { theme.palette.pending })
            }
            (true, None) => (format::directory_size().to_owned(), meta_color),
            (false, _) => (format::size(pane.listing.size(id)), meta_color),
        };
        let box_rect = painter.rect(layout.size.x, 0.0, layout.size.width - 12.0, 0.0);
        painter.cell(
            RunKey::cell(listing, id.0, COLUMN_SIZE, Weight::Regular, text_px),
            &text,
            box_rect,
            baseline,
            size_color,
            Align::Right,
        );
    }
    if pending {
        return;
    }
    if layout.modified.width > 0.0 {
        let text = format::modified(pane.listing.mtime(id), now);
        let box_rect =
            painter.rect(layout.modified.x + 12.0, 0.0, layout.modified.width - 12.0, 0.0);
        painter.cell(
            RunKey::cell(listing, id.0, COLUMN_MODIFIED, Weight::Regular, text_px),
            &text,
            box_rect,
            baseline,
            meta_color,
            Align::Left,
        );
    }
}

/// The most rows the history list draws before it scrolls.
///
/// Eight, and `Layout::compute_with` clamps the strip to a third of the window
/// besides. A panel that grows until there is nowhere to look at files has stopped
/// being a panel; a list that scrolls is what the wheel is for.
pub(crate) const HISTORY_ROWS: usize = 8;

/// Where the history strip's parts are, in logical pixels.
///
/// **The header is a control and so is every row**, and what a press lands on has
/// to be what a person sees. The same discipline [`check_of`] and [`column_parts`]
/// are written with: an [`Area`] and a [`sk_ui::Metrics`], no `Painter` in reach, so
/// no device pixel can get in.
///
/// A row's *rectangle* is not here. The list is scrolled and clipped, so which rows
/// exist on screen is something only the frame that drew them knows — they go into
/// `Hits::history_rows` with their timeline index attached, and a press is tested
/// against those. A second copy of the arithmetic here would be the mistake this
/// type exists to avoid, one level down.
pub(crate) struct HistoryParts {
    /// The header: a title, a count, and the chevron that collapses it.
    pub(crate) header: Area,
    /// The strip the rows are drawn in and hit-tested against. Empty when closed.
    pub(crate) rows: Area,
}

impl HistoryParts {
    /// Splits the strip into its header and its list.
    pub(crate) fn of(strip: Area, metrics: &sk_ui::Metrics) -> Self {
        let header = Area { height: metrics.header_height.min(strip.height), ..strip };
        Self {
            header,
            rows: Area {
                y: header.bottom(),
                height: (strip.bottom() - header.bottom()).max(0.0),
                ..strip
            },
        }
    }
}

/// The visible history: a header that collapses, and a list that jumps.
///
/// **[Rule 3](../../../docs/03-principles.md)'s other half.** `Ctrl+Z` walks back
/// every mutation, which is the half that was done; this is the half that says
/// *what* it would walk back and lets a person land anywhere on the line rather
/// than counting keystrokes. The shape is the one every design tool uses, because
/// it is the one people have already learned: oldest at the top, the present
/// marked, and the future you undid still listed below it in grey.
fn history_at(
    painter: &mut Painter<'_>,
    edits: &crate::edit::Edits,
    strip: Area,
    open: bool,
    offset: f32,
    hovered: Option<usize>,
    hits: &mut Hits,
) {
    if strip.height <= 0.0 {
        hits.history_header = None;
        return;
    }
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let parts = HistoryParts::of(strip, &metrics);
    let ground = painter.rect(strip.x, strip.y, strip.width, strip.height);
    painter.list.rect(ground, theme.palette.chrome, 0.0);
    painter.hairline(strip.x, strip.y, strip.width, theme.palette.divider);

    // The chevron says which way pressing goes, the way a band heading's does.
    let facing = if open { Direction::Down } else { Direction::Right };
    painter.triangle(
        strip.x + metrics.gutter,
        parts.header.y + (parts.header.height - DISCLOSE) / 2.0,
        DISCLOSE,
        DISCLOSE,
        theme.palette.dim,
        facing,
    );
    let text_x = strip.x + metrics.gutter + DISCLOSE + 6.0;
    let baseline = painter.baseline(parts.header.y, parts.header.height, metrics.header_size);
    let bold = TextStyle {
        weight: Weight::Bold,
        size: metrics.header_size,
        color: theme.palette.heading,
        align: Align::Left,
    };
    let room = (strip.right() - metrics.gutter - text_x).max(1.0);
    let drawn = line(painter, i18n::lookup("history.title"), bold, text_x, room, baseline);
    let counted = i18n::plural(
        "history.count",
        edits.history.len() as u64,
        &[&format::grouped(edits.history.len())],
    );
    let dim = TextStyle::new(metrics.header_size, theme.palette.dim);
    let count_x = text_x + painter.logical(drawn) + 8.0;
    let count_room = (strip.right() - metrics.gutter - count_x).max(1.0);
    line(painter, &counted, dim, count_x, count_room, baseline);
    hits.history_header = Some(parts.header);

    hits.history_rows.clear();
    if !open || parts.rows.height <= 0.0 {
        return;
    }
    let clip = painter.rect(parts.rows.x, parts.rows.y, parts.rows.width, parts.rows.height);
    painter.list.push_clip(clip);
    let applied = edits.history.applied();
    for (index, change) in edits.history.timeline().enumerate() {
        let top = parts.rows.y + index as f32 * metrics.row_height - offset;
        if top + metrics.row_height < parts.rows.y || top > parts.rows.bottom() {
            continue;
        }
        history_row(painter, change, index, applied, hovered, top, parts.rows);
        hits.history_rows.push((index, Area { y: top, height: metrics.row_height, ..parts.rows }));
    }
    painter.list.pop_clip();
}

/// One row of the history list.
///
/// **Three states and each one means something.** An entry at or before the present
/// has happened; the one *at* the present is where the list would come back to and
/// carries the accent bar; everything after it is the future that was undone, drawn
/// dim because it is not true of the disk right now.
fn history_row(
    painter: &mut Painter<'_>,
    change: &sk_fs::ops::Change,
    index: usize,
    applied: usize,
    hovered: Option<usize>,
    top: f32,
    strip: Area,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let undone = index >= applied;
    let present = index + 1 == applied;
    if hovered == Some(index) {
        let fill = painter.rect(strip.x, top, strip.width, metrics.row_height);
        painter.list.rect(fill, theme.palette.hover, 0.0);
    }
    if present {
        // A bar at the leading edge rather than a fill: the row is not selected,
        // it is *where we are*, and a selection colour would say the wrong thing.
        let bar = painter.rect(strip.x, top, 2.0, metrics.row_height);
        painter.list.rect(bar, theme.palette.accent, 0.0);
    }
    let (key, argument) = change.summary();
    let text = i18n::format(key, &[&argument]); // skerry-allow: missing-string
    let colour = if undone { theme.palette.dim } else { theme.palette.text };
    let style = TextStyle::new(metrics.text_size, colour);
    let x = strip.x + metrics.gutter + DISCLOSE + 6.0;
    let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
    let room = (strip.right() - metrics.gutter - x).max(1.0);
    line(painter, &text, style, x, room, baseline);
}

fn scrollbar(painter: &mut Painter<'_>, pane: &Pane, layout: &PaneLayout) {
    let theme = *painter.theme;
    let table = table_of(layout, pane);
    let content = crate::groups::Bands::new(
        pane.mode,
        table,
        theme.metrics.row_height,
        &pane.bands,
        pane.listing.rows(),
        pane.closed,
    )
    .content_height();
    let Some((position, thumb)) = sk_ui::thumb(pane.scroll.offset(), content, table.height) else {
        return;
    };
    let track = layout.track();
    let rect = painter.rect(track.x, track.y + position, track.width, thumb);
    painter.list.rect(rect, theme.palette.thumb, painter.px(track.width / 2.0));
}

fn status(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &Layout,
    chrome: &Chrome<'_>,
    diagnostics: &mut Diagnostics<'_>,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.status, theme.palette.chrome);
    painter.hairline(0.0, layout.status.y, layout.status.width, theme.palette.divider);
    // **A determinate bar, or none at all** — rule 7 forbids the indeterminate
    // kind, and until something had counted the total there was nothing honest
    // to draw.
    //
    // **The strip is the bar.** It used to be two logical pixels on the divider,
    // which is honest and invisible: rule 7 also says a long operation stays
    // *visible*, and a copy with a counted ETA behind it was spending two pixels
    // saying so. Filling the strip's own background costs no layout, no new
    // surface and no second place for the fraction to be wrong — it is the same
    // number, drawn at the size of the thing it is about.
    if let Some(fraction) = chrome.progress {
        let filled = painter.rect(
            layout.status.x,
            layout.status.y,
            layout.status.width * fraction,
            layout.status.height,
        );
        painter.list.rect(filled, theme.palette.accent.with_alpha(56), 0.0);
        // A hard edge at the head of the fill, so the boundary is readable at a
        // glance rather than a gradient nobody can locate.
        let edge = painter.rect(
            layout.status.x + layout.status.width * fraction,
            layout.status.y,
            1.0,
            layout.status.height,
        );
        painter.list.rect(edge, theme.palette.accent, 0.0);
    }
    let size = metrics.header_size;
    let baseline = painter.baseline(layout.status.y, layout.status.height, size);

    let rows = pane.listing.rows();
    let selected = pane.selection.count();
    let hidden = pane.hidden_count();
    // **No special case for the filter any more.** The query used to own this line
    // and be the only place it appeared; it is a panel over the list now, and one
    // mode marker is enough.
    let left = if let Some(notice) = chrome.notice {
        notice.to_owned()
    } else if pane.loading {
        i18n::format("status.loading", &[&format::grouped(rows)])
    } else if !pane.filter.is_empty() {
        // **The noun agrees with the total, not with the matches.** "1 of 9 items
        // match" and "3 of 1 item matches" — the second is impossible, but the
        // count that governs is still the one the noun belongs to.
        let held = pane.listing.len();
        i18n::plural(
            "status.filtered",
            held as u64,
            &[&format::grouped(rows), &format::grouped(held), &pane.filter],
        )
    } else if hidden > 0 {
        // **Ahead of the plain count and behind the filter**, which is where it
        // belongs: like the filter it says the list is narrowed, and unlike the
        // filter the headings on screen already say by what. Two keys rather than
        // one appended clause, because a catalogue string that is two sentences
        // glued together is a string no translator can reorder.
        let shown = format::grouped(rows - hidden.min(rows));
        let held = format::grouped(rows);
        if selected > 0 {
            i18n::plural("status.hidden", rows as u64, &[&shown, &held, &format::grouped(selected)])
        } else {
            i18n::plural("status.hidden.none", rows as u64, &[&shown, &held])
        }
    } else if selected > 0 {
        i18n::plural(
            "status.counts",
            rows as u64,
            &[&format::grouped(rows), &format::grouped(selected)],
        )
    } else {
        i18n::plural("status.counts.none", rows as u64, &[&format::grouped(rows)])
    };
    let left = if chrome.watching { left } else { i18n::format("status.unwatched", &[&left]) };
    // Full strength while something is running. The dim grey is right for a row
    // count that is always there and wrong for a sentence about work in flight.
    let ink = if chrome.progress.is_some() { theme.palette.text } else { theme.palette.dim };
    let style = TextStyle::new(size, ink);
    line(painter, &left, style, metrics.gutter, layout.status.width * 0.6, baseline);

    // The frame cost, always visible and always honest: our own work, with the
    // wait for the compositor's presentation slot excluded, because that is
    // pacing rather than cost.
    let p50 = f64::from(diagnostics.frames.percentile(0.5)) / 1000.0;
    let p99 = f64::from(diagnostics.frames.percentile(0.99)) / 1000.0;
    let right = i18n::format("status.frame_cost", &[&format!("{p50:.2}"), &format!("{p99:.2}")]);
    let style = TextStyle::new(metrics.header_size, theme.palette.dim.with_alpha(150)).right();
    // Its own baseline: the left half changes size when the filter is open and
    // this half does not, and two strings on one strip have to sit on one line.
    let baseline = painter.baseline(layout.status.y, layout.status.height, metrics.header_size);
    // **Pause and Cancel, where the operation is.** Both were already on the
    // background menu, which satisfies the rule and not the person: somebody
    // watching a copy should not have to open a menu to stop it. Only while
    // there is something to stop, so the strip gains no permanent furniture.
    let right_edge = strip_buttons(painter, layout, chrome, hits, baseline);
    line(painter, &right, style, 0.0, right_edge, baseline);
}

/// Pause, Stop and Pick up, packed against the right of the status strip.
///
/// Returns where the frame-cost line has to stop. Its own function because
/// `status` is at the hundred-line limit, and because these three are one
/// subject: what can be done to the operation the strip is about.
fn strip_buttons(
    painter: &mut Painter<'_>,
    layout: &Layout,
    chrome: &Chrome<'_>,
    hits: &mut Hits,
    baseline: f32,
) -> f32 {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    hits.pause = None;
    hits.cancel = None;
    hits.pick_up = None;
    let mut right_edge = layout.status.width - metrics.gutter;
    let button = |painter: &mut Painter<'_>, right_edge: &mut f32, key: &'static str, ink| {
        let text = i18n::lookup(key);
        let width = text_width(painter, text, metrics.header_size, false) + 12.0;
        let area = Area {
            x: *right_edge - width,
            y: layout.status.y,
            width,
            height: layout.status.height,
        };
        line(
            painter,
            text,
            TextStyle::new(metrics.header_size, ink),
            area.x + 6.0,
            width,
            baseline,
        );
        *right_edge -= width + 8.0;
        area
    };
    // **Pick up is drawn when nothing is running**, which is the whole of what it
    // is for: the operation stopped, and rule 7 says it stays visible and
    // resumable until somebody decides otherwise.
    if chrome.resumable {
        let ink = theme.palette.accent;
        hits.pick_up = Some(button(painter, &mut right_edge, "status.pick_up", ink));
        right_edge -= metrics.gutter;
    }
    if chrome.progress.is_some() {
        let danger = theme.palette.danger;
        hits.cancel = Some(button(painter, &mut right_edge, "status.cancel", danger));
        if chrome.pausable {
            let key = if chrome.paused { "status.resume" } else { "status.pause" };
            let ink = theme.palette.accent;
            hits.pause = Some(button(painter, &mut right_edge, key, ink));
        }
        right_edge -= metrics.gutter;
    }
    right_edge
}

fn type_ahead(painter: &mut Painter<'_>, layout: &PaneLayout, typed: &str) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let area = Area {
        x: (layout.body.width - 220.0) / 2.0,
        y: layout.body.bottom() - 60.0,
        width: 220.0,
        height: 40.0,
    };
    floating_shape(painter, area);
    let baseline = painter.baseline(area.y, area.height, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(painter, typed, style, area.x + 16.0, area.width - 32.0, baseline);
}

/// How tall a row of the keyboard list may be, and how short it may become
/// before a column is added instead.
const MAX_KEY_STEP: f32 = 24.0;
const MIN_KEY_STEP: f32 = 17.0;

/// The title, the padding and the panel's own edges — everything in the keyboard
/// list that is not a row.
const KEY_CHROME: f32 = 76.0;

/// The widest a column of the keyboard list is worth making.
const KEY_COLUMN: f32 = 340.0;

/// How many columns the keyboard list takes, how many rows each holds, and how
/// tall a row is.
///
/// **Rows are given up before columns are added, and the panel never outgrows
/// its own space.** A column costs the width the labels need — "Extend selection
/// down" ellipsized into the next column's keys the first time this had three of
/// them — and the list is the one surface rule 4 is about, so it may cover the
/// path bar and the status line rather than lose a label. Past four columns the
/// rows are squeezed below the minimum instead, because four columns of
/// [`KEY_COLUMN`] is already wider than most windows and a cramped row can still
/// be read where a row off the bottom of the panel cannot.
fn keys_shape(count: usize, available: f32) -> (usize, usize, f32) {
    let count = count.max(1);
    let columns = (1..=4)
        .find(|columns| count.div_ceil(*columns) as f32 * MIN_KEY_STEP <= available)
        .unwrap_or(4);
    let per_column = count.div_ceil(columns);
    let fits = available / per_column as f32;
    (columns, per_column, fits.min(MAX_KEY_STEP).max(MIN_KEY_STEP.min(fits)))
}

fn keys_overlay(
    painter: &mut Painter<'_>,
    layout: &Layout,
    entries: &[(String, String)],
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let available = (layout.window.height - 16.0 - KEY_CHROME).max(KEY_CHROME);
    let (columns, per_column, step) = keys_shape(entries.len(), available);
    let width = (KEY_COLUMN * columns as f32).min(layout.window.width - 40.0);
    let height = per_column as f32 * step + KEY_CHROME;
    let area = Area {
        x: ((layout.window.width - width) / 2.0).max(0.0),
        y: ((layout.window.height - height) / 2.0).max(8.0),
        width,
        height,
    };
    panel_shape(painter, area, hits);

    let baseline = painter.baseline(area.y + 14.0, 24.0, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(painter, i18n::lookup("keys.title"), style, area.x + 20.0, area.width - 40.0, baseline);

    let column_width = (area.width - 40.0) / columns as f32;
    for (index, (keys, label)) in entries.iter().enumerate() {
        let x = area.x + 20.0 + (index / per_column) as f32 * column_width;
        let y = area.y + 52.0 + (index % per_column) as f32 * step;
        let baseline = painter.baseline(y, step, metrics.text_size);
        let keys_style = TextStyle::new(metrics.header_size, theme.palette.accent).bold();
        line(painter, keys, keys_style, x, column_width * 0.40, baseline);
        let label_style = TextStyle::new(metrics.text_size, theme.palette.text);
        // The label gets the rest of the column minus a gutter, because the
        // longest entry — "Extend selection down" — was being ellipsized into
        // the next column's keys in the first capture of this overlay.
        line(painter, label, label_style, x + column_width * 0.42, column_width * 0.56, baseline);
    }
}

fn stats_overlay(
    // Named apart from the shaping cache's own `hits`, which this function
    // destructures a few lines down.
    areas: &mut Hits,
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    diagnostics: &mut Diagnostics<'_>,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let step = 19.0;
    let (frames, over) = diagnostics.frames.budget();
    let p50 = f64::from(diagnostics.frames.percentile(0.5)) / 1000.0;
    let p99 = f64::from(diagnostics.frames.percentile(0.99)) / 1000.0;
    let worst = f64::from(diagnostics.frames.worst()) / 1000.0;
    let (glyphs, mask, colour) = diagnostics.atlas;
    let (runs, hits, misses) = diagnostics.shaping;
    let (workers, jobs, panics) = diagnostics.pool;
    let last = diagnostics.last;

    let rows: [(&str, String); 11] = [
        (
            i18n::lookup("stats.frame"),
            i18n::format(
                "stats.value.milliseconds3",
                &[&format!("{p50:.2}"), &format!("{p99:.2}"), &format!("{worst:.2}")],
            ),
        ),
        (
            i18n::lookup("stats.budget"),
            i18n::format(
                "stats.value.of",
                &[&format::grouped(over as usize), &format::grouped(frames as usize)],
            ),
        ),
        (i18n::lookup("stats.quads"), format!("{} · {}", format::grouped(last.quads), last.draws)),
        (
            i18n::lookup("stats.glyphs"),
            format!("{} · {}", format::grouped(glyphs), last.rasterized),
        ),
        (i18n::lookup("stats.atlas"), format!("{:.1}% / {:.1}%", mask * 100.0, colour * 100.0)),
        (
            i18n::lookup("stats.shaping"),
            format!(
                "{} · {} / {}",
                format::grouped(runs),
                format::grouped(hits as usize),
                format::grouped(misses as usize)
            ),
        ),
        (
            i18n::lookup("stats.listing"),
            i18n::format(
                "stats.value.milliseconds3",
                &[
                    &format!("{:.1}", pane.names_us as f64 / 1000.0),
                    &format!("{:.1}", pane.meta_us as f64 / 1000.0),
                    &format!("{:.1}", pane.sort_us as f64 / 1000.0),
                ],
            ),
        ),
        (
            i18n::lookup("stats.pool"),
            format!("{workers} · {} / {panics}", format::grouped(jobs as usize)),
        ),
        (
            i18n::lookup("stats.surface"),
            i18n::format(
                "stats.value.surface",
                &[
                    &diagnostics.physical.0,
                    &diagnostics.physical.1,
                    &(diagnostics.scale_120 * 100 / 120),
                ],
            ),
        ),
        (i18n::lookup("stats.capabilities"), diagnostics.capabilities.to_owned()),
        (diagnostics.backend, diagnostics.adapter.to_owned()),
    ];

    let width = 360.0_f32.min(layout.body.width - 24.0);
    let height = rows.len() as f32 * step + 46.0;
    let area =
        Area { x: layout.body.right() - width - 16.0, y: layout.body.y + 16.0, width, height };
    panel_shape(painter, area, areas);

    let baseline = painter.baseline(area.y + 10.0, 22.0, metrics.text_size);
    let title = TextStyle::new(metrics.text_size, theme.palette.text).bold();
    line(painter, i18n::lookup("stats.title"), title, area.x + 14.0, area.width - 28.0, baseline);

    for (index, (label, value)) in rows.iter().enumerate() {
        let y = area.y + 34.0 + index as f32 * step;
        let baseline = painter.baseline(y, step, metrics.header_size);
        let label_style = TextStyle::new(metrics.header_size, theme.palette.dim);
        line(painter, label, label_style, area.x + 14.0, area.width * 0.52, baseline);

        let over_budget = index == 0 && p99 * 1000.0 > f64::from(BUDGET_US);
        let colour = if over_budget { theme.palette.accent } else { theme.palette.text };
        let value_style = TextStyle::new(metrics.header_size, colour).bold().right();
        line(painter, value, value_style, 0.0, area.x + area.width - 14.0, baseline);
    }
}

/// A floating surface with a soft edge.
///
/// The shadow is three concentric translucent rectangles rather than a blur: a
/// real Gaussian would be a second pipeline and a render target, and at this
/// radius three steps are indistinguishable from one. Four quads for an overlay.
/// A panel's shadow and its ground, and a note that the pointer cannot see
/// through it.
fn panel_shape(painter: &mut Painter<'_>, area: Area, hits: &mut Hits) {
    hits.panels.push(area);
    floating_shape(painter, area);
}

/// The same shape, for something that follows the pointer rather than blocking
/// it: the drag card, and the type-ahead buffer that is gone in a second.
fn floating_shape(painter: &mut Painter<'_>, area: Area) {
    let theme = *painter.theme;
    let radius = painter.px(10.0);
    for (spread, alpha) in [(10.0, 18u8), (6.0, 22), (3.0, 26)] {
        let shadow = painter.rect(
            area.x - spread,
            area.y - spread * 0.4 + 3.0,
            area.width + spread * 2.0,
            area.height + spread * 2.0,
        );
        let colour = Color::hex(0x000000).with_alpha(alpha);
        painter.list.rect(shadow, colour, radius + painter.px(spread));
    }
    let surface = if theme.dark {
        theme.palette.chrome.mix(Color::hex(0xffffff), 0.06)
    } else {
        Color::hex(0xffffff)
    };
    let rect = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.rect(rect, surface, radius);
    painter.list.stroke(rect, theme.palette.divider, radius, painter.px(1.0).max(1.0));
}

fn fill(painter: &mut Painter<'_>, area: Area, color: Color) {
    let rect = painter.rect(area.x, area.y, area.width, area.height);
    painter.list.rect(rect, color, 0.0);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        CARET_PAD, Chrome, Diagnostics, Hits, KEY_CHROME, MAX_KEY_STEP, MIN_KEY_STEP, crumb_names,
        draw, field_scroll, keys_shape,
    };
    use crate::frames::Frames;
    use crate::layout::Layout;
    use crate::pane::{Jobs, Pane};
    use crate::panes::Panes;
    use sk_gpu::{DrawList, FrameStats};
    use sk_plat::Size;
    use sk_text::{Fonts, LayoutCache};
    use sk_ui::{Painter, Theme};
    use sk_util::{Pool, Waker};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// **A draft lands exactly where the picture it precedes will.**
    ///
    /// This is the arithmetic behind "a preview should be the same size on screen
    /// as the final": both preserve the same aspect, and fitting one aspect into
    /// one box has a single answer — so an *unclamped* fit of the draft is the
    /// clamped fit of the full render.
    ///
    /// **Within one *logical* pixel**, and the tolerance is arithmetic rather than
    /// slop: the child answers in whole pixels, so an A4 page comes back 283×400
    /// at a third-size box and 848×1200 at a full one, and those two aspects
    /// differ in the fourth decimal. Multiplied by the drawn width that is a
    /// device pixel or two — which is why the bound is a logical pixel and not a
    /// device one, since a device pixel at 200% is half of what a person sees.
    /// Asserted at three scales, because a fit that agreed only at 100% would be
    /// no use.
    #[test]
    fn a_draft_is_drawn_at_the_size_the_full_render_will_be() {
        let mut fonts = Fonts::discover().expect("no usable font");
        let mut cache = LayoutCache::new(64);
        let mut list = sk_gpu::DrawList::default();
        let theme = Theme::dark();
        for scale in [1.0_f32, 1.25, 2.0] {
            let painter = Painter {
                list: &mut list,
                fonts: &mut fonts,
                cache: &mut cache,
                theme: &theme,
                scale,
            };
            let inner = super::Area { x: 48.0, y: 82.0, width: 1002.0, height: 470.0 };
            // What the helper returns for an A4 page asked at a third of the long
            // edge, and at the whole of it.
            let draft = super::fitted(&painter, inner, (283, 400), true);
            let full = super::fitted(&painter, inner, (848, 1200), false);
            for (what, drafted, finished) in [
                ("x", draft.x, full.x),
                ("y", draft.y, full.y),
                ("width", draft.width, full.width),
                ("height", draft.height, full.height),
            ] {
                assert!(
                    (drafted - finished).abs() <= painter.px(1.0),
                    "at scale {scale} the draft's {what} is {drafted} and its \
                     replacement's is {finished}"
                );
            }

            // And a finished render smaller than the panel is *not* enlarged,
            // which is the rule the stand-in deliberately breaks.
            let small = super::fitted(&painter, inner, (64, 64), false);
            assert!(
                small.width <= painter.px(64.0) + 1.0,
                "a small picture was blown up to the panel at scale {scale}"
            );

            // **And the stand-in for a small picture does not break it either.**
            // A thumbnail strictly inside its slot is the source at its own
            // size, so enlarging it would draw the stand-in eleven times where
            // the picture will not be — for the sixty milliseconds before the
            // real one lands. The rule says so, and the geometry agrees to a
            // logical pixel.
            assert!(!super::grows((64, 64)), "a 64-pixel thumbnail is the whole picture");
            assert!(super::grows((128, 96)), "a thumbnail on its slot's edge says nothing");
            assert!(super::grows((96, 128)), "the other edge too");
            let held = super::fitted(&painter, inner, (64, 64), super::grows((64, 64)));
            assert!((small.width - held.width).abs() <= painter.px(1.0));
            assert!((small.x - held.x).abs() <= painter.px(1.0));
            assert!((small.y - held.y).abs() <= painter.px(1.0));
        }
    }

    /// The glyphs a frame would draw, as something comparable.
    ///
    /// Bit patterns rather than floats: two frames are the same frame only if
    /// every glyph lands on exactly the same coordinate, and "close enough" is
    /// not a property this test is about.
    fn frame_of(
        panes: &Panes,
        theme: &Theme,
        fonts: &mut Fonts,
        cache: &mut LayoutCache,
        hits: &mut Hits,
    ) -> Vec<(sk_text::GlyphKey, u32, u32)> {
        let logical = Size::new(1000.0, 620.0);
        let layout =
            Layout::compute_with(logical, &theme.metrics, crate::layout::Frame::None, 0.0, 0.0);
        let (mut leaves, mut dividers) = (Vec::new(), Vec::new());
        let widths = crate::layout::Columns::default();
        panes.layout(layout.content, &theme.metrics, widths, &mut leaves, &mut dividers);
        let mut list = DrawList::default();
        list.begin((1250, 775));
        let mut frames = Frames::new();
        let mut diagnostics = Diagnostics {
            frames: &mut frames,
            last: FrameStats::default(),
            atlas: (0, 0.0, 0.0),
            shaping: (0, 0, 0),
            pool: (0, 0, 0),
            adapter: "",
            backend: "",
            physical: (1250, 775),
            scale_120: 150,
            capabilities: "",
        };
        {
            let mut painter = Painter { list: &mut list, fonts, cache, theme, scale: 1.25 };
            let chrome = Chrome {
                quick_look: None,
                hovered_look: None,
                resumable: false,
                aside_grabbed: false,
                place_drop: None,
                edits: None,
                history: (false, 0.0),
                hovered_edit: None,
                keys: None,
                stats: false,
                typed: None,
                band: None,
                watching: true,
                notice: None,
                hovered_button: None,
                hovered_crumb: None,
                hovered_separator: None,
                crumb_menu: None,
                hovered_tool: None,
                hovered_mode: None,
                places: &[],
                hovered_place: None,
                drop_row: None,
                drop_tab: None,
                drop_pane: None,
                carrying: None,
                search: None,
                clash: None,
                erasing: None,
                answering: 0,
                for_the_rest: false,
                progress: None,
                paused: false,
                pausable: false,
                naming: None,
                filtering: false,
                types: None,
                window: sk_plat::WindowState::default(),
                menu: None,
                path_entry: None,
                palette: None,
                properties: None,
                openers: None,
                hint: "",
                leaves: &leaves,
                dividers: &dividers,
                hovered_divider: None,
                hovered_tab: None,
                hovered_plus: false,
                tab_drop: None,
            };
            // A fixed timestamp: the Modified column is relative to "now", and a
            // clock that ticks between two frames is not the difference this test
            // is looking for.
            draw(&mut painter, panes, &layout, 1_787_000_000, &chrome, &mut diagnostics, hits);
        }
        list.glyphs().map(|(key, x, y)| (key, x.to_bits(), y.to_bits())).collect()
    }

    fn settle(pane: &mut Pane, jobs: &Jobs, updates: &mpsc::Receiver<crate::pane::Message>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match updates.recv_timeout(Duration::from_millis(20)) {
                Ok(message) => {
                    pane.apply(message, jobs);
                }
                Err(_) if !pane.loading => break,
                Err(_) => {}
            }
        }
        while let Ok(message) = updates.try_recv() {
            pane.apply(message, jobs);
        }
    }

    /// **The message that stands in for an empty listing is inside the pane.**
    ///
    /// It was ten pixels from the left of the *window*, which with the sidebar
    /// open — the default — put it underneath the sidebar, so nobody had ever seen
    /// "This folder is empty" or a permission error in the shipping
    /// configuration. It took a column view drawing it across four other columns
    /// for anyone to notice it was being drawn at all.
    #[test]
    fn the_empty_message_is_inside_the_pane_in_every_mode() {
        let metrics = sk_ui::Metrics::default();
        // A body that starts where a sidebar leaves off, which is the case the old
        // arithmetic could not see.
        let body = crate::layout::Area { x: 220.0, y: 130.0, width: 880.0, height: 500.0 };
        let tables = [
            ("details", crate::layout::Area { width: 869.0, ..body }),
            ("grid", body),
            ("columns", crate::trail::pane_area(body, 3, 0.0)),
        ];
        for (mode, table) in tables {
            let (x, width) = super::empty_message(table, body, &metrics);
            assert!(x >= table.x, "in {mode} the message starts left of the table");
            assert!(x >= body.x, "in {mode} it starts under the sidebar");
            assert!(x + width <= body.right(), "in {mode} it runs past the pane");
            assert!(width > 100.0, "in {mode} a {width} box holds no sentence");
        }
    }

    /// **A grid cell's name stays inside the shape behind it, at every zoom.**
    ///
    /// This is a sweep rather than one case because the defect it pins was
    /// invisible at the default row height and obvious at the largest: the pill's
    /// inset scaled with the zoom and the name box's did not, so the name was one
    /// logical pixel wider than the pill at 100% and five wider at 175%, and a
    /// long elided name crossed the selection's border on both sides.
    ///
    /// It checks the vertical too, which is why the label is anchored to the
    /// pill's bottom: a fraction of the cell's height can only be checked against
    /// one combination of zoom and scale at a time, and there are twenty-eight.
    #[test]
    fn a_grid_name_never_crosses_the_cell_it_is_in() {
        for factor in [0.7_f32, 0.85, 1.0, 1.15, 1.3, 1.5, 1.75] {
            let metrics = sk_ui::Metrics::default().zoomed(factor);
            let height = (crate::cells::GRID_HEIGHT * metrics.row_height).round();
            // From the narrowest cell the layout will make to a very wide one, which
            // is what an ultrawide window with few entries produces.
            for width in [4.5 * metrics.row_height, 150.0, 240.0, 600.0] {
                let cell = super::Area { x: 17.0, y: 11.0, width, height };
                let inset = metrics.gutter * 0.5;
                let pill = super::Area {
                    x: cell.x + inset,
                    y: cell.y + inset,
                    width: cell.width - inset * 2.0,
                    height: cell.height - inset * 2.0,
                };
                let label = super::grid_label(cell, &metrics);
                let what = format!("zoom {factor}, cell {width}×{height}");
                assert!(label.x > pill.x, "the name starts left of the pill: {what}");
                assert!(
                    label.x + label.width < pill.x + pill.width,
                    "the name runs past the pill's right edge: {what}"
                );
                assert!(
                    label.y + label.height < pill.y + pill.height,
                    "the name's band crosses the pill's bottom: {what}"
                );
                assert!(label.y > pill.y, "the name is above the pill's top: {what}");

                // And the mark above it does not reach into the band — which is the
                // other half of "one cell geometry", and the failure the mark's own
                // comment records having had once already.
                // `mark_of` reads the layout only in Details and Columns, where the
                // mark sits in a column the layout owns; a grid mark is a function
                // of its cell alone.
                let mark = super::mark_of(
                    super::Mode::Grid,
                    &super::PaneLayout::default(),
                    cell,
                    &metrics,
                );
                assert!(
                    mark.y + mark.height <= label.y,
                    "the mark reaches into the name's band: {what}"
                );
            }
        }
    }

    /// **The check box is on the mark, and never bigger than a hand needs.**
    ///
    /// One rule for three modes, so the assertions are about the rule rather than
    /// about three sets of coordinates: the box sits inside the mark it covers, at
    /// the mark's own top-left, and is capped at [`super::CHECK`] so a grid badge
    /// does not swallow the picture it is a badge on.
    ///
    /// **Asserted against the mark, which is the thing that must not move.** A
    /// press is tested against exactly what is drawn — one function answers both —
    /// so what is left to check is that the answer lands on the mark and is worth
    /// pointing at.
    #[test]
    fn a_check_box_covers_a_marks_corner_in_every_mode() {
        use crate::cells::Mode;
        use crate::layout::Area;

        let metrics = sk_ui::Metrics::default();
        // A body that starts where a sidebar leaves off, and a cell as tall as a
        // grid cell rather than a row, so the two mark sizes are both exercised.
        let body = Area { x: 220.0, y: 130.0, width: 880.0, height: 500.0 };
        let (icon, name, size, modified) =
            crate::layout::columns(body, &metrics, crate::layout::Columns::default());
        let layout =
            super::PaneLayout { body, icon, name, size, modified, ..super::PaneLayout::default() };

        let row = Area { y: 200.0, height: metrics.row_height, ..body };
        let cell = Area { x: 260.0, y: 200.0, width: 110.0, height: 96.0 };
        for (mode, at) in [(Mode::Details, row), (Mode::Grid, cell), (Mode::Columns, row)] {
            let box_ = super::check_of(mode, &layout, at, &metrics);
            let mark = super::mark_of(mode, &layout, at, &metrics);
            assert!(box_.width > 0.0 && (box_.width - box_.height).abs() < f32::EPSILON);
            assert!(box_.width <= super::CHECK, "{mode:?}: a {} box is a poster", box_.width);
            assert!(box_.width >= 10.0, "{mode:?}: a {} box is a target nobody hits", box_.width);
            // On the mark's top-left, and inside it.
            assert!((box_.x - mark.x).abs() < f32::EPSILON, "{mode:?}: not at the mark's left");
            assert!((box_.y - mark.y).abs() < f32::EPSILON, "{mode:?}: not at the mark's top");
            assert!(box_.right() <= mark.right() + f32::EPSILON, "{mode:?}: wider than the mark");
            assert!(box_.bottom() <= mark.bottom() + f32::EPSILON, "{mode:?}: taller");
            // And inside the pane, which is what a press has to be able to reach.
            assert!(box_.x >= body.x, "{mode:?}: the box is under the sidebar");
            assert!(box_.right() <= body.right(), "{mode:?}: the box is off the pane");
        }
    }

    /// **The name box is a fraction of its column, at every scale.**
    ///
    /// This arithmetic mixed a device rectangle into logical values, and the shape
    /// of the failure is what makes it worth a test rather than a comment: at 100%
    /// the row drew correctly, so a golden frame at that scale said nothing. The
    /// error grew with both the scale *and* the column's distance from the left
    /// edge — at 200% the first column lost its names entirely and the second drew
    /// one character and an ellipsis.
    ///
    /// `column_parts` takes no `Painter`, so it cannot see a device pixel; what is
    /// left to assert is that the three parts tile the column in order and leave a
    /// name box worth having.
    #[test]
    fn a_column_row_gives_its_name_most_of_the_column() {
        let metrics = sk_ui::Metrics::default();
        // Four columns across a wide body, which is where the old arithmetic went
        // wrong fastest: the further right the column, the larger the device
        // number that was being subtracted from a logical one.
        let body = crate::layout::Area { x: 220.0, y: 100.0, width: 1200.0, height: 600.0 };
        for area in crate::trail::areas(body, 3, 0.0) {
            let (icon_x, text_x, width) = super::column_parts(area, &metrics);
            assert!(icon_x > area.x, "the mark is inside the column");
            assert!(text_x > icon_x + super::ICON, "the name starts after the mark");
            assert!(
                text_x + width <= area.right() - metrics.gutter,
                "the name runs past the column: {} of {}",
                text_x + width,
                area.right()
            );
            assert!(
                width > area.width * 0.6,
                "a {width} name box in a {} column leaves nothing to read",
                area.width
            );
        }
    }

    /// A directory of known contents, named after the test that asked for it.
    ///
    /// **Per test, not per process.** `cargo test` runs the tests of one crate on
    /// threads of one process, so a path keyed by the process id alone is a path
    /// two tests share — and the first thing this does is delete it. With two
    /// callers that is a `create_dir_all` racing another test's `remove_dir_all`,
    /// which failed one run in ten under load and none at all on an idle machine.
    /// See docs/12-measurement.md.
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-view-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for name in ["alpha-parent.txt", "beta-parent.txt", "gamma-parent.txt"] {
            std::fs::write(root.join(name), b"parent").unwrap();
        }
        for name in ["one-child.txt", "two-child.txt", "three-child.txt", "four-child.txt"] {
            std::fs::write(root.join("sub").join(name), b"child").unwrap();
        }
        root
    }

    /// The invariant that catches any staleness in the text layout cache, at the
    /// level where the reported defect actually lived: **a warm cache draws the
    /// same frame as a cold one.**
    ///
    /// The defect was that the cache key was the entry index and nothing else, so
    /// after navigating into a subdirectory two of its four rows were drawn with
    /// the parent's names and the parent's sizes. Every unit test passed. This is
    /// the assertion that does not.
    /// The root is a component and is drawn as the separator it is; `/` and
    /// `/home/emil` split differently and both have to read correctly.
    #[test]
    fn the_root_is_its_own_crumb_and_is_never_empty() {
        let crumbs = |path: &str| {
            let pane = Pane::new(std::path::PathBuf::from(path));
            crumb_names(&pane)
        };
        assert_eq!(crumbs("/"), vec!["/"]);
        // A path with nothing in it is still one crumb, not none: the draw loop
        // indexes `names.len() - 1` for the leaf.
        assert_eq!(crumbs(""), vec!["/"]);
        assert_eq!(crumbs("/home"), vec!["/", "home"]);
        assert_eq!(crumbs("/home/emil"), vec!["/", "home", "emil"]);
        // Trailing and doubled separators are components the path type folds
        // away, and a crumb per empty string would be a crumb per slash.
        assert_eq!(crumbs("/home//emil/"), vec!["/", "home", "emil"]);
        // A name with a space in it is one component, not two.
        assert_eq!(
            crumbs("/home/emil/VirtualBox VMs"),
            vec!["/", "home", "emil", "VirtualBox VMs"]
        );
    }

    /// Every pane's bar records its own targets, each tagged with the pane it
    /// belongs to — which is the half a rectangle alone cannot say.
    ///
    /// A frame that recorded the focused pane's crumbs and nobody else's would
    /// leave the other half of a split with a path drawn on screen and nothing
    /// in it to click, which reads as a dead bar rather than as a bar that is
    /// somebody else's. And two panes' regions must not overlap, or a click in
    /// one bar would navigate the other.
    #[test]
    fn each_pane_bar_records_its_own_targets_against_its_own_pane() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let root = fixture("pane-bars");
        let waker = Waker::new().expect("eventfd");
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs {
            pool: std::sync::Arc::new(Pool::with_width(4)),
            wake: waker.handle(),
            sender,
            mime: None,
        };
        let theme = Theme::dark();
        let mut cache = LayoutCache::new(4096);

        let mut panes = Panes::new(Pane::new(root.clone()));
        panes.focused_mut().reload(&jobs);
        settle(panes.focused_mut(), &jobs, &updates);
        // Stacked, so both bars are full width, and the lower one a level deeper
        // — a depth the shallower pane's bar cannot produce.
        panes.split(crate::panes::Axis::Below, &jobs);
        panes.focused_mut().open(root.join("sub"), &jobs);
        settle(panes.focused_mut(), &jobs, &updates);

        let mut hits = Hits::default();
        frame_of(&panes, &theme, &mut fonts, &mut cache, &mut hits);

        let ids: Vec<_> = panes.leaves().map(|leaf| leaf.id).collect();
        assert_eq!(ids.len(), 2, "the tree did not split");
        for id in &ids {
            let drawn = |hits: &[super::BarHit]| {
                hits.iter().filter(|hit| hit.leaf == *id && hit.area.width > 0.0).count()
            };
            assert!(drawn(&hits.crumbs) > 0, "a pane's bar recorded no crumb of its own");
            assert!(drawn(&hits.separators) > 0, "a pane's bar recorded no chevron of its own");
        }
        let depth = |id| hits.crumbs.iter().filter(|hit| hit.leaf == id).count();
        assert_eq!(
            depth(ids[1]),
            depth(ids[0]) + 1,
            "both bars reported the same path, so one of them is not its pane's"
        );
        for mine in hits.crumbs.iter().filter(|hit| hit.leaf == ids[0] && hit.area.width > 0.0) {
            let middle = (mine.area.x + mine.area.width / 2.0, mine.area.y + 1.0);
            assert!(
                !hits.crumbs.iter().any(|other| other.leaf != mine.leaf
                    && other.area.width > 0.0
                    && other.area.contains(middle.0, middle.1)),
                "two panes' crumbs overlap, so a click would navigate the wrong one"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_warm_cache_draws_the_same_frame_as_a_cold_one_after_navigating() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let root = fixture("warm-cache");
        let waker = Waker::new().expect("eventfd");
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs {
            pool: std::sync::Arc::new(Pool::with_width(4)),
            wake: waker.handle(),
            sender,
            mime: None,
        };
        let theme = Theme::dark();

        let mut warm = LayoutCache::new(4096);
        let mut panes = Panes::new(Pane::new(root.clone()));
        panes.focused_mut().reload(&jobs);
        settle(panes.focused_mut(), &jobs, &updates);

        // Draw the parent, which fills the cache with the parent's rows.
        let parent = frame_of(&panes, &theme, &mut fonts, &mut warm, &mut Hits::default());
        assert!(!parent.is_empty(), "the parent drew no text at all");

        let pane = panes.focused_mut();
        let sub = (0..pane.listing.len())
            .map(|index| sk_model::EntryId(index as u32))
            .find(|id| pane.listing.name(*id) == "sub")
            .expect("sub");
        pane.selection.replace_with(sub);
        assert!(pane.open_focused(&jobs));
        settle(panes.focused_mut(), &jobs, &updates);

        let with_warm_cache = frame_of(&panes, &theme, &mut fonts, &mut warm, &mut Hits::default());
        let mut cold = LayoutCache::new(4096);
        let with_cold_cache = frame_of(&panes, &theme, &mut fonts, &mut cold, &mut Hits::default());

        assert_eq!(
            with_warm_cache.len(),
            with_cold_cache.len(),
            "a warm cache drew a different number of glyphs than a cold one"
        );
        assert_eq!(
            with_warm_cache, with_cold_cache,
            "a warm cache drew a different frame than a cold one — the layout \
             cache served a previous listing's rows"
        );
        assert_ne!(with_warm_cache, parent, "the child frame is the parent frame");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `Ctrl+L` opens with the whole path and the caret at the end, and a path
    /// longer than the bar is ordinary — so the field that could not scroll drew
    /// an ellipsis where the caret was and every keystroke edited an invisible
    /// tail.
    #[test]
    fn a_field_scrolls_only_as_far_as_the_caret_needs() {
        // Room to spare: nothing moves, and the head of the text stays put.
        assert!(field_scroll(0.0, 200.0).abs() < f32::EPSILON);
        assert!(field_scroll(150.0, 200.0).abs() < f32::EPSILON);
        // Past the edge: shifted just enough to leave the caret inside.
        assert!((field_scroll(200.0, 200.0) - CARET_PAD).abs() < f32::EPSILON);
        assert!((field_scroll(500.0, 200.0) - 300.0 - CARET_PAD).abs() < f32::EPSILON);
        // A box with no room shifts by the pad and clips to nothing, which is
        // what a window narrower than its own chrome deserves — and never a
        // negative shift, which would draw the text to the right of its box.
        assert!(field_scroll(0.0, 0.0) >= 0.0);
        assert!(field_scroll(0.0, -50.0) >= 0.0);
    }

    /// The keyboard list grows by a row every time a command is added, and it has
    /// already lost its last six once. Whatever the count and whatever the
    /// window, every row has to be inside the panel and the panel inside the
    /// window.
    #[test]
    fn the_keyboard_list_always_fits_the_window_it_is_drawn_in() {
        for height in [200.0f32, 300.0, 520.0, 720.0, 1400.0, 2160.0] {
            let available = (height - 16.0 - KEY_CHROME).max(KEY_CHROME);
            for count in [1usize, 12, 40, 41, 80, 200, 1000] {
                let (columns, per_column, step) = keys_shape(count, available);
                assert!((1..=4).contains(&columns), "{count} at {height}: {columns} columns");
                assert!(
                    columns * per_column >= count,
                    "{count} at {height}: {columns}x{per_column} holds fewer than {count}"
                );
                assert!(step > 0.0, "{count} at {height}: a row of {step}");
                assert!(step <= MAX_KEY_STEP, "{count} at {height}: a row of {step}");
                let rows = per_column as f32 * step;
                assert!(
                    rows <= available + 0.01,
                    "{count} at {height}: {rows} of rows in {available} of room"
                );
                // And the step is only squeezed below the minimum when four
                // columns of the minimum genuinely will not fit.
                if step < MIN_KEY_STEP {
                    assert_eq!(columns, 4, "{count} at {height}: squeezed at {columns} columns");
                }
            }
        }
    }
}
