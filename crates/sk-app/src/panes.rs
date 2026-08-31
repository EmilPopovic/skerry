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

use crate::layout::{Area, Tool};
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

fn describe_node(node: &Node) -> crate::session::Node {
    match node {
        Node::Leaf(tabs) => crate::session::Node::Leaf {
            active: tabs.active,
            tabs: tabs
                .panes
                .iter()
                .map(|pane| crate::session::Tab {
                    path: pane.path.clone(),
                    sort: pane.sort,
                    scroll: pane.scroll.offset(),
                })
                .collect(),
        },
        Node::Split { axis, ratio, first, second, .. } => crate::session::Node::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(describe_node(first)),
            second: Box::new(describe_node(second)),
        },
    }
}

fn restore_node(described: &crate::session::Node, next: &mut u32, jobs: &Jobs) -> Node {
    match described {
        crate::session::Node::Leaf { active, tabs } => {
            let id = PaneId(*next);
            *next += 1;
            let panes: Vec<Pane> = tabs
                .iter()
                .map(|tab| {
                    let mut pane = Pane::new(tab.path.clone());
                    pane.sort = tab.sort;
                    pane.scroll_when_loaded(tab.scroll);
                    pane.reload(jobs);
                    pane
                })
                .collect();
            // The parser refuses a leaf with no tabs, and the tree's invariant is
            // that there is always one — restated here because this constructor
            // is reachable from a file rather than from the program.
            let panes = if panes.is_empty() { vec![Pane::new(PathBuf::from("/"))] } else { panes };
            let active = (*active).min(panes.len() - 1);
            Node::Leaf(Tabs { id, panes, active })
        }
        crate::session::Node::Split { axis, ratio, first, second } => {
            let id = SplitId(*next);
            *next += 1;
            Node::Split {
                id,
                axis: *axis,
                ratio: *ratio,
                first: Box::new(restore_node(first, next, jobs)),
                second: Box::new(restore_node(second, next, jobs)),
            }
        }
    }
}

/// The leftmost leaf, which is where the keyboard starts.
fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf(tabs) => tabs.id,
        Node::Split { first, .. } => first_leaf(first),
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

    /// What the tree is, for the session file.
    ///
    /// Total: every tree has a description, and the description of a tree of one
    /// leaf on one tab is what a fresh window writes.
    pub(crate) fn describe(&self) -> crate::session::Node {
        describe_node(&self.root)
    }

    /// Rebuilds a tree from a session's description and starts every read.
    ///
    /// **Every pane, not only the visible one.** A background tab whose read
    /// waits until it is switched to is a tab that takes a second to show
    /// something the first time it is clicked, which is the thing this program
    /// exists not to do — and the reads are on the pool, in parallel, so the cost
    /// is the pool's rather than the window's.
    pub(crate) fn restore(described: &crate::session::Node, jobs: &Jobs) -> Self {
        let mut next = 0;
        let root = restore_node(described, &mut next, jobs);
        let focus = first_leaf(&root);
        Self { root, focus, next, closed: Vec::new() }
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
        let at = self.focused_leaf().active + 1;
        self.insert_tab(path, at, jobs);
        self.focused_leaf_mut().active = at;
    }

    /// Opens tabs beside the showing one, in the order given, and stays on it.
    ///
    /// Staying is what every browser and both of the desktop's file managers do
    /// with "open in a new tab": the command exists to put a directory somewhere
    /// *without* going there, and one that took the view would be `Enter` with a
    /// tab left behind. The strip is always drawn, so the new tabs are visible
    /// where a hidden strip would have made this look like nothing happened.
    ///
    /// The insertion point walks. Three of them at a fixed `active + 1` arrive
    /// in the strip backwards, which is what the test that named the order
    /// caught and an assertion about the count would not have.
    pub(crate) fn open_tabs_behind(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        jobs: &Jobs,
    ) {
        let first = self.focused_leaf().active + 1;
        for (offset, path) in paths.into_iter().enumerate() {
            self.insert_tab(path, first + offset, jobs);
        }
    }

    /// Puts a tab at `at` in the focused leaf and starts its read.
    fn insert_tab(&mut self, path: PathBuf, at: usize, jobs: &Jobs) {
        let mut pane = Pane::new(path);
        pane.reload(jobs);
        let leaf = self.focused_leaf_mut();
        let at = at.min(leaf.panes.len());
        leaf.panes.insert(at, pane);
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
        self.reveal_tab(id, index)
    }

    /// The same, without moving the keyboard.
    ///
    /// A press on a tab is also a press in that pane, so [`Self::show_tab`]
    /// focuses it. A drag resting on one is not: the hand is carrying files, and
    /// taking the keyboard to wherever the pointer happened to pause is a second
    /// thing happening that nobody asked for.
    pub(crate) fn reveal_tab(&mut self, id: PaneId, index: usize) -> bool {
        let Some(leaf) = self.leaf_mut(id) else { return false };
        if index >= leaf.panes.len() || leaf.active == index {
            return false;
        }
        leaf.active = index;
        true
    }

    /// Takes one tab out of a leaf, closing the leaf if it was the last.
    ///
    /// `None` when it was the last tab of the last leaf: a window with no pane
    /// in it is not a window, and a drag that emptied the tree would have
    /// nowhere to put what it was carrying.
    pub(crate) fn take_tab(&mut self, id: PaneId, index: usize) -> Option<Pane> {
        let single = self.leaf_count() == 1;
        let leaf = self.leaf_mut(id)?;
        if index >= leaf.panes.len() {
            return None;
        }
        if leaf.panes.len() == 1 {
            if single {
                return None;
            }
            let taken = leaf.panes.remove(index);
            // The leaf is empty now, so it goes — with the focus moved off it
            // first, because `close_leaf` closes the focused one.
            self.focus = id;
            self.close_leaf();
            return Some(taken);
        }
        let taken = leaf.panes.remove(index);
        leaf.active = leaf.active.min(leaf.panes.len() - 1);
        Some(taken)
    }

    /// Puts a tab into a leaf's strip at `index`, and shows it.
    pub(crate) fn put_tab(&mut self, id: PaneId, index: usize, pane: Pane) {
        let Some(leaf) = self.leaf_mut(id) else { return };
        let at = index.min(leaf.panes.len());
        leaf.panes.insert(at, pane);
        leaf.active = at;
        self.focus = id;
    }

    /// Splits `id` and puts `pane` in the new half, before or after it.
    ///
    /// `before` is what makes a drop on the *left* of a pane land on the left:
    /// the new leaf becomes the split's first child rather than its second.
    pub(crate) fn split_with(&mut self, id: PaneId, axis: Axis, before: bool, pane: Pane) {
        let leaf_id = PaneId(self.mint());
        let split_id = SplitId(self.mint());
        let fresh = Node::Leaf(Tabs { id: leaf_id, panes: vec![pane], active: 0 });
        replace_leaf(&mut self.root, id, move |old| {
            let (first, second) = if before {
                (Box::new(fresh), Box::new(old))
            } else {
                (Box::new(old), Box::new(fresh))
            };
            Node::Split { id: split_id, axis, ratio: 0.5, first, second }
        });
        self.focus = leaf_id;
    }

    pub(crate) fn leaf_mut(&mut self, id: PaneId) -> Option<&mut Tabs> {
        self.leaves_mut().find(|leaf| leaf.id == id)
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

    /// Splits the focused leaf, puts `path` in the new half, and stays put.
    ///
    /// **The keyboard does not follow**, for the reason it does not follow a tab
    /// opened behind: the command exists to put a directory *beside* what is
    /// being done, and the new pane is on screen either way. [`Panes::split`] —
    /// the `F3` one — does move, because there the new half shows the same
    /// directory and staying would look like nothing had happened.
    ///
    /// `split_with` focuses what it makes, which is right for a tab dragged out
    /// of a strip and wrong here, so the focus is put back.
    pub(crate) fn open_split(&mut self, path: PathBuf, axis: Axis, jobs: &Jobs) {
        let focus = self.focus;
        let mut pane = Pane::new(path);
        pane.reload(jobs);
        self.split_with(focus, axis, false, pane);
        self.focus = focus;
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
    /// The tab strip, above everything else in the leaf.
    pub(crate) tabs: Area,
    /// This pane's own path bar: the four navigation buttons and the
    /// breadcrumbs, or the field the path is typed into.
    ///
    /// **Per leaf, not per window.** A pane has a path, a history and a folder
    /// it can be reloaded from; a window has as many of each as it has panes,
    /// and one bar describing whichever pane last took the keyboard makes the
    /// other half of a split a listing with no address. Below the tabs, because
    /// the tabs choose which path the bar is describing.
    pub(crate) bar: Area,
    /// Back, forward, parent and reload, at the left of `bar`.
    ///
    /// All four empty where the bar is too narrow to hold them and a path both,
    /// which is the rule the metadata columns follow: a bar with buttons and no
    /// path is worse than either alone.
    pub(crate) tools: [Area; 4],
    /// The breadcrumbs, which take what the buttons leave.
    pub(crate) path: Area,
    pub(crate) header: Area,
    pub(crate) body: Area,
    pub(crate) icon: Area,
    pub(crate) name: Area,
    pub(crate) size: Area,
    pub(crate) modified: Area,
}

impl PaneLayout {
    /// The navigation button at a point, if any.
    ///
    /// The rectangles the frame drew from, so the dimming, the cursor shape and
    /// the click cannot disagree about where a button is.
    pub(crate) fn tool_at(&self, x: f32, y: f32) -> Option<Tool> {
        Tool::ALL
            .into_iter()
            .zip(self.tools)
            .find_map(|(tool, area)| (area.width > 0.0 && area.contains(x, y)).then_some(tool))
    }
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
    /// Where a tab being dragged would land, while one is being dragged. The
    /// frame shades it, and the release performs exactly it.
    pub(crate) tab_drop: Option<Drop>,
    /// Whether the pointer is over the `+` of the leaf it is in.
    pub(crate) hovered_plus: bool,
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
    let room = (strip.width - PLUS).max(0.0);
    let each = if count == 0 {
        0.0
    } else {
        (room / count as f32).clamp(TAB_WIDTH.0.min(room), TAB_WIDTH.1)
    };
    (0..count).map(move |index| Area {
        x: strip.x + index as f32 * each,
        y: strip.y,
        width: each,
        height: strip.height,
    })
}

/// Where a tab being dragged would land.
///
/// The five zones every editor with drag-to-split uses: the four edges make a
/// split and the middle joins the strip. A drop is described rather than done,
/// so the same value draws the overlay and performs the move — two answers to
/// "where would this go" is how the highlight and the result disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Drop {
    /// Into this leaf's strip, at this index.
    Strip(PaneId, usize),
    /// Splitting this leaf, with the tab in the new half.
    Split(PaneId, Axis, bool),
}

/// How far into a pane an edge zone reaches.
///
/// A quarter: wide enough to aim at without care, narrow enough that the middle
/// half of a pane is unambiguously "join this one".
const EDGE: f32 = 0.25;

impl Drop {
    /// What dropping at `at` over `leaf` would do.
    pub(crate) fn at(leaf: &PaneLayout, tabs: usize, at: sk_plat::Point) -> Self {
        let point = (at.x, at.y);
        if leaf.tabs.contains(point.0, point.1) {
            let index = tab_areas(leaf.tabs, tabs)
                .position(|tab| point.0 < tab.x + tab.width / 2.0)
                .unwrap_or(tabs);
            return Self::Strip(leaf.id, index);
        }
        let across = (point.0 - leaf.area.x) / leaf.area.width.max(1.0);
        let down = (point.1 - leaf.area.y) / leaf.area.height.max(1.0);
        // The nearest edge wins, and the middle is a join. Compared as fractions
        // so a tall narrow pane and a short wide one behave the same.
        let nearest = [
            (across, Self::Split(leaf.id, Axis::Beside, true)),
            (1.0 - across, Self::Split(leaf.id, Axis::Beside, false)),
            (down, Self::Split(leaf.id, Axis::Below, true)),
            (1.0 - down, Self::Split(leaf.id, Axis::Below, false)),
        ]
        .into_iter()
        .min_by(|left, right| left.0.total_cmp(&right.0));
        match nearest {
            Some((distance, split)) if distance < EDGE => split,
            _ => Self::Strip(leaf.id, tabs),
        }
    }

    /// Which leaf the drop is on.
    pub(crate) fn leaf(self) -> PaneId {
        match self {
            Self::Strip(id, _) | Self::Split(id, _, _) => id,
        }
    }

    /// The rectangle to shade while the drag is in flight.
    pub(crate) fn overlay(self, leaf: &PaneLayout, tabs: usize) -> crate::layout::Area {
        match self {
            Self::Strip(_, index) => {
                // A bar at the seam the tab would land in, rather than the whole
                // strip: "between these two" is the question a person is asking.
                let width = 3.0;
                let x = tab_areas(leaf.tabs, tabs)
                    .nth(index)
                    .map_or_else(|| tab_plus(leaf.tabs, tabs).x, |tab| tab.x);
                crate::layout::Area { x: x - width / 2.0, width, ..leaf.tabs }
            }
            Self::Split(_, Axis::Beside, before) => {
                let half = leaf.area.width / 2.0;
                let x = if before { leaf.area.x } else { leaf.area.x + half };
                crate::layout::Area { x, width: half, ..leaf.area }
            }
            Self::Split(_, Axis::Below, before) => {
                let half = leaf.area.height / 2.0;
                let y = if before { leaf.area.y } else { leaf.area.y + half };
                crate::layout::Area { y, height: half, ..leaf.area }
            }
        }
    }
}

/// How much of the strip the `+` takes, at its right end.
const PLUS: f32 = 30.0;

/// The button that opens a tab, at the end of the tabs rather than the end of
/// the strip.
///
/// Beside the last tab is where every browser puts it and where the eye goes
/// after reading the tabs; pinned to the far right it would be a button whose
/// distance from the tabs grows with the window.
pub(crate) fn tab_plus(strip: Area, count: usize) -> Area {
    let after = tab_areas(strip, count).last().map_or(strip.x, Area::right);
    Area {
        x: after.min(strip.right() - PLUS),
        y: strip.y,
        width: PLUS.min(strip.width),
        height: strip.height,
    }
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
    // **Always, even for one tab.** A strip that appears with the second tab is
    // a strip whose arrival shifts the list under the pointer, and it hides the
    // one control that opens a tab at all until somebody has already opened one.
    let strip = Area { height: metrics.tab_height.min(area.height), ..area };
    let bar = Area {
        y: strip.bottom(),
        height: metrics.path_height.min((area.bottom() - strip.bottom()).max(0.0)),
        ..area
    };
    // The buttons come off the left of the bar and the breadcrumbs start after
    // them. Shed together below the width where the path would have nothing
    // left, the same way the metadata columns shed.
    let tools_width = metrics.tool_width * 4.0;
    let tools = std::array::from_fn(|index| {
        if bar.width - tools_width < 120.0 {
            return Area::default();
        }
        Area {
            x: bar.x + metrics.tool_width * index as f32,
            y: bar.y,
            width: metrics.tool_width,
            height: bar.height,
        }
    });
    let toolbar = tools.iter().map(|tool| tool.width).sum::<f32>();
    let path = Area { x: bar.x + toolbar, width: (bar.width - toolbar).max(0.0), ..bar };
    let header = Area { y: bar.bottom(), height: metrics.header_height, ..area };
    let body =
        Area { y: header.bottom(), height: (area.bottom() - header.bottom()).max(0.0), ..area };
    let (icon, name, size, modified) = crate::layout::columns(body, metrics);
    PaneLayout {
        id: tabs.id,
        area,
        tabs: strip,
        bar,
        tools,
        path,
        header,
        body,
        icon,
        name,
        size,
        modified,
    }
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
    // **The one that is *not* the target is the one that survives**, which is
    // the whole of this function and was written backwards: it kept the leaf it
    // was asked to remove, and `Ctrl+F3` closed the pane beside the focused one.
    // A test that counted leaves could not tell — closing either leaves two.
    let keep_second = if matches!(first.as_ref(), Node::Leaf(tabs) if tabs.id == id) {
        Some(true)
    } else if matches!(second.as_ref(), Node::Leaf(tabs) if tabs.id == id) {
        Some(false)
    } else {
        None
    };
    match keep_second {
        Some(keep_second) => {
            let kept = if keep_second {
                std::mem::replace(second.as_mut(), placeholder())
            } else {
                std::mem::replace(first.as_mut(), placeholder())
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
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    /// A pool nothing is ever asked of: these tests exercise the tree, and a
    /// read would put a directory's contents in the way of the shape.
    fn idle() -> (Waker, Jobs, mpsc::Receiver<crate::pane::Message>) {
        let waker = Waker::new().expect("eventfd");
        let (sender, updates) = mpsc::channel();
        let jobs =
            Jobs { pool: std::sync::Arc::new(Pool::with_width(1)), wake: waker.handle(), sender };
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
        // **Which one closes, not just how many are left.** Counting alone
        // passes whichever leaf goes, and this closed the sibling of the
        // focused one for two commits.
        let going = panes.focus();
        assert!(panes.close_leaf());
        assert_eq!(panes.leaf_count(), 2);
        assert!(panes.leaves().all(|leaf| leaf.id != going), "the wrong leaf closed");
        let going = panes.focus();
        assert!(panes.close_leaf());
        assert_eq!(panes.leaf_count(), 1);
        assert!(panes.leaves().all(|leaf| leaf.id != going), "the wrong leaf closed");
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

    /// The whole point of "open in a new tab": three folders queued up and the
    /// view still on the folder they were queued from.
    #[test]
    fn tabs_opened_behind_stack_up_in_order_and_leave_the_view_alone() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.open_tabs_behind(["/usr", "/etc", "/var"].map(PathBuf::from), &jobs);
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"), "a tab took the view");
        let strip: Vec<&Path> =
            panes.focused_leaf().panes.iter().map(|pane| pane.path.as_path()).collect();
        assert_eq!(
            strip,
            [Path::new("/tmp"), Path::new("/usr"), Path::new("/etc"), Path::new("/var")]
        );
    }

    /// The other half of the same promise: a pane beside this one, and the
    /// keyboard still in the pane it was asked from.
    #[test]
    fn a_folder_opened_into_a_pane_lands_beside_the_one_that_asked() {
        let (mut panes, _waker, jobs, _updates) = tree();
        let asked_from = panes.focus();
        panes.open_split(PathBuf::from("/usr"), Axis::Beside, &jobs);
        assert_eq!(panes.leaf_count(), 2);
        assert_eq!(panes.focus(), asked_from, "the keyboard followed the new pane");
        assert_eq!(panes.focused().path, PathBuf::from("/tmp"));
        let shown: Vec<&Path> = panes.visible().map(|pane| pane.path.as_path()).collect();
        assert_eq!(shown, [Path::new("/tmp"), Path::new("/usr")], "the new pane is not second");
    }

    /// What a drag resting on a tab does, and the half of it that is easy to
    /// lose: the strip moves and the keyboard does not.
    ///
    /// A drop that took the keyboard to whichever tab the pointer paused over
    /// would leave the hand carrying files in one pane and every chord aimed at
    /// another, which is a second thing happening that nobody asked for.
    #[test]
    fn a_revealed_tab_shows_without_taking_the_keyboard() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let here = panes.focus();
        let there = panes.leaves().map(|leaf| leaf.id).find(|id| *id != here).unwrap();
        panes.leaf_mut(there).unwrap().panes.push(Pane::new(PathBuf::from("/usr")));

        assert!(panes.reveal_tab(there, 1));
        assert_eq!(panes.focus(), here, "revealing a tab took the keyboard with it");
        assert_eq!(panes.leaf(there).unwrap().showing().path, PathBuf::from("/usr"));
        assert!(!panes.reveal_tab(there, 1), "revealing the tab already showing moved something");
        assert!(!panes.reveal_tab(there, 9), "a tab that does not exist was revealed");

        // And the press it is the quiet half of does take the keyboard, which is
        // the difference the two exist to draw.
        assert!(panes.show_tab(there, 0));
        assert_eq!(panes.focus(), there);
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

    /// The strip is there for one tab as well, so opening a second does not
    /// shift the list under the pointer — and so the `+` exists before anybody
    /// has opened a tab.
    #[test]
    fn one_tab_still_has_a_strip_and_a_plus_beside_it() {
        let (mut panes, _waker, jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        let strip = leaves[0].tabs;
        assert!(strip.height > 0.0, "one tab drew no strip");
        assert!(
            (leaves[0].bar.y - strip.bottom()).abs() < f32::EPSILON,
            "the pane's own path bar is not under the strip"
        );
        assert!(leaves[0].body.bottom() <= leaves[0].area.bottom());

        // The `+` follows the last tab rather than the window's edge, and never
        // overlaps it.
        let one = super::tab_areas(strip, 1).next().expect("a tab");
        let plus = super::tab_plus(strip, 1);
        assert!(plus.x >= one.right() - 0.5, "the plus sits on the tab");
        assert!(plus.right() <= strip.right() + 0.5, "the plus is off the strip");

        panes.open_tab(PathBuf::from("/usr"), &jobs);
        let (after, _) = geometry(&panes);
        assert!(
            (after[0].body.y - leaves[0].body.y).abs() < f32::EPSILON,
            "a second tab moved the list"
        );
        let two = super::tab_areas(after[0].tabs, 2).last().expect("a tab");
        assert!(super::tab_plus(after[0].tabs, 2).x >= two.right() - 0.5);
    }

    /// Enough tabs to fill the strip: the `+` stays on it rather than being
    /// pushed off the right edge by the tabs it belongs to.
    #[test]
    fn the_plus_stays_on_the_strip_however_many_tabs_there_are() {
        let (mut panes, _waker, jobs, _updates) = tree();
        for index in 0..30 {
            panes.open_tab(PathBuf::from(format!("/tmp/{index}")), &jobs);
        }
        let (leaves, _) = geometry(&panes);
        let strip = leaves[0].tabs;
        let plus = super::tab_plus(strip, 31);
        assert!(plus.right() <= strip.right() + 0.5, "the plus ran off the strip");
        assert!(plus.width > 0.0);
    }

    /// The five zones a tab can be dropped in, from the middle out.
    #[test]
    fn the_edges_of_a_pane_split_it_and_the_middle_joins_it() {
        use super::{Axis, Drop};
        let (panes, _waker, _jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        let leaf = leaves[0];
        let at = |across: f32, down: f32| {
            sk_plat::Point::new(
                leaf.area.x + leaf.area.width * across,
                leaf.area.y + leaf.area.height * down,
            )
        };
        let id = leaf.id;
        assert_eq!(Drop::at(&leaf, 1, at(0.05, 0.5)), Drop::Split(id, Axis::Beside, true));
        assert_eq!(Drop::at(&leaf, 1, at(0.95, 0.5)), Drop::Split(id, Axis::Beside, false));
        assert_eq!(Drop::at(&leaf, 1, at(0.5, 0.95)), Drop::Split(id, Axis::Below, false));
        // The middle is a join, not a split, however far down it is.
        assert_eq!(Drop::at(&leaf, 1, at(0.5, 0.5)), Drop::Strip(id, 1));
        // A corner belongs to whichever edge is nearer, and never to both.
        assert_eq!(Drop::at(&leaf, 1, at(0.02, 0.10)), Drop::Split(id, Axis::Beside, true));
    }

    /// A drop on the strip goes *between* two tabs, by which half of a tab the
    /// pointer is on — not onto the tab it happens to be over, which would make
    /// the last position unreachable.
    #[test]
    fn a_drop_on_the_strip_lands_between_two_tabs() {
        use super::{Drop, tab_areas};
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.open_tab(PathBuf::from("/usr"), &jobs);
        panes.open_tab(PathBuf::from("/etc"), &jobs);
        let (leaves, _) = geometry(&panes);
        let leaf = leaves[0];
        let tabs: Vec<_> = tab_areas(leaf.tabs, 3).collect();
        let on = |area: Area, part: f32| {
            sk_plat::Point::new(area.x + area.width * part, area.y + area.height / 2.0)
        };
        assert_eq!(Drop::at(&leaf, 3, on(tabs[0], 0.2)), Drop::Strip(leaf.id, 0));
        assert_eq!(Drop::at(&leaf, 3, on(tabs[0], 0.8)), Drop::Strip(leaf.id, 1));
        assert_eq!(Drop::at(&leaf, 3, on(tabs[2], 0.8)), Drop::Strip(leaf.id, 3), "the end");
    }

    /// The overlay is what the drop *would* do, so the two cannot disagree.
    #[test]
    fn the_overlay_covers_the_half_the_split_would_take() {
        use super::{Axis, Drop};
        let (panes, _waker, _jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        let leaf = leaves[0];
        let left = Drop::Split(leaf.id, Axis::Beside, true).overlay(&leaf, 1);
        assert!((left.x - leaf.area.x).abs() < f32::EPSILON);
        assert!((left.width - leaf.area.width / 2.0).abs() < 0.5);
        let below = Drop::Split(leaf.id, Axis::Below, false).overlay(&leaf, 1);
        assert!((below.bottom() - leaf.area.bottom()).abs() < 0.5);
        assert!(below.y > leaf.area.y, "the lower half started at the top");
        // A strip drop is a seam, not a block: it must not cover the tabs.
        let seam = Drop::Strip(leaf.id, 0).overlay(&leaf, 1);
        assert!(seam.width < 8.0, "the seam is {} wide", seam.width);
    }

    /// Moving the only tab of a leaf takes the leaf with it, and the last tab of
    /// the last leaf cannot be taken at all — a window with no pane is not one.
    #[test]
    fn taking_the_last_tab_of_a_leaf_closes_the_leaf() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let second = panes.focus();
        assert_eq!(panes.leaf_count(), 2);
        let taken = panes.take_tab(second, 0).expect("a tab");
        assert_eq!(panes.leaf_count(), 1, "the emptied leaf stayed");
        let first = panes.focus();
        assert_ne!(first, second, "the emptied leaf is the one that survived");
        panes.put_tab(first, 0, taken);
        assert_eq!(panes.focused_leaf().panes.len(), 2);

        // And the very last one refuses.
        let only = panes.focus();
        assert!(panes.take_tab(only, 0).is_some());
        assert!(panes.take_tab(only, 0).is_none(), "the last tab was taken");
    }

    /// The four navigation buttons come off the left of the pane's own bar, the
    /// breadcrumbs start after them, and both shed at the same width — a bar
    /// with buttons and no path is worse than either alone.
    #[test]
    fn the_navigation_buttons_take_the_left_of_a_pane_bar_and_shed_together() {
        let (panes, _waker, _jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        let wide = leaves[0];
        assert!(wide.tools.iter().all(|tool| tool.width > 0.0));
        assert!(
            (wide.tools[0].x - wide.bar.x).abs() < f32::EPSILON,
            "they start at the bar's edge"
        );
        for pair in wide.tools.windows(2) {
            assert!((pair[0].right() - pair[1].x).abs() < f32::EPSILON, "a gap in the row");
        }
        assert!(
            (wide.tools[3].right() - wide.path.x).abs() < f32::EPSILON,
            "the path does not start where the buttons end"
        );
        assert!((wide.path.right() - wide.bar.right()).abs() < f32::EPSILON);

        // Narrow enough that the path would have nothing left. A pane gets this
        // narrow long before a window does, which is the reason the rule moved
        // down here with the buttons.
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        let narrow = Area { x: 0.0, y: 0.0, width: 200.0, height: 500.0 };
        panes.layout(narrow, &sk_ui::Metrics::default(), &mut leaves, &mut dividers);
        assert!(leaves[0].tools.iter().all(|tool| tool.width <= 0.0));
        assert!((leaves[0].path.width - 200.0).abs() < f32::EPSILON);
    }

    /// Each button is found by the rectangle it is drawn from — rule 2, so the
    /// dimming and the click agree about where it is.
    #[test]
    fn a_press_on_a_navigation_button_finds_that_button() {
        let (panes, _waker, _jobs, _updates) = tree();
        let (leaves, _) = geometry(&panes);
        let leaf = leaves[0];
        for (tool, area) in crate::layout::Tool::ALL.into_iter().zip(leaf.tools) {
            let (x, y) = (area.x + area.width / 2.0, area.y + area.height / 2.0);
            assert_eq!(leaf.tool_at(x, y), Some(tool));
        }
        let middle = leaf.path.x + leaf.path.width / 2.0;
        assert_eq!(leaf.tool_at(middle, leaf.bar.y + 4.0), None, "a breadcrumb is not a button");
    }

    /// Two panes, two bars: each half of a split says where *it* is, which is
    /// the whole reason the bar moved into the leaf.
    #[test]
    fn every_leaf_gets_a_bar_of_its_own_under_its_own_tabs() {
        let (mut panes, _waker, jobs, _updates) = tree();
        panes.split(Axis::Beside, &jobs);
        let (leaves, _) = geometry(&panes);
        assert_eq!(leaves.len(), 2);
        for leaf in &leaves {
            assert!(leaf.bar.height > 0.0, "a pane drew no path bar");
            assert!(
                (leaf.tabs.bottom() - leaf.bar.y).abs() < f32::EPSILON,
                "the tabs are not above"
            );
            assert!((leaf.bar.bottom() - leaf.header.y).abs() < f32::EPSILON);
            assert!(leaf.bar.width <= leaf.area.width + f32::EPSILON);
        }
        assert!(leaves[0].bar.right() <= leaves[1].bar.x, "the two bars overlap");
        // Side by side, so the second bar costs no height at all.
        assert!((leaves[0].body.y - leaves[1].body.y).abs() < f32::EPSILON);
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
