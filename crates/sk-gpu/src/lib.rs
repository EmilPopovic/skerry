//! Device and surface setup, the glyph atlas, instanced quad batching, and
//! present pacing.
//!
//! Does not own: layout, hit-testing, widget state.
//!
//! [ADR 0003](../../../docs/adr/0003-wgpu-first-backend.md) chose `wgpu`, and
//! this crate is the seam it named: no `wgpu` type appears in any signature
//! here, which `tools/check-invariants.py` enforces by refusing the word
//! anywhere above this directory. A hand-written `ash` backend would replace
//! this crate's internals and nothing else.
//!
//! Everything visible is an instanced quad — row backgrounds, selection,
//! focus rings, glyph masks, colour glyphs — so a frame is one buffer write and
//! one draw call per clip region. Spike A measured the GPU side of that at
//! **0.032 ms** worst case of an 8 ms budget, which is why the CPU side is where
//! all the design attention goes.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

mod atlas;
mod renderer;

pub mod capture;
pub mod draw;

pub use draw::{Color, Direction, DrawList, Rect};
pub use renderer::{FrameStats, Renderer};
