//! The pane, the views, and the frame loop.
//!
//! Does not own: anything reusable.
//!
//! # The frame loop, and why the CPU is idle
//!
//! The loop is the one in
//! [docs/04-architecture.md](../../../docs/04-architecture.md#the-frame-loop-precisely):
//! drain platform events, advance animation clocks by real elapsed time, run an
//! immediate-mode pass over the *visible* rows only, submit, and then — if
//! nothing changed and nothing is animating — sleep on the event queue with no
//! timer.
//!
//! Two things make that last clause true rather than aspirational. `sk-plat`
//! blocks in `poll` on the display socket and on the job pool's eventfd, so a
//! finished directory read wakes the loop at the moment it finishes and nothing
//! else does. And the loop only asks for a timeout when a scroll animation owes
//! a frame. There is no 60 Hz tick.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

mod app;
mod bench;
mod capture;
mod commands;
mod crumbs;
mod edit;
mod field;
mod frames;
mod headless;
mod keys;
mod layout;
mod menu;
mod openers;
mod palette;
mod pane;
mod panes;
mod properties;
mod search;
mod session;
mod sidebar;
mod sizes;
mod view;

pub use app::{App, Options};
pub use bench::{Bench, run as bench};
pub use capture::{Capture, run as capture};
// Re-exported because every entry point above returns one, and a binary that
// calls them has to be able to name what they fail with.
pub use pane::Pane;
pub use sk_model::Problem;
