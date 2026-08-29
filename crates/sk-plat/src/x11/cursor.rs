//! Pointer images, from the theme the session already chose.
//!
//! Wayland names a shape and the compositor owns the theme, the size and the
//! scale. X11 has no such protocol: a client resolves the name against a theme
//! itself and hands the server pixels. `x11rb`'s `cursor` feature is that
//! resolver — it reads `Xcursor.theme` and `Xcursor.size` from the same resource
//! database `Xft.dpi` comes from, walks the icon path, and uploads through
//! RENDER.
//!
//! Where no theme resolves — a bare `Xvfb` with no themes installed — the core
//! cursor font is the fallback. It is a set of bitmaps from 1987 and it is
//! always present, which is the trade: a pointer that looks dated beats a
//! pointer that is invisible. [`crate::Capabilities::cursor_shape`] says which
//! one is in use so a screenshot that looks wrong has an explanation.

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use crate::window::Cursor;

/// Glyph indices in the core `cursor` font, which are fixed by tradition and
/// listed in `X11/cursorfont.h`. Each is the *mask* glyph's index minus one,
/// which is why they are all even.
const CORE_ARROW: u16 = 68; // XC_left_ptr
const CORE_HAND: u16 = 58; // XC_hand2
const CORE_TEXT: u16 = 152; // XC_xterm
const CORE_GRABBING: u16 = 52; // XC_fleur
const CORE_DRAGGING: u16 = 92; // XC_plus, the core font's nearest thing to a drop cursor
const CORE_TOP: u16 = 138; // XC_top_side
const CORE_BOTTOM: u16 = 16; // XC_bottom_side
const CORE_LEFT: u16 = 70; // XC_left_side
const CORE_RIGHT: u16 = 96; // XC_right_side
const CORE_TOP_LEFT: u16 = 134; // XC_top_left_corner
const CORE_TOP_RIGHT: u16 = 136; // XC_top_right_corner
const CORE_BOTTOM_LEFT: u16 = 12; // XC_bottom_left_corner
const CORE_BOTTOM_RIGHT: u16 = 14; // XC_bottom_right_corner
const CORE_H_DOUBLE_ARROW: u16 = 108; // XC_sb_h_double_arrow
const CORE_V_DOUBLE_ARROW: u16 = 116; // XC_sb_v_double_arrow

/// The cursors this window has resolved, and how.
#[derive(Default)]
pub(super) struct Cursors {
    theme: Option<x11rb::cursor::Handle>,
    /// The core `cursor` font, opened only if the theme did not resolve.
    font: Option<xproto::Font>,
    /// Resolved handles, because resolving one is a file read and a RENDER
    /// upload and the pointer changes shape on every row it crosses.
    cache: HashMap<Cursor, xproto::Cursor>,
    scale_120: u32,
}

impl Cursors {
    /// Loads the session's cursor theme, or falls back to the core font.
    pub(super) fn open(connection: &impl Connection, screen: usize, scale_120: u32) -> Self {
        let theme =
            x11rb::resource_manager::new_from_default(connection).ok().and_then(|database| {
                x11rb::cursor::Handle::new(connection, screen, &database).ok()?.reply().ok()
            });
        let font = if theme.is_some() {
            None
        } else {
            let font = connection.generate_id().ok();
            if let Some(font) = font {
                let _ = connection.open_font(font, b"cursor");
            }
            font
        };
        Self { theme, font, cache: HashMap::new(), scale_120 }
    }

    /// Whether a real theme resolved, as opposed to the 1987 fallback.
    pub(super) const fn themed(&self) -> bool {
        self.theme.is_some()
    }

    /// Drops the cache so the next resolve happens at the new size.
    ///
    /// A theme resolves at one pixel size, and a cursor that stays 24 pixels on
    /// a display that just went to 200% is half the size of everything else.
    pub(super) fn rescale(&mut self, scale_120: u32) {
        if scale_120 != self.scale_120 {
            self.scale_120 = scale_120;
            self.cache.clear();
        }
    }

    /// The server-side cursor for a shape, resolving it the first time.
    pub(super) fn resolve(
        &mut self,
        connection: &impl Connection,
        cursor: Cursor,
    ) -> xproto::Cursor {
        if let Some(handle) = self.cache.get(&cursor) {
            return *handle;
        }
        let resolved = self.load(connection, cursor).unwrap_or(x11rb::NONE);
        self.cache.insert(cursor, resolved);
        resolved
    }

    fn load(&self, connection: &impl Connection, cursor: Cursor) -> Option<xproto::Cursor> {
        if let Some(theme) = &self.theme {
            // The spec names come first and the ancient ones after, because a
            // theme written this decade has `pointer` and one written in 2003
            // has `hand2` — and a theme that has neither should fall through to
            // the font rather than to nothing.
            for name in cursor.theme_names() {
                if let Ok(handle) = theme.load_cursor(connection, name)
                    && handle != x11rb::NONE
                {
                    return Some(handle);
                }
            }
        }
        let font = self.font?;
        let handle = connection.generate_id().ok()?;
        let glyph = core_glyph(cursor);
        // Black on white, which is what every core cursor has always been. The
        // mask glyph is always the shape glyph plus one, by the font's own
        // convention.
        connection
            .create_glyph_cursor(
                handle,
                font,
                font,
                glyph,
                glyph + 1,
                0,
                0,
                0,
                u16::MAX,
                u16::MAX,
                u16::MAX,
            )
            .ok()?;
        Some(handle)
    }
}

/// The core-font glyph for a shape.
const fn core_glyph(cursor: Cursor) -> u16 {
    use crate::window::Edge;
    match cursor {
        Cursor::Arrow => CORE_ARROW,
        Cursor::Hand => CORE_HAND,
        Cursor::Text => CORE_TEXT,
        Cursor::Grabbing => CORE_GRABBING,
        Cursor::Dragging => CORE_DRAGGING,
        Cursor::Divider { horizontal: true } => CORE_H_DOUBLE_ARROW,
        Cursor::Divider { horizontal: false } => CORE_V_DOUBLE_ARROW,
        Cursor::Resize(edge) => match edge {
            Edge::Top => CORE_TOP,
            Edge::Bottom => CORE_BOTTOM,
            Edge::Left => CORE_LEFT,
            Edge::Right => CORE_RIGHT,
            Edge::TopLeft => CORE_TOP_LEFT,
            Edge::TopRight => CORE_TOP_RIGHT,
            Edge::BottomLeft => CORE_BOTTOM_LEFT,
            Edge::BottomRight => CORE_BOTTOM_RIGHT,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::Edge;

    /// Every core glyph index is even, because each shape's mask is the next
    /// index up. An odd one here means a shape is drawn with another shape's
    /// mask, which renders as a smear.
    #[test]
    fn every_core_glyph_is_a_shape_and_not_a_mask() {
        for cursor in every_cursor() {
            let glyph = core_glyph(cursor);
            assert_eq!(glyph % 2, 0, "{cursor:?} resolves to mask glyph {glyph}");
        }
    }

    /// Two shapes sharing a glyph would make one of them silently look like the
    /// other, which is the sort of thing nobody notices for a year.
    #[test]
    fn no_two_shapes_share_a_core_glyph() {
        let mut seen = Vec::new();
        for cursor in every_cursor() {
            let glyph = core_glyph(cursor);
            assert!(!seen.contains(&glyph), "{cursor:?} reuses glyph {glyph}");
            seen.push(glyph);
        }
    }

    fn every_cursor() -> Vec<Cursor> {
        let mut all =
            vec![Cursor::Arrow, Cursor::Hand, Cursor::Text, Cursor::Grabbing, Cursor::Dragging];
        for edge in [
            Edge::Top,
            Edge::Bottom,
            Edge::Left,
            Edge::Right,
            Edge::TopLeft,
            Edge::TopRight,
            Edge::BottomLeft,
            Edge::BottomRight,
        ] {
            all.push(Cursor::Resize(edge));
        }
        all
    }
}
