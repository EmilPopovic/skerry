// SPDX-License-Identifier: Apache-2.0
//! `EntryId`, the columnar listing every view reads, selection, sort order, the
//! string catalogue, and the problem type that carries a message a human can act
//! on.
//!
//! Does not own: any I/O, any allocation strategy above the store's own, any UI
//! concept.
//!
//! Two things live here that [docs/04-architecture.md](../../../docs/04-architecture.md#crate-responsibilities)
//! first put elsewhere, and both moved for the same reason — the first commit
//! with a caller showed the caller was here:
//!
//! * **The arena and the bitset.** They are not general-purpose containers; they
//!   are the representation of the name column and of the selection. See
//!   [`sk_util`](../sk_util/index.html) for the edge that made this a layering
//!   violation rather than a preference.
//! * **The catalogue.** Every level above needs `t!`, and level 0 is the only
//!   place a crate can be reached from all of them. It depends on nothing.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![forbid(unsafe_code)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod arena;
pub mod bitset;
pub mod collate;
pub mod i18n;
pub mod listing;
pub mod problem;
pub mod radix;
pub mod selection;
pub mod sort;

pub use arena::{StrArena, StrRef};
pub use bitset::BitSet;
pub use collate::natural_cmp;
pub use listing::{EntryId, Flags, Listing, contains_folded};
pub use problem::{Problem, ProblemKind};
pub use radix::KeySet;
pub use selection::Selection;
pub use sort::{Banding, Cuts, Sort, SortColumn};
