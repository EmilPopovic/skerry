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
use crate::frames::{BUDGET_US, Frames};
use crate::layout::{Area, Button, Layout, Tool};
use crate::menu::Menu;
use crate::pane::Pane;
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
    /// One rectangle per sidebar row, in the flattened order the model uses, so
    /// the index a click reports is an index into it.
    pub places: Vec<Area>,
    /// One rectangle per open context-menu row, in the menu's own order.
    ///
    /// The menu's width is the width of its widest shaped row, so this is the
    /// same story as the breadcrumbs': the rectangle is known where the text is
    /// measured, and the next event tests against it.
    pub menu: Vec<Area>,
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
    pane: &Pane,
    layout: &Layout,
    now: i64,
    chrome: &Chrome<'_>,
    diagnostics: &mut Diagnostics<'_>,
    hits: &mut Hits,
) {
    sidebar(painter, pane, layout, chrome, hits);
    path_bar(painter, pane, layout, chrome, hits);
    tool_buttons(painter, layout, chrome);
    window_buttons(painter, layout, chrome);
    header(painter, pane, layout);
    body(painter, pane, layout, now, chrome);
    status(painter, pane, layout, chrome, diagnostics);
    if chrome.stats {
        stats_overlay(painter, pane, layout, diagnostics);
    }
    if let Some(rows) = chrome.keys {
        keys_overlay(painter, layout, rows);
    }
    if let Some(typed) = chrome.typed {
        type_ahead(painter, layout, typed);
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
        i18n::lookup("chrome.hint"),
        hint_style,
        layout.path.x,
        layout.path.width - metrics.gutter,
        baseline,
    ) / painter.scale;

    hits.crumbs.clear();
    let available = layout.path.width - metrics.gutter * 2.0 - hint_width - 12.0;
    let names = crumb_names(pane);
    let widths: Vec<f32> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let bold = index + 1 == names.len();
            crumb_width(painter, name, metrics.path_size, bold)
                + CRUMB_PAD * 2.0
                + if index + 1 == names.len() || index == 0 { 0.0 } else { SEPARATOR }
        })
        .collect();

    // From the right, because the leaf answers "where am I" and everything to
    // its left is context: a path too long for the bar loses its middle and
    // keeps its end, which is the half a person is reading. The root stays,
    // because "/" is one glyph and dropping it makes an absolute path look
    // relative.
    let root = widths.first().copied().unwrap_or_default();
    let mut first = 1;
    // The `… /` mark has a width of its own and the fitting never counted it, so
    // an elided bar overflowed by exactly that much. Reserved up front: it costs
    // one component's worth of room in the case where there is already not
    // enough, which is the case it exists for.
    let mut total = root
        + crumb_width(painter, i18n::lookup("chrome.crumb_elided"), metrics.path_size, false)
        + CRUMB_PAD;
    for index in (1..names.len()).rev() {
        if total + widths[index] > available && index > 1 {
            first = index + 1;
            break;
        }
        total += widths[index];
    }
    // **Never past the leaf.** A leaf wider than the whole bar used to set
    // `first` to `names.len()`, and the draw loop then skipped every index from
    // one — including the leaf. The bar read `/ … /` with the current directory
    // absent and nothing clickable, at window widths the toplevel's own 320 px
    // minimum can reach. It is drawn and clipped instead, which is what the
    // clip region below is for.
    first = first.min(names.len().saturating_sub(1)).max(1);
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

        if chrome.hovered_crumb == Some(index) && !last {
            let shape =
                painter.rect(area.x, area.y + 6.0, area.width - SEPARATOR, area.height - 12.0);
            painter.list.rect(shape, theme.palette.hover, painter.px(metrics.radius));
        }
        let style = if last {
            TextStyle::new(metrics.path_size, theme.palette.text).bold()
        } else {
            TextStyle::new(metrics.path_size, theme.palette.dim)
        };
        line(painter, name, style, cursor + CRUMB_PAD, width - CRUMB_PAD, baseline);
        cursor += width;
        // The root is its own separator, so it does not get another one — the
        // first draft read `/ / home / emil`.
        if !last && index > 0 {
            let style = TextStyle::new(metrics.path_size, theme.palette.dim.with_alpha(120));
            let mark = i18n::lookup("chrome.crumb_separator");
            line(painter, mark, style, cursor - SEPARATOR, SEPARATOR, baseline);
        }
        if index == 0 && elided {
            let style = TextStyle::new(metrics.path_size, theme.palette.dim.with_alpha(150));
            let mark = i18n::lookup("chrome.crumb_elided");
            cursor +=
                line(painter, mark, style, cursor, available, baseline) / painter.scale + CRUMB_PAD;
        }
    }
    painter.list.pop_clip();
}

/// Room for the `/` between two components, in logical pixels.
const SEPARATOR: f32 = 10.0;

/// Space each side of a breadcrumb's text, so the hover shape is not tight
/// against the glyphs and the target is bigger than the word.
const CRUMB_PAD: f32 = 6.0;

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
    painter.cache.get_or_shape_label(text, weight, px_size, painter.fonts).advance / painter.scale
}

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
    let widest = menu
        .items
        .iter()
        .map(|item| {
            let name = text_width(painter, &item.name, metrics.text_size, false);
            let keys = text_width(painter, &item.keys, metrics.header_size, true);
            name + if keys > 0.0 { SPACING + keys } else { 0.0 }
        })
        .fold(0.0f32, f32::max);
    let width = (widest + INSET * 2.0).clamp(WIDTH.0, WIDTH.1);
    let area = crate::menu::place(menu.at, width, crate::menu::height(&menu.items), layout.window);
    panel(painter, area);

    hits.menu.clear();
    let tops: Vec<f32> = crate::menu::row_tops(&menu.items, area).collect();
    for (index, (item, top)) in menu.items.iter().zip(tops).enumerate() {
        hits.menu.push(Area { x: area.x, y: top, width: area.width, height: STEP });
        if item.starts_group {
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
}

/// The name being typed, drawn over the row it belongs to.
fn name_field_at(
    painter: &mut Painter<'_>,
    layout: &Layout,
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
    let inner = painter.rect(area.x + 6.0, 0.0, area.width - 12.0, 0.0);
    painter.label(&naming.text, style, inner, baseline);

    // The caret is measured by shaping what is before it, which is the only
    // answer that survives a ligature or a combining mark.
    let before = &naming.text[..naming.caret.min(naming.text.len())];
    let advance = if before.is_empty() {
        0.0
    } else {
        let px_size = painter.px(style.size);
        painter.cache.get_or_shape_label(before, style.weight, px_size, painter.fonts).advance
    };
    let caret = sk_gpu::Rect::new(
        painter.snap(area.x + 6.0) + advance,
        painter.snap(area.y + 3.0),
        painter.snap(1.5).max(1.0),
        painter.px(area.height - 6.0),
    );
    painter.list.rect(caret, theme.palette.accent, 0.0);
}

/// Where the name field goes for a row: the name column, at that row's height.
///
/// A free function because both the drawing and the hit test need it, and two
/// rectangles computed separately are two rectangles that agree until one of
/// them is edited.
#[must_use]
pub fn name_field(layout: &Layout, metrics: &sk_ui::Metrics, row: usize, offset: f32) -> Area {
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

fn header(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.header, theme.palette.chrome);
    painter.hairline(0.0, layout.header.bottom() - 1.0, layout.header.width, theme.palette.divider);

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

fn body(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout, now: i64, chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let clip = painter.rect(layout.body.x, layout.body.y, layout.body.width, layout.body.height);
    painter.list.push_clip(clip);

    let rows = pane.listing.rows();
    let range = visible_rows(pane.scroll.offset(), layout.body.height, metrics.row_height, rows);
    for row in range {
        row_at(painter, pane, layout, now, row, chrome.types, chrome.naming);
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

    if let Some((from, to)) = chrome.band {
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

/// One row: its shape, its mark, and its three cells.
fn row_at(
    painter: &mut Painter<'_>,
    pane: &Pane,
    layout: &Layout,
    now: i64,
    row: usize,
    types: Option<&MimeDatabase>,
    naming: Option<&Naming>,
) {
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
        if cursor == Some(id) {
            painter.list.stroke(pill, theme.palette.accent, painter.px(metrics.radius), ring);
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
    layout: &Layout,
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

fn scrollbar(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout) {
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
    if chrome.filtering {
        // A mode that takes the keyboard has to look like one, or the first thing
        // a person learns is that typing stopped doing what it did. A tint alone
        // is not enough at these alphas, so the strip also gets an accent rule
        // along its top edge where the hairline would be.
        fill(painter, layout.status, theme.palette.accent.with_alpha(30));
        let rule = painter.rect(0.0, layout.status.y, layout.status.width, 2.0);
        painter.list.rect(rule, theme.palette.accent, 0.0);
    } else {
        painter.hairline(0.0, layout.status.y, layout.status.width, theme.palette.divider);
    }
    let size = if chrome.filtering { metrics.path_size } else { metrics.header_size };
    let baseline = painter.baseline(layout.status.y, layout.status.height, size);

    let rows = pane.listing.rows();
    let selected = pane.selection.count();
    let left = if chrome.filtering {
        // The prompt owns the line while the field has the keyboard: it is what
        // the keys are going into, and a count beside it would compete with the
        // thing being typed.
        i18n::format("status.filter_prompt", &[&pane.filter])
    } else if let Some(notice) = chrome.notice {
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
    let ink = if chrome.filtering { theme.palette.text } else { theme.palette.dim };
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
    line(painter, &right, style, 0.0, layout.status.width - metrics.gutter, baseline);
}

fn type_ahead(painter: &mut Painter<'_>, layout: &Layout, typed: &str) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let area = Area {
        x: (layout.body.width - 220.0) / 2.0,
        y: layout.body.bottom() - 60.0,
        width: 220.0,
        height: 40.0,
    };
    panel(painter, area);
    let baseline = painter.baseline(area.y, area.height, metrics.path_size);
    let style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    line(painter, typed, style, area.x + 16.0, area.width - 32.0, baseline);
}

/// How tall a row of the keyboard list may be, and how short it may become
/// before a column is added instead.
const MAX_KEY_STEP: f32 = 24.0;
const MIN_KEY_STEP: f32 = 17.0;

fn keys_overlay(painter: &mut Painter<'_>, layout: &Layout, entries: &[(String, String)]) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    // **The shape follows the window, not the other way round.** Two columns and
    // a fixed row height were hardcoded, and the overlay grows by a row every
    // time a command is added: five arrived with the operations and the last six
    // fell off the bottom of the screen — a keyboard list that cannot be read
    // being the one surface rule 4 is about.
    //
    // Rows are given up before columns are added, because a column costs width
    // and width is what the labels need: "Extend selection down" ellipsized into
    // the next column's keys the first time this had three of them.
    let chrome = 76.0;
    let available = (layout.body.height - 16.0 - chrome).max(chrome);
    let count = entries.len().max(1);
    let columns = (1..=4)
        .find(|columns| count.div_ceil(*columns) as f32 * MIN_KEY_STEP <= available)
        .unwrap_or(4);
    let per_column = count.div_ceil(columns);
    let step = MAX_KEY_STEP.min(available / per_column as f32).max(MIN_KEY_STEP);

    let width = (340.0 * columns as f32).min(layout.body.width - 40.0);
    let height = per_column as f32 * step + chrome;
    let area = Area {
        x: ((layout.body.width - width) / 2.0).max(0.0),
        y: (layout.body.y + (layout.body.height - height) / 2.0).max(layout.body.y + 8.0),
        width,
        height,
    };
    panel(painter, area);

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
    layout: &Layout,
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
    panel(painter, area);

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
fn panel(painter: &mut Painter<'_>, area: Area) {
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

    use super::{Chrome, Diagnostics, Hits, crumb_names, draw};
    use crate::frames::Frames;
    use crate::layout::Layout;
    use crate::pane::{Jobs, Pane};
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
        pane: &Pane,
        theme: &Theme,
        fonts: &mut Fonts,
        cache: &mut LayoutCache,
    ) -> Vec<(sk_text::GlyphKey, u32, u32)> {
        let logical = Size::new(1000.0, 620.0);
        let layout =
            Layout::compute_with(logical, &theme.metrics, crate::layout::Frame::None, false);
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
                hovered_tool: None,
                places: &[],
                hovered_place: None,
                naming: None,
                tools: [true, true, true, true],
                filtering: false,
                types: None,
                window: sk_plat::WindowState::default(),
                menu: None,
            };
            // A fixed timestamp: the Modified column is relative to "now", and a
            // clock that ticks between two frames is not the difference this test
            // is looking for.
            draw(
                &mut painter,
                pane,
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
        let mut pane = Pane::new(root.clone());
        pane.reload(&jobs);
        settle(&mut pane, &jobs, &updates);

        // Draw the parent, which fills the cache with the parent's rows.
        let parent = frame_of(&pane, &theme, &mut fonts, &mut warm);
        assert!(!parent.is_empty(), "the parent drew no text at all");

        let sub = (0..pane.listing.len())
            .map(|index| sk_model::EntryId(index as u32))
            .find(|id| pane.listing.name(*id) == "sub")
            .expect("sub");
        pane.selection.replace_with(sub);
        assert!(pane.open_focused(&jobs));
        settle(&mut pane, &jobs, &updates);

        let with_warm_cache = frame_of(&pane, &theme, &mut fonts, &mut warm);
        let mut cold = LayoutCache::new(4096);
        let with_cold_cache = frame_of(&pane, &theme, &mut fonts, &mut cold);

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
}
