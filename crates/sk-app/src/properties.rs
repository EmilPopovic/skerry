// SPDX-License-Identifier: Apache-2.0
//! What a file is: the panel behind `Ctrl+I`.
//!
//! **Every value is read on a job.** A `statx`, a `readlink` and a `statfs` are
//! three syscalls that a hung mount turns into a frozen window, so the panel
//! opens empty and fills when the answer arrives — the same shape the sibling
//! menu and the listing already have.
//!
//! For more than one selected item it is a summary rather than a list: a count,
//! a total, and how the count divides between folders and files. A panel that
//! listed forty paths would be a second listing over the one behind it.

use std::path::PathBuf;

use sk_fs::facts::{Accounts, Facts, Kind};

/// How many bits a person can set here: the nine, and not the three above them.
///
/// [11-non-goals.md](../../../docs/11-non-goals.md) admits the basics and
/// declines a permissions editor, so setuid, setgid and sticky are carried
/// through untouched by `sk_fs::ops::set_permissions` and are not controls.
pub(crate) const BITS: usize = 9;

/// The panel, once something has been asked about.
pub(crate) struct Properties {
    /// What was selected when it opened.
    pub(crate) paths: Vec<PathBuf>,
    /// The facts for each, in the order they were asked for. Shorter than
    /// `paths` while they are still arriving.
    pub(crate) facts: Vec<Facts>,
    /// What the name service calls the one path's owner, once it has answered.
    ///
    /// **Its own answer, on its own job.** A name lookup can reach a directory
    /// server, so it arrives after the facts rather than with them — and until
    /// it does, the row says the numbers.
    pub(crate) accounts: Option<Accounts>,
    /// A recursive total for a single folder, once the measurer has one.
    pub(crate) total: Option<u64>,
    /// Whether this process may change the one path's bits.
    ///
    /// **Decided when the facts arrive, not in every frame.** It is the answer
    /// to *is this account the owner*, which does not change while a panel is
    /// open — and a capture states it, so a reference frame does not depend on
    /// which account ran it.
    pub(crate) writable: bool,
    /// Which permission bit the keyboard is on, once it has been put on one.
    pub(crate) aim: Option<usize>,
}

/// One row of the panel: a label already looked up, and a value.
pub(crate) struct Row {
    pub(crate) label: String,
    pub(crate) value: String,
    /// The mode, on the one row whose nine bits are controls rather than a
    /// sentence. `None` everywhere else — including on a permissions row this
    /// process may not change.
    pub(crate) bits: Option<u16>,
}

impl Row {
    /// A row that only says something.
    fn stated(label: String, value: String) -> Self {
        Self { label, value, bits: None }
    }
}

impl Properties {
    pub(crate) fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths, facts: Vec::new(), accounts: None, total: None, writable: false, aim: None }
    }

    /// The mode whose nine bits this panel may change, if any.
    ///
    /// **One file, owned by whoever is running, and not a symlink.** A symlink's
    /// bits mean nothing on Linux and `chmod` would follow it to the file it
    /// points at; a selection of several would be the recursive apply
    /// [11-non-goals.md](../../../docs/11-non-goals.md) declines.
    pub(crate) fn editable(&self) -> Option<u16> {
        let facts = self.only()?;
        (self.writable && facts.kind != Kind::Symlink).then_some(facts.mode)
    }

    /// The mode with bit `index` of the nine turned the other way.
    ///
    /// `index` counts from the owner's read, which is the order `rwxrwxrwx`
    /// prints and the order the boxes are drawn in.
    pub(crate) fn toggled(&self, index: usize) -> Option<u16> {
        let mode = self.editable()?;
        (index < BITS).then(|| mode ^ (1 << (BITS - 1 - index)))
    }

    /// Moves the keyboard one box along, and puts it on the end it came from
    /// when it is not on one yet.
    ///
    /// **It stops at either end rather than wrapping.** Nine boxes are a row, not
    /// a ring, and a `→` that jumped from everyone's execute back to the owner's
    /// read would be a press that undid what the last eight did.
    pub(crate) fn aim_next(&mut self, forward: bool) {
        if self.editable().is_none() {
            return;
        }
        self.aim = Some(match (self.aim, forward) {
            (None, true) => 0,
            (None, false) => BITS - 1,
            (Some(at), true) => (at + 1).min(BITS - 1),
            (Some(at), false) => at.saturating_sub(1),
        });
    }

    /// The one path this panel is about, when it is about one.
    pub(crate) fn only(&self) -> Option<&Facts> {
        (self.paths.len() == 1).then(|| self.facts.first()).flatten()
    }

    /// The title: a filename, or a count.
    pub(crate) fn title(&self) -> String {
        match self.paths.as_slice() {
            [one] => crate::panes::title_of(one).to_owned(),
            many => sk_model::i18n::plural(
                "properties.items",
                many.len() as u64,
                &[&sk_ui::format::grouped(many.len())],
            ),
        }
    }

    /// The rows, built from whatever has arrived.
    pub(crate) fn rows(&self, now: i64) -> Vec<Row> {
        fn label(key: &'static str) -> String {
            sk_model::i18n::lookup(key).to_owned()
        }
        let mut rows = Vec::new();
        if let Some(facts) = self.only() {
            rows.push(Row::stated(label("properties.kind"), label(facts.kind.label_key())));
            if let Some(target) = &facts.target {
                rows.push(Row::stated(label("properties.target"), target.display().to_string()));
            }
            rows.push(Row::stated(label("properties.where"), self.parent()));
            if let Some(original) = &facts.original {
                rows.push(Row::stated(
                    label("properties.original"),
                    original.display().to_string(),
                ));
            }
            rows.push(Row::stated(label("properties.size"), self.size_line(facts)));
            rows.push(Row::stated(
                label("properties.modified"),
                sk_ui::format::modified(facts.modified, now),
            ));
            if let Some(created) = facts.created {
                rows.push(Row::stated(
                    label("properties.created"),
                    sk_ui::format::modified(created, now),
                ));
            }
            rows.push(Row::stated(
                label("properties.accessed"),
                sk_ui::format::modified(facts.accessed, now),
            ));
            // **The same sentence whether or not the bits are controls.** It is
            // what the diagnostic report and the accessibility tree read, and a
            // row that said one thing when it could be changed and another when
            // it could not would be two rows to test.
            rows.push(Row {
                label: label("properties.permissions"),
                value: format!("{} ({:04o})", sk_fs::facts::permissions(facts.mode), facts.mode),
                bits: self.editable(),
            });
            rows.push(Row::stated(label("properties.owner"), self.owner_line(facts)));
            if facts.links > 1 {
                rows.push(Row::stated(
                    label("properties.links"),
                    sk_ui::format::grouped(usize::try_from(facts.links).unwrap_or(0)),
                ));
            }
            if let Some(filesystem) = facts.filesystem {
                rows.push(Row::stated(label("properties.filesystem"), filesystem.to_owned()));
            }
            return rows;
        }

        let folders = self.facts.iter().filter(|it| it.kind == Kind::Directory).count();
        let files = self.facts.len() - folders;
        // Two rows rather than one sentence: "1 folders, 2 files" is what a
        // sentence with no plural machinery behind it says, and plural machinery
        // for one string is a translation format this program does not have.
        rows.push(Row::stated(label("properties.folders"), sk_ui::format::grouped(folders)));
        rows.push(Row::stated(label("properties.files"), sk_ui::format::grouped(files)));
        rows.push(Row::stated(label("properties.where"), self.parent()));
        let bytes: u64 = self.facts.iter().map(|it| it.size).sum();
        rows.push(Row::stated(label("properties.size"), sk_ui::format::size(bytes)));
        rows
    }

    /// Who owns it: the names where the name service knew them, the numbers
    /// where it did not.
    ///
    /// **Per side, not all or nothing.** A file written inside a container has an
    /// owner nothing on this machine can name and a group it can, and `1000:disk`
    /// says more than either half dropped would.
    fn owner_line(&self, facts: &Facts) -> String {
        let known = self.accounts.as_ref().filter(|it| it.uid == facts.uid && it.gid == facts.gid);
        let named = |name: Option<&String>, number: u32| {
            name.cloned().unwrap_or_else(|| number.to_string())
        };
        let owner = named(known.and_then(|it| it.owner.as_ref()), facts.uid);
        let group = named(known.and_then(|it| it.group.as_ref()), facts.gid);
        format!("{owner}:{group}")
    }

    /// The size line: the total for a folder, the bytes for a file, and what it
    /// occupies where that differs by enough to be worth saying.
    fn size_line(&self, facts: &Facts) -> String {
        if facts.kind == Kind::Directory {
            return match self.total {
                Some(bytes) => sk_ui::format::size(bytes),
                None => sk_model::i18n::lookup("properties.measuring").to_owned(),
            };
        }
        let size = sk_ui::format::size(facts.size);
        // **Only the surprising cases.** Every file occupies a whole number of
        // blocks, so a byte count and an allocation almost always differ a
        // little and saying so every time is noise. What is worth a line is a
        // file that occupies *less* than it claims — sparse, or on a compressing
        // filesystem — or more than a block's slack, which means somebody
        // reserved space.
        let block = 4096;
        let sparse = facts.on_disk + block < facts.size;
        let reserved = facts.on_disk > facts.size + block;
        if sparse || reserved {
            return sk_model::i18n::format(
                "properties.size_on_disk",
                &[&size, &sk_ui::format::size(facts.on_disk)],
            );
        }
        size
    }

    /// The folder everything in the panel is in, or the count of folders it is
    /// spread across.
    fn parent(&self) -> String {
        let mut parents = self.paths.iter().filter_map(|path| path.parent());
        let Some(first) = parents.next() else { return String::new() };
        if parents.all(|it| it == first) {
            return first.display().to_string();
        }
        sk_model::i18n::lookup("properties.several_folders").to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{BITS, Properties};
    use sk_fs::facts::{Accounts, Facts, Kind};
    use std::path::PathBuf;

    /// A file whose facts have arrived, as a panel holds them.
    fn one(kind: Kind, mode: u16) -> Facts {
        Facts { kind, mode, uid: 1000, gid: 1000, size: 10, on_disk: 4096, ..Facts::default() }
    }

    /// A panel over one path whose bits this process may set.
    fn owned(facts: Facts) -> Properties {
        let mut panel = Properties::new(vec![PathBuf::from("/tmp/thing")]);
        panel.writable = true;
        panel.facts.push(facts);
        panel
    }

    fn facts(kind: Kind, size: u64, on_disk: u64) -> Facts {
        Facts { kind, size, on_disk, mode: 0o644, modified: 1, accessed: 1, ..Facts::default() }
    }

    #[test]
    fn one_file_names_its_kind_its_size_and_its_mode() {
        let mut panel = Properties::new(vec![PathBuf::from("/tmp/thing.txt")]);
        panel.facts.push(facts(Kind::File, 4096, 4096));
        let rows = panel.rows(1_000_000);
        let value =
            |label: &str| rows.iter().find(|row| row.label == label).map(|row| row.value.clone());
        assert_eq!(value("Where"), Some("/tmp".to_owned()));
        assert_eq!(value("Permissions"), Some("rw-r--r-- (0644)".to_owned()));
        assert!(value("Size").is_some_and(|it| it.contains('4')));
        assert_eq!(panel.title(), "thing.txt");
    }

    /// A 40-byte file occupies a block, and saying so every time is noise. A
    /// sparse file is the case the line exists for.
    /// Every file occupies a whole number of blocks, so a byte count and an
    /// allocation nearly always differ a little. What is worth a line is a file
    /// that occupies *less* than it claims.
    #[test]
    fn what_a_file_occupies_is_shown_only_when_it_is_surprising() {
        let quiet = |size, on_disk| {
            let mut panel = Properties::new(vec![PathBuf::from("/tmp/thing")]);
            panel.facts.push(facts(Kind::File, size, on_disk));
            let rows = panel.rows(1);
            let row = rows.into_iter().find(|row| row.label == "Size").expect("a size row");
            !row.value.contains('(')
        };
        assert!(quiet(40, 4096), "a small file in a block said so");
        assert!(quiet(4096, 4096), "an exact block said so");
        assert!(quiet(5000, 8192), "a block of slack said so");
        assert!(!quiet(1_073_741_824, 12_288), "a sparse gigabyte hid what it occupies");
        assert!(!quiet(1024, 1_048_576), "a reserved megabyte hid what it occupies");
    }

    #[test]
    fn a_folder_waits_for_its_total_rather_than_reporting_the_directory_entry() {
        let mut panel = Properties::new(vec![PathBuf::from("/tmp/folder")]);
        panel.facts.push(facts(Kind::Directory, 4096, 4096));
        let measuring = panel.rows(1);
        let size = measuring.iter().find(|row| row.label == "Size").expect("a size row");
        assert_eq!(size.value, "Measuring…");
        panel.total = Some(123_456);
        let done = panel.rows(1);
        let size = done.iter().find(|row| row.label == "Size").expect("a size row");
        assert_eq!(size.value, "121 KiB", "the total replaced nothing");
    }

    #[test]
    fn several_items_are_a_summary_and_not_a_list() {
        let mut panel = Properties::new(vec![
            PathBuf::from("/tmp/one"),
            PathBuf::from("/tmp/two"),
            PathBuf::from("/other/three"),
        ]);
        panel.facts.push(facts(Kind::Directory, 4096, 4096));
        panel.facts.push(facts(Kind::File, 100, 512));
        panel.facts.push(facts(Kind::File, 200, 512));
        assert_eq!(panel.title(), "3 items");
        assert!(panel.only().is_none());
        let rows = panel.rows(1);
        let value =
            |label: &str| rows.iter().find(|row| row.label == label).map(|row| row.value.clone());
        assert_eq!(value("Folders"), Some("1".to_owned()));
        assert_eq!(value("Files"), Some("2".to_owned()));
        assert_eq!(value("Where"), Some("Several folders".to_owned()));
    }

    #[test]
    fn the_owner_is_named_once_the_name_service_answers_and_numbered_until_then() {
        let mut panel = Properties::new(vec![PathBuf::from("/tmp/thing.txt")]);
        panel.facts.push(one(Kind::File, 0o644));
        let owner = |panel: &Properties| {
            panel
                .rows(1)
                .into_iter()
                .find(|row| row.label == "Owner")
                .map(|row| row.value)
                .expect("an owner row")
        };
        assert_eq!(owner(&panel), "1000:1000", "the numbers stand until the names arrive");
        panel.accounts =
            Some(Accounts { uid: 1000, gid: 1000, owner: Some("ada".to_owned()), group: None });
        assert_eq!(owner(&panel), "ada:1000", "a name nobody knows is still a number");
        // An answer about somebody else is not an answer about this file: the
        // panel can be reopened over a second path before the first replies.
        panel.accounts = Some(Accounts {
            uid: 7,
            gid: 7,
            owner: Some("nobody".to_owned()),
            group: Some("nobody".to_owned()),
        });
        assert_eq!(owner(&panel), "1000:1000");
    }

    #[test]
    fn the_nine_bits_are_controls_only_where_a_chmod_could_land() {
        assert_eq!(owned(one(Kind::File, 0o644)).editable(), Some(0o644));
        assert_eq!(owned(one(Kind::Directory, 0o755)).editable(), Some(0o755));
        assert_eq!(
            owned(one(Kind::Symlink, 0o777)).editable(),
            None,
            "a symlink's bits are the target's"
        );
        let mut theirs = owned(one(Kind::File, 0o644));
        theirs.writable = false;
        assert_eq!(theirs.editable(), None, "somebody else's file is a statement");
        let mut several =
            Properties::new(vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]);
        several.writable = true;
        several.facts.push(one(Kind::File, 0o644));
        several.facts.push(one(Kind::File, 0o644));
        assert_eq!(several.editable(), None, "a selection is not a recursive apply");
    }

    /// The order is `rwxrwxrwx`: the first box is the owner's read and the last
    /// is everyone else's execute.
    #[test]
    fn a_box_toggles_the_bit_under_it_and_nothing_above_the_nine() {
        let panel = owned(one(Kind::File, 0o4644));
        assert_eq!(panel.toggled(0), Some(0o4244), "the owner's read");
        assert_eq!(panel.toggled(7), Some(0o4646), "everyone else's write");
        assert_eq!(panel.toggled(BITS), None, "there is no tenth bit here");
    }

    #[test]
    fn the_keyboard_lands_on_a_bit_and_stops_at_either_end() {
        let mut panel = owned(one(Kind::File, 0o644));
        assert_eq!(panel.aim, None, "nothing is aimed at until a key asks");
        panel.aim_next(true);
        assert_eq!(panel.aim, Some(0));
        panel.aim_next(false);
        assert_eq!(panel.aim, Some(0), "the first box is the left end");
        for _ in 0..20 {
            panel.aim_next(true);
        }
        assert_eq!(panel.aim, Some(BITS - 1), "the last box is the right end");
    }

    /// A panel over somebody else's file has nothing to move the keyboard
    /// between, so an arrow means what every other key means there.
    #[test]
    fn a_panel_with_no_controls_takes_no_aim() {
        let mut panel = owned(one(Kind::Symlink, 0o777));
        panel.aim_next(true);
        assert_eq!(panel.aim, None);
    }
}
