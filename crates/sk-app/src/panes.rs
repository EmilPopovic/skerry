//! The pane tree: splits, and the tabs inside each leaf.
//!
//! [ADR 0010](../../../docs/adr/0010-a-split-tree-of-tab-strips.md) decides the
//! shape. A binary split is an axis, a ratio and two children, so splitting
//! replaces a leaf, closing replaces a parent with a sibling, and a divider is
//! one `f32` between two subtrees and can never involve a third.
//!
//! Every leaf holds at least one tab and the tree holds at least one leaf, which
//! is why `close_tab` and `close_leaf` report whether there was anything left to
//! close rather than leaving a window with nothing in it.

use std::path::{Path, PathBuf};

use crate::layout::Area;
use crate::pane::{Jobs, Pane};
use sk_ui::Metrics;

/// Names one leaf for as long as it exists, and never again.
///
/// A leaf is named rather than addressed by its position in the tree, because
/// the next split renumbers every position and a command that ran a frame later
/// would arrive at a different pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PaneId(u32);

/// Names one divider, for a drag that moves it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SplitId(u32);

/// Which way a split divides its two children.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Axis {
    /// Side by side, divided by a vertical line.
    Beside,
    /// Stacked, divided by a horizontal line.
    Below,
}

/// How wide the grab band on a divider is, in logical pixels.
///
/// The same six the window's own resize border uses, and for the same reason:
/// it is what GTK and Qt both put on a splitter, so it is what a person's hands
/// already expect.
pub(crate) const DIVIDER: f32 = 6.0;

/// How close to an edge a ratio may be dragged.
///
/// A pane at zero width is a pane with no way to grab it back.
const MIN_RATIO: f32 = 0.08;

/// One leaf: a strip of tabs, one of them showing.
pub(crate) struct Tabs {
    pub(crate) id: PaneId,
    /// At least one, always.
    pub(crate) panes: Vec<Pane>,
    pub(crate) active: usize,
}

impl Tabs {
    pub(crate) fn showing(&self) -> &Pane {
        // The invariant this file exists to keep: a leaf holds at least one tab
        // and `active` indexes it.
        &self.panes[self.active.min(self.panes.len() - 1)]
    }

    pub(crate) fn showing_mut(&mut self) -> &mut Pane {
        let index = self.active.min(self.panes.len() - 1);
        &mut self.panes[index]
    }

    /// What the tab strip puts on each tab.
    pub(crate) fn titles(&self) -> impl Iterator<Item = &str> {
        self.panes.iter().map(|pane| title_of(&pane.path))
    }
}

/// The name a tab carries: the directory's own, and `/` for the root.
pub(crate) fn title_of(path: &Path) -> &str {
    path.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("/")
}

enum Node {
    Leaf(Tabs),
    Split { id: SplitId, axis: Axis, ratio: f32, first: Box<Node>, second: Box<Node> },
}

/// The tree, and which leaf has the keyboard.
pub(crate) struct Panes {
    root: Node,
    focus: PaneId,
    next: u32,
    /// Paths of closed tabs, newest last, for `tab.reopen`.
    ///
    /// Paths and not panes: a closed tab's listing is stale by the time anyone
    /// reopens it, and keeping the listing keeps its memory too.
    closed: Vec<PathBuf>,
}

/// How many closed tabs can be reopened.
///
/// Deep enough to undo a run of closes, shallow enough that it is not a history.
const REOPENABLE: usize = 16;

impl Panes {
    pub(crate) fn new(pane: Pane) -> Self {
        let id = PaneId(0);
        Self {
            root: Node::Leaf(Tabs { id, panes: vec![pane], active: 0 }),
            focus: id,
            next: 1,
            closed: Vec::new(),
        }
    }

    fn mint(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    pub(crate) fn focus(&self) -> PaneId {
        self.focus
    }

    /// Puts the keyboard on `id`, if it is still a leaf. Whether anything moved.
    pub(crate) fn focus_on(&mut self, id: PaneId) -> bool {
        if self.focus == id || !self.leaves().any(|leaf| leaf.id == id) {
            return false;
        }
        self.focus = id;
        true
    }

    pub(crate) fn focused(&self) -> &Pane {
        self.focused_leaf().showing()
    }

    pub(crate) fn focused_mut(&mut self) -> &mut Pane {
        self.focused_leaf_mut().showing_mut()
    }

    pub(crate) fn focused_leaf(&self) -> &Tabs {
        let focus = self.focus;
        // The focused id always names a live leaf: every operation that removes
        // one moves the focus first.
        self.leaves().find(|leaf| leaf.id == focus).unwrap_or_else(|| self.first_leaf())
    }

    fn first_leaf(&self) -> &Tabs {
        let mut node = &self.root;
        loop {
            match node {
                Node::Leaf(tabs) => return tabs,
                Node::Split { first, .. } => node = first,
            }
        }
    }

    pub(crate) fn focused_leaf_mut(&mut self) -> &mut Tabs {
        let focus = self.focus;
        let mut node = &mut self.root;
        loop {
            match node {
                Node::Leaf(_) => break,
                Node::Split { first, second, .. } => {
                    node = if contains(first, focus) { first } else { second };
                }
            }
        }
        match node {
            Node::Leaf(tabs) => tabs,
            // Unreachable: the loop only leaves on a leaf.
            Node::Split { .. } => unreachable!(),
        }
    }

    pub(crate) fn leaves(&self) -> impl Iterator<Item = &Tabs> {
        let mut stack = vec![&self.root];
        std::iter::from_fn(move || {
            while let Some(node) = stack.pop() {
                match node {
                    Node::Leaf(tabs) => return Some(tabs),
                    Node::Split { first, second, .. } => {
                        stack.push(second);
                        stack.push(first);
                    }
                }
            }
            None
        })
    }

    fn leaves_mut(&mut self) -> impl Iterator<Item = &mut Tabs> {
        let mut stack = vec![&mut self.root];
        std::iter::from_fn(move || {
            while let Some(node) = stack.pop() {
                match node {
                    Node::Leaf(tabs) => return Some(tabs),
                    Node::Split { first, second, .. } => {
                        stack.push(second);
                        stack.push(first);
                    }
                }
            }
            None
        })
    }

    /// How many leaves the tree holds.
    pub(crate) fn leaf_count(&self) -> usize {
        self.leaves().count()
    }

    /// The pane showing in each leaf, in layout order.
    pub(crate) fn visible(&self) -> impl Iterator<Item = &Pane> {
        self.leaves().map(Tabs::showing)
    }

    /// The pane showing in each leaf, for a change that has to be applied.
    pub(crate) fn visible_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.leaves_mut().map(Tabs::showing_mut)
    }

    /// Every pane, showing or not — a background tab has reads in flight too.
    pub(crate) fn every_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.leaves_mut().flat_map(|leaf| leaf.panes.iter_mut())
    }

    /// Whether the pane waiting for `generation` is on screen.
    ///
    /// A reply to a background tab changes nothing anybody can see, and a frame
    /// drawn for it is a frame drawn for nothing.
    pub(crate) fn is_showing(&self, generation: u64) -> bool {
        self.visible().any(|pane| pane.generation() == generation)
    }

    /// One leaf by name.
    pub(crate) fn leaf(&self, id: PaneId) -> Option<&Tabs> {
        self.leaves().find(|leaf| leaf.id == id)
    }

    /// The pane a job's reply belongs to.
    ///
    /// Read generations are process-wide, so this matches at most one pane and a
    /// reply for a pane that has been closed matches none. Background tabs are
    /// searched too: closing a directory's window does not cancel its read.
    pub(crate) fn by_generation_mut(&mut self, generation: u64) -> Option<&mut Pane> {
        self.every_mut().find(|pane| pane.generation() == generation)
    }

    /// Opens a tab in the focused leaf and shows it.
    pub(crate) fn open_tab(&mut self, path: PathBuf, jobs: &Jobs) {
        let mut pane = Pane::new(path);
        pane.reload(jobs);
        let leaf = self.focused_leaf_mut();
        leaf.panes.insert(leaf.active + 1, pane);
        leaf.active += 1;
    }

    /// Closes the showing tab. `false` when it was the last tab of the last leaf.
    pub(crate) fn close_tab(&mut self) -> bool {
        let single = self.leaf_count() == 1;
        let leaf = self.focused_leaf_mut();
        if leaf.panes.len() == 1 {
            if single {
                return false;
            }
            let path = leaf.showing().path.clone();
            self.closed.push(path);
            self.trim_closed();
            return self.close_leaf();
        }
        let index = leaf.active;
        let gone = leaf.panes.remove(index);
        leaf.active = index.min(leaf.panes.len() - 1);
        self.closed.push(gone.path);
        self.trim_closed();
        true
    }

    fn trim_closed(&mut self) {
        while self.closed.len() > REOPENABLE {
            self.closed.remove(0);
        }
    }

    /// Reopens the most recently closed tab. `false` when none were.
    pub(crate) fn reopen(&mut self, jobs: &Jobs) -> bool {
        let Some(path) = self.closed.pop() else { return false };
        self.open_tab(path, jobs);
        true
    }

    pub(crate) fn can_reopen(&self) -> bool {
        !self.closed.is_empty()
    }

    /// Moves to another tab in the focused leaf, wrapping at both ends.
    pub(crate) fn step_tab(&mut self, delta: isize) -> bool {
        let leaf = self.focused_leaf_mut();
        let count = leaf.panes.len();
        if count < 2 {
            return false;
        }
        let Ok(count) = isize::try_from(count) else { return false };
        let Ok(from) = isize::try_from(leaf.active) else { return false };
        let next = from.wrapping_add(delta).rem_euclid(count);
        let Ok(next) = usize::try_from(next) else { return false };
        leaf.active = next;
        true
    }

    /// Shows the tab at `index` of the leaf `id`. Whether anything moved.
    pub(crate) fn show_tab(&mut self, id: PaneId, index: usize) -> bool {
        self.focus_on(id);
        let leaf = self.focused_leaf_mut();
        if index >= leaf.panes.len() || leaf.active == index {
            return false;
        }
        leaf.active = index;
        true
    }

    /// Splits the focused leaf, and puts the keyboard on the new half.
    pub(crate) fn split(&mut self, axis: Axis, jobs: &Jobs) {
        let path = self.focused().path.clone();
        let mut pane = Pane::new(path);
        pane.reload(jobs);
        let leaf_id = PaneId(self.mint());
        let split_id = SplitId(self.mint());
        let focus = self.focus;
        let fresh = Node::Leaf(Tabs { id: leaf_id, panes: vec![pane], active: 0 });
        replace_leaf(&mut self.root, focus, |old| Node::Split {
            id: split_id,
            axis,
            ratio: 0.5,
            first: Box::new(old),
            second: Box::new(fresh),
        });
        self.focus = leaf_id;
    }

    /// Closes the focused leaf and every tab in it. `false` when it is the last.
    pub(crate) fn close_leaf(&mut self) -> bool {
        if self.leaf_count() == 1 {
            return false;
        }
        let focus = self.focus;
        let Some(survivor) = collapse(&mut self.root, focus) else { return false };
        self.focus = survivor;
        true
    }

    /// Puts the keyboard on the next leaf in layout order, wrapping.
    pub(crate) fn step_leaf(&mut self, delta: isize) -> bool {
        let order: Vec<PaneId> = self.leaves().map(|leaf| leaf.id).collect();
        if order.len() < 2 {
            return false;
        }
        let Some(at) = order.iter().position(|id| *id == self.focus) else { return false };
        let Ok(count) = isize::try_from(order.len()) else { return false };
        let Ok(from) = isize::try_from(at) else { return false };
        let next = from.wrapping_add(delta).rem_euclid(count);
        let Ok(next) = usize::try_from(next) else { return false };
        self.focus = order[next];
        true
    }

    /// Moves one divider. `at` is a fraction of the split's own extent.
    pub(crate) fn set_ratio(&mut self, id: SplitId, at: f32) {
        let mut stack = vec![&mut self.root];
        while let Some(node) = stack.pop() {
            if let Node::Split { id: it, ratio, first, second, .. } = node {
                if *it == id {
                    *ratio = at.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                    return;
                }
                stack.push(second);
                stack.push(first);
            }
        }
    }

    /// The rectangles the frame draws into, and the dividers between them.
    ///
    /// Both are recomputed into buffers the frame already owns rather than
    /// returned, because this runs every frame and a `Vec` per frame per pane is
    /// an allocation the budget in
    /// [09](../../../docs/09-performance-budgets.md) has no room for.
    pub(crate) fn layout(
        &self,
        content: Area,
        metrics: &Metrics,
        leaves: &mut Vec<PaneLayout>,
        dividers: &mut Vec<Divider>,
    ) {
        leaves.clear();
        dividers.clear();
        walk(&self.root, content, metrics, leaves, dividers);
    }
}

/// Where one leaf's parts are.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaneLayout {
    pub(crate) id: PaneId,
    /// The whole leaf, tab strip included.
    pub(crate) area: Area,
    /// The tab strip. Zero height when the leaf holds one tab, because a strip
    /// that names the one thing on screen is a row of pixels that says nothing.
    pub(crate) tabs: Area,
    pub(crate) header: Area,
    pub(crate) body: Area,
    pub(crate) icon: Area,
    pub(crate) name: Area,
    pub(crate) size: Area,
    pub(crate) modified: Area,
}

impl PaneId {
    /// The leaf a fresh tree starts with.
    pub(crate) const FIRST: Self = Self(0);
}

impl Default for PaneId {
    fn default() -> Self {
        Self::FIRST
    }
}

/// The widest a tab is drawn, and the narrowest before the strip clips.
///
/// A tab is a target and a label. 200 is where a directory name stops needing
/// more; 72 is where one stops being readable, and a leaf with more tabs than
/// that has `tab.next` and the palette.
const TAB_WIDTH: (f32, f32) = (72.0, 200.0);

/// Where the tree is on screen, and what the pointer is on.
///
/// One value rather than six fields on `App`, because all six are answers to
/// "where did the last layout put the panes" and all six are recomputed by the
/// same call. A hit test reads this; nothing else does.
#[derive(Default)]
pub(crate) struct Placement {
    /// Each leaf, in layout order.
    pub(crate) leaves: Vec<PaneLayout>,
    /// The dividers between them.
    pub(crate) dividers: Vec<Divider>,
    /// The leaf with the keyboard, copied out so a hit test reads a field.
    pub(crate) here: PaneLayout,
    /// The tab under the pointer: which leaf, and which of its tabs.
    pub(crate) hovered_tab: Option<(PaneId, usize)>,
    /// The divider under the pointer.
    pub(crate) hovered_divider: Option<SplitId>,
    /// The divider being dragged, while one is.
    pub(crate) dragging: Option<SplitId>,
}

/// One leaf filling `content`: what a capture and a benchmark have.
///
/// Both draw one directory into a window that never splits, and both need the
/// four values a frame is drawn from. Here rather than twice in two files,
/// because "a headless frame is one pane" is a fact about the tree.
pub(crate) struct Single {
    pub(crate) panes: Panes,
    pub(crate) leaves: Vec<PaneLayout>,
    pub(crate) dividers: Vec<Divider>,
    pub(crate) here: PaneLayout,
}

pub(crate) fn single(pane: Pane, content: Area, metrics: &Metrics) -> Single {
    let panes = Panes::new(pane);
    let (mut leaves, mut dividers) = (Vec::new(), Vec::new());
    panes.layout(content, metrics, &mut leaves, &mut dividers);
    let here = leaves.first().copied().unwrap_or_default();
    Single { panes, leaves, dividers, here }
}

/// Where each tab of a leaf sits, in order.
///
/// Shared by the frame and the hit test rather than recorded while drawing: a
/// tab's width is a function of the strip and the count, not of the shaped text,
/// so the two answers cannot drift and no rectangle has to be remembered.
pub(crate) fn tab_areas(strip: Area, count: usize) -> impl Iterator<Item = Area> {
    let each = if count == 0 {
        0.0
    } else {
        (strip.width / count as f32).clamp(TAB_WIDTH.0.min(strip.width), TAB_WIDTH.1)
    };
    (0..count).map(move |index| Area {
        x: strip.x + index as f32 * each,
        y: strip.y,
        width: each,
        height: strip.height,
    })
}

/// The close cross on one tab, or `None` where the tab is too narrow for it.
pub(crate) fn tab_close(tab: Area) -> Option<Area> {
    let side = 18.0;
    (tab.width >= TAB_WIDTH.0).then(|| Area {
        x: tab.right() - side - 6.0,
        y: tab.y + (tab.height - side) / 2.0,
        width: side,
        height: side,
    })
}

/// One divider, and the split it moves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Divider {
    pub(crate) id: SplitId,
    pub(crate) axis: Axis,
    /// The grab band, which is wider than the line that is drawn.
    pub(crate) area: Area,
}

fn walk(
    node: &Node,
    area: Area,
    metrics: &Metrics,
    leaves: &mut Vec<PaneLayout>,
    dividers: &mut Vec<Divider>,
) {
    match node {
        Node::Leaf(tabs) => leaves.push(leaf_layout(tabs, area, metrics)),
        Node::Split { id, axis, ratio, first, second } => {
            let (one, two, band) = divide(area, *axis, *ratio);
            dividers.push(Divider { id: *id, axis: *axis, area: band });
            walk(first, one, metrics, leaves, dividers);
            walk(second, two, metrics, leaves, dividers);
        }
    }
}

/// Splits `area` in two, with the divider's band between them.
fn divide(area: Area, axis: Axis, ratio: f32) -> (Area, Area, Area) {
    match axis {
        Axis::Beside => {
            let cut = (area.width * ratio).round();
            let first = Area { width: (cut - DIVIDER / 2.0).max(0.0), ..area };
            let x = area.x + cut + DIVIDER / 2.0;
            let second = Area { x, width: (area.right() - x).max(0.0), ..area };
            let band = Area { x: area.x + cut - DIVIDER / 2.0, width: DIVIDER, ..area };
            (first, second, band)
        }
        Axis::Below => {
            let cut = (area.height * ratio).round();
            let first = Area { height: (cut - DIVIDER / 2.0).max(0.0), ..area };
            let y = area.y + cut + DIVIDER / 2.0;
            let second = Area { y, height: (area.bottom() - y).max(0.0), ..area };
            let band = Area { y: area.y + cut - DIVIDER / 2.0, height: DIVIDER, ..area };
            (first, second, band)
        }
    }
}

fn leaf_layout(tabs: &Tabs, area: Area, metrics: &Metrics) -> PaneLayout {
    let strip = if tabs.panes.len() > 1 {
        Area { height: metrics.tab_height, ..area }
    } else {
        Area { height: 0.0, ..area }
    };
    let header = Area { y: strip.bottom(), height: metrics.header_height, ..area };
    let body =
        Area { y: header.bottom(), height: (area.bottom() - header.bottom()).max(0.0), ..area };
    let (icon, name, size, modified) = crate::layout::columns(body, metrics);
    PaneLayout { id: tabs.id, area, tabs: strip, header, body, icon, name, size, modified }
}

/// Whether `focus` is somewhere under `node`.
fn contains(node: &Node, focus: PaneId) -> bool {
    match node {
        Node::Leaf(tabs) => tabs.id == focus,
        Node::Split { first, second, .. } => contains(first, focus) || contains(second, focus),
    }
}

/// Replaces the leaf named by `id` with whatever `make` builds around it.
fn replace_leaf(node: &mut Node, id: PaneId, make: impl FnOnce(Node) -> Node) {
    let target = match node {
        Node::Leaf(tabs) => tabs.id == id,
        Node::Split { first, second, .. } => {
            if contains(first, id) {
                replace_leaf(first, id, make);
            } else {
                replace_leaf(second, id, make);
            }
            return;
        }
    };
    if target {
        // The leaf is moved out and the replacement moved in. A placeholder is
        // needed because `make` consumes the old node, and there is no way to
        // own a `&mut`'s contents without leaving something behind.
        let taken = std::mem::replace(node, Node::Leaf(Tabs { id, panes: Vec::new(), active: 0 }));
        *node = make(taken);
    }
}

/// Removes the leaf named by `id`, replacing its parent with its sibling.
///
/// Returns the leaf the keyboard should land on: the first one in what the
/// sibling turned out to be.
fn collapse(node: &mut Node, id: PaneId) -> Option<PaneId> {
    let Node::Split { first, second, .. } = node else { return None };
    let side = if matches!(first.as_ref(), Node::Leaf(tabs) if tabs.id == id) {
        Some(false)
    } else if matches!(second.as_ref(), Node::Leaf(tabs) if tabs.id == id) {
        Some(true)
    } else {
        None
    };
    match side {
        Some(keep_second) => {
            let (kept, _) = if keep_second {
                (std::mem::replace(second.as_mut(), placeholder()), ())
            } else {
                (std::mem::replace(first.as_mut(), placeholder()), ())
            };
            *node = kept;
            Some(first_id(node))
        }
        None => collapse(first, id).or_else(|| collapse(second, id)),
    }
}

fn placeholder() -> Node {
    Node::Leaf(Tabs { id: PaneId(u32::MAX), panes: Vec::new(), active: 0 })
}

fn first_id(node: &Node) -> PaneId {
    match node {
        Node::Leaf(tabs) => tabs.id,
        Node::Split { first, .. } => first_id(first),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Axis, Divider, PaneLayout, Panes};
    use crate::layout::Area;
    use crate::pane::{Jobs, Pane};
    use sk_util::{Pool, Waker};
    use std::path::PathBuf;
    use std::sync::mpsc;

    /// A pool nothing is ever asked of: these tests exercise the tree, and a
    /// read would put a directory's contents in the way of the shape.
    fn idle() -> (Waker, Jobs, mpsc::Receiver<crate::pane::Message>) {
        let waker = Waker::new().expect("eventfd");
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: Pool::with_width(1), wake: waker.handle(), sender };
        (waker, jobs, updates)
    }

    fn tree() -> (Panes, Waker, Jobs, mpsc::Receiver<crate::pane::Message>) {
        let (waker, jobs, updates) = idle();
        (Panes::new(Pane::new(PathBuf::from("/tmp"))), waker, jobs, updates)
    }

    #[test]
    fn a_fresh_tree_is_one_leaf_of_one_tab() {
        let (panes, _waker, _jobs, _updates) = tree();
        assert_eq!(panes.leaf_count(), 1);
        assert_eq!(panes.visible().count(), 1);
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"));
    }

    #[test]
    fn splitting_puts_the_keyboard_on_the_new_half() {
        let (mut panes, _waker, jobs, _updates) = tree();
        let before = panes.focus();
        panes.split(Axis::Beside, &jobs);
        assert_eq!(panes.leaf_count(), 2);
        assert_ne!(panes.focus(), before, "the keyboard stayed on the old half");
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"), "the new half starts here");
    }

    #[test]
    fn closing_a_leaf_leaves_its_sibling_and_the_last_one_cannot_be_closed() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        panes.split(Axis::Below, &jobs);
        assert_eq!(panes.leaf_count(), 3);
        assert!(panes.close_leaf());
        assert_eq!(panes.leaf_count(), 2);
        assert!(panes.close_leaf());
        assert_eq!(panes.leaf_count(), 1);
        assert!(!panes.close_leaf(), "the last leaf was closed");
    }

    #[test]
    fn a_closed_tab_can_be_reopened_and_the_last_one_refuses_to_close() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.open_tab(PathBuf::from("/usr"), &jobs);
        assert_eq!(panes.focused().path, PathBuf::from("/usr"));
        assert!(panes.close_tab());
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"));
        assert!(!panes.close_tab(), "the last tab of the last leaf closed");
        assert!(panes.can_reopen());
        assert!(panes.reopen(&jobs));
        assert_eq!(panes.focused().path, PathBuf::from("/usr"));
    }

    #[test]
    fn tabs_and_leaves_both_wrap_at_both_ends() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.open_tab(PathBuf::from("/usr"), &jobs);
        assert!(panes.step_tab(1));
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"), "the tabs did not wrap");
        panes.split(Axis::Beside, &jobs);
        let first = panes.focus();
        assert!(panes.step_leaf(1));
        assert!(panes.step_leaf(1));
        assert_eq!(panes.focus(), first, "the leaves did not wrap");
    }

    /// The reason generations are process-wide: two panes reach the same count
    /// within a second of each other, and a reply routed by count alone lands in
    /// whichever pane asks first.
    #[test]
    fn a_reply_reaches_exactly_the_pane_that_asked() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let generations: Vec<u64> = panes.visible().map(Pane::generation).collect();
        assert_eq!(generations.len(), 2);
        assert_ne!(generations[0], generations[1], "two panes share a generation");
        for generation in generations {
            assert!(panes.by_generation_mut(generation).is_some());
        }
        assert!(panes.by_generation_mut(u64::MAX).is_none());
    }

    fn geometry(panes: &Panes) -> (Vec<PaneLayout>, Vec<Divider>) {
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        let content = Area { x: 0.0, y: 100.0, width: 1000.0, height: 500.0 };
        panes.layout(content, &sk_ui::Metrics::default(), &mut leaves, &mut dividers);
        (leaves, dividers)
    }

    #[test]
    fn two_leaves_side_by_side_share_the_width_with_a_divider_between_them() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let (leaves, dividers) = geometry(&panes);
        assert_eq!(leaves.len(), 2);
        assert_eq!(dividers.len(), 1);
        assert!(leaves[0].area.right() <= leaves[1].area.x, "the two halves overlap");
        let band = dividers[0].area;
        assert!(band.x >= leaves[0].area.right() - 0.5 && band.right() <= leaves[1].area.x + 0.5);
        assert!((leaves[0].area.width - leaves[1].area.width).abs() < 2.0, "a half is not half");
    }

    #[test]
    fn a_single_tab_shows_no_strip_and_a_second_one_does() {
        let (mut panes, _waker, jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        assert!(leaves[0].tabs.height <= 0.0, "one tab drew a strip");
        assert!((leaves[0].header.y - leaves[0].area.y).abs() < f32::EPSILON);
        panes.open_tab(PathBuf::from("/usr"), &jobs);
        let (leaves, _) = geometry(&panes);
        assert!(leaves[0].tabs.height > 0.0, "two tabs drew no strip");
        assert!(
            (leaves[0].header.y - leaves[0].tabs.bottom()).abs() < f32::EPSILON,
            "the header is not under the strip"
        );
        assert!(leaves[0].body.bottom() <= leaves[0].area.bottom());
    }

    #[test]
    fn a_divider_cannot_be_dragged_to_an_edge() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let (_, dividers) = geometry(&panes);
        let id = dividers[0].id;
        panes.set_ratio(id, 0.0);
        let (leaves, _) = geometry(&panes);
        assert!(leaves[0].area.width > 40.0, "a pane was squeezed to nothing");
        panes.set_ratio(id, 1.0);
        let (leaves, _) = geometry(&panes);
        assert!(leaves[1].area.width > 40.0, "a pane was squeezed to nothing");
    }
}
