// SPDX-License-Identifier: Apache-2.0
//! The layout cache: shaped runs kept between frames, keyed by what they are for.

use std::collections::HashMap;

use crate::escape::Hidden;
use crate::fonts::{Fonts, Weight};
use crate::shape::ShapedRun;

/// Identifies a shaped run by what it is for rather than by its text.
///
/// A lookup is then a hash of twelve bytes instead of a hash of a filename, and
/// the arena is not touched at all on a hit.
///
/// **The listing is part of the key and is not optional.** An `EntryId` is an
/// index, so without it entry 3 of a directory and entry 3 of the next one are
/// the same key and the cache serves the first when asked for the second. That
/// is not hypothetical — see `sk_model::Listing::version`.
///
/// **A key says where a string is, not what it is,** which is why
/// [`LayoutCache::get_or_shape`] verifies the content of every hit. A cell's
/// text changes underneath a fixed key twice in this product: a folder total
/// climbs while the walk runs, and a `Modified` cell reading `21:40` becomes
/// `26 Aug` when the clock passes midnight. Both were drawn from the first run
/// ever shaped there, forever.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RunKey {
    /// Which listing, from `sk_model::Listing::version`.
    pub listing: u32,
    /// Which entry in that listing.
    pub entry: u32,
    /// Which column of that entry.
    pub column: u8,
    /// Which weight, so a bold header and a regular cell do not collide.
    pub weight: u8,
    /// Size in quarter device pixels, so a fractional scale change is a miss
    /// rather than a stale run at the wrong size.
    pub quarter_px: u16,
}

impl RunKey {
    /// A key for one cell of one listing.
    #[must_use]
    pub fn cell(listing: u32, entry: u32, column: u8, weight: Weight, px_size: f32) -> Self {
        Self {
            listing,
            entry,
            column,
            weight: weight as u8,
            quarter_px: (px_size * 4.0).round().clamp(0.0, f32::from(u16::MAX)) as u16,
        }
    }

    /// The size this key was made for.
    #[must_use]
    pub fn px_size(self) -> f32 {
        f32::from(self.quarter_px) / 4.0
    }
}

/// A two-generation cache of shaped runs.
///
/// A lookup checks `live`, then `stale` and promotes. When `live` outgrows the
/// cap it becomes `stale` wholesale and a fresh `live` starts, so eviction is
/// O(1) amortized with no sweep. That matters because a sweep is a frame spike,
/// and a frame spike is exactly what the 8 ms budget measures.
pub struct LayoutCache {
    live: HashMap<RunKey, Cell>,
    stale: HashMap<RunKey, Cell>,
    /// Runs keyed by their *content* rather than by a row.
    ///
    /// The row-keyed cache above cannot hold a status line: its key says which
    /// entry and which column, and the text at that key changes every time the
    /// selection does. A stale hit would draw last frame's sentence. So chrome
    /// text is keyed by a hash of the string and the hit is **verified against
    /// the stored string**, which makes a hash collision impossible to observe
    /// rather than merely unlikely.
    labels: HashMap<u64, Label>,
    cap: usize,
    hits: u64,
    misses: u64,
    generations: u64,
}

struct Label {
    text: String,
    end: usize,
    hidden: Hidden,
    weight: u8,
    quarter_px: u16,
    run: ShapedRun,
}

/// A run and a fingerprint of the string it was shaped from.
///
/// The fingerprint is 64 bits rather than the string itself because a cell hit
/// happens a few thousand times a second and the point of the row cache is that
/// it never touches the name arena. A collision draws one wrong cell and is
/// checked for in debug builds by [`ShapedRun::source_len`]; at 64 bits over the
/// few hundred strings a screen holds, that is not a thing that happens.
struct Cell {
    content: u64,
    run: ShapedRun,
}

impl LayoutCache {
    /// A cache holding roughly `cap` runs per generation.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            live: HashMap::with_capacity(cap * 2),
            stale: HashMap::with_capacity(cap * 2),
            labels: HashMap::with_capacity(64),
            cap,
            hits: 0,
            misses: 0,
            generations: 0,
        }
    }

    /// Runs currently held across both generations, plus chrome labels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len() + self.stale.len() + self.labels.len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Hits, misses and generation rolls, for the frame profiler.
    #[must_use]
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.generations)
    }

    /// The shaped run for a string, keyed by the string itself.
    ///
    /// For chrome — a path, a column header, a status line, a help entry. Cheap
    /// on a hit: one hash of a short string and a comparison, no allocation.
    pub fn get_or_shape_label(
        &mut self,
        text: &str,
        weight: Weight,
        px_size: f32,
        fonts: &mut Fonts,
    ) -> &ShapedRun {
        self.get_or_shape_part(text, text.len(), weight, px_size, Hidden::Escaped, fonts)
    }

    /// The shaped run for the first `end` bytes of `text`, drawn as `hidden`
    /// says — [`Fonts::shape_part`], kept like a label.
    ///
    /// **Keyed by the whole of `text` and by `end`, not by the prefix.** Two
    /// prefixes with the same bytes are different runs when what follows them
    /// decides whether their last joiner is escaped.
    pub fn get_or_shape_part(
        &mut self,
        text: &str,
        end: usize,
        weight: Weight,
        px_size: f32,
        hidden: Hidden,
        fonts: &mut Fonts,
    ) -> &ShapedRun {
        let quarter_px = (px_size * 4.0).round().clamp(0.0, f32::from(u16::MAX)) as u16;
        let weight_bits = weight as u8;
        let key = label_hash(text, (end, hidden), weight_bits, quarter_px);
        let stale = self.labels.get(&key).is_none_or(|label| {
            label.text != text
                || label.end != end
                || label.hidden != hidden
                || label.weight != weight_bits
                || label.quarter_px != quarter_px
        });
        if stale {
            self.misses += 1;
            // Chrome strings are few but not bounded — a status line changes with
            // every selection. Clearing wholesale at the cap is the same O(1)
            // eviction as the row cache, for the same reason: a sweep is a frame
            // spike.
            if self.labels.len() >= LABEL_CAP {
                self.labels.clear();
                self.generations += 1;
            }
            let run = fonts.shape_part(text, end, px_size, weight, hidden);
            let text = text.to_owned();
            self.labels
                .insert(key, Label { text, end, hidden, weight: weight_bits, quarter_px, run });
        } else {
            self.hits += 1;
        }
        self.labels
            .get(&key)
            .map_or_else(|| EMPTY.get_or_init(ShapedRun::default), |label| &label.run)
    }

    /// The shaped run for `key`, shaping `text` on a miss **or on a change**.
    ///
    /// There is no `invalidate`, and there was: the key carries the listing, so a
    /// new listing simply cannot hit an old entry and eviction handles the memory.
    /// A cache with an `invalidate` is a cache with a caller who has to remember
    /// to call it, and that caller did not.
    ///
    /// The content check is here rather than at the call site for the same
    /// reason. The key names a position — this row, this column — and a position
    /// is not a promise about a string; a caller cannot forget a check it is not
    /// offered.
    pub fn get_or_shape(&mut self, key: RunKey, text: &str, fonts: &mut Fonts) -> &ShapedRun {
        let content = content_hash(text);
        let fresh = |cell: &Cell| cell.content == content;
        // Cheap collision detector for debug builds and tests: a hit that passed
        // the fingerprint but was shaped from a different string. Comparing
        // lengths costs nothing.
        debug_assert!(
            self.live
                .get(&key)
                .or_else(|| self.stale.get(&key))
                .is_none_or(|cell| !fresh(cell) || cell.run.source_len == text.len()),
            "cache hit for {key:?} was shaped from a different string than {text:?}"
        );
        if self.live.get(&key).is_some_and(fresh) {
            self.hits += 1;
        } else if self.stale.get(&key).is_some_and(fresh) {
            self.hits += 1;
            if let Some(cell) = self.stale.remove(&key) {
                self.live.insert(key, cell);
            }
        } else {
            self.misses += 1;
            let weight =
                if key.weight == Weight::Bold as u8 { Weight::Bold } else { Weight::Regular };
            let run = fonts.shape(text, key.px_size(), weight);
            // A cell whose text changed reuses its slot, so a folder total that
            // climbs fifteen times a second does not roll a generation.
            if !self.live.contains_key(&key) && self.live.len() >= self.cap {
                self.generations += 1;
                std::mem::swap(&mut self.live, &mut self.stale);
                self.live.clear();
            }
            self.live.insert(key, Cell { content, run });
        }
        // The insert above guarantees the key is present.
        self.live.get(&key).map_or_else(|| EMPTY.get_or_init(ShapedRun::default), |cell| &cell.run)
    }
}

static EMPTY: std::sync::OnceLock<ShapedRun> = std::sync::OnceLock::new();

/// Chrome labels held before the map is cleared.
const LABEL_CAP: usize = 256;

/// FNV-1a over the string and the other key fields.
///
/// Not for security and not for distribution quality — the hit is verified
/// against the stored string, so this only has to be fast and to spread a few
/// hundred short strings.
fn label_hash(text: &str, part: (usize, Hidden), weight: u8, quarter_px: u16) -> u64 {
    let (end, hidden) = part;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let fields = [weight, hidden as u8];
    for byte in text
        .as_bytes()
        .iter()
        .chain(&fields)
        .chain(&quarter_px.to_le_bytes())
        .chain(&(end as u64).to_le_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// FNV-1a over a cell's string, to notice that it changed.
fn content_hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{LayoutCache, RunKey};
    use crate::fonts::{Fonts, Weight};

    #[test]
    fn a_key_distinguishes_listing_weight_column_and_size() {
        let base = RunKey::cell(1, 7, 0, Weight::Regular, 13.0);
        assert_ne!(base, RunKey::cell(1, 7, 1, Weight::Regular, 13.0));
        assert_ne!(base, RunKey::cell(1, 7, 0, Weight::Bold, 13.0));
        assert_ne!(base, RunKey::cell(1, 7, 0, Weight::Regular, 16.25));
        assert_eq!(base, RunKey::cell(1, 7, 0, Weight::Regular, 13.0));
        assert!((base.px_size() - 13.0).abs() < f32::EPSILON);
    }

    /// The reported defect, as a unit: the same row of two different listings is
    /// two different keys. Without this, navigating into a subdirectory drew the
    /// parent's names in the child's rows.
    #[test]
    fn the_same_row_of_two_listings_is_two_keys() {
        let first = RunKey::cell(41, 3, 0, Weight::Regular, 13.0);
        let second = RunKey::cell(42, 3, 0, Weight::Regular, 13.0);
        assert_ne!(first, second);
    }

    /// And the cache actually serves them apart, which is the property the key
    /// exists for.
    #[test]
    fn a_new_listing_cannot_hit_the_previous_listings_run() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(64);
        let parent = cache
            .get_or_shape(
                RunKey::cell(1, 3, 0, Weight::Regular, 13.0),
                "emil_popovic.pdf",
                &mut fonts,
            )
            .advance;
        let child = cache
            .get_or_shape(
                RunKey::cell(2, 3, 0, Weight::Regular, 13.0),
                "Vernii.kicad_pro",
                &mut fonts,
            )
            .advance;
        assert!(
            (parent - child).abs() > f32::EPSILON,
            "two different strings shaped to the same width: {parent} and {child}"
        );
        assert_eq!(cache.stats().0, 0, "neither lookup was a hit");
    }

    #[test]
    fn a_second_lookup_is_a_hit_and_costs_no_shaping() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(64);
        let key = RunKey::cell(1, 1, 0, Weight::Regular, 13.0);
        let first = cache.get_or_shape(key, "Makefile", &mut fonts).advance;
        let second = cache.get_or_shape(key, "Makefile", &mut fonts).advance;
        assert!((first - second).abs() < f32::EPSILON);
        assert_eq!(cache.stats(), (1, 1, 0));
    }

    /// The reported defect, as a unit. A folder total climbs while the walk
    /// runs; the row, the column and the listing do not change while it does.
    /// Before this check the size column drew the first string ever shaped
    /// there — a dash — and never anything else.
    #[test]
    fn a_cell_whose_text_changed_is_reshaped_rather_than_served_stale() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(64);
        let key = RunKey::cell(1, 3, 1, Weight::Regular, 13.0);
        let dash = cache.get_or_shape(key, "—", &mut fonts).advance;
        let total = cache.get_or_shape(key, "1.4 GB", &mut fonts).advance;
        assert!((dash - total).abs() > f32::EPSILON, "both shaped to {dash}");
        assert_eq!(cache.stats().0, 0, "the changed string was served from the cache");
    }

    /// And a changed cell reuses its slot. A total that updates fifteen times a
    /// second at forty visible rows would otherwise roll a generation every few
    /// seconds and re-shape every name on screen with it.
    #[test]
    fn a_cell_that_changes_repeatedly_does_not_evict_its_neighbours() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(8);
        let neighbour = RunKey::cell(1, 0, 0, Weight::Regular, 13.0);
        cache.get_or_shape(neighbour, "quarterly-report.pdf", &mut fonts);
        let climbing = RunKey::cell(1, 1, 1, Weight::Regular, 13.0);
        for step in 0..200u32 {
            cache.get_or_shape(climbing, &format!("{step} MB"), &mut fonts);
        }
        assert_eq!(cache.stats().2, 0, "a changing cell rolled a generation");
        let before = cache.stats().1;
        cache.get_or_shape(neighbour, "quarterly-report.pdf", &mut fonts);
        assert_eq!(cache.stats().1, before, "the neighbour was evicted and re-shaped");
    }

    /// The second instance of the same mechanism: a window left open past
    /// midnight. `format::modified` returns a clock time for today and a date
    /// otherwise, so every `Modified` cell changes at 00:00 with nothing else
    /// about the row changing at all.
    #[test]
    fn a_modified_cell_follows_the_clock_past_midnight() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(64);
        let key = RunKey::cell(7, 12, 2, Weight::Regular, 13.0);
        let today = cache.get_or_shape(key, "21:40", &mut fonts).advance;
        let yesterday = cache.get_or_shape(key, "26 Aug", &mut fonts).advance;
        assert!((today - yesterday).abs() > f32::EPSILON, "both shaped to {today}");
    }

    #[test]
    fn outgrowing_the_cap_rolls_a_generation_instead_of_sweeping() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(8);
        for entry in 0..40u32 {
            cache.get_or_shape(RunKey::cell(1, entry, 0, Weight::Regular, 13.0), "x", &mut fonts);
        }
        let (_, _, generations) = cache.stats();
        assert!(generations >= 4, "{generations} generations for 40 runs at a cap of 8");
        // Two generations of a cap of 8 is the ceiling, so memory is bounded.
        assert!(cache.len() <= 17, "{} runs held", cache.len());
    }

    #[test]
    fn a_run_that_fell_out_of_live_is_promoted_rather_than_reshaped() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let mut cache = LayoutCache::new(4);
        let key = RunKey::cell(1, 0, 0, Weight::Regular, 13.0);
        cache.get_or_shape(key, "kept", &mut fonts);
        for entry in 1..5u32 {
            cache.get_or_shape(RunKey::cell(1, entry, 0, Weight::Regular, 13.0), "x", &mut fonts);
        }
        let (_, misses_before, _) = cache.stats();
        cache.get_or_shape(key, "kept", &mut fonts);
        let (_, misses_after, _) = cache.stats();
        assert_eq!(misses_before, misses_after, "the stale generation was not consulted");
    }
}
