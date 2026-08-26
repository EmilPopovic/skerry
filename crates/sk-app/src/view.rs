//! The Details view and the window chrome.
//!
//! One immediate-mode pass over the **visible rows only**. At a million entries
//! the row loop below runs about forty times, which is the whole reason a
//! million entries costs the same as a hundred.

use sk_gpu::{Color, FrameStats};
use sk_model::{Flags, SortColumn, i18n};
use sk_plat::{Point, WindowState};
use sk_text::Weight;
use sk_ui::{Align, Painter, RunKey, TextStyle, format, visible_rows};

use crate::frames::{BUDGET_US, Frames};
use crate::layout::{Area, Button, Layout};
use crate::pane::Pane;

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
    /// What the compositor says the window is, for the frame the window draws.
    pub window: WindowState,
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
) {
    path_bar(painter, pane, layout, chrome);
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

fn path_bar(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout, _chrome: &Chrome<'_>) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    // The whole strip, buttons included: the path area stops short of them and a
    // gap in the title bar's background is the first thing an eye finds.
    let strip = Area { width: layout.path.width + strip_width(layout), ..layout.path };
    fill(painter, strip, theme.palette.chrome);
    painter.hairline(0.0, strip.bottom() - 1.0, strip.width, theme.palette.divider);

    let baseline = painter.baseline(layout.path.y, layout.path.height, metrics.path_size);
    let hint_style = TextStyle::new(metrics.header_size, theme.palette.dim.with_alpha(150)).right();
    let hint_width = line(
        painter,
        i18n::lookup("chrome.hint"),
        hint_style,
        0.0,
        layout.path.width - metrics.gutter,
        baseline,
    ) / painter.scale;

    // The final component answers "where am I"; the rest is context. Drawing
    // them in one weight is what makes a path bar unreadable at a glance, and a
    // breadcrumb with hit regions belongs to M2 rather than here.
    let full = pane.path.to_string_lossy();
    // `/` and `/home/emil` split differently and both have to read correctly:
    // the root has no leaf of its own, so it becomes the leaf.
    let (parent, leaf) = match full.rsplit_once('/') {
        Some((parent, leaf)) if !leaf.is_empty() => (parent, leaf),
        _ => ("", full.as_ref()),
    };

    let available = layout.path.width - metrics.gutter * 2.0 - hint_width - 12.0;
    let mut cursor = metrics.gutter;
    if !parent.is_empty() {
        let style = TextStyle::new(metrics.path_size, theme.palette.dim);
        let with_separator = format!("{parent}/");
        let used = line(painter, &with_separator, style, cursor, available * 0.65, baseline);
        cursor += used / painter.scale;
    }
    let leaf_style = TextStyle::new(metrics.path_size, theme.palette.text).bold();
    let remaining = (available + metrics.gutter - cursor).max(0.0);
    line(painter, leaf, leaf_style, cursor, remaining, baseline);
}

/// The width the window buttons take, or zero when the compositor draws them.
fn strip_width(layout: &Layout) -> f32 {
    layout.buttons.iter().map(|button| button.width).sum()
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
        row_at(painter, pane, layout, now, row);
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
fn row_at(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout, now: i64, row: usize) {
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

        let pill = painter.rect(
            metrics.gutter * 0.5,
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
            theme.palette.accent
        } else {
            theme.palette.dim
        };
        if directory {
            painter.folder(icon_box, icon_color);
        } else {
            painter.file(icon_box, icon_color);
        }

        let baseline = painter.baseline(top, metrics.row_height, metrics.text_size);
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
    let track = layout.body.height;
    if content <= track || track <= 0.0 {
        return;
    }
    let thumb = (track * track / content).clamp(28.0, track);
    let extent = (content - track).max(1.0);
    let position = (pane.scroll.offset() / extent).clamp(0.0, 1.0) * (track - thumb);
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
    let baseline = painter.baseline(layout.status.y, layout.status.height, metrics.header_size);

    let rows = pane.listing.rows();
    let selected = pane.selection.count();
    let left = if let Some(notice) = chrome.notice {
        notice.to_owned()
    } else if pane.loading {
        i18n::format("status.loading", &[&format::grouped(rows)])
    } else if selected > 0 {
        i18n::format("status.counts", &[&format::grouped(rows), &format::grouped(selected)])
    } else {
        i18n::format("status.counts.none", &[&format::grouped(rows)])
    };
    let left = if chrome.watching { left } else { i18n::format("status.unwatched", &[&left]) };
    let style = TextStyle::new(metrics.header_size, theme.palette.dim);
    line(painter, &left, style, metrics.gutter, layout.status.width * 0.6, baseline);

    // The frame cost, always visible and always honest: our own work, with the
    // wait for the compositor's presentation slot excluded, because that is
    // pacing rather than cost.
    let p50 = f64::from(diagnostics.frames.percentile(0.5)) / 1000.0;
    let p99 = f64::from(diagnostics.frames.percentile(0.99)) / 1000.0;
    let right = i18n::format("status.frame_cost", &[&format!("{p50:.2}"), &format!("{p99:.2}")]);
    let style = TextStyle::new(metrics.header_size, theme.palette.dim.with_alpha(150)).right();
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

fn keys_overlay(painter: &mut Painter<'_>, layout: &Layout, entries: &[(String, String)]) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let step = 24.0;
    let per_column = entries.len().max(1).div_ceil(2);

    let width = 660.0_f32.min(layout.body.width - 40.0);
    let height = per_column as f32 * step + 76.0;
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

    let column_width = (area.width - 40.0) / 2.0;
    for (index, (keys, label)) in entries.iter().enumerate() {
        let x = area.x + 20.0 + (index / per_column) as f32 * column_width;
        let y = area.y + 52.0 + (index % per_column) as f32 * step;
        let baseline = painter.baseline(y, step, metrics.text_size);
        let keys_style = TextStyle::new(metrics.header_size, theme.palette.accent).bold();
        line(painter, keys, keys_style, x, column_width * 0.38, baseline);
        let label_style = TextStyle::new(metrics.text_size, theme.palette.text);
        // The label gets the rest of the column minus a gutter, because the
        // longest entry — "Extend selection down" — was being ellipsized into
        // the next column's keys in the first capture of this overlay.
        line(painter, label, label_style, x + column_width * 0.40, column_width * 0.54, baseline);
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

    use super::{Chrome, Diagnostics, draw};
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
        let layout = Layout::compute(logical, &theme.metrics);
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
                window: sk_plat::WindowState::default(),
            };
            // A fixed timestamp: the Modified column is relative to "now", and a
            // clock that ticks between two frames is not the difference this test
            // is looking for.
            draw(&mut painter, pane, &layout, 1_787_000_000, &chrome, &mut diagnostics);
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
