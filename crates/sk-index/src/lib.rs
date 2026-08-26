//! Recursive walks and the aggregation over them.
//!
//! Does not own: rendering results, or the query UI.
//!
//! What is here and what is owed: **recursive size aggregation** is here, pulled
//! forward from M4 because a folder with no size is a folder you have to open to
//! understand. Flattened views, fuzzy matching, ignore rules and the persistent
//! index that [ADR 0008](../../../docs/adr/0008-persistent-index-is-required.md)
//! made mandatory are all M4's and are not here.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod size;

pub use size::{Progress, Sizes, measure};
