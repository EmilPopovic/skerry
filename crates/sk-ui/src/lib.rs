//! The immediate-mode pass: layout primitives, text painting, virtualized lists,
//! scrolling and hit-testing.
//!
//! Does not own: which widgets a file manager needs.
//!
//! Everything here works in **device pixels**, converted from logical pixels by
//! the scale `sk-plat` negotiated. That conversion happens in exactly one place
//! ([`Painter::px`]) so a hit test and the rectangle it tests against cannot
//! disagree at 125% — which is the class of bug that makes fractional scaling
//! feel broken rather than merely blurry.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod format;
pub mod painter;
pub mod scroll;
pub mod theme;

pub use painter::{Align, Painter, TextStyle, caret_offset};
pub use scroll::{MIN_THUMB, Scroll, thumb, visible_rows};
pub use sk_gpu::Direction;
pub use theme::{Kinds, Metrics, Palette, Theme};

// Re-exported so a view needs one `use` for the drawing vocabulary rather than
// three, and so the layout cache's key type travels with the painter that needs
// it.
pub use sk_text::{RunKey, Weight};
