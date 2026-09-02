//! What a file can be shown as, and what its thumbnail is called.
//!
//! **Kind detection lives here and not in `sk-preview`.** Both are level 2, so the
//! layering forbids the edge — and that turns out to be the right shape rather
//! than a constraint worked around: this crate already holds the MIME database,
//! and a preview host that guessed from an extension would disagree with the mark
//! drawn beside the name.
//!
//! # The three answers, and why text is not a fourth process
//!
//! A picture goes to `skerry-thumbnailer` and a document's page to
//! `skerry-preview-doc`, both under [ADR
//! 0006](../../../docs/adr/0006-out-of-process-decoders.md)'s sandbox, because
//! decoding is the attack surface. Reading a text file is not decoding —
//! `sk_fs::text` already does it in this process for *Copy the text inside* — so a
//! text preview needs no child, no round trip and no sandbox. Sending it to one
//! would be paying for a process to do a `read`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sk_desktop::{Category, MimeDatabase};
use sk_gpu::ImageKey;
use sk_preview::{Ask, Kind, Want};

/// What can be drawn in place of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Shows {
    /// A picture: PNG, JPEG or SVG, through `skerry-thumbnailer`.
    ///
    /// The only one that gets a *thumbnail*. A page rendered through an external
    /// tool costs a process per file, which is affordable for the one document
    /// somebody is looking at and not for a grid of forty.
    Picture,
    /// A document's page, through `skerry-preview-doc`. Preview only.
    ///
    /// Which page is the look's business — [ADR
    /// 0019](../../../docs/adr/0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md)
    /// — and there is no thumbnail of one: a page costs a process, which is
    /// affordable for the document somebody has open and not for a grid of forty.
    Page,
    /// Its own text, read here. Preview only.
    Text,
}

/// What `name` can be shown as, given what the databases say about it.
///
/// **From the MIME string rather than the category**, because `Document` holds
/// both a PDF and a spreadsheet and only one of them has a renderer. The category
/// is the fallback for a file the databases do not name, where `Code` is a strong
/// enough signal that it is text.
pub(crate) fn shows(name: &str, kind: Category, mime: Option<&MimeDatabase>) -> Option<Shows> {
    if kind == Category::Folder {
        return None;
    }
    match mime.and_then(|mime| mime.by_name(name)) {
        Some(kind) if kind.starts_with("image/") => Some(Shows::Picture),
        Some("application/pdf") => Some(Shows::Page),
        Some(kind) if kind.starts_with("text/") => Some(Shows::Text),
        // The databases have an opinion and it is none of the three.
        Some(_) => None,
        // They have none. `Code` is a file somebody writes in an editor, which is
        // text whatever it is called — a `Makefile` has no extension at all.
        None => (kind == Category::Code).then_some(Shows::Text),
    }
}

/// The numbers the atlas knows thumbnails by, and what they stand for.
///
/// **A counter rather than a hash of the key.** A hash collision would put one
/// file's picture in another file's cell, which is the one failure mode a
/// thumbnail must not have; the map costs a pointer per visible image and cannot
/// collide. See [ADR
/// 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)
/// for what the key is made of.
#[derive(Default)]
pub(crate) struct Thumbs {
    named: HashMap<sk_preview::Key, ImageKey>,
    next: u64,
}

impl Thumbs {
    /// The atlas key for a file at a modification time, minting one if new.
    pub(crate) fn name(&mut self, path: &Path, mtime: i64) -> (sk_preview::Key, ImageKey) {
        let key = sk_preview::Key { path: path.to_path_buf(), mtime, within: THUMBNAIL, page: 0 };
        if let Some(image) = self.named.get(&key) {
            return (key, *image);
        }
        self.next += 1;
        let image = ImageKey(self.next);
        self.named.insert(key.clone(), image);
        (key, image)
    }

    /// A fresh key for the one preview picture.
    ///
    /// **Not in the map.** A preview is not keyed by a file: it is whatever is in
    /// the panel now, replaced whole each time, and giving it a name from the same
    /// counter is what stops a stale reply from a resize drawing over a newer one.
    pub(crate) fn name_preview(&mut self) -> ImageKey {
        self.next += 1;
        ImageKey(self.next)
    }

    /// The atlas key an answer belongs to, if anything still wants it.
    pub(crate) fn look_up(&self, key: &sk_preview::Key) -> Option<ImageKey> {
        self.named.get(key).copied()
    }

    /// Forgets names for files that are no longer anywhere on screen.
    ///
    /// **Bounded by the same thing the atlas is.** The atlas evicts by what a
    /// frame drew and holds 256; this map would otherwise grow for the life of the
    /// session, one entry per image ever scrolled past. Keeping twice the atlas's
    /// capacity means a name survives a scroll back and forth without being
    /// re-minted, and nothing survives a session in a photograph directory.
    pub(crate) fn forget_the_oldest(&mut self) {
        const KEEP: usize = 512;
        if self.named.len() <= KEEP {
            return;
        }
        // The lowest numbers are the ones minted longest ago.
        let mut held: Vec<(ImageKey, sk_preview::Key)> =
            self.named.iter().map(|(key, image)| (*image, key.clone())).collect();
        held.sort_unstable_by_key(|(image, _)| *image);
        for (_, key) in held.iter().take(self.named.len() - KEEP) {
            self.named.remove(key);
        }
    }
}

/// The size every thumbnail is asked for at: ADR 0018's one slot size.
pub(crate) const THUMBNAIL: (u32, u32) = (sk_gpu::THUMBNAIL_SLOT, sk_gpu::THUMBNAIL_SLOT);

/// A want for one file's thumbnail.
pub(crate) fn thumbnail(path: PathBuf, mtime: i64) -> Want {
    Want { kind: Kind::Image, ask: Ask::Thumbnail, path, mtime, within: THUMBNAIL, page: 0 }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Shows, Thumbs, shows};
    use sk_desktop::Category;
    use std::path::Path;

    /// Without a database nothing is a picture, and `Code` is still text.
    ///
    /// **Which is the state of the first few frames of a session**: the MIME
    /// databases are read on a job, so every name answers `Unknown` until they
    /// land. A file manager that decided then and cached the answer would show no
    /// thumbnails for the whole session.
    #[test]
    fn with_no_database_only_the_category_speaks() {
        assert_eq!(shows("photo.png", Category::Image, None), None);
        assert_eq!(shows("main.rs", Category::Code, None), Some(Shows::Text));
        assert_eq!(shows("anything", Category::Unknown, None), None);
        assert_eq!(shows("a-folder", Category::Folder, None), None);
    }

    /// A folder is never previewed, whatever else is true of it.
    #[test]
    fn a_folder_shows_nothing() {
        assert_eq!(shows("pictures", Category::Folder, None), None);
    }

    /// **A name is stable, and two files never share one.** A collision would put
    /// one file's picture in another file's cell, which is the one failure mode a
    /// thumbnail must not have.
    #[test]
    fn a_name_is_stable_per_file_and_per_modification_time() {
        let mut thumbs = Thumbs::default();
        let (key, first) = thumbs.name(Path::new("/a/one.png"), 7);
        let (_, again) = thumbs.name(Path::new("/a/one.png"), 7);
        assert_eq!(first, again, "the same file was named twice");
        assert_eq!(thumbs.look_up(&key), Some(first));

        // A different file, and the same file at a different time, are different
        // pictures — ADR 0018's mtime in the key.
        let (_, other) = thumbs.name(Path::new("/a/two.png"), 7);
        let (_, changed) = thumbs.name(Path::new("/a/one.png"), 8);
        assert_ne!(first, other);
        assert_ne!(first, changed);
        assert_eq!(thumbs.look_up(&key), Some(first));
    }

    /// The map is bounded, or a session spent scrolling a photograph directory
    /// grows one entry per picture ever passed.
    #[test]
    fn the_names_do_not_grow_without_end() {
        let mut thumbs = Thumbs::default();
        let mut kept = Vec::new();
        for index in 0..700 {
            kept.push(thumbs.name(Path::new(&format!("/a/{index}.png")), 1));
        }
        thumbs.forget_the_oldest();
        // The newest survive and the oldest are gone, which is what a scroll back
        // up needs.
        assert!(thumbs.look_up(&kept[699].0).is_some(), "the newest was dropped");
        assert!(thumbs.look_up(&kept[0].0).is_none(), "the oldest was kept");
    }
}
