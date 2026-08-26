//! Colours and metrics, in one place because a palette argued twice is a
//! palette that drifts.
//!
//! The values are chosen, not sampled from another program. Three constraints
//! shaped them:
//!
//! * **Contrast is measured, not eyeballed.** Body text against the background
//!   is above 9:1 in both themes, and the dimmed metadata columns stay above
//!   4.5:1 — the WCAG AA threshold — so a size column is quiet without being
//!   unreadable. A "subtle" grey that fails contrast is not subtle, it is broken.
//! * **One accent, used for one thing.** Selection, focus and the directory mark
//!   are the same hue. A second accent would have to mean something, and nothing
//!   in a file list needs a second meaning.
//! * **No zebra striping.** Alternating row backgrounds exist to guide the eye
//!   across wide tables; at three columns they add a texture that competes with
//!   the text. Row separation comes from the hover and selection shapes instead.

use sk_gpu::Color;

/// Every colour the interface uses.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Behind everything.
    pub background: Color,
    /// The header and status strips, one step off the background.
    pub chrome: Color,
    /// Hairlines between regions.
    pub divider: Color,
    /// Body text.
    pub text: Color,
    /// Metadata columns and column headers.
    pub dim: Color,
    /// Selection, focus, and the directory mark.
    pub accent: Color,
    /// Text on a selected row.
    pub selected_text: Color,
    /// Under the pointer.
    pub hover: Color,
    /// A row whose metadata has not arrived, or a broken link.
    pub pending: Color,
    /// The scrollbar thumb.
    pub thumb: Color,
    /// A window button under the pointer.
    pub button_hover: Color,
    /// The close button under the pointer, which is the one destructive thing
    /// in the frame and says so.
    pub close_hover: Color,
}

/// Sizes, in logical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// One row of the list.
    pub row_height: f32,
    /// The path bar at the top.
    pub path_height: f32,
    /// The column header strip.
    pub header_height: f32,
    /// The status strip at the bottom.
    pub status_height: f32,
    /// Space between the pane edge and a row's content.
    pub gutter: f32,
    /// Body text size.
    pub text_size: f32,
    /// Column header text size.
    pub header_size: f32,
    /// Path bar text size.
    pub path_size: f32,
    /// Corner radius of a selection or hover shape.
    pub radius: f32,
    /// Width of the size column.
    pub size_column: f32,
    /// Width of the modified column.
    pub modified_column: f32,
    /// Width of the icon column, including its trailing space.
    pub icon_column: f32,
    /// Width of one window button, when the window draws its own frame.
    pub button_width: f32,
    /// How far in from a window edge a press starts a resize, when the window
    /// draws its own frame.
    pub resize_border: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            // 26 logical pixels at a 13 px body: 1.55 line spacing between
            // baselines, which is where a list stops feeling cramped and stays
            // dense enough that a screen holds forty files.
            row_height: 26.0,
            path_height: 44.0,
            header_height: 28.0,
            status_height: 28.0,
            gutter: 10.0,
            text_size: 13.0,
            header_size: 11.0,
            path_size: 13.5,
            radius: 6.0,
            size_column: 96.0,
            modified_column: 128.0,
            icon_column: 26.0,
            // 46 x 44: wider than it is tall, because three of them sit in a row
            // and a square target next to a square target reads as a grid. The
            // whole strip is 138 logical pixels, which is a fifth of a narrow
            // window and no more than a title bar costs anywhere else.
            button_width: 46.0,
            // 6 logical pixels. Fitts' law wants more and a list wants none;
            // this is what GTK and Qt both use for a client-side frame, so it is
            // what a user's hands already expect.
            resize_border: 6.0,
        }
    }
}

/// A palette and a set of metrics.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// The colours.
    pub palette: Palette,
    /// The sizes.
    pub metrics: Metrics,
    /// Whether this is the dark palette, which the folder mark leans on.
    pub dark: bool,
}

impl Theme {
    /// The dark palette.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            palette: Palette {
                background: Color::hex(0x14161a),
                chrome: Color::hex(0x191c22),
                divider: Color::hex(0xffffff).with_alpha(18),
                text: Color::hex(0xe7eaf0),
                dim: Color::hex(0x99a2b3),
                accent: Color::hex(0x5aa0f2),
                selected_text: Color::hex(0xf4f7fc),
                hover: Color::hex(0xffffff).with_alpha(14),
                pending: Color::hex(0x646d7d),
                thumb: Color::hex(0xffffff).with_alpha(52),
                button_hover: Color::hex(0xffffff).with_alpha(28),
                close_hover: Color::hex(0xe5484d),
            },
            metrics: Metrics::default(),
            dark: true,
        }
    }

    /// The light palette.
    #[must_use]
    pub fn light() -> Self {
        Self {
            palette: Palette {
                background: Color::hex(0xfbfbfd),
                chrome: Color::hex(0xf2f3f6),
                divider: Color::hex(0x000000).with_alpha(22),
                text: Color::hex(0x1b1e25),
                dim: Color::hex(0x5f6675),
                accent: Color::hex(0x1f6feb),
                selected_text: Color::hex(0x0d1117),
                hover: Color::hex(0x000000).with_alpha(12),
                pending: Color::hex(0x9aa1ae),
                thumb: Color::hex(0x000000).with_alpha(64),
                button_hover: Color::hex(0x000000).with_alpha(22),
                close_hover: Color::hex(0xd93a3f),
            },
            metrics: Metrics::default(),
            dark: false,
        }
    }

    /// The background of a row, given what is true about it.
    #[must_use]
    pub fn row_background(&self, selected: bool, hovered: bool) -> Color {
        if selected {
            // A tint rather than a solid fill: the name has to stay the most
            // legible thing on the row, and a saturated bar behind thirteen-pixel
            // text is what makes a selected filename harder to read than an
            // unselected one.
            self.palette.accent.with_alpha(if self.dark { 56 } else { 40 })
        } else if hovered {
            self.palette.hover
        } else {
            Color::NONE
        }
    }

    /// The colour of a name, given what is true about the entry.
    #[must_use]
    pub fn name_color(&self, selected: bool, directory: bool, broken: bool) -> Color {
        if broken {
            self.palette.pending
        } else if selected {
            self.palette.selected_text
        } else if directory {
            // Directories lift slightly towards the accent rather than changing
            // hue: at this size a coloured filename reads as a link, and a link
            // is a promise about what a click does that a folder does not keep.
            self.palette.text.mix(self.palette.accent, 0.25)
        } else {
            self.palette.text
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Theme;
    use sk_gpu::Color;

    /// Relative luminance per WCAG 2.1, from an sRGB-encoded colour.
    fn luminance(color: Color) -> f64 {
        let (red, green, blue, _) = color.components();
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.040_45 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (light, dark) = if x > y { (x, y) } else { (y, x) };
        (light + 0.05) / (dark + 0.05)
    }

    #[test]
    fn body_text_and_dim_text_both_meet_contrast_in_both_themes() {
        for theme in [Theme::dark(), Theme::light()] {
            let background = theme.palette.background;
            assert!(
                contrast(theme.palette.text, background) >= 7.0,
                "body text is {:.1}:1",
                contrast(theme.palette.text, background)
            );
            assert!(
                contrast(theme.palette.dim, background) >= 4.5,
                "dimmed metadata is {:.1}:1, under the AA threshold",
                contrast(theme.palette.dim, background)
            );
        }
    }

    #[test]
    fn a_selected_row_is_a_tint_and_never_hides_the_name() {
        for theme in [Theme::dark(), Theme::light()] {
            let selected = theme.row_background(true, false);
            assert!(!selected.is_invisible());
            assert!(theme.row_background(false, false).is_invisible(), "an idle row draws nothing");
        }
    }

    #[test]
    fn a_broken_link_is_dimmed_whatever_else_is_true_of_it() {
        let theme = Theme::dark();
        assert_eq!(theme.name_color(true, true, true), theme.palette.pending);
    }
}
