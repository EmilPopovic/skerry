// SPDX-License-Identifier: MIT OR Apache-2.0
//! Drag-and-drop, in terms neither display server owns.
//!
//! The two halves are not symmetric and neither is the vocabulary.
//!
//! **The action sets cannot be unified.** XDND has copy, move, link, ask and
//! private; Wayland has copy, move and ask. Spike D found this by running both,
//! and [`DragAction`] is the union rather than the intersection — the policy
//! above this crate greys out what the current server cannot express, because
//! the alternative is a "link here" that silently does something else.
//!
//! **A source cannot know what happened to what it gave away.** Spike D measured
//! `wl_data_source.action` and `wl_data_offer.action` sent *zero* times across
//! every peer pairing, including the ones where a file demonstrably landed, and
//! `dnd_finished` delivered by some peers and not others. A move is "copy, then
//! delete the originals once the drop is confirmed" and the confirmation is the
//! part that does not arrive — so **this crate never reports a drag as a move to
//! its caller**, and the caller must never delete anything because of one. Every
//! outgoing drag is a copy as far as the originals are concerned. That is the
//! safe direction and it is the only one available.

use crate::event::Point;

/// What may be done with what a drag is carrying.
///
/// The union of both servers' vocabularies. [`DragAction::Link`] and
/// [`DragAction::Private`] have no Wayland equivalent and
/// [`Window::drag_actions`](crate::Window::drag_actions) says so, so a menu can
/// dim what this session cannot do rather than offering it and failing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DragAction {
    /// The receiver takes a copy and the original stays.
    #[default]
    Copy,
    /// The receiver takes it and the source removes the original.
    ///
    /// Meaningful **into** this window. Never reported for a drag out of it —
    /// see this module's header.
    Move,
    /// A symbolic link, which Wayland cannot express.
    Link,
    /// The receiver should ask which of the above.
    Ask,
    /// XDND's `XdndActionPrivate`: whatever the two ends agreed privately.
    Private,
}

impl DragAction {
    /// Whether a Wayland compositor can express this at all.
    #[must_use]
    pub const fn on_wayland(self) -> bool {
        matches!(self, Self::Copy | Self::Move | Self::Ask)
    }
}

/// A drag hovering over this window, and what it is carrying.
#[derive(Clone, Debug, Default)]
pub struct Drag {
    /// The MIME types the source is offering.
    pub offered: Vec<String>,
    /// What the source says may be done with them.
    pub actions: Vec<DragAction>,
    /// Where the pointer is, in logical pixels.
    pub position: Point,
}

impl Drag {
    /// The first of `wanted` the source is offering.
    #[must_use]
    pub fn choose<'a>(&self, wanted: &[&'a str]) -> Option<&'a str> {
        wanted.iter().copied().find(|want| self.offered.iter().any(|have| have == want))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wanted_order_wins_and_not_the_offered_one() {
        let drag = Drag {
            offered: vec!["text/plain".to_owned(), "text/uri-list".to_owned()],
            ..Drag::default()
        };
        assert_eq!(drag.choose(&["text/uri-list", "text/plain"]), Some("text/uri-list"));
        assert_eq!(drag.choose(&["image/png"]), None);
    }

    /// Two of the five have no Wayland spelling, and a caller that offered one
    /// there would be offering something that cannot happen.
    #[test]
    fn the_actions_wayland_cannot_express_say_so() {
        assert!(DragAction::Copy.on_wayland());
        assert!(DragAction::Move.on_wayland());
        assert!(DragAction::Ask.on_wayland());
        assert!(!DragAction::Link.on_wayland());
        assert!(!DragAction::Private.on_wayland());
    }
}
