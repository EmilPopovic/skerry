// SPDX-License-Identifier: Apache-2.0
//! The context menu: the pointer's way into the command registry.
//!
//! **Rule 2 from the other end.** The keyboard reaches every command through the
//! registry's chords; this is how a pointer reaches the ones that are not a
//! button — and it reaches them through the same registry, so a menu row cannot
//! run something the palette will not find or describe a key that is not bound.
//!
//! **A row can open a second menu.** Where a row is a question with several
//! answers — which colour, which view, which application — the answers sit in a
//! submenu beside the parent rather than as seven rows of it, so the background
//! menu is twenty-two rows instead of forty-three and a folder's menu ends in
//! one row called Colour instead of seven swatches. A submenu is another table
//! of command identifiers, named in the parent's table by an identifier of its
//! own under `menu.`, which is a prefix no command has. The one exception is
//! Open with, whose rows are *data* — whatever the `.desktop` files on this
//! machine say opens the file — and arrive from a job started when the row
//! opens: [`Kind::OpenWith`].
//!
//! The tables below are the contract
//! `every_command_can_be_reached_without_a_keyboard` checks: a command that is
//! neither a button, nor a column header, nor on one of these menus or the
//! submenus they open, nor in the exemption table with a reason, fails the build.

use std::path::PathBuf;

use sk_cmd::Registry;
use sk_desktop::DesktopEntry;
use sk_model::i18n;
use sk_plat::{Point, Size};

use crate::layout::Area;
use crate::openers::Handlers;

/// What a secondary click on a row offers.
///
/// Grouped, and the groups are the separators: the ways of seeing the file
/// first — Open because it is what a double click already does, and the quick
/// look because it is the one that does not leave — then the two clipboard
/// entries together, and the two that change the file last, where a slip costs
/// the most and the pointer is least likely to be.
pub const ON_ROW: &[&[&str]] = &[
    &["go.open", "menu.open_in", "menu.open_with", "view.quick_look"],
    &["file.copy", "file.cut", "file.paste"],
    &["file.copy_path", "file.copy_content"],
    &["file.extract"],
    &["file.duplicate", "file.rename", "file.trash", "file.delete"],
    // **Pin, on a row, is about the row.** A folder somebody has found is the
    // commonest thing they want on the list of places, and reaching it used to
    // mean entering the folder first — the command's target is the entry the menu
    // opened over, so this row and the background's mean two different folders
    // and both read as "this folder".
    &["place.pin", "file.properties"],
    // **The colours, last and behind one row.** Seven swatches at the foot of
    // every file's menu were seven rows dimmed on anything that is not a folder;
    // one row, dimmed for the same reason, opens onto them — and each still draws
    // its own swatch there, because a list of colour names with no colours on it
    // is a menu a person has to try rather than read.
    &["menu.colour"],
];

/// Recoverable entries offer restore rather than mutations of their stored names.
pub const IN_TRASH: &[&[&str]] =
    &[&["file.restore"], &["view.quick_look", "file.properties", "file.copy", "file.copy_path"]];

/// What a secondary click on a row of the Recent view offers.
///
/// **Its own table, as the trash has one.** Forget is the row that belongs to
/// this view alone, and on every other row's menu it would be a dimmed row that
/// menu has no room for — it is at its height at 900×520.
pub const IN_RECENT: &[&[&str]] = &[
    &["go.open", "menu.open_with", "view.quick_look"],
    &["recent.forget"],
    &["file.copy", "file.copy_path"],
    &["file.properties"],
];

/// What a secondary click inside a text preview offers.
///
/// **Two rows, and they are the pointer's way to the look's own scope.** `Ctrl+A`
/// and `Ctrl+C` mean the text in the panel rather than the files behind it —
/// [`sk_cmd::Scope::Look`] — and [rule
/// 5](../../../docs/03-principles.md) wants both of them reachable without a
/// keyboard. A menu is what every text view offers for exactly this, and it costs
/// no new furniture: the rows come out of the same registry the chords do.
pub const IN_A_LOOK: &[&[&str]] = &[
    &["look.select_all", "look.copy"],
    &["look.edit", "look.save", "look.discard"],
    &["look.play", "look.find", "look.source"],
];

/// What a secondary click on a sidebar row offers.
///
/// **Three rows, and all three are about that row.** The background menu's
/// entries are about the directory the list is showing, so offering them over a
/// place would make New folder mean somewhere else — and the place's own two
/// verbs have nowhere else to live. Which row it is comes from
/// [`crate::app::App`]'s own target rather than from a command argument, because
/// a command runs against `&mut App` and takes none:
/// [ADR 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md).
///
/// Pin is deliberately absent: every row of the sidebar is already a place, so
/// the row would be dimmed every time the menu opened.
pub const ON_A_PLACE: &[&[&str]] = &[
    &["place.expand"],
    &["place.unpin", "place.raise", "place.lower"],
    // **The three a disk has** — [ADR
    // 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).
    // Dimmed over a row they mean nothing for, which is every place that is not
    // a volume, rather than absent: a menu that changes shape by row is a menu
    // whose rows move under a hand.
    &["volume.mount", "volume.unmount", "volume.eject"],
];

/// What a secondary click on anything else offers.
///
/// **The verbs stay on the menu and the questions go behind a row.** Undo, New
/// folder and Find are one press each because each is one thing; which view,
/// which sort, which grouping and which tab or split are each a set of answers,
/// and a set of answers is a submenu. Forty-three rows was taller than a 900×520
/// window and scrolled; twenty-two still does, at 125%, so the frame of a
/// scrolled menu is still this one.
pub const ON_BACKGROUND: &[&[&str]] = &[
    &["file.new_folder", "file.paste"],
    &["edit.undo", "edit.redo", "menu.operation"],
    &["select.all", "view.filter", "go.search", "go.search_everywhere", "menu.index"],
    &["menu.view", "menu.sort", "menu.group"],
    &["menu.panes"],
    &[
        "place.pin",
        "file.terminal",
        "view.path_entry",
        "go.recent",
        "file.paste_location",
        "go.reload",
    ],
    &["view.palette", "help.keys", "settings.open"],
];

/// A row that opens a second table beside the first.
pub struct Submenu {
    /// What a parent table names it by. Under `menu.`, which no command is
    /// under, so a table cannot mistake one for the other — a test says so.
    pub id: &'static str,
    /// The catalogue key for its name: the question its rows answer.
    pub name: &'static str,
    /// Its rows, grouped as the tables above are.
    pub groups: &'static [&'static [&'static str]],
    /// The rows whose name is shorter here than in the palette, as `(command,
    /// catalogue key)`: under a heading that has already asked the question,
    /// "Red" and not "Colour it red". A row not listed keeps its command's name.
    pub shorts: &'static [(&'static str, &'static str)],
}

/// Where else to open the entry: a tab, or either half of a split.
pub const OPEN_IN: Submenu = Submenu {
    id: "menu.open_in",
    name: "menu.open_in",
    groups: &[&["go.open_new_tab", "go.open_beside", "go.open_below"]],
    shorts: &[
        ("go.open_new_tab", "menu.open_in.new_tab"),
        ("go.open_beside", "menu.open_in.beside"),
        ("go.open_below", "menu.open_in.below"),
    ],
};

/// The six colours in spectrum order, and the one that takes the colour away
/// behind a divider, because it is the opposite of the rest.
pub const COLOUR: Submenu = Submenu {
    id: "menu.colour",
    name: "menu.colour",
    groups: &[
        &[
            "view.colour_red",
            "view.colour_orange",
            "view.colour_yellow",
            "view.colour_green",
            "view.colour_blue",
            "view.colour_purple",
        ],
        &["view.colour_none"],
    ],
    shorts: &[
        ("view.colour_red", "menu.colour.red"),
        ("view.colour_orange", "menu.colour.orange"),
        ("view.colour_yellow", "menu.colour.yellow"),
        ("view.colour_green", "menu.colour.green"),
        ("view.colour_blue", "menu.colour.blue"),
        ("view.colour_purple", "menu.colour.purple"),
    ],
};

/// The three geometries a pane lays out in, then the four toggles about what
/// the pane shows around them.
pub const VIEW: Submenu = Submenu {
    id: "menu.view",
    name: "menu.view",
    groups: &[
        &["view.details", "view.grid", "view.columns"],
        &["view.hidden", "view.sidebar", "view.columns_reset", "view.theme"],
    ],
    shorts: &[],
};

/// The three columns a listing sorts on. On the menu as well as on the column
/// headers, because a grid has no headers.
pub const SORT: Submenu = Submenu {
    id: "menu.sort",
    name: "menu.sort",
    groups: &[&["view.sort_name", "view.sort_size", "view.sort_modified"]],
    shorts: &[
        ("view.sort_name", "menu.sort.name"),
        ("view.sort_size", "menu.sort.size"),
        ("view.sort_modified", "menu.sort.modified"),
    ],
};

/// The four groupings, and the one row that reopens every collapsed band.
pub const GROUP: Submenu = Submenu {
    id: "menu.group",
    name: "menu.group",
    groups: &[
        &["view.group_none", "view.group_time", "view.group_size", "view.group_kind"],
        &["view.bands_show"],
    ],
    shorts: &[
        ("view.group_time", "menu.group.time"),
        ("view.group_size", "menu.group.size"),
        ("view.group_kind", "menu.group.kind"),
    ],
};

/// Building and forgetting a search index — [ADR
/// 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md).
pub const INDEX: Submenu = Submenu {
    id: "menu.index",
    name: "menu.index",
    groups: &[&["index.build", "index.build_all", "index.forget"]],
    shorts: &[("index.build", "menu.index.build"), ("index.build_all", "menu.index.build_all")],
};

/// What can be done to the operation in flight, and then to one that failed.
pub const OPERATION: Submenu = Submenu {
    id: "menu.operation",
    name: "menu.operation",
    groups: &[
        &["edit.pause", "edit.cancel", "edit.resume"],
        &["edit.skip_failed", "edit.dismiss_issue"],
    ],
    shorts: &[],
};

/// Windows and tabs, then splits — [ADR
/// 0010](../../../docs/adr/0010-a-split-tree-of-tab-strips.md).
pub const PANES: Submenu = Submenu {
    id: "menu.panes",
    name: "menu.panes",
    groups: &[
        &["window.new", "tab.new", "tab.close", "tab.reopen"],
        &["pane.split_beside", "pane.split_below", "pane.close"],
    ],
    shorts: &[],
};

/// Every submenu a table may name.
pub const SUBMENUS: &[&Submenu] =
    &[&OPEN_IN, &COLOUR, &VIEW, &SORT, &GROUP, &INDEX, &OPERATION, &PANES];

/// The Open with row's identifier, which doubles as the catalogue key for its
/// name. Not a [`Submenu`]: its rows are what a job finds, not a table.
pub const OPEN_WITH: &str = "menu.open_with";

/// The command the Open with row stands for: its `enabled` is the row's, and the
/// reachability test counts the row as the pointer's way to it. One name read
/// by all three, because three copies of a string are three ways to drift.
pub const OPEN_WITH_COMMAND: &str = "file.open_with";

/// The submenu a table's identifier names, if it names one.
#[must_use]
pub fn submenu(id: &str) -> Option<&'static Submenu> {
    SUBMENUS.iter().copied().find(|sub| sub.id == id)
}

/// What a row is, which is what choosing it does.
pub enum Kind {
    /// A command: chosen, it runs and the menu closes.
    Command,
    /// A table of its own: chosen, it opens beside this menu.
    Submenu(&'static Submenu),
    /// The Open with row, over `path`.
    ///
    /// **Its rows are data, and they arrive late.** Which applications open a
    /// file is what this machine's `mimeapps.list` and `.desktop` files say,
    /// resolved on a job — the type alone can mean opening the file — so `found`
    /// is `None` for the frames between the menu opening and the answer landing,
    /// and the submenu says `Reading…` for those frames rather than nothing.
    /// The row itself is dimmed exactly as `file.open_with` would be.
    OpenWith {
        /// The file the menu opened over, so an answer about another file — a
        /// job that outlived the menu it was started for — is refused.
        path: PathBuf,
        /// What the job found, once it has.
        found: Option<Handlers>,
    },
    /// One application on the Open with submenu, and the file to hand it.
    Launch {
        /// The application.
        entry: DesktopEntry,
        /// The file.
        path: PathBuf,
    },
    /// A row that says something and does nothing: `Reading…`, or that nothing
    /// installed opens the file. Never enabled.
    Note,
}

/// One row of an open menu.
pub struct Item {
    /// What it is: a command's identifier, a submenu's, or an application's
    /// `.desktop` name — which is what the harness reports and a scenario aims
    /// by.
    pub id: String,
    /// What it is called, through the catalogue.
    pub name: String,
    /// Its chord, so the menu teaches the keyboard — rule 4.
    pub keys: String,
    /// Whether it can run right now. A row that cannot is dimmed and not
    /// removed, so the menu is the same shape every time it opens.
    pub enabled: bool,
    /// Whether a divider goes above this row.
    pub starts_group: bool,
    /// What choosing it does.
    pub kind: Kind,
}

/// What choosing a row does, taken out of the menu so the menu can close first.
pub enum Choice {
    /// Run this command.
    Run(String),
    /// Hand this file to this application.
    Launch {
        /// The application.
        entry: DesktopEntry,
        /// The file.
        path: PathBuf,
    },
}

impl Item {
    /// Whether choosing it opens a submenu rather than running something.
    #[must_use]
    pub fn opens(&self) -> bool {
        matches!(self.kind, Kind::Submenu(_) | Kind::OpenWith { .. })
    }

    /// What choosing it does — nothing for a row that opens, says, or cannot run.
    #[must_use]
    pub fn choice(&self) -> Option<Choice> {
        if !self.enabled {
            return None;
        }
        match &self.kind {
            Kind::Command => Some(Choice::Run(self.id.clone())),
            Kind::Launch { entry, path } => {
                Some(Choice::Launch { entry: entry.clone(), path: path.clone() })
            }
            Kind::Submenu(_) | Kind::OpenWith { .. } | Kind::Note => None,
        }
    }

    /// The identifiers of the rows this one opens onto, for the diagnostic line:
    /// a scenario that aims at a colour has to know which row to open first.
    #[must_use]
    pub fn holds(&self) -> Vec<&str> {
        match &self.kind {
            Kind::Submenu(sub) => {
                sub.groups.iter().flat_map(|group| group.iter().copied()).collect()
            }
            Kind::OpenWith { found: Some(found), .. } => {
                found.rows.iter().map(|entry| entry.id.as_str()).collect()
            }
            Kind::OpenWith { found: None, .. }
            | Kind::Command
            | Kind::Launch { .. }
            | Kind::Note => Vec::new(),
        }
    }
}

/// An open menu.
pub struct Menu {
    /// Where the pointer was, or where the cursor row is when a key opened it.
    pub at: Point,
    /// The rows, in order.
    pub items: Vec<Item>,
    /// The row the keyboard or the pointer is on.
    pub active: Option<usize>,
    /// The first row drawn, for a menu taller than the window it is in.
    pub scroll: usize,
    /// The submenu one of the rows has open, while one is. Its owner is the
    /// active row for as long as it is open, whatever the pointer does.
    pub sub: Option<Box<Sub>>,
}

/// A submenu, hanging off one row of the menu that holds it.
pub struct Sub {
    /// The row it hangs off.
    pub owner: usize,
    /// The menu itself, with rows, a highlight and a scroll of its own.
    pub menu: Menu,
    /// Where the pointer last was over the owner's row, when a pointer opened
    /// it: the apex of the triangle a move towards the submenu may cut across
    /// the rows below the owner in — [`Sub::heading_in`].
    pub from: Option<Point>,
}

/// Builds a menu from a table of command identifiers.
///
/// An identifier the registry does not have is skipped rather than shown broken;
/// a test asserts that never happens, which is the difference between a typo
/// failing the build and a typo becoming an empty row.
///
/// `enabled` is passed rather than the context a command runs against, because
/// the two callers have different ones: the program asks the registry about its
/// `App`, and a capture has no `App` and states the answers it wants a golden
/// frame of.
#[must_use]
pub fn build<C>(
    groups: &[&[&str]],
    registry: &Registry<C>,
    at: Point,
    enabled: impl Fn(&str) -> bool,
) -> Option<Menu> {
    let mut items: Vec<Item> = Vec::new();
    for group in groups {
        let mut first = true;
        for id in *group {
            let Some(mut item) = row(id, registry, &enabled) else { continue };
            item.starts_group = first && !items.is_empty();
            items.push(item);
            first = false;
        }
    }
    (!items.is_empty()).then_some(Menu { at, items, active: None, scroll: 0, sub: None })
}

/// One row of a table, or nothing for an identifier that is neither a command
/// nor a submenu.
fn row<C>(id: &str, registry: &Registry<C>, enabled: &impl Fn(&str) -> bool) -> Option<Item> {
    if id == OPEN_WITH {
        return Some(Item {
            id: id.to_owned(),
            name: i18n::lookup(OPEN_WITH).to_owned(),
            keys: String::new(),
            enabled: enabled(OPEN_WITH_COMMAND),
            starts_group: false,
            kind: Kind::OpenWith { path: PathBuf::new(), found: None },
        });
    }
    if let Some(sub) = submenu(id) {
        return Some(Item {
            id: id.to_owned(),
            name: i18n::lookup(sub.name).to_owned(),
            keys: String::new(),
            enabled: sub.groups.iter().flat_map(|group| group.iter()).any(|child| enabled(child)),
            starts_group: false,
            kind: Kind::Submenu(sub),
        });
    }
    let command = registry.find(id)?;
    let described = registry.describe(command)?;
    Some(Item {
        id: described.id.to_owned(),
        name: described.name.to_owned(),
        keys: described.keys,
        enabled: enabled(described.id),
        starts_group: false,
        kind: Kind::Command,
    })
}

/// The Open with submenu, in whichever of its three states the answer is in.
fn applications(answer: Option<&Handlers>, at: Point) -> Menu {
    let note = |id: &str, name: String| Item {
        id: id.to_owned(),
        name,
        keys: String::new(),
        enabled: false,
        starts_group: false,
        kind: Kind::Note,
    };
    let items = match answer {
        None => vec![note("menu.reading", i18n::lookup("menu.reading").to_owned())],
        Some(found) if found.rows.is_empty() => {
            vec![note("menu.nothing_opens", i18n::format("error.no_handler", &[&found.mime]))]
        }
        Some(found) => found
            .rows
            .iter()
            .map(|entry| Item {
                id: entry.id.clone(),
                name: entry.name.clone(),
                keys: String::new(),
                enabled: true,
                starts_group: false,
                kind: Kind::Launch { entry: entry.clone(), path: found.path.clone() },
            })
            .collect(),
    };
    Menu { at, items, active: None, scroll: 0, sub: None }
}

impl Menu {
    /// Moves the highlight, skipping rows that cannot run and wrapping at both
    /// ends.
    ///
    /// Skipping rather than stopping: a dimmed row the arrows land on is a key
    /// press that looks like the menu is stuck, and Undo is dimmed for the whole
    /// first minute a person uses the program.
    pub fn step(&mut self, forward: bool) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        let mut index = match self.active {
            Some(active) => active,
            // One step from beyond the end lands on the first row, and from
            // beyond the start on the last.
            None if forward => count - 1,
            None => 0,
        };
        for _ in 0..count {
            index = if forward { (index + 1) % count } else { (index + count - 1) % count };
            if self.items[index].enabled {
                self.active = Some(index);
                return;
            }
        }
    }

    /// Puts the highlight on a row the pointer found, or takes it off.
    ///
    /// Returns whether anything changed, because a frame is only worth drawing
    /// if it does.
    pub fn hover(&mut self, index: Option<usize>) -> bool {
        let wanted = index.filter(|row| self.items.get(*row).is_some_and(|item| item.enabled));
        let changed = self.active != wanted;
        self.active = wanted;
        changed
    }

    /// Opens the submenu under `owner`, if that row has one and can run, and
    /// says whether it did.
    ///
    /// `from` is where the pointer is when a pointer asked, and nothing when a
    /// key did. Opening puts the highlight on the owner and leaves the submenu's
    /// own highlight off, which is where a pointer wants it; a key asks for the
    /// first row afterwards.
    pub fn expand<C>(
        &mut self,
        owner: usize,
        registry: &Registry<C>,
        enabled: impl Fn(&str) -> bool,
        from: Option<Point>,
    ) -> bool {
        let Some(item) = self.items.get(owner).filter(|item| item.enabled) else { return false };
        let menu = match &item.kind {
            Kind::Submenu(sub) => build(sub.groups, registry, self.at, enabled).map(|mut menu| {
                for item in &mut menu.items {
                    if let Some((_, key)) =
                        sub.shorts.iter().find(|(command, _)| *command == item.id)
                    {
                        item.name = i18n::lookup(key).to_owned();
                    }
                }
                menu
            }),
            Kind::OpenWith { found, .. } => Some(applications(found.as_ref(), self.at)),
            Kind::Command | Kind::Launch { .. } | Kind::Note => None,
        };
        let Some(menu) = menu else { return false };
        self.active = Some(owner);
        self.sub = Some(Box::new(Sub { owner, menu, from }));
        true
    }

    /// Closes the open submenu, leaving the highlight on the row it hung off,
    /// and says whether there was one.
    pub fn collapse(&mut self) -> bool {
        self.sub.take().is_some()
    }

    /// The menu the keyboard is in: the open submenu, else this one.
    pub fn innermost_mut(&mut self) -> &mut Menu {
        match self.sub {
            Some(ref mut sub) => &mut sub.menu,
            None => self,
        }
    }

    /// Names the file the Open with row is about. Whether there is such a row —
    /// which is whether a job needs starting for it.
    pub fn asks_about(&mut self, file: PathBuf) -> bool {
        let Some(item) =
            self.items.iter_mut().find(|item| matches!(item.kind, Kind::OpenWith { .. }))
        else {
            return false;
        };
        item.kind = Kind::OpenWith { path: file, found: None };
        true
    }

    /// Takes delivery of the applications for the Open with row. Whether the
    /// row was about that file — an answer about another one is refused, because
    /// the job that produced it was started for a menu that has since closed.
    pub fn answer(&mut self, found: Handlers) -> bool {
        let Some(index) = self.items.iter().position(
            |item| matches!(&item.kind, Kind::OpenWith { path, .. } if *path == found.path),
        ) else {
            return false;
        };
        // A submenu already open on the row is showing `Reading…`, and the
        // answer is what it was waiting for. One a key opened was promised its
        // first row lit, and `Reading…` could not be.
        if let Some(sub) = self.sub.as_mut()
            && sub.owner == index
        {
            sub.menu = applications(Some(&found), self.at);
            if sub.from.is_none() {
                sub.menu.step(true);
            }
        }
        self.items[index].kind = Kind::OpenWith { path: found.path.clone(), found: Some(found) };
        true
    }

    /// Asks again which rows can run, for every row of this menu and of the
    /// submenu it has open. Whether anything changed.
    ///
    /// **Every frame a menu is open**, because the answers change under it — an
    /// operation lands and Undo lights up — and a row that opens costs one
    /// question per child until the first that can run. A highlight cannot sit
    /// on a row that has stopped being runnable, and a submenu cannot hang off
    /// one.
    pub fn settle(&mut self, enabled: &impl Fn(&str) -> bool) -> bool {
        let mut moved = false;
        for item in &mut self.items {
            let now = match &item.kind {
                Kind::Command => enabled(&item.id),
                Kind::Submenu(sub) => {
                    sub.groups.iter().flat_map(|group| group.iter()).any(|child| enabled(child))
                }
                Kind::OpenWith { .. } => enabled(OPEN_WITH_COMMAND),
                Kind::Launch { .. } => true,
                Kind::Note => false,
            };
            moved |= item.enabled != now;
            item.enabled = now;
        }
        if self.active.is_some_and(|row| !self.items.get(row).is_some_and(|item| item.enabled)) {
            self.active = None;
            moved = true;
        }
        let owner_runs = self
            .sub
            .as_ref()
            .is_some_and(|sub| self.items.get(sub.owner).is_some_and(|item| item.enabled));
        if owner_runs {
            if let Some(sub) = self.sub.as_mut() {
                moved |= sub.menu.settle(enabled);
            }
        } else if self.sub.take().is_some() {
            moved = true;
        }
        moved
    }

    /// The rows that fit in `room` starting at the scroll, and the first's index.
    ///
    /// **A menu can be taller than the window.** The background menu is the
    /// pointer's way to every command that is not a button, so it grows with the
    /// registry — and at 900×520 it is already taller than the window it opens
    /// in. `place` flips it rather than clipping it, which cannot help when
    /// there is no side it fits on.
    #[must_use]
    pub fn window(&self, room: f32) -> (usize, &[Item]) {
        let first = self.scroll.min(self.items.len().saturating_sub(1));
        let mut used = PAD * 2.0;
        let mut last = first;
        for (offset, item) in self.items[first..].iter().enumerate() {
            // The divider above the first drawn row would be a line under
            // nothing.
            let step = STEP + if item.starts_group && offset > 0 { DIVIDER } else { 0.0 };
            if used + step > room && last > first {
                break;
            }
            used += step;
            last += 1;
        }
        (first, &self.items[first..last.min(self.items.len())])
    }

    /// Puts the highlighted row inside the window of `room`.
    pub fn reveal(&mut self, room: f32) {
        let Some(active) = self.active else { return };
        if active < self.scroll {
            self.scroll = active;
            return;
        }
        // Walk the scroll forward until the active row is one of the drawn ones,
        // which is one step per row rather than arithmetic over a mix of row
        // heights and divider heights.
        while !self.window(room).1.is_empty() {
            let (first, drawn) = self.window(room);
            if active < first + drawn.len() {
                return;
            }
            self.scroll += 1;
        }
    }

    /// Scrolls by a wheel notch. Whether anything moved.
    pub fn wheel(&mut self, delta: f32, room: f32) -> bool {
        let was = self.scroll;
        // **The same direction the list scrolls.** A positive delta is the wheel
        // turned down and it brings *later* rows into view, here as everywhere
        // else. These two arms were the other way round, so a tall menu answered
        // the wheel backwards — and the scenario that covered it was written
        // against the wrong direction, which is why nobody noticed.
        if delta > 0.0 {
            let (first, drawn) = self.window(room);
            if first + drawn.len() < self.items.len() {
                self.scroll += 1;
            }
        } else if delta < 0.0 {
            self.scroll = self.scroll.saturating_sub(1);
        }
        // **A submenu whose owner has scrolled out goes with it.** The keys
        // would otherwise walk a panel the view no longer draws, and Enter would
        // run one of its rows unseen.
        let (first, drawn) = self.window(room);
        let owner_gone =
            self.sub.as_ref().is_some_and(|sub| !(first..first + drawn.len()).contains(&sub.owner));
        if owner_gone {
            self.sub = None;
        }
        self.scroll != was
    }

    /// The identifier of the highlighted row.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.items.get(self.active?).map(|item| item.id.as_str())
    }

    /// The highlighted row.
    #[must_use]
    pub fn active_item(&self) -> Option<&Item> {
        self.items.get(self.active?)
    }
}

impl Sub {
    /// Whether a pointer at `position`, over the parent but off the owner's row,
    /// is on its way to the submenu.
    ///
    /// **The triangle every desktop menu draws.** Its apex is where the pointer
    /// left the owner's row and its base is the submenu's near edge, and a
    /// pointer inside it is crossing the rows below the owner to reach a row of
    /// the submenu. Lighting those rows up — and swapping the submenu for
    /// theirs — is what makes a diagonal move to a submenu impossible, and a
    /// person who wanted one of those rows moves straight down, which leaves the
    /// triangle within a pixel or two of its apex. Nothing is timed: a menu that
    /// waits to see whether the pointer stops is a menu that reacts late to the
    /// pointer that did not.
    #[must_use]
    pub fn heading_in(&self, position: Point, panel: Area, owner: Area) -> bool {
        let Some(from) = self.from else { return false };
        let edge = if panel.x >= owner.x + owner.width / 2.0 { panel.x } else { panel.right() };
        inside(position, from, Point::new(edge, panel.y), Point::new(edge, panel.bottom()))
    }
}

/// Whether `point` is inside, or on an edge of, the triangle `a b c`.
fn inside(point: Point, a: Point, b: Point, c: Point) -> bool {
    let side = |p: Point, q: Point| (q.x - p.x) * (point.y - p.y) - (q.y - p.y) * (point.x - p.x);
    let sides = [side(a, b), side(b, c), side(c, a)];
    let negative = sides.iter().any(|side| *side < 0.0);
    let positive = sides.iter().any(|side| *side > 0.0);
    !(negative && positive)
}

/// How close to the window's edge a menu may come.
pub const MARGIN: f32 = 4.0;
/// The vertical space around the rows.
pub const PAD: f32 = 6.0;
/// One row.
pub const STEP: f32 = 26.0;
/// The extra height a divider costs.
pub const DIVIDER: f32 = 9.0;
/// The text inset at each side.
pub const INSET: f32 = 12.0;
/// The least space between a name and its chord.
pub const SPACING: f32 = 28.0;
/// The narrowest and widest a menu may be.
pub const WIDTH: (f32, f32) = (200.0, 460.0);
/// How far a submenu's panel sits over its parent's edge, so the two read as one
/// thing rather than as a panel that happens to be nearby.
pub const OVERLAP: f32 = 4.0;
/// The mark at the right of a row that opens a submenu: a chevron this wide.
pub const CHEVRON: f32 = 8.0;

/// How tall a menu showing exactly these rows is.
///
/// The first row's divider does not count: a line above the top row is a line
/// under nothing, which is what a scrolled menu would otherwise draw.
#[must_use]
pub fn height(items: &[Item]) -> f32 {
    let dividers = items.iter().skip(1).filter(|item| item.starts_group).count();
    PAD * 2.0 + items.len() as f32 * STEP + dividers as f32 * DIVIDER
}

/// Where the panel goes, given the size the view measured for it.
///
/// **Flipped, never clipped.** A menu whose bottom rows are off the window is a
/// menu whose last two entries do not exist, and the last two entries of the row
/// menu are Rename and Move to Trash. The flip puts the corner the pointer is at
/// on the other side of the pointer, which is what every menu on the desktop
/// does and therefore the only behaviour that will not surprise.
#[must_use]
pub fn place(at: Point, width: f32, height: f32, window: Size) -> Area {
    let margin = MARGIN;
    let mut x = at.x;
    if x + width > window.width - margin {
        x = at.x - width;
    }
    let mut y = at.y;
    if y + height > window.height - margin {
        y = at.y - height;
    }
    Area {
        x: x.max(margin).min((window.width - width - margin).max(margin)),
        y: y.max(margin).min((window.height - height - margin).max(margin)),
        width,
        height,
    }
}

/// Where a submenu's panel goes: beside its parent, its first row level with
/// the row it hangs off.
///
/// **To the right, else to the left, never over.** A submenu drawn over its
/// parent covers the row it came from, and a person who moved right to reach it
/// has nowhere to move. Down it is clamped rather than flipped: a submenu that
/// opened upwards would put its first row somewhere other than beside its
/// owner, which is the one place a hand moving sideways expects to find it.
#[must_use]
pub fn place_beside(owner: Area, parent: Area, width: f32, height: f32, window: Size) -> Area {
    let mut x = parent.right() - OVERLAP;
    if x + width > window.width - MARGIN {
        x = parent.x - width + OVERLAP;
    }
    let y = owner.y - PAD;
    Area {
        x: x.max(MARGIN).min((window.width - width - MARGIN).max(MARGIN)),
        y: y.max(MARGIN).min((window.height - height - MARGIN).max(MARGIN)),
        width,
        height,
    }
}

/// The top of each row inside a placed panel, in the order of [`Menu::items`].
///
/// One function, read by the draw and by the hit test, because two copies of
/// this arithmetic is how a click lands on Move to Trash when the pointer was on
/// Rename.
pub fn row_tops(items: &[Item], panel: Area) -> impl Iterator<Item = f32> + '_ {
    let mut y = panel.y + PAD;
    items.iter().enumerate().map(move |(index, item)| {
        if item.starts_group && index > 0 {
            y += DIVIDER;
        }
        let top = y;
        y += STEP;
        top
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::PathBuf;

    use sk_desktop::DesktopEntry;
    use sk_plat::{Point, Size};

    use super::{
        COLOUR, Choice, IN_A_LOOK, IN_RECENT, IN_TRASH, Item, Kind, Menu, ON_A_PLACE,
        ON_BACKGROUND, ON_ROW, OPEN_WITH, SUBMENUS, Sub, height, inside, place, place_beside,
        row_tops, submenu,
    };
    use crate::layout::Area;
    use crate::openers::Handlers;

    fn menu(enabled: &[bool]) -> Menu {
        let items = enabled
            .iter()
            .enumerate()
            .map(|(index, enabled)| Item {
                id: "file.copy".to_owned(),
                name: format!("row {index}"),
                keys: String::new(),
                enabled: *enabled,
                starts_group: false,
                kind: Kind::Command,
            })
            .collect();
        Menu { at: Point::new(0.0, 0.0), items, active: None, scroll: 0, sub: None }
    }

    fn entry(id: &str, name: &str) -> DesktopEntry {
        DesktopEntry {
            id: id.to_owned(),
            path: PathBuf::from(format!("/usr/share/applications/{id}")),
            name: name.to_owned(),
            exec: "true %f".to_owned(),
            terminal: false,
            mime_types: vec!["text/plain".to_owned()],
            no_display: false,
            try_exec: None,
        }
    }

    /// Undo is dimmed for the whole first minute a person uses the program, and
    /// an arrow key that lands on it looks like the menu is stuck.
    #[test]
    fn the_arrows_skip_what_cannot_run_and_wrap_at_both_ends() {
        let mut menu = menu(&[true, false, false, true]);
        menu.step(true);
        assert_eq!(menu.active, Some(0));
        menu.step(true);
        assert_eq!(menu.active, Some(3), "two dimmed rows were not skipped");
        menu.step(true);
        assert_eq!(menu.active, Some(0), "it did not wrap");
        menu.step(false);
        assert_eq!(menu.active, Some(3), "backwards did not wrap");
    }

    #[test]
    fn a_menu_with_nothing_enabled_never_highlights_a_row() {
        let mut menu = menu(&[false, false]);
        menu.step(true);
        assert_eq!(menu.active, None);
        assert!(!menu.hover(Some(1)), "a dimmed row took the highlight");
        assert_eq!(menu.active_id(), None);
    }

    #[test]
    fn the_pointer_and_the_keyboard_share_one_highlight() {
        let mut menu = menu(&[true, true]);
        assert!(menu.hover(Some(1)));
        assert_eq!(menu.active_id(), Some("file.copy"));
        assert!(!menu.hover(Some(1)), "the same row twice asked for a frame");
        assert!(menu.hover(None));
        assert_eq!(menu.active, None);
    }

    /// The last two entries of the row menu are Rename and Move to Trash, and a
    /// menu opened near the bottom edge used to put them off the window.
    #[test]
    fn a_menu_near_an_edge_flips_instead_of_falling_off_the_window() {
        let window = Size::new(800.0, 600.0);
        let near = place(Point::new(780.0, 580.0), 240.0, 200.0, window);
        assert!(near.x < 780.0 && near.right() <= 800.0, "{near:?}");
        assert!(near.y < 580.0 && near.bottom() <= 600.0, "{near:?}");

        let room = place(Point::new(100.0, 100.0), 240.0, 200.0, window);
        assert!((room.x - 100.0).abs() < f32::EPSILON, "{room:?}");
        assert!((room.y - 100.0).abs() < f32::EPSILON, "{room:?}");
    }

    /// A menu taller than the window has nowhere to flip to, and the arithmetic
    /// that clamps it must not put it off the top instead.
    #[test]
    fn a_menu_taller_than_the_window_still_starts_on_screen() {
        let window = Size::new(320.0, 200.0);
        let area = place(Point::new(10.0, 190.0), 480.0, 400.0, window);
        assert!(area.x >= 4.0 && area.y >= 4.0, "{area:?}");
    }

    #[test]
    fn a_divider_moves_every_row_below_it_down_by_its_own_height() {
        let mut menu = menu(&[true, true, true]);
        menu.items[1].starts_group = true;
        let panel =
            place(Point::new(0.0, 0.0), 240.0, height(&menu.items), Size::new(800.0, 600.0));
        let tops: Vec<f32> = row_tops(&menu.items, panel).collect();
        assert_eq!(tops.len(), 3);
        assert!((tops[1] - tops[0] - super::STEP - super::DIVIDER).abs() < 0.01, "{tops:?}");
        assert!((tops[2] - tops[1] - super::STEP).abs() < 0.01, "{tops:?}");
        // The panel is exactly as tall as the rows it holds, plus its padding.
        assert!((panel.bottom() - super::PAD - tops[2] - super::STEP).abs() < 0.01, "{tops:?}");
    }

    /// **A menu answers the wheel the way the list does.** A positive delta is the
    /// wheel turned down and it shows what is further along; the two arms were
    /// swapped, and a menu taller than the window scrolled backwards.
    #[test]
    fn the_wheel_scrolls_a_menu_the_way_it_scrolls_a_list() {
        let mut tall = menu(&[true; 20]);
        // Room for a handful of its rows, so there is somewhere to scroll to.
        let room = 120.0;
        assert_eq!(tall.scroll, 0);
        assert!(tall.wheel(66.0, room), "a wheel down moved nothing");
        assert_eq!(tall.scroll, 1, "a wheel down shows what is further along");
        assert!(tall.wheel(-66.0, room));
        assert_eq!(tall.scroll, 0, "and a wheel up comes back");
        assert!(!tall.wheel(-66.0, room), "the top clamps rather than wrapping");
    }

    /// A `menu.` identifier in a table that names no submenu would build as no
    /// row at all, and a submenu naming a submenu would be a depth nothing here
    /// draws.
    #[test]
    fn every_submenu_row_names_a_submenu_and_no_submenu_nests() {
        for table in [ON_ROW, IN_TRASH, IN_RECENT, IN_A_LOOK, ON_A_PLACE, ON_BACKGROUND] {
            for id in table.iter().flat_map(|group| group.iter()) {
                if id.starts_with("menu.") {
                    assert!(*id == OPEN_WITH || submenu(id).is_some(), "`{id}` opens nothing");
                }
            }
        }
        for sub in SUBMENUS {
            assert!(sub.id.starts_with("menu."), "`{}` is not under `menu.`", sub.id);
            for child in sub.groups.iter().flat_map(|group| group.iter()) {
                assert!(!child.starts_with("menu."), "`{}` holds `{child}`", sub.id);
            }
        }
        let mut ids: Vec<&str> = SUBMENUS.iter().map(|sub| sub.id).collect();
        ids.push(OPEN_WITH);
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two submenus share an identifier");
    }

    /// A lookup that misses returns its key, so a row would read `menu.colour`.
    #[test]
    fn every_submenu_has_a_name_in_the_catalogue() {
        use sk_model::i18n::lookup;
        for sub in SUBMENUS {
            assert_ne!(lookup(sub.name), sub.name, "`{}` has no name", sub.id);
        }
        assert_ne!(lookup(OPEN_WITH), OPEN_WITH);
        assert_ne!(lookup("menu.reading"), "menu.reading");
    }

    /// A short name names a row of its own submenu and is in the catalogue, and
    /// the row wears it: Colour opens onto "Red", not "Colour it red".
    #[test]
    fn a_short_name_belongs_to_a_row_of_its_submenu_and_the_row_wears_it() {
        use sk_model::i18n::lookup;
        for sub in SUBMENUS {
            for (command, key) in sub.shorts {
                let held = sub.groups.iter().flat_map(|group| group.iter()).any(|id| id == command);
                assert!(held, "`{}` shortens `{command}`, which is not on it", sub.id);
                assert_ne!(lookup(key), *key, "`{key}` is not in the catalogue");
            }
        }
        let registry = crate::commands::registry();
        let mut menu = super::build(ON_ROW, &registry, Point::new(0.0, 0.0), |_| true).unwrap();
        let colour = menu.items.iter().position(|item| item.id == COLOUR.id).unwrap();
        assert!(menu.expand(colour, &registry, |_| true, None));
        let sub = menu.sub.as_ref().unwrap();
        assert_eq!(sub.menu.items[0].name, "Red");
        assert_eq!(
            sub.menu.items[6].name,
            lookup("action.colour_none"),
            "the unlisted row lost its name"
        );
    }

    /// The background menu is taller than a small window, and a wheel notch can
    /// take a submenu's owner off the panel: the submenu goes with it, or the
    /// keys would walk a panel nobody can see.
    #[test]
    fn a_submenu_closes_when_its_owner_scrolls_out_of_the_window() {
        let registry = crate::commands::registry();
        let mut menu =
            super::build(ON_BACKGROUND, &registry, Point::new(0.0, 0.0), |_| true).unwrap();
        let view = menu.items.iter().position(|item| item.id == super::VIEW.id).unwrap();
        assert!(menu.expand(view, &registry, |_| true, None));
        // Room for the first few rows only, so the View row is drawn only while
        // the scroll is near it.
        let room = super::PAD * 2.0 + super::STEP * 3.0;
        menu.scroll = view.saturating_sub(1);
        assert!(menu.wheel(66.0, room), "nothing scrolled");
        assert!(menu.sub.is_some(), "the owner is still drawn and the submenu went");
        assert!(menu.wheel(66.0, room));
        assert!(menu.wheel(66.0, room));
        assert!(menu.sub.is_none(), "the owner scrolled out and the submenu stayed");
    }

    /// The colours over a file: seven dimmed rows became one dimmed row, and a
    /// dimmed row opens nothing.
    #[test]
    fn a_submenu_row_is_dimmed_when_nothing_in_it_can_run_and_opens_when_something_can() {
        let registry = crate::commands::registry();
        let at = Point::new(0.0, 0.0);
        let mut over_a_file =
            super::build(ON_ROW, &registry, at, |id| !id.starts_with("view.colour")).unwrap();
        let colour = over_a_file.items.iter().position(|item| item.id == COLOUR.id).unwrap();
        assert!(!over_a_file.items[colour].enabled);
        assert!(!over_a_file.expand(colour, &registry, |_| true, None));
        assert!(over_a_file.sub.is_none());

        let mut over_a_folder = super::build(ON_ROW, &registry, at, |_| true).unwrap();
        assert!(over_a_folder.expand(colour, &registry, |_| true, None));
        assert_eq!(over_a_folder.active, Some(colour), "the owner is not highlighted");
        let sub = over_a_folder.sub.as_ref().unwrap();
        assert_eq!(sub.owner, colour);
        assert_eq!(sub.menu.items.len(), 7);
        assert!(sub.menu.items[6].starts_group, "Remove the colour is not behind a divider");
        assert!(
            matches!(sub.menu.items[0].choice(), Some(Choice::Run(id)) if id == "view.colour_red")
        );
        assert!(over_a_folder.items[colour].choice().is_none(), "an opener ran as a command");
        assert!(over_a_folder.collapse());
        assert_eq!(over_a_folder.active, Some(colour), "collapsing lost the owner");
    }

    /// The row's owner stops being runnable — the selection moved off the
    /// folder — and the submenu hanging off it has to go with the highlight.
    #[test]
    fn settling_closes_a_submenu_whose_owner_stopped_being_runnable() {
        let registry = crate::commands::registry();
        let mut menu = super::build(ON_ROW, &registry, Point::new(0.0, 0.0), |_| true).unwrap();
        let colour = menu.items.iter().position(|item| item.id == COLOUR.id).unwrap();
        assert!(menu.expand(colour, &registry, |_| true, None));
        assert!(!menu.settle(&|_| true), "nothing changed and it said something did");
        assert!(menu.settle(&|id| !id.starts_with("view.colour")));
        assert!(menu.sub.is_none());
        assert_eq!(menu.active, None);
    }

    /// The Open with row says `Reading…` until the job answers, then lists what
    /// it found, and an answer about another file — from a menu that has since
    /// closed — is refused.
    #[test]
    fn the_open_with_row_waits_for_its_answer_and_refuses_another_files() {
        let registry = crate::commands::registry();
        let mut menu = super::build(ON_ROW, &registry, Point::new(0.0, 0.0), |_| true).unwrap();
        let open_with = menu.items.iter().position(|item| item.id == OPEN_WITH).unwrap();
        assert!(menu.asks_about(PathBuf::from("/tmp/a.txt")));
        assert!(menu.expand(open_with, &registry, |_| true, None));
        {
            let sub = menu.sub.as_ref().unwrap();
            assert_eq!(sub.menu.items.len(), 1);
            assert!(!sub.menu.items[0].enabled, "`Reading…` is a row that can be chosen");
            assert!(matches!(sub.menu.items[0].kind, Kind::Note));
        }
        assert!(menu.items[open_with].holds().is_empty());

        let other = Handlers {
            path: PathBuf::from("/tmp/b.txt"),
            mime: "text/plain".to_owned(),
            rows: vec![entry("other.desktop", "Other")],
        };
        assert!(!menu.answer(other), "an answer about another file was taken");

        let found = Handlers {
            path: PathBuf::from("/tmp/a.txt"),
            mime: "text/plain".to_owned(),
            rows: vec![entry("probe.desktop", "Probe"), entry("gedit.desktop", "Text Editor")],
        };
        assert!(menu.answer(found));
        assert_eq!(menu.items[open_with].holds(), ["probe.desktop", "gedit.desktop"]);
        let sub = menu.sub.as_ref().unwrap();
        assert_eq!(sub.menu.items.len(), 2, "the open submenu was not rebuilt");
        assert_eq!(sub.menu.active, Some(0), "a submenu a key opened has no row lit");
        assert_eq!(sub.menu.items[1].name, "Text Editor");
        match sub.menu.items[0].choice() {
            Some(Choice::Launch { entry, path }) => {
                assert_eq!(entry.id, "probe.desktop");
                assert_eq!(path, PathBuf::from("/tmp/a.txt"));
            }
            _ => panic!("an application row is not a launch"),
        }
    }

    /// Nothing installed opens the file: the submenu says so, in the sentence
    /// the panel would have, rather than opening onto nothing.
    #[test]
    fn an_empty_answer_is_a_sentence_and_not_an_empty_submenu() {
        let registry = crate::commands::registry();
        let mut menu = super::build(ON_ROW, &registry, Point::new(0.0, 0.0), |_| true).unwrap();
        let open_with = menu.items.iter().position(|item| item.id == OPEN_WITH).unwrap();
        assert!(menu.asks_about(PathBuf::from("/tmp/a.bin")));
        let found = Handlers {
            path: PathBuf::from("/tmp/a.bin"),
            mime: "application/octet-stream".to_owned(),
            rows: Vec::new(),
        };
        assert!(menu.answer(found));
        assert!(menu.expand(open_with, &registry, |_| true, None));
        let sub = menu.sub.as_ref().unwrap();
        assert_eq!(sub.menu.items.len(), 1);
        assert!(!sub.menu.items[0].enabled);
        assert!(
            sub.menu.items[0].name.contains("application/octet-stream"),
            "{}",
            sub.menu.items[0].name
        );
    }

    /// The submenu's first row is level with the row it hangs off, to the right
    /// of the parent; at the right edge it goes to the left instead, and near
    /// the bottom it slides up rather than off.
    #[test]
    fn a_submenu_sits_beside_its_owner_and_goes_left_at_the_edge() {
        let window = Size::new(800.0, 600.0);
        let parent = Area { x: 100.0, y: 100.0, width: 240.0, height: 300.0 };
        let owner = Area { x: 100.0, y: 158.0, width: 240.0, height: super::STEP };
        let beside = place_beside(owner, parent, 200.0, 120.0, window);
        assert!((beside.x - (parent.right() - super::OVERLAP)).abs() < 0.01, "{beside:?}");
        assert!((beside.y + super::PAD - owner.y).abs() < 0.01, "{beside:?}");

        let at_the_edge = Area { x: 540.0, y: 100.0, width: 240.0, height: 300.0 };
        let left = place_beside(owner, at_the_edge, 200.0, 120.0, window);
        assert!((left.right() - (at_the_edge.x + super::OVERLAP)).abs() < 0.01, "{left:?}");

        let low = Area { x: 100.0, y: 560.0, width: 240.0, height: super::STEP };
        let raised = place_beside(low, parent, 200.0, 120.0, window);
        assert!(raised.bottom() <= 600.0 - super::MARGIN, "{raised:?}");
    }

    /// A pointer leaving the owner's row towards the submenu passes over the
    /// rows below it, and must not take them; one going straight down must.
    #[test]
    fn a_move_towards_the_submenu_is_not_a_move_onto_the_rows_it_crosses() {
        let owner = Area { x: 100.0, y: 150.0, width: 240.0, height: super::STEP };
        let panel = Area { x: 336.0, y: 144.0, width: 200.0, height: 200.0 };
        let sub = Sub { owner: 0, menu: menu(&[true]), from: Some(Point::new(200.0, 160.0)) };
        assert!(
            sub.heading_in(Point::new(300.0, 250.0), panel, owner),
            "a diagonal was a change of row"
        );
        assert!(
            !sub.heading_in(Point::new(200.0, 250.0), panel, owner),
            "straight down was a diagonal"
        );
        assert!(
            !sub.heading_in(Point::new(150.0, 100.0), panel, owner),
            "moving away was a diagonal"
        );
        // Opened by a key, there is no apex and no triangle.
        let keyed = Sub { owner: 0, menu: menu(&[true]), from: None };
        assert!(!keyed.heading_in(Point::new(300.0, 250.0), panel, owner));
        // And with the submenu on the left, the triangle points left.
        let left = Area { x: 0.0, y: 144.0, width: 96.0, height: 200.0 };
        assert!(sub.heading_in(Point::new(120.0, 250.0), left, owner));
        assert!(!sub.heading_in(Point::new(300.0, 250.0), left, owner));
    }

    #[test]
    fn a_point_on_an_edge_of_the_triangle_counts_as_inside() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(10.0, 0.0);
        let c = Point::new(0.0, 10.0);
        assert!(inside(Point::new(2.0, 2.0), a, b, c));
        assert!(inside(Point::new(5.0, 0.0), a, b, c));
        assert!(!inside(Point::new(6.0, 6.0), a, b, c));
    }
}
