//! The Details view and the window chrome.
//!
//! One immediate-mode pass over the **visible rows only**. At a million entries
//! the row loop below runs about forty times, which is the whole reason a
//! million entries costs the same as a hundred.

use sk_gpu::{Color, FrameStats};
use sk_model::{Flags, SortColumn, i18n};
use sk_plat::Point;
use sk_text::Weight;
use sk_ui::{Align, Painter, RunKey, TextStyle, format, visible_rows};

use crate::frames::{BUDGET_US, Frames};
use crate::keys::BINDINGS;
use crate::layout::{Area, Layout};
use crate::pane::Pane;

/// Which column a row cache key is for.
const COLUMN_NAME: u8 = 0;
const COLUMN_SIZE: u8 = 1;
const COLUMN_MODIFIED: u8 = 2;

/// What the overlays are showing.
pub struct Chrome<'a> {
    /// The keyboard list.
    pub keys: bool,
    /// The frame statistics.
    pub stats: bool,
    /// The type-ahead buffer, while it is live.
    pub typed: Option<&'a str>,
    /// The rubber-band rectangle, while one is being dragged.
    pub band: Option<(Point, Point)>,
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
    path_bar(painter, pane, layout);
    header(painter, pane, layout);
    body(painter, pane, layout, now, chrome);
    status(painter, pane, layout, diagnostics);
    if chrome.stats {
        stats_overlay(painter, pane, layout, diagnostics);
    }
    if chrome.keys {
        keys_overlay(painter, layout);
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

fn path_bar(painter: &mut Painter<'_>, pane: &Pane, layout: &Layout) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.path, theme.palette.chrome);
    painter.hairline(0.0, layout.path.bottom() - 1.0, layout.path.width, theme.palette.divider);

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
            Align::Right => (area.x, area.width - inset),
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
                Align::Left => painter.snap(x),
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
            RunKey::cell(id.0, COLUMN_NAME, Weight::Regular, text_px),
            pane.listing.name(id),
            name_box,
            baseline,
            theme.name_color(selected, directory, broken),
            Align::Left,
        );

        // A row whose metadata has not arrived shows nothing in the metadata
        // columns rather than a zero. Spike B's whole argument is that the name
        // arrives first; printing `0 B` for the milliseconds in between would
        // spend that advantage on a lie.
        if flags.has(Flags::META_PENDING) {
            return;
        }
        let meta_color = if selected { theme.palette.selected_text } else { theme.palette.dim };
        if layout.size.width > 0.0 {
            let text = if directory {
                format::directory_size().to_owned()
            } else {
                format::size(pane.listing.size(id))
            };
            let box_rect = painter.rect(layout.size.x, 0.0, layout.size.width - 12.0, 0.0);
            painter.cell(
                RunKey::cell(id.0, COLUMN_SIZE, Weight::Regular, text_px),
                &text,
                box_rect,
                baseline,
                meta_color,
                Align::Right,
            );
        }
        if layout.modified.width > 0.0 {
            let text = format::modified(pane.listing.mtime(id), now);
            let box_rect =
                painter.rect(layout.modified.x + 12.0, 0.0, layout.modified.width - 12.0, 0.0);
            painter.cell(
                RunKey::cell(id.0, COLUMN_MODIFIED, Weight::Regular, text_px),
                &text,
                box_rect,
                baseline,
                meta_color,
                Align::Left,
            );
        }
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
    diagnostics: &mut Diagnostics<'_>,
) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    fill(painter, layout.status, theme.palette.chrome);
    painter.hairline(0.0, layout.status.y, layout.status.width, theme.palette.divider);
    let baseline = painter.baseline(layout.status.y, layout.status.height, metrics.header_size);

    let rows = pane.listing.rows();
    let selected = pane.selection.count();
    let left = if pane.loading {
        i18n::format("status.loading", &[&format::grouped(rows)])
    } else if selected > 0 {
        i18n::format("status.counts", &[&format::grouped(rows), &format::grouped(selected)])
    } else {
        i18n::format("status.counts.none", &[&format::grouped(rows)])
    };
    let style = TextStyle::new(metrics.header_size, theme.palette.dim);
    line(painter, &left, style, metrics.gutter, layout.status.width * 0.6, baseline);

    // The frame cost, always visible and always honest: our own work, with the
    // wait for the compositor's presentation slot excluded, because that is
    // pacing rather than cost.
    let p50 = f64::from(diagnostics.frames.percentile(0.5)) / 1000.0;
    let p99 = f64::from(diagnostics.frames.percentile(0.99)) / 1000.0;
    let right = format!("{p50:.2} / {p99:.2} ms");
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

fn keys_overlay(painter: &mut Painter<'_>, layout: &Layout) {
    let theme = *painter.theme;
    let metrics = theme.metrics;
    let step = 24.0;
    // One row per binding, read from the same table the key handler reads. An
    // action missing from that table is a build failure, not a missing line.
    let entries: Vec<(&str, &str)> = BINDINGS
        .iter()
        .map(|(_, keys, label)| (*keys, i18n::lookup(label)))
        .chain(std::iter::once((i18n::lookup("keys.typing"), i18n::lookup("action.type_ahead"))))
        .collect();
    let per_column = entries.len().div_ceil(2);

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

    let rows: [(&str, String); 10] = [
        (i18n::lookup("stats.frame"), format!("{p50:.2} / {p99:.2} / {worst:.2} ms")),
        (
            i18n::lookup("stats.budget"),
            format!("{} of {}", format::grouped(over as usize), format::grouped(frames as usize)),
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
            format!(
                "{:.1} / {:.1} / {:.1} ms",
                pane.names_us as f64 / 1000.0,
                pane.meta_us as f64 / 1000.0,
                pane.sort_us as f64 / 1000.0
            ),
        ),
        (
            i18n::lookup("stats.pool"),
            format!("{workers} · {} / {panics}", format::grouped(jobs as usize)),
        ),
        (
            i18n::lookup("stats.surface"),
            format!(
                "{}×{} at {}%",
                diagnostics.physical.0,
                diagnostics.physical.1,
                diagnostics.scale_120 * 100 / 120
            ),
        ),
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
