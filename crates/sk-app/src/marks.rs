// SPDX-License-Identifier: Apache-2.0
//! Which of Skerry's own marks a thing gets.
//!
//! The drawing is [`sk_ui::marks`]; this is the decision, and it lives here for
//! the reason kind detection does — it needs the MIME database, the listing and
//! the places together, and only this crate sees all three.
//!
//! **The type first, then the kind.** The nine kinds a grouped pane bands by are
//! the floor: every file has one, and each has a mark. A type the database says
//! more about — a spreadsheet inside `Document`, a web page inside `Code` —
//! gets the more specific mark, read from the `generic-icon` shared-mime-info
//! names for it where it names one and from the type's own spelling where not.

use sk_desktop::{Category, MimeDatabase};
use sk_ui::Mark;

/// The mark for a file whose type is `mime` and whose kind is `kind`.
pub(crate) fn of_file(kind: Category, mime: Option<&str>, types: Option<&MimeDatabase>) -> Mark {
    if let Some(mark) =
        mime.and_then(|mime| by_type(mime, types.and_then(|types| types.generic_icon(mime))))
    {
        return mark;
    }
    match kind {
        Category::Folder => Mark::Folder,
        Category::Image => Mark::Image,
        Category::Audio => Mark::Audio,
        Category::Video => Mark::Video,
        Category::Archive => Mark::Archive,
        Category::Code => Mark::Code,
        // Prose is two shapes: what is only written, and what is laid out.
        Category::Document if mime.is_some_and(|it| it.starts_with("text/")) => Mark::Text,
        Category::Document => Mark::Document,
        Category::Executable => Mark::Executable,
        Category::Unknown => Mark::Unknown,
    }
}

/// A mark more specific than the kind, where the type has one.
fn by_type(mime: &str, generic: Option<&str>) -> Option<Mark> {
    match generic {
        Some("x-office-spreadsheet") => return Some(Mark::Spreadsheet),
        Some("x-office-presentation") => return Some(Mark::Presentation),
        Some("font-x-generic") => return Some(Mark::Font),
        Some("text-html") => return Some(Mark::Web),
        _ => {}
    }
    SPELLED.iter().find(|(part, _)| mime.contains(part)).map(|(_, mark)| *mark)
}

/// Pieces of a type's name that say what it is, for the types the database
/// names no generic for — most disk images, and every font a package spells
/// `application/x-font-*`.
///
/// **Checked in order, and the order matters once**: `text/csv` is text to the
/// kinds and a table to anybody looking at it, so the table is first.
const SPELLED: &[(&str, Mark)] = &[
    ("text/csv", Mark::Spreadsheet),
    ("spreadsheet", Mark::Spreadsheet),
    ("ms-excel", Mark::Spreadsheet),
    ("presentation", Mark::Presentation),
    ("powerpoint", Mark::Presentation),
    ("font/", Mark::Font),
    ("-font", Mark::Font),
    ("text/html", Mark::Web),
    ("xhtml", Mark::Web),
    ("cd-image", Mark::DiskImage),
    ("iso9660", Mark::DiskImage),
    ("disk-image", Mark::DiskImage),
    ("diskimage", Mark::DiskImage),
    ("vmdk", Mark::DiskImage),
    ("qemu-disk", Mark::DiskImage),
    ("virtualbox-vdi", Mark::DiskImage),
    ("epub", Mark::Document),
];

/// The mark for one of the well-known directories.
pub(crate) const fn of_place(place: sk_desktop::places::Kind) -> Mark {
    use sk_desktop::places::Kind;
    match place {
        Kind::Home => Mark::Home,
        Kind::Desktop => Mark::Desktop,
        Kind::Documents => Mark::Documents,
        Kind::Downloads => Mark::Downloads,
        Kind::Music => Mark::Music,
        Kind::Pictures => Mark::Pictures,
        Kind::Videos => Mark::Videos,
        Kind::Other => Mark::Folder,
    }
}

/// The well-known directories directly inside `here`, by name, with their marks.
///
/// **Once a pane a frame, not once a row**: a row asks whether its name is one of
/// these, which is a comparison of two strings rather than a path built and
/// compared for every folder on screen.
pub(crate) fn specials<'p>(
    places: &'p [crate::sidebar::Row],
    here: &std::path::Path,
) -> Vec<(&'p str, Mark)> {
    places
        .iter()
        .filter_map(|row| {
            let crate::sidebar::Mark::Place(place) = row.mark else { return None };
            let mark = of_place(place);
            (mark != Mark::Folder && row.path.parent() == Some(here))
                .then(|| row.path.file_name()?.to_str().map(|name| (name, mark)))
                .flatten()
        })
        .collect()
}

/// The kind hue a mark is drawn in where nothing more specific says one — the
/// specimen sheet's colours, and the ones a row would give the same thing.
fn hue_of(mark: Mark, palette: &sk_ui::Palette) -> sk_gpu::Color {
    let kinds = palette.kinds;
    match mark {
        _ if mark.is_folder() => kinds.folder,
        Mark::Trash | Mark::TrashFull | Mark::Recent => palette.dim,
        Mark::Drive | Mark::Removable | Mark::Optical | Mark::Unknown => kinds.unknown,
        Mark::Image => kinds.image,
        // A network mount shares audio's hue in the sidebar, where it was first.
        Mark::Audio | Mark::Network => kinds.audio,
        Mark::Video => kinds.video,
        Mark::Archive | Mark::DiskImage => kinds.archive,
        Mark::Code | Mark::Web => kinds.code,
        Mark::Executable => kinds.executable,
        _ => kinds.document,
    }
}

/// Every mark at the four sizes the window draws them, each under its name:
/// the frame a person looks at before blessing a change to any of them.
///
/// **Four sizes because they are four different problems.** Thirteen logical
/// pixels is a details row, where an emblem is five pixels wide and has to be a
/// silhouette first; forty-eight is a grid cell, where it is a drawing and every
/// misplaced unit shows.
pub(crate) fn specimen(painter: &mut sk_ui::Painter<'_>, window: sk_plat::Size) {
    const SIZES: [f32; 4] = [48.0, 32.0, 20.0, 13.0];
    const CELL: (f32, f32) = (150.0, 84.0);
    const MARGIN: f32 = 16.0;
    let theme = *painter.theme;
    let across = (((window.width - MARGIN * 2.0) / CELL.0).floor() as usize).max(1);
    for (index, mark) in Mark::ALL.iter().enumerate() {
        let x = MARGIN + (index % across) as f32 * CELL.0;
        let y = MARGIN + (index / across) as f32 * CELL.1;
        let hue = hue_of(*mark, &theme.palette);
        let mut left = x;
        for size in SIZES {
            let top = y + (SIZES[0] - size) / 2.0;
            let area = painter.rect(left, top, size, size);
            painter.icon(sk_ui::Icon::Mark(*mark), area, hue);
            left += size + 8.0;
        }
        let name = format!("{mark:?}");
        let label = painter.rect(x, y + SIZES[0] + 6.0, CELL.0 - 8.0, 18.0);
        let baseline = painter.baseline(y + SIZES[0] + 6.0, 18.0, theme.metrics.header_size);
        painter.label(
            &name,
            sk_ui::TextStyle::new(theme.metrics.header_size, theme.palette.dim),
            label,
            baseline,
        );
    }
}

#[cfg(test)]
mod tests {
    use sk_desktop::{Category, MimeDatabase};
    use sk_ui::Mark;

    use super::of_file;

    fn database() -> MimeDatabase {
        let mut types = MimeDatabase::default();
        types.absorb_generic_icons(
            "application/vnd.oasis.opendocument.spreadsheet:x-office-spreadsheet\n\
             application/vnd.oasis.opendocument.presentation:x-office-presentation\n\
             application/pdf:x-office-document\n\
             font/ttf:font-x-generic\n",
        );
        types
    }

    /// **The kind is the floor and the type refines it.** Every kind has a mark
    /// of its own, and a type the database says more about gets a more specific
    /// one — the case that makes a folder of office files legible at a glance.
    #[test]
    fn a_type_refines_the_mark_its_kind_would_give() {
        let types = database();
        let of = |kind, mime| of_file(kind, Some(mime), Some(&types));
        assert_eq!(
            of(Category::Document, "application/vnd.oasis.opendocument.spreadsheet"),
            Mark::Spreadsheet
        );
        assert_eq!(
            of(Category::Document, "application/vnd.oasis.opendocument.presentation"),
            Mark::Presentation
        );
        assert_eq!(of(Category::Document, "application/pdf"), Mark::Document);
        assert_eq!(of(Category::Document, "text/plain"), Mark::Text);
        assert_eq!(of(Category::Document, "text/csv"), Mark::Spreadsheet);
        assert_eq!(of(Category::Code, "text/html"), Mark::Web);
        assert_eq!(of(Category::Code, "text/rust"), Mark::Code);
        assert_eq!(of(Category::Unknown, "font/ttf"), Mark::Font);
        assert_eq!(of(Category::Archive, "application/x-cd-image"), Mark::DiskImage);
        assert_eq!(of(Category::Archive, "application/zip"), Mark::Archive);
    }

    /// Before the databases arrive every file still gets its kind's mark, and a
    /// program with no extension is a program.
    #[test]
    fn without_a_database_the_kind_decides() {
        assert_eq!(of_file(Category::Executable, None, None), Mark::Executable);
        assert_eq!(of_file(Category::Unknown, None, None), Mark::Unknown);
        assert_eq!(of_file(Category::Image, Some("image/png"), None), Mark::Image);
    }
}
