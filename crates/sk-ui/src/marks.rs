// SPDX-License-Identifier: Apache-2.0
//! Skerry's own icon set: every mark the window draws when no theme is asked
//! for, and under a theme's icon until it arrives.
//!
//! **Drawn, not decoded.** Each mark is a handful of the display list's own
//! shapes — rounded rectangles, rings, round-capped segments and triangles —
//! on a sixteen-unit grid scaled to the box, so a mark costs quads rather than
//! a raster, is crisp at every fractional scale, and is on screen in the first
//! frame. That is [ADR
//! 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md)'s
//! `marks` source made into a set worth choosing.
//!
//! # The grammar
//!
//! Three silhouettes, and an emblem inside each:
//!
//! * **A folder** — a tab a shade behind a body — for every directory. A
//!   well-known one carries its emblem cut into the body in a darker tone of the
//!   same colour: a house, a screen, lines, an arrow, a note, a mountain, a play
//!   button.
//! * **A page** — an outline — for every file. What the file is goes inside it:
//!   lines for text, a heading and lines for a document, `< >` for code, a
//!   grid, bars, a letter, a prompt, a disc, a globe, a zipper.
//! * **A free shape** for what is neither: a drive, a stick, a disc, a globe, a
//!   bin, a clock.
//!
//! The colour is the caller's — the kind hue a row is already drawn in — so the
//! shape says what a thing is and the hue says it again from across the pane.
//! An emblem never carries a second hue: at thirteen logical pixels two hues in
//! one mark read as noise, where one hue in two tones reads as depth.

use sk_gpu::{Color, Direction, DrawList, Rect};

/// One mark in the set.
///
/// **Named for what it stands for, not for what it looks like**, because the
/// caller decides by meaning — a Downloads folder, a spreadsheet — and the
/// drawing is this module's to change.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Mark {
    /// A directory with nothing more specific to say.
    Folder,
    /// The home directory.
    Home,
    /// `XDG_DESKTOP_DIR`.
    Desktop,
    /// `XDG_DOCUMENTS_DIR`.
    Documents,
    /// `XDG_DOWNLOAD_DIR`.
    Downloads,
    /// `XDG_MUSIC_DIR`.
    Music,
    /// `XDG_PICTURES_DIR`.
    Pictures,
    /// `XDG_VIDEOS_DIR`.
    Videos,
    /// `XDG_TEMPLATES_DIR`.
    Templates,
    /// `XDG_PUBLICSHARE_DIR`.
    Public,
    /// The trash, with nothing in it.
    Trash,
    /// The trash, holding something.
    TrashFull,
    /// Recently opened files.
    Recent,
    /// A fixed disk, mounted or not.
    Drive,
    /// A disk that can be pulled out: a stick, a card.
    Removable,
    /// A disc in a drive.
    Optical,
    /// A filesystem on another machine.
    Network,
    /// Plain text.
    Text,
    /// A file nothing more is known about.
    Unknown,
    /// A laid-out document: PDF, a word processor's file, a book.
    Document,
    /// Source, markup or a script.
    Code,
    /// A picture.
    Image,
    /// Sound.
    Audio,
    /// Moving pictures.
    Video,
    /// Something holding other files.
    Archive,
    /// A table of numbers.
    Spreadsheet,
    /// Slides.
    Presentation,
    /// A typeface.
    Font,
    /// Something the system can run.
    Executable,
    /// An image of a whole disk: an ISO, a virtual machine's drive.
    DiskImage,
    /// A web page.
    Web,
}

impl Mark {
    /// Every mark, in the order the specimen sheet lays them out: folders, the
    /// free shapes, then pages.
    pub const ALL: [Self; 31] = [
        Self::Folder,
        Self::Home,
        Self::Desktop,
        Self::Documents,
        Self::Downloads,
        Self::Music,
        Self::Pictures,
        Self::Videos,
        Self::Templates,
        Self::Public,
        Self::Trash,
        Self::TrashFull,
        Self::Recent,
        Self::Drive,
        Self::Removable,
        Self::Optical,
        Self::Network,
        Self::Text,
        Self::Unknown,
        Self::Document,
        Self::Code,
        Self::Image,
        Self::Audio,
        Self::Video,
        Self::Archive,
        Self::Spreadsheet,
        Self::Presentation,
        Self::Font,
        Self::Executable,
        Self::DiskImage,
        Self::Web,
    ];

    /// Whether it is drawn as a folder, which is what a caller colouring by kind
    /// needs to know.
    #[must_use]
    pub const fn is_folder(self) -> bool {
        matches!(
            self,
            Self::Folder
                | Self::Home
                | Self::Desktop
                | Self::Documents
                | Self::Downloads
                | Self::Music
                | Self::Pictures
                | Self::Videos
                | Self::Templates
                | Self::Public
        )
    }
}

/// Draws `mark` inside `at`, a device-pixel box, in `ink`.
pub(crate) fn draw(list: &mut DrawList, mark: Mark, at: Rect, ink: Color) {
    // Square and centred, so a mark handed a wide box is not stretched: every
    // coordinate below is on a sixteen-unit grid.
    let side = at.width.min(at.height);
    let square =
        Rect::new(at.x + (at.width - side) / 2.0, at.y + (at.height - side) / 2.0, side, side);
    let mut pen = Pen { list, at: square, unit: side / 16.0, ink };
    if mark.is_folder() {
        pen.folder(mark);
    } else if is_page(mark) {
        pen.page();
        pen.page_emblem(mark);
    } else {
        pen.free(mark);
    }
}

/// Whether it is drawn as a page: every file, and nothing else.
const fn is_page(mark: Mark) -> bool {
    matches!(
        mark,
        Mark::Text
            | Mark::Unknown
            | Mark::Document
            | Mark::Code
            | Mark::Image
            | Mark::Audio
            | Mark::Video
            | Mark::Archive
            | Mark::Spreadsheet
            | Mark::Presentation
            | Mark::Font
            | Mark::Executable
            | Mark::DiskImage
            | Mark::Web
    )
}

/// A box, a grid unit and an ink: the three things every shape below needs.
struct Pen<'a> {
    list: &'a mut DrawList,
    at: Rect,
    /// One sixteenth of the box, in device pixels.
    unit: f32,
    ink: Color,
}

/// How much darker an emblem is than the silhouette it is cut into.
///
/// **Towards black on either palette**, for the reason the folder's tab is: it
/// is the one direction that reads as a recess on a dark ground and a light one.
const RECESS: f32 = 0.5;

impl Pen<'_> {
    fn area(&self, x: f32, y: f32, width: f32, height: f32) -> Rect {
        let unit = self.unit;
        Rect::new(self.at.x + x * unit, self.at.y + y * unit, width * unit, height * unit)
    }

    /// The thinnest line the grid draws: a hairline never goes under a pixel.
    fn weight(&self, units: f32) -> f32 {
        (units * self.unit).max(1.0)
    }

    fn fill(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32) {
        let area = self.area(x, y, width, height);
        self.list.rect(area, self.ink, radius * self.unit);
    }

    fn dot(&mut self, x: f32, y: f32, diameter: f32) {
        self.fill(x - diameter / 2.0, y - diameter / 2.0, diameter, diameter, diameter / 2.0);
    }

    fn ring(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, weight: f32) {
        let area = self.area(x, y, width, height);
        let weight = self.weight(weight);
        self.list.stroke(area, self.ink, radius * self.unit, weight);
    }

    fn circle(&mut self, x: f32, y: f32, diameter: f32, weight: f32) {
        let half = diameter / 2.0;
        self.ring(x - half, y - half, diameter, diameter, half, weight);
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), weight: f32) {
        let weight = self.weight(weight);
        self.list.segment(
            self.at,
            (from.0 / 16.0, from.1 / 16.0),
            (to.0 / 16.0, to.1 / 16.0),
            self.ink,
            weight,
        );
    }

    fn path(&mut self, points: &[(f32, f32)], weight: f32) {
        for pair in points.windows(2) {
            self.line(pair[0], pair[1], weight);
        }
    }

    fn triangle(&mut self, x: f32, y: f32, width: f32, height: f32, towards: Direction) {
        let area = self.area(x, y, width, height);
        self.list.triangle(area, self.ink, towards);
    }

    /// Runs `draw` in the recessed tone of the current ink.
    fn recessed(&mut self, draw: impl FnOnce(&mut Self)) {
        let ink = self.ink;
        self.ink = ink.mix(Color::hex(0x000000), RECESS);
        draw(self);
        self.ink = ink;
    }

    /// Two tones and not one: the tab a shade behind the body — the shape a
    /// folder has had since it stopped being drawn in perspective. An outline was
    /// tried, and at fifteen logical pixels it is four hairlines around a hole.
    fn folder(&mut self, mark: Mark) {
        let ink = self.ink;
        self.ink = ink.mix(Color::hex(0x000000), 0.25);
        self.fill(1.0, 2.0, 7.2, 4.5, 1.2);
        self.ink = ink;
        self.fill(0.0, 4.5, 16.0, 10.5, 1.6);
        self.recessed(|pen| pen.folder_emblem(mark));
    }

    /// The emblem cut into a folder's body, centred on (8, 10) and inside
    /// ±3.5 units of it — the body is 10.5 tall and a mark that touches its edge
    /// reads as a second folder.
    fn folder_emblem(&mut self, mark: Mark) {
        match mark {
            Mark::Home => {
                self.triangle(4.4, 6.4, 7.2, 3.6, Direction::Up);
                self.fill(5.6, 9.8, 4.8, 3.6, 0.3);
            }
            Mark::Desktop => {
                self.ring(4.6, 6.8, 6.8, 4.6, 0.6, 1.3);
                self.line((8.0, 11.6), (8.0, 12.8), 1.2);
                self.line((6.2, 13.0), (9.8, 13.0), 1.2);
            }
            Mark::Documents => {
                self.line((5.2, 7.6), (10.8, 7.6), 1.3);
                self.line((5.2, 10.0), (10.8, 10.0), 1.3);
                self.line((5.2, 12.4), (8.8, 12.4), 1.3);
            }
            Mark::Downloads => {
                self.line((8.0, 6.6), (8.0, 12.2), 1.4);
                self.path(&[(5.4, 9.8), (8.0, 12.4), (10.6, 9.8)], 1.4);
            }
            Mark::Music => {
                self.fill(4.8, 10.6, 3.6, 2.8, 1.4);
                self.line((7.7, 12.0), (7.7, 6.6), 1.3);
                self.line((7.7, 6.6), (10.8, 8.0), 1.3);
            }
            Mark::Pictures => {
                self.triangle(4.2, 8.2, 5.8, 5.0, Direction::Up);
                self.triangle(7.6, 9.8, 4.2, 3.4, Direction::Up);
                self.dot(10.4, 7.4, 2.0);
            }
            Mark::Videos => self.triangle(6.0, 6.8, 5.0, 6.4, Direction::Right),
            Mark::Templates => {
                // A dashed frame: a page that is a shape to be filled in.
                for (from, to) in [
                    ((5.2, 6.8), (7.0, 6.8)),
                    ((9.0, 6.8), (10.8, 6.8)),
                    ((5.2, 13.2), (7.0, 13.2)),
                    ((9.0, 13.2), (10.8, 13.2)),
                    ((5.2, 8.6), (5.2, 11.4)),
                    ((10.8, 8.6), (10.8, 11.4)),
                ] {
                    self.line(from, to, 1.2);
                }
            }
            Mark::Public => {
                // Two people: what a shared folder has been drawn as everywhere.
                self.dot(6.4, 8.0, 2.4);
                self.fill(4.4, 10.0, 4.0, 3.2, 1.6);
                self.dot(9.8, 8.0, 2.4);
                self.fill(7.8, 10.0, 4.0, 3.2, 1.6);
            }
            _ => {}
        }
    }

    /// An outlined page, the silhouette every file shares. Its inside runs from
    /// (4.5, 3.5) to (11.5, 13.5), which is where every emblem below is drawn.
    fn page(&mut self) {
        self.ring(2.9, 1.0, 10.2, 14.0, 1.4, 1.3);
    }

    fn page_emblem(&mut self, mark: Mark) {
        match mark {
            Mark::Text => self.text(),
            Mark::Document => self.document(),
            Mark::Code => self.code(),
            Mark::Image => self.image(),
            Mark::Audio => self.audio(),
            Mark::Video => self.video(),
            Mark::Archive => self.archive(),
            Mark::Spreadsheet => self.spreadsheet(),
            Mark::Presentation => self.presentation(),
            Mark::Font => self.font(),
            Mark::Executable => self.executable(),
            Mark::DiskImage => self.disk_image(),
            Mark::Web => self.web(),
            _ => {}
        }
    }

    fn text(&mut self) {
        self.line((5.4, 6.0), (10.6, 6.0), 1.3);
        self.line((5.4, 8.6), (10.6, 8.6), 1.3);
        self.line((5.4, 11.2), (8.8, 11.2), 1.3);
    }

    /// A heading over lines: laid out, where text is only written.
    fn document(&mut self) {
        self.fill(5.0, 4.2, 6.0, 2.4, 0.4);
        self.line((5.4, 8.8), (10.6, 8.8), 1.1);
        self.line((5.4, 10.8), (10.6, 10.8), 1.1);
        self.line((5.4, 12.8), (8.8, 12.8), 1.1);
    }

    fn code(&mut self) {
        self.path(&[(6.8, 5.6), (4.8, 8.2), (6.8, 10.8)], 1.3);
        self.path(&[(9.2, 5.6), (11.2, 8.2), (9.2, 10.8)], 1.3);
    }

    fn image(&mut self) {
        self.triangle(4.4, 8.0, 5.4, 5.2, Direction::Up);
        self.triangle(7.6, 9.8, 3.9, 3.4, Direction::Up);
        self.dot(9.6, 5.6, 2.2);
    }

    fn audio(&mut self) {
        self.fill(4.6, 10.2, 3.8, 3.0, 1.5);
        self.line((7.7, 11.6), (7.7, 4.4), 1.3);
        self.line((7.7, 4.4), (10.9, 6.0), 1.3);
    }

    fn video(&mut self) {
        self.triangle(5.8, 4.8, 5.2, 6.8, Direction::Right);
    }

    /// A zipper down the middle and its pull, which is how an archive has been
    /// told from a page for as long as there have been either.
    fn archive(&mut self) {
        for (row, left) in [(2.4, true), (3.8, false), (5.2, true), (6.6, false)] {
            let x = if left { 6.8 } else { 8.0 };
            self.fill(x, row, 1.2, 1.0, 0.2);
        }
        self.fill(6.6, 8.2, 2.8, 3.8, 0.6);
        self.recessed(|pen| pen.fill(7.4, 9.8, 1.2, 1.2, 0.3));
    }

    fn spreadsheet(&mut self) {
        self.ring(4.6, 4.4, 6.8, 8.4, 0.5, 1.1);
        self.line((8.0, 4.8), (8.0, 12.4), 1.0);
        self.line((5.0, 7.2), (11.0, 7.2), 1.0);
        self.line((5.0, 10.0), (11.0, 10.0), 1.0);
    }

    fn presentation(&mut self) {
        self.fill(4.8, 9.4, 1.8, 3.4, 0.3);
        self.fill(7.1, 7.2, 1.8, 5.6, 0.3);
        self.fill(9.4, 5.0, 1.8, 7.8, 0.3);
    }

    /// A capital drawn in strokes: a font's own glyph would be the font's, and
    /// the machine it is drawn on might not have it.
    fn font(&mut self) {
        self.path(&[(5.0, 12.6), (8.0, 4.4), (11.0, 12.6)], 1.4);
        self.line((6.2, 9.8), (9.8, 9.8), 1.3);
    }

    fn executable(&mut self) {
        self.path(&[(5.0, 5.8), (7.4, 8.2), (5.0, 10.6)], 1.4);
        self.line((8.4, 11.0), (11.0, 11.0), 1.4);
    }

    fn disk_image(&mut self) {
        self.circle(8.0, 8.4, 6.6, 1.2);
        self.dot(8.0, 8.4, 1.6);
    }

    fn web(&mut self) {
        self.circle(8.0, 8.4, 7.0, 1.1);
        self.ring(6.6, 4.9, 2.8, 7.0, 1.4, 1.0);
        self.line((4.8, 8.4), (11.2, 8.4), 1.0);
    }

    fn free(&mut self, mark: Mark) {
        match mark {
            Mark::Trash | Mark::TrashFull => self.bin(mark == Mark::TrashFull),
            Mark::Recent => {
                self.circle(8.0, 8.0, 13.0, 1.4);
                self.path(&[(8.0, 4.6), (8.0, 8.0), (10.8, 9.6)], 1.4);
            }
            Mark::Drive => {
                self.fill(0.5, 4.0, 15.0, 8.0, 2.0);
                self.recessed(|pen| {
                    pen.line((3.2, 8.0), (8.0, 8.0), 1.3);
                    pen.dot(11.8, 8.0, 1.8);
                });
            }
            Mark::Removable => {
                self.ring(6.0, 1.2, 4.0, 3.8, 0.4, 1.2);
                self.fill(4.4, 5.0, 7.2, 10.0, 1.6);
                self.recessed(|pen| pen.dot(8.0, 11.4, 1.8));
            }
            Mark::Optical => {
                self.circle(8.0, 8.0, 13.0, 1.4);
                self.dot(8.0, 8.0, 3.2);
            }
            Mark::Network => {
                self.circle(8.0, 8.0, 13.0, 1.3);
                self.ring(5.0, 1.5, 6.0, 13.0, 3.0, 1.1);
                self.line((1.8, 8.0), (14.2, 8.0), 1.1);
            }
            _ => {}
        }
    }

    /// A bin: outlined when empty and filled when it holds something, so the
    /// difference is legible at thirteen pixels without a second shape.
    fn bin(&mut self, full: bool) {
        self.line((2.6, 3.8), (13.4, 3.8), 1.4);
        self.ring(6.2, 1.2, 3.6, 2.6, 0.6, 1.1);
        if full {
            self.fill(3.8, 5.4, 8.4, 9.6, 1.2);
            self.recessed(|pen| {
                pen.line((6.6, 7.6), (6.6, 12.6), 1.1);
                pen.line((9.4, 7.6), (9.4, 12.6), 1.1);
            });
        } else {
            self.ring(3.8, 5.4, 8.4, 9.6, 1.2, 1.3);
            self.line((6.6, 7.6), (6.6, 12.6), 1.1);
            self.line((9.4, 7.6), (9.4, 12.6), 1.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use sk_gpu::{Color, DrawList, Rect};

    use super::{Mark, draw};

    fn quads(mark: Mark) -> usize {
        let mut list = DrawList::default();
        list.begin((64, 64));
        draw(&mut list, mark, Rect::new(8.0, 8.0, 48.0, 48.0), Color::hex(0x3584e4));
        list.len()
    }

    /// **Every mark draws something**, and no two folders or two pages draw the
    /// same count of shapes where one of them is the bare silhouette — a mark
    /// that fell through to the silhouette is the defect this set exists to
    /// prevent, and a count is what a missing arm changes.
    #[test]
    fn every_mark_draws_more_than_its_silhouette() {
        let folder = quads(Mark::Folder);
        let page = quads(Mark::Unknown);
        for mark in Mark::ALL {
            let count = quads(mark);
            assert!(count > 0, "{mark:?} drew nothing");
            if mark.is_folder() && mark != Mark::Folder {
                assert!(count > folder, "{mark:?} is a bare folder");
            }
            if super::is_page(mark) && mark != Mark::Unknown {
                assert!(count > page, "{mark:?} is a bare page");
            }
        }
    }

    #[test]
    fn the_list_of_marks_names_each_once() {
        for (index, mark) in Mark::ALL.iter().enumerate() {
            assert!(!Mark::ALL[index + 1..].contains(mark), "{mark:?} is listed twice");
        }
    }
}
