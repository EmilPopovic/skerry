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

use sk_fs::facts::{Facts, Kind};

/// The panel, once something has been asked about.
pub(crate) struct Properties {
    /// What was selected when it opened.
    pub(crate) paths: Vec<PathBuf>,
    /// The facts for each, in the order they were asked for. Shorter than
    /// `paths` while they are still arriving.
    pub(crate) facts: Vec<Facts>,
    /// A recursive total for a single folder, once the measurer has one.
    pub(crate) total: Option<u64>,
}

/// One row of the panel: a label already looked up, and a value.
pub(crate) struct Row {
    pub(crate) label: String,
    pub(crate) value: String,
}

impl Properties {
    pub(crate) fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths, facts: Vec::new(), total: None }
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
            rows.push(Row {
                label: label("properties.kind"),
                value: label(facts.kind.label_key()),
            });
            if let Some(target) = &facts.target {
                rows.push(Row {
                    label: label("properties.target"),
                    value: target.display().to_string(),
                });
            }
            rows.push(Row { label: label("properties.where"), value: self.parent() });
            rows.push(Row { label: label("properties.size"), value: self.size_line(facts) });
            rows.push(Row {
                label: label("properties.modified"),
                value: sk_ui::format::modified(facts.modified, now),
            });
            if let Some(created) = facts.created {
                rows.push(Row {
                    label: label("properties.created"),
                    value: sk_ui::format::modified(created, now),
                });
            }
            rows.push(Row {
                label: label("properties.accessed"),
                value: sk_ui::format::modified(facts.accessed, now),
            });
            rows.push(Row {
                label: label("properties.permissions"),
                value: format!("{} ({:04o})", sk_fs::facts::permissions(facts.mode), facts.mode),
            });
            rows.push(Row {
                label: label("properties.owner"),
                value: format!("{}:{}", facts.uid, facts.gid),
            });
            if facts.links > 1 {
                rows.push(Row {
                    label: label("properties.links"),
                    value: sk_ui::format::grouped(usize::try_from(facts.links).unwrap_or(0)),
                });
            }
            if let Some(filesystem) = facts.filesystem {
                rows.push(Row {
                    label: label("properties.filesystem"),
                    value: filesystem.to_owned(),
                });
            }
            return rows;
        }

        let folders = self.facts.iter().filter(|it| it.kind == Kind::Directory).count();
        let files = self.facts.len() - folders;
        // Two rows rather than one sentence: "1 folders, 2 files" is what a
        // sentence with no plural machinery behind it says, and plural machinery
        // for one string is a translation format this program does not have.
        rows.push(Row {
            label: label("properties.folders"),
            value: sk_ui::format::grouped(folders),
        });
        rows.push(Row { label: label("properties.files"), value: sk_ui::format::grouped(files) });
        rows.push(Row { label: label("properties.where"), value: self.parent() });
        let bytes: u64 = self.facts.iter().map(|it| it.size).sum();
        rows.push(Row { label: label("properties.size"), value: sk_ui::format::size(bytes) });
        rows
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

    use super::Properties;
    use sk_fs::facts::{Facts, Kind};
    use std::path::PathBuf;

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
}
