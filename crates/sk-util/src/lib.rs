// SPDX-License-Identifier: Apache-2.0
//! The one job pool for the process, its cancellation tokens, and the eventfd a
//! finished job wakes the frame loop with.
//!
//! Does not own: domain types. Nothing here knows what a file is.
//!
//! # Why the arena and the bitset are not here
//!
//! [docs/04-architecture.md](../../../docs/04-architecture.md#crate-responsibilities)
//! originally put bump arenas and bitsets in this crate alongside the pool. Both
//! moved to `sk-model` on the first commit that had a caller, because each had
//! exactly one: the arena backs the name column and the bitset backs the
//! selection. Leaving them here made `sk-model` depend on `sk-util`, which is a
//! sideways edge inside level 0 and `tools/check-layering.py` refuses it — and
//! the refusal was right. A general-purpose container crate would undo the data
//! layout it exists to serve.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![forbid(unsafe_code)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod pool;

pub use pool::{Cancel, Lane, Pause, Pool, WakeHandle, Waker};

/// Whether an optional runtime facility was disabled for a degradation check.
///
/// This never authorizes bypassing mandatory confinement.
#[must_use]
pub fn disabled(capability: &str) -> bool {
    std::env::var("SKERRY_DISABLE")
        .is_ok_and(|value| value.split(',').any(|name| name.trim() == capability))
}
