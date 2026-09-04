// SPDX-License-Identifier: MIT OR Apache-2.0
//! Font discovery, shaping, line layout, the layout cache, and glyph
//! rasterization.
//!
//! Does not own: the glyph atlas — that is GPU memory, so it belongs to `sk-gpu`.
//!
//! [ADR 0004](../../../docs/adr/0004-swash-for-text.md) chose `swash` for
//! shaping and rasterization in one dependency. Everything above it here is
//! ours: which fonts exist, which one a cluster falls back to, what a line
//! costs, and what is cached between frames.
//!
//! Real shaping, bidi and fallback are here from the first commit rather than
//! after the first bug report, because File Pilot's missing Unicode support is a
//! defect to learn from and not one to inherit.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod discover;
pub mod fonts;
pub mod layout;
pub mod shape;

pub use fonts::{Fonts, GlyphImage, GlyphKey, Metrics, SUBPIXEL_BUCKETS, Weight};
pub use layout::{LayoutCache, RunKey};
pub use shape::{ShapedGlyph, ShapedRun};
