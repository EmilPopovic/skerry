// SPDX-License-Identifier: Apache-2.0
//! Recursive walks and the aggregation over them.
//!
//! Does not own: rendering results, or the query UI.
//!
//! What is here and what is owed: **recursive size aggregation** and **a search
//! over one subtree or over every local filesystem**, both pulled forward from
//! M4 — a folder with no size is a folder you have to open to understand, and a
//! file manager that cannot find a file is a reason to open a terminal.
//!
//! The **persistent index** that [ADR
//! 0008](../../../docs/adr/0008-persistent-index-is-required.md) made mandatory
//! is here too, in [`store`], with its format settled by [ADR
//! 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md) —
//! and it is what makes the wider scope answerable: **8,472 ms walked against
//! 36.2 ms scanned** over 6.9 million entries, which is
//! [ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)'s
//! measurement and the reason that record exists.
//!
//! Still M4's and still absent: extension facets, and ignore rules.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![forbid(unsafe_code)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod find;
pub mod size;
pub mod store;

pub use find::{Found, Hit, find};
pub use size::{Progress, Sizes, measure};
pub use store::Index;
