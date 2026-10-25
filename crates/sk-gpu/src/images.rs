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
pub struct Preview {
    /// What is held, oldest-drawn first is not guaranteed — `frame` decides.
    held: Vec<Kept>,
    /// Pixels waiting for the queue, in arrival order.
    pending: Vec<(ImageKey, (u32, u32), Vec<u8>)>,
    /// Which key the renderer's bind group currently points at.
    bound: Option<ImageKey>,
    /// What the budget may not drop. See [`Preview::keep`].
    pinned: Vec<ImageKey>,
    frame: u64,
    /// The widest or tallest texture the device makes. See [`Preview::hold`].
    most_side: u32,
}

/// One held picture: what it is of, how big, and when it was last drawn.
struct Kept {
    key: ImageKey,
    size: (u32, u32),
    /// The frame this was last drawn in, which is what eviction sorts on.
    frame: u64,
    /// Whether it has a texture, which a picture past the device's limit never
    /// gets.
    on_device: bool,
}

impl Kept {
    /// What it costs, in bytes of RGBA on the device.
    fn bytes(&self) -> usize {
        if self.on_device { self.size.0 as usize * self.size.1 as usize * 4 } else { 0 }
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
    /// A store for a device whose textures are at most `most_side` on a side.
    #[must_use]
    pub fn new(most_side: u32) -> Self {
        Self {
            held: Vec::new(),
            pending: Vec::new(),
            bound: None,
            pinned: Vec::new(),
            frame: 0,
            most_side,
        }
    }

    /// Whether the device makes a texture of `size`.
    #[must_use]
    pub fn fits(&self, size: (u32, u32)) -> bool {
        size.0 <= self.most_side && size.1 <= self.most_side
    }

    /// Takes a picture. The next `flush` allocates a texture and uploads it.
    ///
    /// **The size is recorded now and the pixels later**, which is not tidiness:
    /// the quads for a frame are resolved *before* the frame's uploads run, and
    /// resolving asks `size_of`. Registering only on upload meant the frame a page
    /// arrived on drew nothing at all, then the next one drew it — a page that
    /// appeared one frame late, every time.
    ///
    /// **A picture past the device's limit is held as a size and never
    /// uploaded.** `wgpu` answers a `create_texture` past it with a panic, and a
    /// picture that draws nothing is what the atlas already does with one it
    /// cannot place. Held rather than refused, because a page the store does not
    /// hold is one the panel asks for again — and the child would answer with the
    /// same picture, one round trip after another.
    pub fn hold(&mut self, key: ImageKey, size: (u32, u32), pixels: &[u8]) {
        if size.0 == 0 || size.1 == 0 || pixels.len() != (size.0 as usize * size.1 as usize * 4) {
            return;
        }
        if self.held.iter().any(|kept| kept.key == key) {
            return;
        }
        let on_device = self.fits(size);
        self.held.push(Kept { key, size, frame: self.frame, on_device });
        if on_device {
            self.pending.push((key, size, pixels.to_vec()));
        }
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

    /// Pictures the budget may not drop, whatever it costs.
    ///
    /// **[ADR 0028](../../../docs/adr/0028-the-page-you-are-on-and-its-two-neighbours-are-never-evicted.md).**
    /// The page on screen and the one on either side of it are the difference
    /// between turning pages and watching a placeholder, and they are pinned
    /// rather than merely recently drawn: a neighbour has *never* been drawn, so
    /// every eviction rule that sorts on when a picture was last on screen picks
    /// it first. Replaced whole each frame by the one caller that knows which
    /// pages those are.
    pub fn keep(&mut self, keys: &[ImageKey]) {
        self.pinned.clear();
        self.pinned.extend_from_slice(keys);
    }

    /// The keys to drop so the budget holds, least recently drawn first.
    ///
    /// **Never this frame's, and never a pinned one**, which is the atlas's rule
    /// applied to a second store plus ADR 0028's: a full store degrades to a page
    /// that has to be rendered again, and never to the wrong page on screen.
    pub fn over_budget(&mut self) -> Vec<ImageKey> {
        let mut total: usize = self.held.iter().map(Kept::bytes).sum();
        let mut dropped = Vec::new();
        while total > MOST_HELD_BYTES && self.held.len() > 1 {
            let current = self.frame;
            let pinned = &self.pinned;
            let Some(at) = self
                .held
                .iter()
                .enumerate()
                .filter(|(_, kept)| kept.frame != current && !pinned.contains(&kept.key))
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
        self.pinned.clear();
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

    use super::{ACROSS, ImageKey, Images, Preview, SLOT, origin};

    fn pixels(width: u32, height: u32) -> Vec<u8> {
        vec![7u8; (width * height * 4) as usize]
    }

    /// **Nothing past the device's limit reaches `create_texture`**, which is
    /// fed from `take` and nothing else — and the picture is still held as a
    /// size, or the panel would ask for it again on every frame it is missing.
    #[test]
    fn a_picture_past_the_device_limit_is_held_and_never_uploaded() {
        let mut store = Preview::new(2048);
        let (wide, fine) = (ImageKey(1), ImageKey(2));
        store.hold(wide, (2049, 1), &pixels(2049, 1));
        assert_eq!(store.size_of(wide), Some((2049, 1)), "dropped, so it would be asked for again");
        assert!(!store.owes_upload(), "a texture past the limit was queued");
        assert_eq!(store.stats(), (1, 0), "it is counted at bytes it never took");

        store.hold(fine, (2048, 1), &pixels(2048, 1));
        assert_eq!(store.take().map(|(key, size, _)| (key, size)), Some((fine, (2048, 1))));
        assert!(store.take().is_none());
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

/// The icon atlas: four bands of equal slots in one texture.
///
/// **[ADR 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md),
/// and it is not [`Images`].** That grid is 128×128 slots keyed by path,
/// modification time and size, and its whole argument rests on one sentence: a
/// thumbnail is one size. An icon is not. It is keyed by theme, name, size and
/// scale, and it is drawn from 16 device pixels in a details row to something
/// near 92 in a grid cell at 200%; the freedesktop per-size directories exist
/// precisely because 16-pixel art is not 48-pixel art downscaled. A 16-pixel icon
/// in a 64 KiB slot spends 1/64th of the pixels it paid for.
///
/// So: one 1024×1024 RGBA8 texture — 4 MiB — cut into four equal bands of 256
/// rows. **The slot count quadruples as the side halves**, which is the right way
/// round: a details list shows the most rows and draws the smallest mark, a grid
/// draws the largest mark in the fewest cells.
///
/// | Band | Slot | Slots | A slot | The band |
/// | --- | --- | --- | --- | --- |
/// | 128 device px | 128×128 | 16 | 64 KiB | 1 MiB |
/// | 64 device px | 64×64 | 64 | 16 KiB | 1 MiB |
/// | 32 device px | 32×32 | 256 | 4 KiB | 1 MiB |
/// | 16 device px | 16×16 | 1,024 | 1 KiB | 1 MiB |
///
/// Eviction and the upload budget are [`Images`]', unchanged: least recently
/// drawn, never a slot drawn this frame, and the same 256 KiB a frame. At the 16
/// band a slot is 1 KiB, so 256 icons arrive inside the budget that carries four
/// thumbnails.
///
/// **One texel until the first icon**, like the thumbnail atlas — a window under
/// `icons = marks` allocates none of the four megabytes.
#[derive(Default)]
pub struct Icons {
    entries: HashMap<ImageKey, Held>,
    pending: Vec<Queued>,
    /// Slots never yet handed out, per band. Once a band reaches its end,
    /// eviction begins *within that band* — a 16-pixel icon can never take a
    /// 128-pixel slot, because it would leave 63/64ths of it unread.
    next: [u32; BANDS.len()],
    frame: u64,
    used: bool,
}

/// The atlas texture's side, in texels.
pub const ICON_SIDE: u32 = 1024;

/// How tall one band is. Four equal quarters of [`ICON_SIDE`].
const BAND_ROWS: u32 = ICON_SIDE / 4;

/// Each band's slot side, largest first — which is also top to bottom.
const BANDS: [u32; 4] = [128, 64, 32, 16];

/// An icon waiting for the queue, at the corner it goes to.
///
/// **Its corner rather than its slot**, because a slot index means nothing
/// without the band it is in and this list is drained by a caller that has
/// neither.
struct Queued {
    at: (u32, u32),
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Where one icon is: which band, and which slot of it.
struct Held {
    band: u8,
    slot: u32,
    width: u32,
    height: u32,
    /// The frame this was last drawn in, which is what eviction sorts on.
    seen: u64,
}

/// The band an icon of `device_px` is drawn from.
///
/// **At or above, never upscaled.** A 24-pixel mark comes from the 32 band and a
/// 92-pixel grid mark at 200% from the 128 band, because a theme's 32-pixel art
/// is not its 16-pixel art doubled and drawing it as if it were is the one thing
/// an icon theme exists to avoid.
///
/// Above 128 it is the 128 band, and the picture is drawn at 128 and centred in
/// the larger box rather than blown up to fill it — `icon_quad` never enlarges.
/// That is ADR 0018's own revisit trigger restated: a view that draws a mark
/// larger than that wants either a bigger largest band or a fifth size, and the
/// second is a different decision.
#[must_use]
pub fn icon_band(device_px: u32) -> u32 {
    BANDS.into_iter().rev().find(|side| *side >= device_px).unwrap_or(BANDS[0])
}

impl Icons {
    /// A new frame. Everything drawn from now on is marked with this number.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Whether anything has ever been inserted, which is what makes the renderer
    /// swap its one-texel placeholder for the real atlas.
    #[must_use]
    pub fn in_use(&self) -> bool {
        self.used
    }

    /// How many icons are held, and how full the four bands are together.
    #[must_use]
    pub fn stats(&self) -> (usize, f32) {
        let slots: u32 = BANDS.into_iter().map(slots_in).sum();
        (self.entries.len(), self.entries.len() as f32 / slots as f32)
    }

    /// Whether `key` is held, without marking it as drawn.
    ///
    /// A separate call from [`Self::get`] for the reason [`Images::holds`] is: a
    /// caller deciding whether to *fetch* something must not make it look more
    /// recently used than it is.
    #[must_use]
    pub fn holds(&self, key: ImageKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// How many uploads are waiting, for the diagnostic line.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Where `key` is, marking it as wanted this frame.
    pub fn get(&mut self, key: ImageKey) -> Option<Slot> {
        let frame = self.frame;
        let held = self.entries.get_mut(&key)?;
        held.seen = frame;
        Some(place_in_band(held.band, held.slot, held.width, held.height))
    }

    /// Puts an icon in the band that fits it, evicting the least recently drawn
    /// of that band if it must.
    ///
    /// `pixels` is straight `RGBA8`. An image larger than the largest band is
    /// refused rather than scaled: the caller asked the child for a box, so a
    /// bigger answer is a protocol violation.
    pub fn insert(
        &mut self,
        key: ImageKey,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Option<Slot> {
        if width == 0 || height == 0 || width > BANDS[0] || height > BANDS[0] {
            return None;
        }
        if pixels.len() != (width * height * 4) as usize {
            return None;
        }
        if let Some(slot) = self.get(key) {
            return Some(slot);
        }
        let side = icon_band(width.max(height));
        let band = u8::try_from(BANDS.iter().position(|it| *it == side)?).ok()?;
        let slot = self.free_slot(band)?;
        self.used = true;
        self.entries.insert(key, Held { band, slot, width, height, seen: self.frame });
        self.pending.push(Queued {
            at: corner(band, slot),
            width,
            height,
            pixels: pixels.to_vec(),
        });
        Some(place_in_band(band, slot, width, height))
    }

    /// A slot nobody is using in `band`, or the one drawn longest ago in it.
    fn free_slot(&mut self, band: u8) -> Option<u32> {
        let side = *BANDS.get(band as usize)?;
        let at = band as usize;
        let taken = *self.next.get(at)?;
        if taken < slots_in(side) {
            *self.next.get_mut(at)? = taken + 1;
            return Some(taken);
        }
        // **Never a slot this frame has drawn**, which is `Images`' rule: taking
        // one would replace an icon that is on screen with a different picture in
        // the same row.
        let frame = self.frame;
        let (oldest, _) = self
            .entries
            .iter()
            .filter(|(_, held)| held.band == band && held.seen != frame)
            .min_by_key(|(key, held)| (held.seen, **key))?;
        let oldest = *oldest;
        let held = self.entries.remove(&oldest)?;
        // Anything queued for that corner is queued for an icon nobody wants any
        // more.
        let freed = corner(band, held.slot);
        self.pending.retain(|it| it.at != freed);
        Some(held.slot)
    }

    /// Uploads what fits in this frame's budget.
    pub fn flush(&mut self, queue: &wgpu::Queue, texture: &wgpu::Texture) {
        let mut spent = 0usize;
        let mut at = 0usize;
        while at < self.pending.len() {
            let Some(image) = self.pending.get(at) else { break };
            let bytes = image.pixels.len();
            if spent > 0 && spent + bytes > UPLOAD_BUDGET_BYTES {
                break;
            }
            spent += bytes;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: image.at.0, y: image.at.1, z: 0 },
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

/// How many slots a band of `side`-texel squares holds.
const fn slots_in(side: u32) -> u32 {
    (ICON_SIDE / side) * (BAND_ROWS / side)
}

/// One slot's top-left corner in texels.
fn corner(band: u8, slot: u32) -> (u32, u32) {
    let at = band as usize;
    let Some(side) = BANDS.get(at).copied() else { return (0, 0) };
    let across = ICON_SIDE / side;
    let top = u32::try_from(at).unwrap_or(0) * BAND_ROWS;
    ((slot % across) * side, top + (slot / across) * side)
}

fn place_in_band(band: u8, slot: u32, width: u32, height: u32) -> Slot {
    let (x, y) = corner(band, slot);
    Slot { x, y, width, height }
}

#[cfg(test)]
mod band_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{BAND_ROWS, BANDS, ICON_SIDE, Icons, ImageKey, corner, icon_band, slots_in};

    fn pixels(width: u32, height: u32) -> Vec<u8> {
        vec![9u8; (width * height * 4) as usize]
    }

    /// The table in [`Icons`]' own documentation, asserted rather than claimed:
    /// four megabytes, four equal quarters, and 1,360 slots.
    #[test]
    fn the_four_bands_are_the_ones_the_record_names() {
        assert_eq!(ICON_SIDE, 1024);
        assert_eq!(ICON_SIDE * ICON_SIDE * 4, 4 * 1024 * 1024, "the texture is not 4 MiB");
        assert_eq!(BAND_ROWS * 4, ICON_SIDE, "the bands are not four equal quarters");
        let counts: Vec<u32> = BANDS.into_iter().map(slots_in).collect();
        assert_eq!(counts, vec![16, 64, 256, 1024]);
        assert_eq!(counts.iter().sum::<u32>(), 1360);
        // Each band is a megabyte: the slot count quadruples as its side halves.
        for (side, count) in BANDS.into_iter().zip(counts) {
            assert_eq!(side * side * 4 * count, 1024 * 1024, "the {side} band is not 1 MiB");
        }
    }

    /// **At or above the device size, never upscaled.** A 24-pixel mark comes
    /// from the 32 band and a 92-pixel grid mark at 200% from the 128 band.
    #[test]
    fn a_mark_is_drawn_from_the_band_at_or_above_it() {
        assert_eq!(icon_band(1), 16);
        assert_eq!(icon_band(16), 16);
        assert_eq!(icon_band(17), 32);
        assert_eq!(icon_band(24), 32);
        assert_eq!(icon_band(32), 32);
        assert_eq!(icon_band(33), 64);
        assert_eq!(icon_band(92), 128);
        assert_eq!(icon_band(128), 128);
        // ADR 0018's own revisit trigger in this atlas's terms: above the largest
        // band there is nowhere else to go.
        assert_eq!(icon_band(200), 128);
    }

    /// Every slot of every band has its own corner, all of them inside the
    /// texture, and no two anywhere near each other.
    #[test]
    fn every_slot_of_every_band_has_its_own_corner() {
        let mut seen = Vec::new();
        for (at, side) in BANDS.into_iter().enumerate() {
            let band = u8::try_from(at).unwrap();
            for slot in 0..slots_in(side) {
                let (x, y) = corner(band, slot);
                assert!(x + side <= ICON_SIDE, "band {side} slot {slot} runs off the right");
                assert!(y + side <= ICON_SIDE, "band {side} slot {slot} runs off the bottom");
                // A band owns its own quarter and nothing else.
                let top = u32::try_from(at).unwrap() * BAND_ROWS;
                assert!((top..top + BAND_ROWS).contains(&y), "band {side} left its quarter");
                assert!(!seen.contains(&(x, y)), "band {side} slot {slot} lands on another");
                seen.push((x, y));
            }
        }
        assert_eq!(seen.len(), 1360);
    }

    /// **One texel until the first icon.** A window under `icons = marks` never
    /// inserts one and so allocates none of the four megabytes.
    #[test]
    fn an_atlas_nobody_has_used_is_not_in_use() {
        let mut icons = Icons::default();
        assert!(!icons.in_use());
        assert!(icons.get(ImageKey(1)).is_none());
        assert!(!icons.holds(ImageKey(1)));
        assert!(!icons.in_use(), "asking about an absent icon allocated one");
        assert_eq!(icons.stats(), (0, 0.0));

        icons.insert(ImageKey(1), 16, 16, &pixels(16, 16));
        assert!(icons.in_use());
    }

    /// An icon lands in the band its own size names, which is what stops a
    /// 16-pixel mark spending a 64 KiB slot.
    #[test]
    fn an_icon_lands_in_its_own_band() {
        let mut icons = Icons::default();
        for (key, side) in [(1u64, 16u32), (2, 32), (3, 64), (4, 128)] {
            let slot = icons.insert(ImageKey(key), side, side, &pixels(side, side)).unwrap();
            let at = BANDS.iter().position(|it| *it == side).unwrap();
            let top = u32::try_from(at).unwrap() * BAND_ROWS;
            assert_eq!(slot.y, top, "a {side}-pixel icon is not in the {side} band");
            assert_eq!((slot.width, slot.height), (side, side));
        }
        // A picture that is not square takes the band its longer side needs.
        let wide = icons.insert(ImageKey(5), 48, 20, &pixels(48, 20)).unwrap();
        assert_eq!(wide.y, BAND_ROWS, "a 48-pixel icon is not in the 64 band");
    }

    /// The same key twice is the same slot and one upload, and a buffer that
    /// disagrees with the dimensions beside it is refused.
    #[test]
    fn an_icon_goes_in_once_and_comes_back_from_the_same_slot() {
        let mut icons = Icons::default();
        let first = icons.insert(ImageKey(1), 32, 24, &pixels(32, 24)).unwrap();
        let again = icons.insert(ImageKey(1), 32, 24, &pixels(32, 24)).unwrap();
        assert_eq!(first, again);
        assert_eq!(icons.pending(), 1, "the same icon was queued twice");
        assert_eq!(icons.get(ImageKey(1)), Some(first));

        assert!(icons.insert(ImageKey(2), 129, 8, &pixels(129, 8)).is_none(), "129 fits no band");
        assert!(icons.insert(ImageKey(3), 0, 8, &[]).is_none());
        assert!(icons.insert(ImageKey(4), 8, 8, &pixels(4, 4)).is_none());
    }

    /// **A full band evicts within itself.** A 16-pixel icon can never take a
    /// 128-pixel slot: it would leave sixty-three sixty-fourths of it unread, and
    /// the whole argument for four bands is that it should not.
    #[test]
    fn a_full_band_takes_its_own_oldest_and_never_another_bands() {
        let mut icons = Icons::default();
        let all = slots_in(128);
        for index in 0..all {
            icons.begin_frame();
            assert!(
                icons.insert(ImageKey(u64::from(index)), 128, 128, &pixels(128, 128)).is_some()
            );
        }
        // The other three bands are untouched by any of that.
        assert_eq!(icons.stats().0 as u32, all);

        icons.begin_frame();
        assert!(icons.get(ImageKey(u64::from(all - 1))).is_some());
        assert!(icons.insert(ImageKey(9999), 128, 128, &pixels(128, 128)).is_some());
        assert!(icons.get(ImageKey(0)).is_none(), "the oldest was kept");
        assert!(icons.get(ImageKey(u64::from(all - 1))).is_some(), "this frame's went");
        assert_eq!(icons.stats().0 as u32, all, "the count moved");

        // And a small icon still fits, because its band was never full.
        icons.begin_frame();
        let small = icons.insert(ImageKey(5000), 16, 16, &pixels(16, 16)).unwrap();
        assert_eq!(small.y, 3 * BAND_ROWS, "a full 128 band cost the 16 band its slot");
    }

    /// A band every slot of which is on screen refuses rather than replacing one:
    /// a row that stays blank for a frame is better than a row showing another
    /// file's icon.
    #[test]
    fn a_band_entirely_in_use_this_frame_refuses() {
        let mut icons = Icons::default();
        icons.begin_frame();
        for index in 0..slots_in(128) {
            icons.insert(ImageKey(u64::from(index)), 128, 128, &pixels(128, 128));
        }
        assert!(icons.insert(ImageKey(9999), 128, 128, &pixels(128, 128)).is_none());
        // The 64 band is not full and takes it, which is what makes the refusal a
        // band's and not the atlas's.
        assert!(icons.insert(ImageKey(9999), 64, 64, &pixels(64, 64)).is_some());
    }
}
