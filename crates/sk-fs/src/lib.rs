//! `getdents64` enumeration, batched `statx`, and the device probe that decides
//! which pool lane a directory reads on.
//!
//! Does not own: deciding *what* to enumerate, or why.
//!
//! Everything here runs on a job and takes a cancellation token. Nothing here is
//! callable from the UI thread, and that is not a convention — it is rule 1, and
//! [Spike B](../../../spikes/b-enumeration/README.md) measured what breaking it
//! costs: the same call that takes 4 ms on the pool took **28,000 ms** on the
//! frame loop with one unresponsive mount.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod copy;
pub mod device;
pub mod mounts;
pub mod ops;
pub mod scan;
pub mod text;
pub mod watch;

pub use copy::{Copied, Placement, Strategy, copy_file, move_file};
pub use device::{Device, lane_for};
pub use mounts::{Mount, mounts};
pub use scan::{META_CHUNK, MetaRow, Names, metadata, names};
pub use text::{TEXT_LIMIT, read_text};
pub use watch::{Change, Watcher, can_watch};
