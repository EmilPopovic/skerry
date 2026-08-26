//! Wayland protocol handling, keyboard mapping, pointer input, monitors and
//! fractional scaling, and window lifecycle.
//!
//! Does not own: drawing anything.
//!
//! [ADR 0002](../../../docs/adr/0002-own-platform-layer.md) argues why this is
//! ours and prices it. What arrives from here is already policy: logical
//! coordinates, resolved keysyms, counted clicks, coalesced scroll — the things
//! a general-purpose windowing crate either decides differently or leaves out.
//!
//! # What is here and what is owed
//!
//! Wayland windowing, input, scale, the clipboard and the primary selection are
//! here. **X11, drag-and-drop and IME are not yet**, and M0 is not finished
//! without them; Spike D proved each of them against real GTK and Qt peers, and
//! the ordering invariants it found are recorded in [`wayland`] and in
//! [`wayland::clipboard`].

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod event;
pub mod keys;
pub mod wayland;

pub use event::{Button, Event, Key, KeyEvent, Modifiers, Named, Point, Size};
pub use wayland::clipboard::{Clipboard, Content, Selection, decode_uri};
pub use wayland::{Capabilities, RawHandles, Window, WindowConfig};
