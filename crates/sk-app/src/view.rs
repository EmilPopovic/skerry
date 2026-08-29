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
use sk_ui::{Align, Direction, Painter, RunKey, TextStyle, format, visible_rows};

use crate::edit::Naming;
use crate::field::Field;
use crate::frames::{BUDGET_US, Frames};
use crate::layout::{Area, Button, Layout, Tool};
use crate::menu::Menu;
use crate::palette::Palette;
use crate::pane::Pane;
use crate::panes::{Divider, PaneId, PaneLayout, Panes, SplitId};
use crate::sidebar::{Entry, Mark, Section};

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
    /// The path component under the pointer.
    pub hovered_crumb: Option<usize>,
    /// The navigation button under the pointer.
    pub hovered_tool: Option<Tool>,
    /// The sidebar's contents, once the job that reads them has finished.
    ///
    /// A slice rather than the model, because the view needs the arrangement and
    /// not the channel the arrangement arrived on — which is what lets a capture
    /// build one synchronously and have a golden frame of it.
    pub places: &'a [Section],
    /// The sidebar row under the pointer.
    pub hovered_place: Option<usize>,
    /// The row a drag of files would drop into, while one is in flight.
    pub drop_row: Option<(PaneId, usize)>,
    /// The pane whose whole list would take a drop, ringed rather than a row
    /// highlighted, because the target is the directory it is showing.
    pub drop_pane: Option<PaneId>,
    /// The name-taken question, while one is open.
    pub clash: Option<&'a crate::edit::Clash>,
    /// Which answer it is on.
    pub answering: usize,
    /// Whether the answer would cover the rest of the queue.
    pub for_the_rest: bool,
    /// How far along a running operation is, once that is knowable.
    pub progress: Option<f32>,
    /// The breadcrumb separator under the pointer.
    pub hovered_separator: Option<usize>,
    /// The sibling menu a separator has opened, while one is open.
    pub crumb_menu: Option<&'a crate::crumbs::CrumbMenu>,
    /// The name being typed, if one is.
    pub naming: Option<&'a Naming>,
    /// Whether each of the four navigation buttons can run right now, from the
    /// command registry rather than from a second copy of the same question.
    pub tools: [bool; 4],
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

/// Hit regions this frame produced, for the next event to test against.
///
/// A breadcrumb's rectangle is a function of the shaped width of a directory
/// name, so it is known where the text is drawn and nowhere else. Recording it
/// as the frame is built is the immediate-mode answer; the alternative is
/// measuring every component twice, once to draw and once to hit-test, and the
/// two measurements drifting apart at 125%.
#[derive(Default)]
pub struct Hits {
    /// One rectangle per path component, root first. The index is the number of
    /// components to keep when it is clicked.
    pub crumbs: Vec<Area>,
    /// One rectangle per *drawn* row of a separator's menu, in draw order — so
    /// the index a click reports is an offset from the menu's scroll.
    pub crumb_menu: Vec<Area>,
    /// One rectangle per **separator**, indexed by the component on its left.
    ///
    /// A separator is a target of its own — clicking one lists the directories
    /// inside the component to its left, which are the siblings of the component
    /// to its right. There is one after the last component too: its list is the
    /// children of where you already are.
    pub separators: Vec<Area>,
    /// One rectangle per sidebar row, in the flattened order the model uses, so
    /// the index a click reports is an index into it.
    pub places: Vec<Area>,
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
    /// One rectangle per answer to an open name-taken question, in panel order.
    pub answers: Vec<Area>,
    /// Its "for the rest of them" toggle, when there is a rest.
    pub for_the_rest: Option<Area>,
    /// The palette's query field, so a click into it can place the caret.
    pub palette_field: Option<Area>,
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
    sidebar(painter, pane, layout, chrome, hits);
    path_bar(painter, pane, layout, chrome, hits);
    tool_buttons(painter, layout, chrome);
    window_buttons(painter, layout, chrome);
    for leaf in chrome.leaves {
        let Some(tabs) = panes.leaf(leaf.id) else { continue };
        let has_keyboard = leaf.id == focus;
        tab_strip(painter, tabs, leaf, chrome, has_keyboard);
        header(painter, tabs.showing(), leaf);
        body(painter, tabs.showing(), leaf, now, chrome, has_keyboard);
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
    status(painter, pane, layout, chrome, diagnostics);
    if chrome.filtering || !pane.filter.is_empty() {
        find_popup(painter, pane, here, chrome.filtering);
    }
    if chrome.stats {
        stats_overlay(painter, pane, here, diagnostics);
    }
    if let Some(rows) = chrome.keys {
        keys_overlay(painter, layout, rows);
    }
    if let Some(menu) = chrome.crumb_menu {
        crumb_menu(painter, layout, menu, hits);
    }
    if let Some(palette) = chrome.palette {
        command_palette(painter, layout, palette, hits);
    }
    if let Some(panel) = chrome.properties {
        properties(painter, layout, panel, now);
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
    if let Some(typed) = chrome.typed {
        type_ahead(painter, here, typed);
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

fn path_bar(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &Layout,
    chrome: &Chrome<'_>,
    hits: &mut Hits,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    // The whole strip, buttons included: the path area stops short of them and a
    // gap in the title bar's background is the first thing an eye finds.
    let strip = Area { x: 0.0, width: layout.path.right() + strip_width(layout), ..layout.path };
    fill(painter, strip, theme.palette.chrome);
    painter.hairline(0.0, strip.bottom() - 1.0, strip.width, theme.palette.divider);

    let baseline = painter.baseline(layout.path.y, layout.path.height, metrics.path_size);
    let hint_style = TextStyle::new(metrics.header_size, theme.palette.dim.with_alpha(150)).right();
    let hint_width = line(
        painter,
        chrome.hint,
        hint_style,
        layout.path.x,
        layout.path.width - metrics.gutter,
        baseline,
    ) / painter.scale;

    hits.crumbs.clear();
    hits.separators.clear();
    if let Some(field) = chrome.path_entry {
        // No breadcrumbs and no hit regions for them: the strip is a text field
        // now, and a click in it belongs to the caret.
        path_entry(painter, layout, field, baseline);
        return;
    }
    let available = layout.path.width - metrics.gutter * 2.0 - hint_width - 12.0;
    let names = crumb_names(pane);
    let widths = crumb_widths(painter, &names, metrics.path_size);

    let first = first_crumb(painter, &names, &widths, available, metrics.path_size);
    let elided = first > 1;
    let mut cursor = layout.path.x + metrics.gutter;

    // Clipped to its own region, so a path that will not fit cannot draw over
    // the keyboard hint or under the window buttons.
    let region = painter.rect(layout.path.x, layout.path.y, layout.path.width, layout.path.height);
    painter.list.push_clip(region);
    for (index, name) in names.iter().enumerate() {
        if index > 0 && index < first {
            continue;
        }
        let last = index + 1 == names.len();
        let width = widths[index];
        let area = Area { x: cursor, y: layout.path.y, width, height: layout.path.height };
        while hits.crumbs.len() < index {
            hits.crumbs.push(Area::default());
        }
        hits.crumbs.push(area);

        let text_cell = width - SEPARATOR;
        if chrome.hovered_crumb == Some(index) && text_cell > 0.0 {
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
        let cell =
            Area { x: cursor, y: layout.path.y, width: SEPARATOR, height: layout.path.height };
        while hits.separators.len() < index {
            hits.separators.push(Area::default());
        }
        hits.separators.push(cell);
        if chrome.hovered_separator == Some(index)
            || chrome.crumb_menu.map(|it| it.depth) == Some(index)
        {
            let shape = painter.rect(cell.x, cell.y + 6.0, cell.width, cell.height - 12.0);
            painter.list.rect(shape, theme.palette.hover, painter.px(metrics.radius));
        }
        let mark = if index == 0 {
            i18n::lookup("chrome.crumb_root")
        } else {
            i18n::lookup("chrome.crumb_separator")
        };
        let shade = if chrome.hovered_separator == Some(index) {
            theme.palette.text
        } else {
            theme.palette.dim.with_alpha(150)
        };
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
            let cell = Area { x: cursor, y: layout.path.y, width: SEPARATOR, ..layout.path };
            line(painter, mark, style.centred(), cell.x, cell.width, baseline);
            cursor += SEPARATOR;
        }
    }
    painter.list.pop_clip();
}

/// The path being typed, across the strip the breadcrumbs were on.
fn path_entry(painter: &mut Painter<'_>, layout: &Layout, field: &Field, baseline: f32) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let area = path_field(layout, &metrics);
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
pub fn path_field(layout: &Layout, metrics: &sk_ui::Metrics) -> Area {
    Area {
        x: layout.path.x + metrics.gutter * 0.5,
        y: layout.path.y + 6.0,
        width: (layout.path.width - metrics.gutter).max(0.0),
        height: (layout.path.height - 12.0).max(0.0),
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
    panel_shape(painter, area);

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
        let inner = area.width - INSET * 2.0;
        let keys_width = text_width(painter, &item.keys, metrics.header_size, true);
        let name_style = TextStyle::new(metrics.text_size, name_colour);
        let x = area.x + INSET;
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
        y: layout.path.bottom(),
        width,
        height,
    };
    panel_shape(painter, area);

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

/// The Properties panel: a title, and a label-and-value row per fact.
///
/// Centred, because it is a statement rather than a thing to aim at — nothing in
/// it is clickable, and a panel anchored to a row would move when the row did.
fn properties(
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
    panel_shape(painter, area);

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
    panel_shape(painter, area);

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
    panel_shape(painter, area);
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
        let ring = painter.px(1.0).round().max(1.0);
        painter.list.stroke(box_, theme.palette.dim, painter.px(3.0), ring);
        if for_the_rest {
            let tick = painter.rect(left + 4.0, top + 10.0, 6.0, 6.0);
            painter.list.rect(tick, theme.palette.accent, painter.px(1.0));
        }
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
    panel_shape(painter, area);

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
fn find_popup(painter: &mut Painter<'_>, pane: &Pane, layout: &PaneLayout, focused: bool) {
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
    panel_shape(painter, area);
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
    layout: &PaneLayout,
    naming: &Naming,
    top: f32,
    baseline: f32,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    // `name_field` derives the row's top from an index and a scroll offset; here
    // the top is already in hand, so the one rectangle is taken from it and only
    // its `y` replaced. Both callers still read the same width and inset.
    let area = Area { y: top + 2.0, ..name_field(layout, &metrics, naming.row, 0.0) };
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
pub fn name_field(layout: &PaneLayout, metrics: &sk_ui::Metrics, row: usize, offset: f32) -> Area {
    let top = layout.body.y + row as f32 * metrics.row_height - offset;
    Area {
        x: layout.name.x - 4.0,
        y: top + 2.0,
        width: (layout.name.width + metrics.size_column * 0.5)
            .min(layout.body.right() - layout.name.x + 4.0),
        height: metrics.row_height - 4.0,
    }
}

/// The colour of the mark at the head of a row, from the file's kind.
///
/// **By name only.** `MimeDatabase::by_name` is a hash lookup and, for the few
/// globs that are not `*.ext`, a short scan — no allocation and no syscall, which
/// is what lets this run once per visible row per frame. `of` would sniff the
/// first bytes of a file with an unhelpful name, and that is a `read` on the
/// frame loop.
///
/// Grey until the databases arrive, which is the first few milliseconds after
/// start-up. A row that changes colour once is better than a first frame that
/// waits for `globs2`.
fn kind_color(theme: &sk_ui::Theme, types: Option<&MimeDatabase>, name: &str) -> Color {
    let kinds = theme.palette.kinds;
    let Some(mime) = types.and_then(|types| types.by_name(name)) else {
        return kinds.unknown;
    };
    match sk_desktop::category(mime) {
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

/// The width the window buttons take, or zero when the compositor draws them.
fn strip_width(layout: &Layout) -> f32 {
    layout.buttons.iter().map(|button| button.width).sum()
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
    let area = layout.sidebar;
    if area.width <= 0.0 {
        return;
    }
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, area, theme.palette.sidebar);
    // The divider is on the sidebar's edge rather than the list's, so the list
    // owns its own left gutter and a row's hover shape does not touch it.
    let edge = painter.rect(area.right() - 1.0, area.y, 1.0, area.height);
    painter.list.rect(edge, theme.palette.divider, 0.0);

    let clip = painter.rect(area.x, area.y, area.width - 1.0, area.height);
    painter.list.push_clip(clip);
    let current = crate::sidebar::current(chrome.places, &pane.path);
    let mut y = area.y + 6.0;
    let mut index = 0;
    for section in chrome.places {
        let baseline = painter.baseline(y, metrics.sidebar_heading + 12.0, metrics.sidebar_heading);
        let style = TextStyle::new(metrics.sidebar_heading, theme.palette.heading).bold();
        line(
            painter,
            i18n::lookup(section.title),
            style,
            area.x + 14.0,
            area.width - 20.0,
            baseline,
        );
        y += metrics.sidebar_heading + 12.0;

        for entry in &section.entries {
            let row = Area { x: area.x, y, width: area.width - 1.0, height: metrics.sidebar_row };
            hits.places.push(row);
            place_row(
                painter,
                entry,
                row,
                current == Some(index),
                chrome.hovered_place == Some(index),
            );
            y += metrics.sidebar_row;
            index += 1;
        }
        y += 8.0;
    }
    painter.list.pop_clip();
}

/// One row of the sidebar.
fn place_row(painter: &mut Painter<'_>, entry: &Entry, row: Area, current: bool, hovered: bool) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let pill = painter.rect(row.x + 6.0, row.y + 1.0, row.width - 12.0, row.height - 2.0);
    if current {
        painter.list.rect(
            pill,
            theme.palette.accent.with_alpha(if theme.dark { 56 } else { 40 }),
            painter.px(metrics.radius),
        );
    } else if hovered {
        painter.list.rect(pill, theme.palette.hover, painter.px(metrics.radius));
    }

    let icon = painter.rect(row.x + 14.0, row.y + (row.height - 15.0) / 2.0, 15.0, 15.0);
    let kinds = theme.palette.kinds;
    match entry.mark {
        Mark::Volume => painter.disk(icon, if entry.remote { kinds.audio } else { kinds.unknown }),
        Mark::Place(_) => painter.folder(icon, kinds.folder),
    }

    let ink = if current { theme.palette.selected_text } else { theme.palette.text };
    let baseline = painter.baseline(row.y, row.height, metrics.text_size);
    let box_rect = painter.rect(row.x + 37.0, 0.0, row.width - 47.0, 0.0);
    painter.label(&entry.label, TextStyle::new(metrics.text_size, ink), box_rect, baseline);
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
fn tool_buttons(painter: &mut Painter<'_>, layout: &Layout, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    for ((tool, area), enabled) in Tool::ALL.into_iter().zip(layout.tools).zip(chrome.tools) {
        if area.width <= 0.0 {
            continue;
        }
        let hovered = chrome.hovered_tool == Some(tool);
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
        let colour = if active { theme.palette.text } else { theme.palette.dim };
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
    let columns: [(SortColumn, Area, Align, f32); 3] = [
        (SortColumn::Name, layout.name, Align::Left, 0.0),
        (SortColumn::Size, layout.size, Align::Right, 12.0),
        (SortColumn::Modified, layout.modified, Align::Left, 12.0),
    ];

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

fn body(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    now: i64,
    chrome: &Chrome<'_>,
    has_keyboard: bool,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let clip = painter.rect(layout.body.x, layout.body.y, layout.body.width, layout.body.height);
    painter.list.push_clip(clip);

    let rows = pane.listing.rows();
    let range = visible_rows(pane.scroll.offset(), layout.body.height, metrics.row_height, rows);
    for row in range {
        let context = RowContext {
            types: chrome.types,
            naming: chrome.naming.filter(|_| has_keyboard),
            drop_row: chrome.drop_row.filter(|(id, _)| *id == layout.id).map(|(_, row)| row),
        };
        row_at(painter, pane, layout, now, row, context);
    }

    if rows == 0 && !pane.loading {
        let message = pane
            .problem
            .as_ref()
            .map_or_else(|| i18n::lookup("status.empty").to_owned(), sk_model::Problem::message);
        let baseline =
            painter.baseline(layout.body.y + 36.0, metrics.row_height, metrics.path_size);
        let style = TextStyle::new(metrics.path_size, theme.palette.dim);
        let width = layout.body.width - metrics.gutter * 2.0;
        line(painter, &message, style, metrics.gutter, width, baseline);
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

/// One row: its shape, its mark, and its three cells.
fn row_at(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &PaneLayout,
    now: i64,
    row: usize,
    context: RowContext<'_>,
) {
    let RowContext { types, naming, drop_row } = context;
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let cursor = pane.selection.cursor();
    let ring = painter.px(1.0).round().max(1.0);
    let icon_size = 16.0;
    let text_px = painter.px(metrics.text_size);
    let listing = pane.listing.version();

    {
        let Some(id) = pane.listing.at(row) else { return };
        let top = layout.body.y + row as f32 * metrics.row_height - pane.scroll.offset();
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
        let pill = painter.rect(
            layout.body.x + metrics.gutter * 0.5,
            top + 1.0,
            layout.body.width - metrics.gutter,
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

        let icon_box = painter.rect(
            layout.icon.x + (layout.icon.width - icon_size) / 2.0,
            top + (metrics.row_height - icon_size) / 2.0,
            icon_size,
            icon_size,
        );
        let icon_color = if broken {
            theme.palette.pending
        } else if directory {
            theme.palette.kinds.folder
        } else {
            // The mode bit before the name: a program is usually called
            // something with no extension at all — `runme`, `configure`, half of
            // `/usr/bin` — so a name-only answer calls it unknown, and what it
            // is is the one thing about it worth marking.
            let by_name = kind_color(&theme, types, pane.listing.name(id));
            if flags.has(Flags::EXEC) && by_name == theme.palette.kinds.unknown {
                theme.palette.kinds.executable
            } else {
                by_name
            }
        };
        if directory {
            painter.folder(icon_box, icon_color);
        } else {
            painter.file(icon_box, icon_color);
        }

        let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
        if let Some(naming) = naming.filter(|naming| naming.row == row) {
            // The field replaces the name and the metadata columns both: they
            // describe the file this row *was*, and a size beside a name being
            // changed is a fact about something that is about to stop existing
            // under that name.
            name_field_at(painter, layout, naming, top, baseline);
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

fn scrollbar(painter: &mut Painter<'_>, pane: &Pane, layout: &PaneLayout) {
    let theme = *painter.theme;
    let content = pane.listing.rows() as f32 * theme.metrics.row_height;
    let Some((position, thumb)) = sk_ui::thumb(pane.scroll.offset(), content, layout.body.height)
    else {
        return;
    };
    let width = 8.0;
    let rect =
        painter.rect(layout.body.right() - width - 3.0, layout.body.y + position, width, thumb);
    painter.list.rect(rect, theme.palette.thumb, painter.px(width / 2.0));
}

fn status(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &Layout,
    chrome: &Chrome<'_>,
    diagnostics: &mut Diagnostics<'_>,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.status, theme.palette.chrome);
    painter.hairline(0.0, layout.status.y, layout.status.width, theme.palette.divider);
    // **A determinate bar, or none at all** — rule 7 forbids the indeterminate
    // kind, and until something had counted the total there was nothing honest
    // to draw. On the divider rather than beside the text: it is the full width
    // of the window, so a glance reads it without finding it first.
    if let Some(fraction) = chrome.progress {
        let bar = painter.rect(0.0, layout.status.y, layout.status.width * fraction, 2.0);
        painter.list.rect(bar, theme.palette.accent, 0.0);
    }
    let size = metrics.header_size;
    let baseline = painter.baseline(layout.status.y, layout.status.height, size);

    let rows = pane.listing.rows();
    let selected = pane.selection.count();
    // **No special case for the filter any more.** The query used to own this line
    // and be the only place it appeared; it is a panel over the list now, and one
    // mode marker is enough.
    let left = if let Some(notice) = chrome.notice {
        notice.to_owned()
    } else if pane.loading {
        i18n::format("status.loading", &[&format::grouped(rows)])
    } else if !pane.filter.is_empty() {
        i18n::format(
            "status.filtered",
            &[&format::grouped(rows), &format::grouped(pane.listing.len()), &pane.filter],
        )
    } else if selected > 0 {
        i18n::format("status.counts", &[&format::grouped(rows), &format::grouped(selected)])
    } else {
        i18n::format("status.counts.none", &[&format::grouped(rows)])
    };
    let left = if chrome.watching { left } else { i18n::format("status.unwatched", &[&left]) };
    let style = TextStyle::new(size, theme.palette.dim);
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
    line(painter, &right, style, 0.0, layout.status.width - metrics.gutter, baseline);
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
    panel_shape(painter, area);
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

fn keys_overlay(painter: &mut Painter<'_>, layout: &Layout, entries: &[(String, String)]) {
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
    panel_shape(painter, area);

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
    panel_shape(painter, area);

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
fn panel_shape(painter: &mut Painter<'_>, area: Area) {
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
    ) -> Vec<(sk_text::GlyphKey, u32, u32)> {
        let logical = Size::new(1000.0, 620.0);
        let layout =
            Layout::compute_with(logical, &theme.metrics, crate::layout::Frame::None, false);
        let (mut leaves, mut dividers) = (Vec::new(), Vec::new());
        panes.layout(layout.content, &theme.metrics, &mut leaves, &mut dividers);
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
                places: &[],
                hovered_place: None,
                drop_row: None,
                drop_pane: None,
                clash: None,
                answering: 0,
                for_the_rest: false,
                progress: None,
                naming: None,
                tools: [true, true, true, true],
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
            draw(
                &mut painter,
                panes,
                &layout,
                1_787_000_000,
                &chrome,
                &mut diagnostics,
                &mut Hits::default(),
            );
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

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-view-{}", std::process::id()));
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

    #[test]
    fn a_warm_cache_draws_the_same_frame_as_a_cold_one_after_navigating() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let root = fixture();
        let waker = Waker::new().expect("eventfd");
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: Pool::with_width(4), wake: waker.handle(), sender };
        let theme = Theme::dark();

        let mut warm = LayoutCache::new(4096);
        let mut panes = Panes::new(Pane::new(root.clone()));
        panes.focused_mut().reload(&jobs);
        settle(panes.focused_mut(), &jobs, &updates);

        // Draw the parent, which fills the cache with the parent's rows.
        let parent = frame_of(&panes, &theme, &mut fonts, &mut warm);
        assert!(!parent.is_empty(), "the parent drew no text at all");

        let pane = panes.focused_mut();
        let sub = (0..pane.listing.len())
            .map(|index| sk_model::EntryId(index as u32))
            .find(|id| pane.listing.name(*id) == "sub")
            .expect("sub");
        pane.selection.replace_with(sub);
        assert!(pane.open_focused(&jobs));
        settle(panes.focused_mut(), &jobs, &updates);

        let with_warm_cache = frame_of(&panes, &theme, &mut fonts, &mut warm);
        let mut cold = LayoutCache::new(4096);
        let with_cold_cache = frame_of(&panes, &theme, &mut fonts, &mut cold);

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
