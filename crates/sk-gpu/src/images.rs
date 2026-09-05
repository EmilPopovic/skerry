// SPDX-License-Identifier: Apache-2.0
//! The thumbnail atlas: one texture, a grid of equal slots.
//!
//! **[ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md).**
//! The glyph allocator next door is a shelf packer whose own doc comment says it
//! is "wrong for long-lived heterogeneous images" — and that warning is addressed
//! at exactly this file. Its premise is false here: a thumbnail is 128×128 because
//! the XDG specification's `normal` size is, so every entry is the same size,
//! placement is an index, and there is no fragmentation to pack around.
//!
//! Which is also what makes eviction possible at all. A shelf cannot give a row
//! back; a slot is free the moment nothing points at it.
//!
//! **Allocated on the first thumbnail and never resized.** A window that opens no
//! image pays no texture, which is what makes sixteen megabytes affordable against
//! an RSS budget that is already missed.

use std::collections::HashMap;

/// One slot's side, in texels. The XDG thumbnail specification's `normal`.
pub const SLOT: u32 = 128;

/// Slots across and down. 16 × 16 is 256 thumbnails in a 2048² RGBA texture.
pub const ACROSS: u32 = 16;

/// The atlas texture's side.
pub const SIDE: u32 = SLOT * ACROSS;

/// How many texels may be uploaded per frame.
///
/// **Four thumbnails**, which is the same 256 KiB the glyph atlas is bounded by
/// and for the same reason: step 5 of the frame loop in
/// [04-architecture.md](../../../docs/04-architecture.md#the-frame-loop-precisely)
/// promises "a thousand new thumbnails cannot blow the budget", and this is the
/// half of that promise about thumbnails. A screenful arrives over several frames
/// and every one of those frames is inside 8 ms.
const UPLOAD_BUDGET_BYTES: usize = 256 * 1024;

/// What a caller names an image by.
///
/// **Opaque above `sk-gpu`.** The window knows a thumbnail by a path, a
/// modification time and a size; this knows it by a number, which is what keeps
/// `wgpu` and `PathBuf` on opposite sides of [ADR
/// 0003](../../../docs/adr/0003-wgpu-first-backend.md)'s seam.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ImageKey(pub u64);

/// Where an image sits in the atlas, in texels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Slot {
    /// The slot's origin.
    pub x: u32,
    /// The slot's origin.
    pub y: u32,
    /// What the image actually fills, which is at most the slot.
    pub width: u32,
    /// What the image actually fills.
    pub height: u32,
}

struct Entry {
    slot: u32,
    width: u32,
    height: u32,
    /// The frame this was last drawn in, which is what eviction sorts on.
    seen: u64,
}

/// Images waiting for the queue, oldest first.
struct Pending {
    slot: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// The atlas: what is in it, what is queued, and which frame it is.
#[derive(Default)]
pub struct Images {
    entries: HashMap<ImageKey, Entry>,
    pending: Vec<Pending>,
    /// Slots never yet handed out. Once this reaches the end, eviction begins.
    next: u32,
    frame: u64,
    /// Whether anything has ever been inserted, which is what allocates the
    /// texture.
    used: bool,
}

impl Images {
    /// A new frame. Everything drawn from now on is marked with this number.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Whether anything has ever been inserted, which is what makes the renderer
    /// swap its one-texel placeholder for the real atlas — ADR 0018's lazy
    /// allocation.
    #[must_use]
    pub fn in_use(&self) -> bool {
        self.used
    }

    /// How many images are held, and how full the atlas is.
    #[must_use]
    pub fn stats(&self) -> (usize, f32) {
        let slots = (ACROSS * ACROSS) as f32;
        (self.entries.len(), self.entries.len() as f32 / slots)
    }

    /// Whether `key` is held, without marking it as drawn.
    ///
    /// **A separate call from [`Self::get`] on purpose.** `get` is the drawing
    /// path and stamps the frame that eviction sorts on; a caller deciding whether
    /// to *fetch* something must not make it look more recently used than it is,
    /// or the least-recently-drawn entry would be whatever nobody asked about.
    /// The size held under `key`, without marking it as drawn either.
    #[must_use]
    pub fn size_of(&self, key: ImageKey) -> Option<(u32, u32)> {
        self.entries.get(&key).map(|slot| (slot.width, slot.height))
    }

    #[must_use]
    pub fn holds(&self, key: ImageKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// Where `key` is, marking it as wanted this frame.
    ///
    /// **Marking is the whole of the eviction policy.** A slot touched this frame
    /// is never taken, so a full atlas degrades to thumbnails that come and go
    /// rather than to a cell showing the wrong picture.
    pub fn get(&mut self, key: ImageKey) -> Option<Slot> {
        let frame = self.frame;
        let entry = self.entries.get_mut(&key)?;
        entry.seen = frame;
        Some(place(entry.slot, entry.width, entry.height))
    }

    /// Puts an image in, evicting the least recently drawn if it must.
    ///
    /// `pixels` is straight `RGBA8`, `width` by `height`, and neither may exceed
    /// [`SLOT`] — the child fits the image into that box before it is sent, so a
    /// larger one is a protocol violation rather than something to scale here.
    /// Returns where it went, or nothing if it did not fit or the atlas is
    /// entirely in use by this frame.
    pub fn insert(
        &mut self,
        key: ImageKey,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Option<Slot> {
        if width == 0 || height == 0 || width > SLOT || height > SLOT {
            return None;
        }
        if pixels.len() != (width * height * 4) as usize {
            return None;
        }
        if let Some(slot) = self.get(key) {
            return Some(slot);
        }
        let slot = self.free_slot()?;
        self.used = true;
        self.entries.insert(key, Entry { slot, width, height, seen: self.frame });
        self.pending.push(Pending { slot, width, height, pixels: pixels.to_vec() });
        Some(place(slot, width, height))
    }

    /// A slot nobody is using, or the one drawn longest ago.
    fn free_slot(&mut self) -> Option<u32> {
        if self.next < ACROSS * ACROSS {
            let slot = self.next;
            self.next += 1;
            return Some(slot);
        }
        // **Never a slot this frame has drawn.** Taking one would replace a
        // thumbnail that is on screen with a different picture in the same cell,
        // which is worse than the cell staying empty for a frame.
        let frame = self.frame;
        let (oldest, _) = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.seen != frame)
            .min_by_key(|(key, entry)| (entry.seen, **key))?;
        let oldest = *oldest;
        let slot = self.entries.remove(&oldest)?.slot;
        // Anything queued for that slot is queued for a picture nobody asked for
        // any more.
        self.pending.retain(|it| it.slot != slot);
        Some(slot)
    }

    /// How many uploads are waiting, for the diagnostic line.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Uploads what fits in this frame's budget.
    pub fn flush(&mut self, queue: &wgpu::Queue, texture: &wgpu::Texture) {
        let mut spent = 0usize;
        let mut at = 0usize;
        while at < self.pending.len() {
            let bytes = self.pending[at].pixels.len();
            if spent > 0 && spent + bytes > UPLOAD_BUDGET_BYTES {
                break;
            }
            spent += bytes;
            let image = &self.pending[at];
            let (x, y) = origin(image.slot);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &image.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(image.width * 4),
                    rows_per_image: Some(image.height),
                },
                wgpu::Extent3d {
                    width: image.width,
                    height: image.height,
                    depth_or_array_layers: 1,
                },
            );
            at += 1;
        }
        self.pending.drain(..at);
    }
}

/// The one image that is not in the grid.
///
/// **[ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)'s
/// second half.** A preview is one image at a time, sized to the panel drawing it,
/// so it is not 128 pixels square and never will be. Putting it in a slot would
/// mean either a second grid of a different size or the heterogeneous packing this
/// module exists to avoid; a texture of its own is neither.
#[derive(Default)]
pub struct Preview {
    /// What is held, oldest-drawn first is not guaranteed — `frame` decides.
    held: Vec<Kept>,
    /// Pixels waiting for the queue, in arrival order.
    pending: Vec<(ImageKey, (u32, u32), Vec<u8>)>,
    /// Which key the renderer's bind group currently points at.
    bound: Option<ImageKey>,
    frame: u64,
}

/// One held picture: what it is of, how big, and when it was last drawn.
struct Kept {
    key: ImageKey,
    size: (u32, u32),
    /// The frame this was last drawn in, which is what eviction sorts on.
    frame: u64,
}

impl Kept {
    /// What it costs, in bytes of RGBA.
    fn bytes(&self) -> usize {
        self.size.0 as usize * self.size.1 as usize * 4
    }
}

/// The most bytes of preview held at once.
///
/// **Twenty-four mebibytes, and a byte budget rather than a count** — [ADR
/// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md).
/// A page of A4 at a 1200-pixel panel is 4.07 MB and the same page on a 4K panel
/// is 11.5 MB, so a count would be sixteen megabytes on one machine and
/// forty-six on another. This is the same promise everywhere and holds fewer
/// pages where each is dearer.
const MOST_HELD_BYTES: usize = 24 * 1024 * 1024;

impl Preview {
    /// Takes a picture. The next `flush` allocates a texture and uploads it.
    ///
    /// **The size is recorded now and the pixels later**, which is not tidiness:
    /// the quads for a frame are resolved *before* the frame's uploads run, and
    /// resolving asks `size_of`. Registering only on upload meant the frame a page
    /// arrived on drew nothing at all, then the next one drew it — a page that
    /// appeared one frame late, every time.
    pub fn hold(&mut self, key: ImageKey, size: (u32, u32), pixels: &[u8]) {
        if size.0 == 0 || size.1 == 0 || pixels.len() != (size.0 as usize * size.1 as usize * 4) {
            return;
        }
        if self.held.iter().any(|kept| kept.key == key) {
            return;
        }
        self.held.push(Kept { key, size, frame: self.frame });
        self.pending.push((key, size, pixels.to_vec()));
    }

    /// The size held under `key`, if it is held.
    #[must_use]
    pub fn size_of(&self, key: ImageKey) -> Option<(u32, u32)> {
        self.held.iter().find(|kept| kept.key == key).map(|kept| kept.size)
    }

    /// Whether anything is waiting for the queue.
    #[must_use]
    pub fn owes_upload(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The next picture waiting, and what to make a texture for.
    pub fn take(&mut self) -> Option<(ImageKey, (u32, u32), Vec<u8>)> {
        self.pending.pop()
    }

    /// Marks `key` as drawn this frame, so eviction leaves it alone.
    pub fn drawn(&mut self, key: ImageKey) {
        let frame = self.frame;
        if let Some(kept) = self.held.iter_mut().find(|kept| kept.key == key) {
            kept.frame = frame;
        }
    }

    /// Advances the frame counter. Called once per frame by the renderer.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Which key the bind group points at, and whether it has to be repointed.
    #[must_use]
    pub fn is_bound(&self, key: ImageKey) -> bool {
        self.bound == Some(key)
    }

    /// Which key the bind group points at.
    #[must_use]
    pub fn bound(&self) -> Option<ImageKey> {
        self.bound
    }

    /// Records that the bind group now points at `key`.
    pub fn bound_to(&mut self, key: Option<ImageKey>) {
        self.bound = key;
    }

    /// The keys to drop so the budget holds, least recently drawn first.
    ///
    /// **Never this frame's**, which is the atlas's rule applied to a second
    /// store: a full store degrades to a page that has to be rendered again, and
    /// never to the wrong page on screen.
    pub fn over_budget(&mut self) -> Vec<ImageKey> {
        let mut total: usize = self.held.iter().map(Kept::bytes).sum();
        let mut dropped = Vec::new();
        while total > MOST_HELD_BYTES && self.held.len() > 1 {
            let current = self.frame;
            let Some(at) = self
                .held
                .iter()
                .enumerate()
                .filter(|(_, kept)| kept.frame != current)
                .min_by_key(|(_, kept)| kept.frame)
                .map(|(at, _)| at)
            else {
                break;
            };
            total -= self.held[at].bytes();
            let gone = self.held.remove(at);
            if self.bound == Some(gone.key) {
                self.bound = None;
            }
            dropped.push(gone.key);
        }
        dropped
    }

    /// Forgets everything. For a panel that has closed.
    pub fn forget_all(&mut self) -> Vec<ImageKey> {
        self.pending.clear();
        self.bound = None;
        self.held.drain(..).map(|kept| kept.key).collect()
    }

    /// How many pictures are held and how many bytes they are, for a diagnostic.
    #[must_use]
    pub fn stats(&self) -> (usize, usize) {
        (self.held.len(), self.held.iter().map(Kept::bytes).sum())
    }
}

/// A slot's origin in texels.
fn origin(slot: u32) -> (u32, u32) {
    ((slot % ACROSS) * SLOT, (slot / ACROSS) * SLOT)
}

fn place(slot: u32, width: u32, height: u32) -> Slot {
    let (x, y) = origin(slot);
    Slot { x, y, width, height }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{ACROSS, ImageKey, Images, SLOT, origin};

    fn pixels(width: u32, height: u32) -> Vec<u8> {
        vec![7u8; (width * height * 4) as usize]
    }

    #[test]
    fn every_slot_has_its_own_corner_and_none_overlap() {
        let mut seen = Vec::new();
        for slot in 0..ACROSS * ACROSS {
            let (x, y) = origin(slot);
            assert!(x + SLOT <= super::SIDE && y + SLOT <= super::SIDE, "slot {slot} is outside");
            assert!(!seen.contains(&(x, y)), "slot {slot} lands on another");
            seen.push((x, y));
        }
        assert_eq!(seen.len() as u32, ACROSS * ACROSS);
    }

    /// **Nothing is allocated until something is inserted**, which is what makes
    /// a folder of text files cost what it did before the atlas existed.
    #[test]
    fn an_atlas_nobody_has_used_is_not_in_use() {
        let mut images = Images::default();
        assert!(!images.in_use());
        assert_eq!(images.stats().0, 0);
        assert!(images.get(ImageKey(1)).is_none());
        assert!(!images.in_use(), "asking about an absent image allocated one");

        images.insert(ImageKey(1), 4, 4, &pixels(4, 4));
        assert!(images.in_use());
    }

    #[test]
    fn an_image_goes_in_once_and_comes_back_from_the_same_slot() {
        let mut images = Images::default();
        let first = images.insert(ImageKey(1), 64, 32, &pixels(64, 32)).unwrap();
        assert_eq!((first.width, first.height), (64, 32));
        // A second insert of the same key is the same slot and no second upload.
        let again = images.insert(ImageKey(1), 64, 32, &pixels(64, 32)).unwrap();
        assert_eq!(first, again);
        assert_eq!(images.pending(), 1, "the same image was queued twice");
        assert_eq!(images.get(ImageKey(1)), Some(first));
    }

    #[test]
    fn an_image_that_does_not_fit_a_slot_is_refused_rather_than_scaled() {
        let mut images = Images::default();
        assert!(images.insert(ImageKey(1), SLOT + 1, 8, &pixels(SLOT + 1, 8)).is_none());
        assert!(images.insert(ImageKey(2), 0, 8, &[]).is_none());
        // And a buffer that disagrees with the dimensions beside it: the child is
        // ours, so this is a bound against a bug rather than an attacker.
        assert!(images.insert(ImageKey(3), 8, 8, &pixels(4, 4)).is_none());
        assert!(!images.in_use());
    }

    /// **A full atlas evicts the least recently drawn, and never this frame's.**
    #[test]
    fn a_full_atlas_takes_the_slot_nobody_has_drawn_lately() {
        let mut images = Images::default();
        let all = ACROSS * ACROSS;
        for index in 0..all {
            images.begin_frame();
            assert!(images.insert(ImageKey(u64::from(index)), 8, 8, &pixels(8, 8)).is_some());
        }
        assert_eq!(images.stats().0 as u32, all);

        // A new frame in which only the last one is drawn. The oldest goes.
        images.begin_frame();
        assert!(images.get(ImageKey(u64::from(all - 1))).is_some());
        let taken = images.insert(ImageKey(9999), 8, 8, &pixels(8, 8));
        assert!(taken.is_some(), "a full atlas refused to evict");
        assert!(images.get(ImageKey(0)).is_none(), "the oldest was kept");
        assert!(images.get(ImageKey(u64::from(all - 1))).is_some(), "this frame's went");
        assert_eq!(images.stats().0 as u32, all, "the count moved");
    }

    /// An atlas whose every slot is on screen refuses rather than replacing one:
    /// a cell that stays empty for a frame is better than a cell showing another
    /// file's picture.
    #[test]
    fn an_atlas_entirely_in_use_this_frame_refuses() {
        let mut images = Images::default();
        let all = ACROSS * ACROSS;
        images.begin_frame();
        for index in 0..all {
            images.insert(ImageKey(u64::from(index)), 8, 8, &pixels(8, 8));
        }
        // Same frame, so every entry is marked, and there is nowhere to put this.
        assert!(images.insert(ImageKey(9999), 8, 8, &pixels(8, 8)).is_none());
    }
}
