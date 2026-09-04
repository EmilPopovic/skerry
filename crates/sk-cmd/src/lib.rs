// SPDX-License-Identifier: MIT OR Apache-2.0
//! The command registry and keymap resolution.
//!
//! Does not own: any command's implementation.
//!
//! Rule 2 is that every action, toggle and setting is **one entry in one
//! registry**, and that the palette, the menus, the keymap and the config file
//! are four views of that single list so they cannot drift apart. This crate is
//! the list. Nothing here knows what a file is; the registry is generic over the
//! context a command runs against, which is what keeps a level-2 crate from
//! needing to know about level 4.
//!
//! What is here and what is owed: the registry, chord resolution, the undo stack
//! and the palette's ranking. The palette's *surface* is `sk-app`'s, because it
//! draws — see [docs/04-architecture.md](../../../docs/04-architecture.md#crate-responsibilities).

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod chord;
pub mod fuzzy;
pub mod history;
pub mod registry;

pub use chord::Chord;
pub use fuzzy::score;
pub use history::History;
pub use registry::{CommandId, CommandSpec, Group, Registry, Scope};
