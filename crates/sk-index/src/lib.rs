//! Recursive walks and the aggregation over them.
//!
//! Does not own: rendering results, or the query UI.
//!
//! What is here and what is owed: **recursive size aggregation** and **a search
//! over one subtree**, both pulled forward from M4 — a folder with no size is a
//! folder you have to open to understand, and a file manager that cannot find a
//! file is a reason to open a terminal.
//!
//! The **persistent index** that [ADR
//! 0008](../../../docs/adr/0008-persistent-index-is-required.md) made mandatory
//! is here too, in [`store`], with its format settled by [ADR
//! 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md) —
//! which is what makes a search over a *whole home directory* answerable rather
//! than only a subtree.
//!
//! Still M4's and still absent: extension facets, and ignore rules.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod find;
pub mod size;
pub mod store;

pub use find::{Found, Hit, find};
pub use size::{Progress, Sizes, measure};
pub use store::Index;
