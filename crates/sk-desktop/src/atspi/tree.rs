// SPDX-License-Identifier: Apache-2.0
//! What a screen reader is told about: nodes, roles, states, and the text in
//! them.
//!
//! **[ADR 0026](../../../../docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md).**
//! The tree is assembled once a frame from what the frame recorded, so a control
//! that is on screen and a control that is in here are the same list. Nothing in
//! this module talks to a bus: it is the data, and `super::serve` is the wire.
//!
//! **Numbers, not names.** A role and a state are integers in AT-SPI's own
//! enumerations, and the two tables below are the whole of the mapping. They are
//! written out rather than derived because the enumerations are somebody else's
//! and a wrong number is a role a reader announces incorrectly rather than an
//! error anybody sees.

/// A node's role, as AT-SPI numbers them.
///
/// Only the roles this window has. The numbers are ATK's, unchanged since
/// AT-SPI 2.0 and shared by every toolkit on the bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Role {
    /// The application itself, which is the root of what we publish.
    Application = 75,
    /// A top-level window with a frame.
    Frame = 23,
    /// A grouping with no semantics of its own.
    Panel = 39,
    /// A list of items — a listing, the sidebar, a set of results.
    List = 31,
    /// One item in a list.
    ListItem = 32,
    /// A button that does something when pressed.
    PushButton = 43,
    /// One line of editable text.
    Entry = 79,
    /// Static text.
    Label = 29,
    /// A menu that is open.
    Menu = 33,
    /// One row of a menu.
    MenuItem = 35,
    /// One tab of a tab strip.
    PageTab = 37,
    /// The strip itself.
    PageTabList = 38,
    /// A scrollable region.
    ScrollPane = 49,
    /// The line at the bottom that says what is happening.
    StatusBar = 54,
    /// A region of read-only text, such as a preview.
    Text = 61,
}

impl Role {
    /// What `GetRoleName` answers: AT-SPI's own spelling, lower case with spaces.
    #[must_use]
    pub const fn spelled(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Frame => "frame",
            Self::Panel => "panel",
            Self::List => "list",
            Self::ListItem => "list item",
            Self::PushButton => "push button",
            Self::Entry => "entry",
            Self::Label => "label",
            Self::Menu => "menu",
            Self::MenuItem => "menu item",
            Self::PageTab => "page tab",
            Self::PageTabList => "page tab list",
            Self::ScrollPane => "scroll pane",
            Self::StatusBar => "statusbar",
            Self::Text => "text",
        }
    }
}

/// The state bits this window sets, as AT-SPI numbers them.
///
/// A state set travels as two `u32`s, low word first — which is why these are
/// bit *positions* and not a bitflags type.
pub mod state {
    /// The window that has the keyboard.
    pub const ACTIVE: u64 = 1 << 1;
    /// Text that can be typed into.
    pub const EDITABLE: u64 = 1 << 7;
    /// Not greyed out.
    pub const ENABLED: u64 = 1 << 8;
    /// Can take the keyboard.
    pub const FOCUSABLE: u64 = 1 << 11;
    /// Has it now.
    pub const FOCUSED: u64 = 1 << 12;
    /// Can be chosen.
    pub const SELECTABLE: u64 = 1 << 22;
    /// Is chosen.
    pub const SELECTED: u64 = 1 << 23;
    /// Responds to input.
    pub const SENSITIVE: u64 = 1 << 24;
    /// On screen inside its parent, rather than scrolled away.
    pub const SHOWING: u64 = 1 << 25;
    /// Would be on screen if its parent were.
    pub const VISIBLE: u64 = 1 << 30;
    /// A list whose items can be chosen more than one at a time.
    pub const MULTISELECTABLE: u64 = 1 << 18;
    /// Text with more than one line in it.
    pub const MULTILINE: u64 = 1 << 17;
    /// The states everything drawn and usable has.
    pub const ORDINARY: u64 = ENABLED | SENSITIVE | SHOWING | VISIBLE;
}

/// Where a node is, in screen pixels.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rect {
    /// Distance from the left of the screen.
    pub x: i32,
    /// Distance from the top.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// The text a node holds, for the `Text` interface.
///
/// **Characters, not bytes.** Every offset AT-SPI names is a character offset,
/// and a caret reported in bytes lands a screen reader in the middle of `é`.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Text {
    /// What it says.
    pub value: String,
    /// Where the caret is, in characters.
    pub caret: i32,
    /// The selected range, in characters.
    pub selection: Option<(i32, i32)>,
}

/// One thing on screen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Node {
    /// Its own number. Zero is the application.
    pub id: u32,
    /// What it is.
    pub role: Role,
    /// What it is called — the thing a reader says.
    pub name: String,
    /// A longer form, where the name is not enough on its own.
    pub description: String,
    /// The state bits from [`state`].
    pub states: u64,
    /// Where it is.
    pub extents: Rect,
    /// The nodes inside it, in the order they are drawn.
    pub children: Vec<u32>,
    /// Its text, where it has any.
    pub text: Option<Text>,
}

impl Node {
    /// A node with the states everything visible has.
    #[must_use]
    pub fn new(id: u32, role: Role, name: impl Into<String>) -> Self {
        Self {
            id,
            role,
            name: name.into(),
            description: String::new(),
            states: state::ORDINARY,
            extents: Rect::default(),
            children: Vec::new(),
            text: None,
        }
    }

    /// The same, with more state bits.
    #[must_use]
    pub const fn with(mut self, states: u64) -> Self {
        self.states |= states;
        self
    }

    /// The same, placed.
    #[must_use]
    pub const fn at(mut self, extents: Rect) -> Self {
        self.extents = extents;
        self
    }

    /// The interfaces this node answers to, which is a function of what it holds.
    #[must_use]
    pub fn interfaces(&self) -> Vec<&'static str> {
        let mut names = vec!["org.a11y.atspi.Accessible", "org.a11y.atspi.Component"];
        if self.id == 0 {
            names.push("org.a11y.atspi.Application");
        }
        if self.text.is_some() {
            names.push("org.a11y.atspi.Text");
        }
        names
    }
}

/// Everything on screen, and what has the keyboard.
///
/// **A flat vector and not a graph.** Every node names its children by number,
/// the parent is found by scanning, and the whole thing is rebuilt each time
/// rather than patched — a tree of a few hundred nodes costs less to rebuild
/// than to reconcile, and a rebuild cannot drift from the frame it came from.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Tree {
    /// The nodes, in no particular order. Node zero is the application.
    pub nodes: Vec<Node>,
    /// Which node has the keyboard, if any.
    pub focus: Option<u32>,
    /// Whether the window is the active one.
    pub active: bool,
}

impl Tree {
    /// The node with that number.
    #[must_use]
    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Which node holds `id` as a child, and where in its list it sits.
    #[must_use]
    pub fn parent_of(&self, id: u32) -> Option<(u32, usize)> {
        self.nodes.iter().find_map(|node| {
            node.children.iter().position(|child| *child == id).map(|at| (node.id, at))
        })
    }

    /// The deepest node covering a point, for `GetAccessibleAtPoint`.
    ///
    /// **Deepest wins**, which is what a hit test means: a row inside a list
    /// inside a frame is what the pointer is on, and answering the frame would
    /// make flat review useless.
    #[must_use]
    pub fn at_point(&self, x: i32, y: i32) -> Option<u32> {
        let mut best: Option<(u32, i64)> = None;
        for node in &self.nodes {
            let it = node.extents;
            if x < it.x || y < it.y || x >= it.x + it.width || y >= it.y + it.height {
                continue;
            }
            let area = i64::from(it.width) * i64::from(it.height);
            if best.is_none_or(|(_, smallest)| area < smallest) {
                best = Some((node.id, area));
            }
        }
        best.map(|(id, _)| id)
    }
}

/// One thing that changed between two trees, as a signal to send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// A node took the keyboard.
    Focused(u32),
    /// A node lost it.
    Unfocused(u32),
    /// The caret moved inside a node, to that character offset.
    Caret(u32, i32),
    /// What is selected inside a node changed.
    Selection(u32),
    /// A node's children are not the ones it had.
    Children(u32),
    /// The window became active, or stopped being.
    Active(bool),
}

/// What changed between two trees, as the signals that say so.
///
/// **Diffed rather than reported by each call site.** A signal emitted where a
/// thing changes is a signal somebody forgets to emit; a diff of two trees is
/// wrong only if the tree is wrong, which is one thing to get right instead of
/// forty.
#[must_use]
pub fn changes(before: &Tree, after: &Tree) -> Vec<Change> {
    let mut out = Vec::new();
    if before.active != after.active {
        out.push(Change::Active(after.active));
    }
    if before.focus != after.focus {
        if let Some(gone) = before.focus.filter(|id| after.node(*id).is_some()) {
            out.push(Change::Unfocused(gone));
        }
        if let Some(now) = after.focus {
            out.push(Change::Focused(now));
        }
    }
    for node in &after.nodes {
        let Some(was) = before.node(node.id) else { continue };
        if was.children != node.children {
            out.push(Change::Children(node.id));
        }
        let (Some(old), Some(new)) = (was.text.as_ref(), node.text.as_ref()) else { continue };
        if old.caret != new.caret || old.value != new.value {
            out.push(Change::Caret(node.id, new.caret));
        }
        if old.selection != new.selection {
            out.push(Change::Selection(node.id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Change, Node, Rect, Role, Text, Tree, changes, state};

    fn sample() -> Tree {
        let mut list = Node::new(2, Role::List, "Files").with(state::MULTISELECTABLE);
        list.children = vec![3, 4];
        list.extents = Rect { x: 0, y: 40, width: 800, height: 500 };
        let mut frame = Node::new(1, Role::Frame, "skerry");
        frame.children = vec![2];
        frame.extents = Rect { x: 0, y: 0, width: 800, height: 600 };
        let mut root = Node::new(0, Role::Application, "Skerry");
        root.children = vec![1];
        Tree {
            nodes: vec![
                root,
                frame,
                list,
                Node::new(3, Role::ListItem, "alpha").at(Rect {
                    x: 0,
                    y: 40,
                    width: 800,
                    height: 26,
                }),
                Node::new(4, Role::ListItem, "beta").at(Rect {
                    x: 0,
                    y: 66,
                    width: 800,
                    height: 26,
                }),
            ],
            focus: Some(3),
            active: true,
        }
    }

    #[test]
    fn a_parent_is_found_by_the_child_it_names() {
        let tree = sample();
        assert_eq!(tree.parent_of(3), Some((2, 0)));
        assert_eq!(tree.parent_of(4), Some((2, 1)));
        assert_eq!(tree.parent_of(1), Some((0, 0)));
        assert_eq!(tree.parent_of(0), None, "the application has no parent here");
    }

    /// The deepest node covering the point, which is what a hit test means.
    #[test]
    fn a_point_finds_the_smallest_thing_over_it() {
        let tree = sample();
        assert_eq!(tree.at_point(10, 50), Some(3));
        assert_eq!(tree.at_point(10, 70), Some(4));
        // Below the rows and inside the list, which is the list itself.
        assert_eq!(tree.at_point(10, 300), Some(2));
        assert_eq!(tree.at_point(10, 10), Some(1), "above the list is the frame");
        assert_eq!(tree.at_point(-5, 10), None);
    }

    /// A node says which interfaces it answers to, and text is what decides.
    #[test]
    fn the_interfaces_follow_what_the_node_holds() {
        let plain = Node::new(9, Role::ListItem, "x");
        assert!(!plain.interfaces().contains(&"org.a11y.atspi.Text"));
        let mut field = Node::new(9, Role::Entry, "x");
        field.text = Some(Text::default());
        assert!(field.interfaces().contains(&"org.a11y.atspi.Text"));
        let root = Node::new(0, Role::Application, "Skerry");
        assert!(root.interfaces().contains(&"org.a11y.atspi.Application"));
    }

    /// Focus moving is two signals, and the order is lost-then-gained.
    #[test]
    fn a_moved_focus_is_reported_as_a_loss_and_a_gain() {
        let before = sample();
        let mut after = sample();
        after.focus = Some(4);
        assert_eq!(changes(&before, &after), vec![Change::Unfocused(3), Change::Focused(4)]);
    }

    /// A window that stops being active says so, and nothing else changed.
    #[test]
    fn an_inactive_window_is_one_change() {
        let before = sample();
        let mut after = sample();
        after.active = false;
        assert_eq!(changes(&before, &after), vec![Change::Active(false)]);
    }

    /// A caret that moved and a selection that changed are separate signals.
    #[test]
    fn the_caret_and_the_selection_are_two_signals() {
        let mut before = sample();
        let mut field = Node::new(5, Role::Entry, "name");
        field.text = Some(Text { value: "alpha".to_owned(), caret: 0, selection: None });
        before.nodes.push(field.clone());
        let mut after = before.clone();
        let Some(node) = after.nodes.iter_mut().find(|it| it.id == 5) else { panic!() };
        node.text = Some(Text { value: "alpha".to_owned(), caret: 3, selection: Some((0, 3)) });
        assert_eq!(changes(&before, &after), vec![Change::Caret(5, 3), Change::Selection(5)]);
    }

    /// A node whose children changed says so, once.
    #[test]
    fn children_that_changed_are_one_signal_on_the_parent() {
        let before = sample();
        let mut after = sample();
        let Some(list) = after.nodes.iter_mut().find(|it| it.id == 2) else { panic!() };
        list.children = vec![3];
        assert_eq!(changes(&before, &after), vec![Change::Children(2)]);
    }

    /// Two identical trees are nothing to say.
    #[test]
    fn nothing_changed_is_no_signals() {
        assert!(changes(&sample(), &sample()).is_empty());
    }
}
