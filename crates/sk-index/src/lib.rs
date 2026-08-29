//! Recursive walks and the aggregation over them.
//!
//! Does not own: rendering results, or the query UI.
//!
//! What is here and what is owed: **recursive size aggregation** and **a search
//! over one subtree**, both pulled forward from M4 — a folder with no size is a
//! folder you have to open to understand, and a file manager that cannot find a
//! file is a reason to open a terminal.
//!
//! Still M4's and still absent: the **persistent index** that [ADR
//! 0008](../../../docs/adr/0008-persistent-index-is-required.md) made mandatory,
//! which is what makes a search over the *whole filesystem* answerable at all;
//! the **flattened view**, meaning results as a listing that can be selected,
//! copied and dragged in place rather than navigated to; extension facets; and
//! ignore rules.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod find;
pub mod size;

pub use find::{Found, Hit, find};
pub use size::{Progress, Sizes, measure};
