// SPDX-License-Identifier: Apache-2.0
//! Icon themes: `index.theme`, the freedesktop search, and a path.
//!
//! **[ADR 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).**
//! This answers with a *file* and nothing else. It opens no image, spawns no
//! process and knows nothing about an atlas — the crate that turns a path into
//! pixels is `sk-preview`, which is level 2 like this one and therefore invisible
//! to it. `sk-app` is the caller that joins them.
//!
//! # Two answers, not one
//!
//! A theme that could not be loaded is a [`Refused`]; a name the loaded theme
//! does not have is [`Theme::find`] answering `None`. They are different states
//! reached from different code because a person who asked for our own marks and
//! a machine with a broken `index.theme` must not be told the same thing.
//!
//! # Why the directories are walked once
//!
//! The specification's search is a `stat` per subdirectory, base directory and
//! extension, and a miss walks all of them. One `getdents64` per directory at
//! load turns every later lookup into a hash and a size comparison. That load,
//! whole inheritance chain and `hicolor` included, is **51.9 ms for breeze,
//! 24.8 ms for Yaru, 15.4 ms for Adwaita and 13.6 ms for `hicolor` alone** — best
//! of five warm, release build, over the eight icon directories this machine's
//! `XDG_DATA_DIRS` names. Every one of those is over a frame budget, which is why
//! ADR 0042 puts the load on the job pool and nothing on rule 1's start-up list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Why a theme could not be loaded.
///
/// **Not the same as a name the theme does not hold.** This is the state ADR
/// 0042 requires to be reachable only when somebody asked for a theme and did not
/// get one; a miss inside a loaded theme is [`Theme::find`] answering nothing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Nothing in any icon directory declares a theme by that name.
    Unknown,
    /// Its `index.theme` is there, and neither it nor any theme it inherits —
    /// short of the `hicolor` every chain ends at — names a directory this build
    /// can search.
    Empty,
}

/// The theme every theme falls through to, which the specification requires to
/// exist.
pub const FALLBACK: &str = "hicolor";

/// How far the inheritance chain is followed.
///
/// **A bound rather than a cycle check alone.** A theme may inherit a theme that
/// inherits it back, and a chain of a hundred themes is a packaging accident
/// rather than a configuration anybody made — both end here rather than in a
/// walk that does not finish.
const MOST_INHERITED: usize = 32;

/// Where themes are installed, most specific first.
///
/// `$HOME/.icons` — which the specification keeps for compatibility and which
/// several desktops still write — then `icons` under each of [`data_dirs`]. The
/// first hit wins, which is how somebody's own theme overrides a distribution's.
///
/// [`data_dirs`]: crate::data_dirs
#[must_use]
pub fn icon_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(5);
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|it| it.is_absolute()) {
        dirs.push(home.join(".icons"));
    }
    dirs.extend(crate::data_dirs().into_iter().map(|dir| dir.join("icons")));
    dirs
}

/// The file extensions the specification names, in the order it searches them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Ext {
    Png,
    Svg,
    Xpm,
}

impl Ext {
    fn of(name: &str) -> Option<(&str, Self)> {
        let (stem, ext) = name.rsplit_once('.')?;
        let ext = match ext {
            "png" => Self::Png,
            "svg" => Self::Svg,
            "xpm" => Self::Xpm,
            _ => return None,
        };
        (!stem.is_empty()).then_some((stem, ext))
    }

    const fn suffix(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
            Self::Xpm => "xpm",
        }
    }
}

/// What a directory's `Type` key says about the sizes it serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sizing {
    /// Exactly `size`.
    Fixed,
    /// Anything between the two, inclusive.
    Scalable { min: u32, max: u32 },
    /// Within `slack` of `size`, which is the specification's default.
    Threshold { slack: u32 },
}

/// One directory the search walks: where it is, and what it serves.
struct Directory {
    /// Which theme in the chain it belongs to.
    ///
    /// **The search is per theme, not over a flattened list.** The specification
    /// recurses into a parent only once the child has answered nothing at all, so
    /// a badly-sized icon in the theme somebody chose beats a perfectly-sized one
    /// in the theme it inherits.
    theme: u16,
    path: PathBuf,
    size: u32,
    scale: u32,
    sizing: Sizing,
}

impl Directory {
    /// Whether it serves this size at this scale exactly — the specification's
    /// `DirectoryMatchesSize`.
    fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.sizing {
            Sizing::Fixed => self.size == size,
            Sizing::Scalable { min, max } => (min..=max).contains(&size),
            Sizing::Threshold { slack } => {
                self.size.saturating_sub(slack) <= size && size <= self.size.saturating_add(slack)
            }
        }
    }

    /// How far it is from this size — `DirectorySizeDistance`, in device pixels.
    fn distance(&self, size: u32, scale: u32) -> u32 {
        let wanted = size.saturating_mul(scale);
        let span = |low: u32, high: u32| {
            let low = low.saturating_mul(self.scale);
            let high = high.saturating_mul(self.scale);
            if wanted < low { low - wanted } else { wanted.saturating_sub(high) }
        };
        match self.sizing {
            Sizing::Fixed => self.size.saturating_mul(self.scale).abs_diff(wanted),
            Sizing::Scalable { min, max } => span(min, max),
            Sizing::Threshold { slack } => {
                span(self.size.saturating_sub(slack), self.size.saturating_add(slack))
            }
        }
    }
}

/// Which directory holds a name, and under which extension.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Held {
    dir: u32,
    ext: Ext,
}

/// A loaded theme and everything it inherits, ready to answer names.
pub struct Theme {
    /// What was asked for, which is what the settings panel says.
    name: String,
    /// How many themes are in the chain, including `hicolor`.
    themes: u16,
    /// Every searchable directory, in chain order and then in `Directories`
    /// order.
    dirs: Vec<Directory>,
    /// Every icon name this theme can draw, and where each one is.
    names: HashMap<Box<str>, Vec<Held>>,
}

/// Its name and its two counts.
///
/// **Not the derive.** A derived `Debug` over breeze prints 19,853 paths, which
/// is what a failing assertion would put on a terminal.
impl std::fmt::Debug for Theme {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{} ({} names in {} directories)", self.name, self.names.len(), self.dirs.len())
    }
}

impl Theme {
    /// Loads `name` and everything it inherits, ending at `hicolor`.
    ///
    /// **A directory walk, and it belongs on a job.** 51.9 ms for breeze and
    /// 15.4 ms for Adwaita — the numbers at the top of this file.
    ///
    /// # Errors
    ///
    /// [`Refused::Unknown`] where nothing declares a theme by that name, and
    /// [`Refused::Empty`] where the file is there and names no directories.
    pub fn load(name: &str) -> Result<Self, Refused> {
        Self::load_from(name, &icon_dirs())
    }

    /// The same, abandoning the walk when `cancel` is tripped.
    ///
    /// **Because holding `→` on the icons row queues one of these per repeat**,
    /// and each is the 51.9 ms and 5,200 directory opens the head of this file
    /// measures. A walk nobody wants the answer to should stop walking.
    ///
    /// # Errors
    ///
    /// As [`Self::load`]; a cancelled walk reports what it had, which is
    /// [`Refused::Empty`] unless the theme's own directories came first.
    pub fn load_cancellably(
        name: &str,
        bases: &[PathBuf],
        cancel: &sk_util::Cancel,
    ) -> Result<Self, Refused> {
        Self::within(name, bases, cancel)
    }

    /// The same, over base directories the caller names.
    ///
    /// **The seam the tests use.** Pointing `XDG_DATA_DIRS` at a fixture instead
    /// would change it for every other test in this process too, which is how a
    /// MIME test three modules away started reporting a PNG as unknown.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_from(name: &str, bases: &[PathBuf]) -> Result<Self, Refused> {
        Self::within(name, bases, &sk_util::Cancel::new())
    }

    fn within(name: &str, bases: &[PathBuf], cancel: &sk_util::Cancel) -> Result<Self, Refused> {
        // **Each `index.theme` is read once.** The chain walk reads them all to
        // follow `Inherits`, and re-reading them to follow `Directories` would be
        // a second pass over `hicolor`'s 55,507 bytes for every theme that
        // inherits it, which is all of them.
        let chain = inheritance(name, bases);
        if chain.first().is_none_or(|first| first.declared.is_empty()) {
            return Err(Refused::Unknown);
        }
        let mut theme =
            Self { name: name.to_owned(), themes: 0, dirs: Vec::new(), names: HashMap::new() };
        for (step, link) in chain.iter().enumerate() {
            let Ok(step) = u16::try_from(step) else { break };
            theme.themes = step + 1;
            for group in searchable(link, bases) {
                if cancel.is_cancelled() {
                    break;
                }
                theme.take(step, group);
            }
        }
        // **The theme that was asked for and what it inherits, but not the
        // `hicolor` every chain ends at.** That one is always there and usually
        // has files, so counting it would call an empty theme loaded and then draw
        // `hicolor` under its name. Counting only the theme itself refused the
        // other common shape: a theme that is nothing *but* an inheritance —
        // `/usr/share/icons/default` is `Inherits=Adwaita` and not one line more.
        let own = |dir: &Directory| {
            dir.theme == 0
                || chain.get(usize::from(dir.theme)).is_some_and(|link| link.name != FALLBACK)
        };
        if theme.dirs.iter().any(own) { Ok(theme) } else { Err(Refused::Empty) }
    }

    /// The name that was asked for.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many files it can draw, for the diagnostic line.
    #[must_use]
    pub fn holds(&self) -> usize {
        self.names.len()
    }

    /// The file to draw `name` at `size` logical pixels and `scale`, or nothing.
    ///
    /// **Nothing is a miss, not a failure.** The theme is loaded and this name is
    /// not in it, so the caller draws its own mark and the next name may still
    /// hit — which is the distinction ADR 0042 asks for by name.
    #[must_use]
    pub fn find(&self, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        let held = self.names.get(name)?;
        for theme in 0..self.themes {
            let mine = || held.iter().copied().filter(move |it| self.dir(*it).theme == theme);
            if let Some(hit) = mine().find(|it| self.dir(*it).matches(size, scale)) {
                return Some(self.file(hit, name));
            }
            if let Some(nearest) = mine().min_by_key(|it| self.dir(*it).distance(size, scale)) {
                return Some(self.file(nearest, name));
            }
        }
        None
    }

    fn dir(&self, held: Held) -> &Directory {
        // The index came from `take`, which pushed the directory it names.
        #[expect(clippy::indexing_slicing, reason = "held is minted beside its directory")]
        &self.dirs[held.dir as usize]
    }

    fn file(&self, held: Held, name: &str) -> PathBuf {
        self.dir(held).path.join(format!("{name}.{}", held.ext.suffix()))
    }

    /// Records one directory and everything in it.
    ///
    /// **One entry per name per directory, not one per file.** The specification
    /// searches `png`, then `svg`, then `xpm` inside a directory and stops at the
    /// first that exists, so the other two are never reachable — and keeping them
    /// would mean sorting every list each time a directory landed.
    fn take(&mut self, theme: u16, group: Group) {
        let Ok(index) = u32::try_from(self.dirs.len()) else { return };
        // A directory a theme declares and nothing installed is the common case —
        // 650 declarations across eight base directories, of which a few dozen
        // exist — so the miss is the read failing rather than a `stat` first.
        let Ok(found) = sk_fs::names(&group.path, &sk_util::Cancel::new()) else { return };
        let mut here: HashMap<Box<str>, Ext> = HashMap::new();
        for name in found.names.iter() {
            let Some(name) = name.to_str() else { continue };
            let Some((stem, ext)) = Ext::of(name) else { continue };
            here.entry(stem.into()).and_modify(|had| *had = (*had).min(ext)).or_insert(ext);
        }
        if here.is_empty() {
            return;
        }
        for (stem, ext) in here {
            // Directories arrive in search order, so a name's list is already in
            // it and never needs sorting.
            self.names.entry(stem).or_default().push(Held { dir: index, ext });
        }
        self.dirs.push(Directory {
            theme,
            path: group.path,
            size: group.size,
            scale: group.scale,
            sizing: group.sizing,
        });
    }
}

/// One directory named by an `index.theme`, once it has been found on disk.
struct Group {
    path: PathBuf,
    size: u32,
    scale: u32,
    sizing: Sizing,
}

/// Every `index.theme` declaring `name`, most specific first.
fn declarations(name: &str, bases: &[PathBuf]) -> Vec<String> {
    if !plain(name) {
        return Vec::new();
    }
    bases.iter().filter_map(|base| crate::read(&base.join(name).join("index.theme"))).collect()
}

/// One theme in the chain, and the `index.theme` files that declared it.
struct Link {
    name: String,
    declared: Vec<String>,
}

/// The themes to search, in order, ending at `hicolor`, each with what it said.
///
/// Breadth first through `Inherits`, which is the order the specification's
/// recursion visits them in, with a theme never visited twice. The files are
/// kept rather than re-read: see [`Theme::within`].
fn inheritance(name: &str, bases: &[PathBuf]) -> Vec<Link> {
    let mut chain = vec![Link { name: name.to_owned(), declared: declarations(name, bases) }];
    let mut at = 0;
    while at < chain.len() && chain.len() < MOST_INHERITED {
        let mut found: Vec<String> = Vec::new();
        {
            let Some(link) = chain.get(at) else { break };
            for text in &link.declared {
                let index = Ini::of(text);
                let inherits = index.value("Icon Theme", "Inherits").unwrap_or_default();
                for parent in inherits.split(',') {
                    let parent = parent.trim();
                    if plain(parent) && !found.iter().any(|had| had == parent) {
                        found.push(parent.to_owned());
                    }
                }
            }
        }
        at += 1;
        for parent in found {
            if !chain.iter().any(|had| had.name == parent) {
                chain.push(Link { declared: declarations(&parent, bases), name: parent });
            }
        }
    }
    if !chain.iter().any(|had| had.name == FALLBACK) {
        chain.push(Link { name: FALLBACK.to_owned(), declared: declarations(FALLBACK, bases) });
    }
    chain
}

/// The directories one theme declares, in `Directories` order.
///
/// **Every base directory, without asking whether it is there.** The
/// specification's directory list is a union over the base directories, and a
/// `stat` before the `readdir` is a second syscall to learn what the first one
/// reports anyway — 5,200 of them for `hicolor`'s 650 directories across eight
/// bases. [`Theme::take`] drops the ones that do not open.
fn searchable(link: &Link, bases: &[PathBuf]) -> Vec<Group> {
    let mut out = Vec::new();
    let Some(text) = link.declared.first() else { return out };
    let theme = link.name.as_str();
    let index = Ini::of(text);
    let listed = ["Directories", "ScaledDirectories"]
        .into_iter()
        .filter_map(|key| index.value("Icon Theme", key))
        .flat_map(|list| list.split(','))
        .map(str::trim);
    let mut seen: Vec<&str> = Vec::new();
    for subdir in listed {
        if subdir.is_empty() || seen.contains(&subdir) || !under(subdir) {
            continue;
        }
        seen.push(subdir);
        let Some((size, scale, sizing)) = describe(&index, subdir) else { continue };
        out.extend(bases.iter().map(|base| Group {
            path: base.join(theme).join(subdir),
            size,
            scale,
            sizing,
        }));
    }
    out
}

/// What one directory group says about the size it serves.
fn describe(index: &Ini<'_>, subdir: &str) -> Option<(u32, u32, Sizing)> {
    let number = |key: &str| index.value(subdir, key).and_then(|it| it.trim().parse::<u32>().ok());
    let size = number("Size")?;
    if size == 0 {
        return None;
    }
    let scale = number("Scale").unwrap_or(1).max(1);
    let sizing = match index.value(subdir, "Type").map(str::trim) {
        Some("Fixed") => Sizing::Fixed,
        Some("Scalable") => Sizing::Scalable {
            min: number("MinSize").unwrap_or(size),
            max: number("MaxSize").unwrap_or(size).max(number("MinSize").unwrap_or(size)),
        },
        // Threshold is the specification's default, and 2 is its default slack.
        _ => Sizing::Threshold { slack: number("Threshold").unwrap_or(2) },
    };
    Some((size, scale, sizing))
}

/// A desktop-entry-format file, parsed once: `[Group]` headers, `Key=Value`
/// lines, `#` comments, and the first occurrence of a key winning.
///
/// **Once, rather than a scan per key.** `hicolor`'s `index.theme` is 55,507
/// bytes and declares 650 directories, each of which is asked six questions —
/// scanning from the top for every one of them is 214 MB of comparisons and
/// 150 ms of a load that the rest of this file measures in single digits.
struct Ini<'t> {
    groups: HashMap<&'t str, Vec<(&'t str, &'t str)>>,
}

impl<'t> Ini<'t> {
    fn of(text: &'t str) -> Self {
        let mut groups: HashMap<&'t str, Vec<(&'t str, &'t str)>> = HashMap::new();
        let mut here: Option<&'t str> = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(name) = line.strip_prefix('[').and_then(|it| it.strip_suffix(']')) {
                here = Some(name);
                groups.entry(name).or_default();
                continue;
            }
            let Some(group) = here else { continue };
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, found)) = line.split_once('=') {
                groups.entry(group).or_default().push((key.trim(), found));
            }
        }
        Self { groups }
    }

    fn value(&self, group: &str, key: &str) -> Option<&'t str> {
        let pairs = self.groups.get(group)?;
        pairs.iter().find(|(had, _)| *had == key).map(|(_, found)| *found)
    }
}

/// Whether a theme name is one path component and not a traversal.
///
/// **A file we did not write names this directory**, and the name is joined onto
/// an XDG path — so `..` is refused here rather than by whatever the join lands
/// on. The same rule the archive reader's destination has, for the same reason.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && !Path::new(name).is_absolute()
}

/// Whether a declared subdirectory stays under the theme.
///
/// `48x48/places` is two components, so this is `plain` applied to each of them.
fn under(subdir: &str) -> bool {
    !subdir.is_empty() && !subdir.starts_with('/') && subdir.split('/').all(plain)
}

/// The icon theme the desktop's own files say it is using.
///
/// **The files, not the portal.** `org.freedesktop.portal.Settings` is a D-Bus
/// call and `sk-desktop` is below the crate that holds the connection, so the
/// portal is asked in `sk-app` and this is what answers where no portal does —
/// which is most of a bare Sway session. GTK's `settings.ini` first because both
/// GNOME and the GTK-based desktops write it; `kdeglobals` after, for a Plasma
/// session that has no GTK configuration at all.
#[must_use]
pub fn from_the_desktop_files() -> Option<String> {
    let files = crate::config_dirs().into_iter().flat_map(|dir| {
        [
            (dir.join("gtk-4.0/settings.ini"), "Settings", "gtk-icon-theme-name"),
            (dir.join("gtk-3.0/settings.ini"), "Settings", "gtk-icon-theme-name"),
            (dir.join("kdeglobals"), "Icons", "Theme"),
        ]
    });
    for (path, group, key) in files {
        let Some(text) = crate::read(&path) else { continue };
        let Some(name) = Ini::of(&text).value(group, key) else { continue };
        let name = name.trim().trim_matches('"');
        if plain(name) {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Directory, Ext, Held, Ini, Refused, Sizing, Theme, describe, plain, under};
    use std::path::{Path, PathBuf};

    /// A theme tree on disk, so the search runs against directories rather than a
    /// mock: the specification's rules are about what `readdir` finds.
    ///
    /// **Its own base directory rather than `XDG_DATA_DIRS`.** Every test in a
    /// crate shares one process, and pointing that variable at a fixture is what
    /// made a MIME test three modules away report a PNG as unknown.
    struct Installed {
        root: PathBuf,
    }

    impl Installed {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("skerry-icons-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn load(&self, name: &str) -> Result<Theme, Refused> {
            Theme::load_from(name, &[self.root.join("icons")])
        }

        /// Where one theme's files are, which is what a found path is under.
        fn inside(&self, theme: &str) -> PathBuf {
            self.root.join("icons").join(theme)
        }

        fn theme(&self, name: &str, index: &str) {
            let dir = self.inside(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("index.theme"), index).unwrap();
        }

        fn icon(&self, theme: &str, subdir: &str, file: &str) {
            let dir = self.inside(theme).join(subdir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), [0u8; 4]).unwrap();
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn hicolor(at: &Installed) {
        at.theme(
            "hicolor",
            "[Icon Theme]\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("hicolor", "16x16/places", "folder.png");
        at.icon("hicolor", "16x16/places", "last-resort.png");
    }

    /// **A theme nobody installed is a refusal, and it is not a miss.** ADR 0042
    /// requires the two to be reachable from different code, and this is the half
    /// that says so from the load.
    #[test]
    fn a_theme_that_is_not_installed_is_refused() {
        let at = Installed::new("absent");
        hicolor(&at);
        assert_eq!(at.load("nothing-called-this").unwrap_err(), Refused::Unknown);
    }

    /// A name a loaded theme does not hold is nothing at all, which the caller
    /// draws its own mark for — and the next name may still hit.
    #[test]
    fn a_name_the_theme_does_not_hold_is_a_miss_and_not_a_refusal() {
        let at = Installed::new("miss");
        hicolor(&at);
        let theme = at.load("hicolor").unwrap();
        assert!(theme.find("not-an-icon-anybody-ships", 16, 1).is_none());
        assert!(theme.find("folder", 16, 1).is_some(), "the next name missed too");
    }

    /// The whole chain: a theme, what it inherits, and `hicolor` at the end —
    /// which the specification requires whether or not anything names it.
    #[test]
    fn inheritance_falls_through_to_hicolor_even_unnamed() {
        let at = Installed::new("chain");
        hicolor(&at);
        at.theme(
            "middle",
            "[Icon Theme]\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("middle", "16x16/places", "user-home.png");
        at.theme(
            "top",
            "[Icon Theme]\nInherits=middle\nDirectories=16x16/places\n\n\
             [16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("top", "16x16/places", "folder.png");

        let theme = at.load("top").unwrap();
        let found = theme.find("folder", 16, 1).unwrap();
        assert!(found.starts_with(at.inside("top")), "{found:?}");
        let inherited = theme.find("user-home", 16, 1).unwrap();
        assert!(inherited.starts_with(at.inside("middle")), "{inherited:?}");
        // `top` names neither `hicolor` nor anything that does.
        let terminal = theme.find("last-resort", 16, 1).unwrap();
        assert!(terminal.starts_with(at.inside("hicolor")), "{terminal:?}");
    }

    /// **A theme that has the name at all beats a parent that has it in the right
    /// size.** The specification recurses into a parent only once the child has
    /// answered nothing, which is a rule a flattened search gets backwards.
    #[test]
    fn a_wrong_size_in_the_chosen_theme_beats_a_right_one_in_its_parent() {
        let at = Installed::new("order");
        hicolor(&at);
        at.theme(
            "parent",
            "[Icon Theme]\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("parent", "16x16/places", "folder.png");
        at.theme(
            "child",
            "[Icon Theme]\nInherits=parent\nDirectories=64x64/places\n\n\
             [64x64/places]\nSize=64\nType=Fixed\n",
        );
        at.icon("child", "64x64/places", "folder.png");

        let theme = at.load("child").unwrap();
        let found = theme.find("folder", 16, 1).unwrap();
        assert!(found.starts_with(at.inside("child")), "{found:?}");
    }

    /// A theme inheriting itself, or a pair inheriting each other, loads rather
    /// than walking forever.
    #[test]
    fn a_cycle_in_inherits_ends() {
        let at = Installed::new("cycle");
        hicolor(&at);
        at.theme(
            "loop-a",
            "[Icon Theme]\nInherits=loop-b,loop-a\nDirectories=16x16/places\n\n\
             [16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("loop-a", "16x16/places", "folder.png");
        at.theme("loop-b", "[Icon Theme]\nInherits=loop-a\nDirectories=\n");
        assert!(at.load("loop-a").is_ok());
    }

    /// **`png` before `svg` before `xpm`**, which is the order the specification
    /// lists and the order a person notices when a theme ships both.
    #[test]
    fn the_extensions_are_searched_in_the_order_the_specification_names() {
        let at = Installed::new("ext");
        hicolor(&at);
        at.theme(
            "both",
            "[Icon Theme]\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("both", "16x16/places", "folder.svg");
        at.icon("both", "16x16/places", "folder.png");
        let theme = at.load("both").unwrap();
        assert_eq!(theme.find("folder", 16, 1).unwrap().extension().unwrap(), "png");
    }

    /// A theme with no size that matches still answers, with the nearest it has —
    /// `LookupIcon`'s second half, and what keeps a 24-pixel mark drawable in a
    /// theme that ships 22 and 32.
    #[test]
    fn a_size_nothing_serves_exactly_takes_the_nearest() {
        let at = Installed::new("nearest");
        hicolor(&at);
        at.theme(
            "steps",
            "[Icon Theme]\nDirectories=22x22/places,32x32/places\n\n\
             [22x22/places]\nSize=22\nType=Fixed\n\n[32x32/places]\nSize=32\nType=Fixed\n",
        );
        at.icon("steps", "22x22/places", "folder.png");
        at.icon("steps", "32x32/places", "folder.png");
        let theme = at.load("steps").unwrap();
        let found = theme.find("folder", 24, 1).unwrap();
        assert!(found.starts_with(at.inside("steps/22x22")), "{found:?}");
        let bigger = theme.find("folder", 30, 1).unwrap();
        assert!(bigger.starts_with(at.inside("steps/32x32")), "{bigger:?}");
    }

    /// **A scale is not a size.** A theme's `16x16@2x` art is drawn for a
    /// 16-pixel mark on a 200% screen and never for a 32-pixel one at 100%.
    #[test]
    fn a_scaled_directory_answers_only_its_own_scale() {
        let at = Installed::new("scale");
        hicolor(&at);
        at.theme(
            "twice",
            "[Icon Theme]\nDirectories=16x16/places\nScaledDirectories=16x16@2x/places\n\n\
             [16x16/places]\nSize=16\nType=Fixed\n\n\
             [16x16@2x/places]\nSize=16\nScale=2\nType=Fixed\n",
        );
        at.icon("twice", "16x16/places", "folder.png");
        at.icon("twice", "16x16@2x/places", "folder.png");
        let theme = at.load("twice").unwrap();
        assert!(theme.find("folder", 16, 2).unwrap().starts_with(at.inside("twice/16x16@2x")));
        assert!(theme.find("folder", 16, 1).unwrap().starts_with(at.inside("twice/16x16/")));
    }

    /// A theme that is nothing but an inheritance is the theme it inherits —
    /// `/usr/share/icons/default`'s shape, `Inherits=Adwaita` and no more.
    #[test]
    fn a_theme_that_only_inherits_is_the_theme_it_inherits() {
        let at = Installed::new("stub");
        hicolor(&at);
        at.theme(
            "base",
            "[Icon Theme]\nName=Base\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
        );
        at.icon("base", "16x16/places", "folder.svg");
        at.theme("stub", "[Icon Theme]\nInherits=base\n");
        let theme = at.load("stub").expect("a stub that inherits a real theme was refused");
        assert!(theme.find("folder", 16, 1).unwrap().starts_with(at.inside("base/16x16")));
    }

    /// A theme whose `index.theme` names nothing searchable is empty rather than
    /// unknown, because the two are different things to say to a person.
    #[test]
    fn a_theme_with_no_directories_is_empty_rather_than_unknown() {
        let at = Installed::new("empty");
        hicolor(&at);
        at.theme("hollow", "[Icon Theme]\nName=Hollow\n");
        assert_eq!(at.load("hollow").unwrap_err(), Refused::Empty);
    }

    /// **A name from a file we did not write never leaves its directory.** The
    /// theme name is joined onto an XDG path, so a traversal is refused here
    /// rather than by whatever the join lands on.
    #[test]
    fn a_theme_name_that_is_a_path_is_refused() {
        for name in ["../../etc", "/etc", "..", "", "a/b", "a\0b"] {
            assert!(!plain(name), "{name:?} was taken as a theme name");
        }
        assert!(plain("Adwaita") && plain("breeze-dark"));
        assert!(under("48x48/places") && !under("../48x48") && !under("/48x48"));
    }

    #[test]
    fn a_group_says_what_size_it_serves() {
        let text = "[Icon Theme]\nDirectories=a,b,c\n\n\
                    [a]\nSize=48\nType=Fixed\n\n\
                    [b]\nSize=48\nType=Scalable\nMinSize=16\nMaxSize=512\n\n\
                    [c]\nSize=22\n";
        let index = Ini::of(text);
        assert_eq!(describe(&index, "a"), Some((48, 1, Sizing::Fixed)));
        assert_eq!(describe(&index, "b"), Some((48, 1, Sizing::Scalable { min: 16, max: 512 })));
        // No `Type` at all is Threshold with a slack of 2, per the specification.
        assert_eq!(describe(&index, "c"), Some((22, 1, Sizing::Threshold { slack: 2 })));
        assert_eq!(describe(&index, "d"), None);
        assert_eq!(index.value("Icon Theme", "Directories"), Some("a,b,c"));
        assert_eq!(index.value("Icon Theme", "Inherits"), None);
        // A key outside every group, and a group that is not there at all.
        assert_eq!(Ini::of("stray=1\n[g]\nk=v\n").value("g", "k"), Some("v"));
        assert_eq!(Ini::of("[g]\nk=v\n").value("h", "k"), None);
    }

    #[test]
    fn a_directorys_distance_is_in_device_pixels() {
        let fixed = |size, scale| Directory {
            theme: 0,
            path: PathBuf::new(),
            size,
            scale,
            sizing: Sizing::Fixed,
        };
        assert_eq!(fixed(16, 1).distance(16, 1), 0);
        assert_eq!(fixed(32, 1).distance(16, 1), 16);
        // 16 at 200% is 32 device pixels, which a 32-pixel directory is nearer to
        // than a 16-pixel one — the reason scale is in the key at all.
        assert!(fixed(32, 1).distance(16, 2) < fixed(16, 1).distance(16, 2));
    }

    #[test]
    fn only_the_three_named_extensions_are_files() {
        assert_eq!(Ext::of("folder.png"), Some(("folder", Ext::Png)));
        assert_eq!(Ext::of("folder.svg"), Some(("folder", Ext::Svg)));
        assert_eq!(Ext::of("folder.xpm"), Some(("folder", Ext::Xpm)));
        assert_eq!(Ext::of("folder.ico"), None);
        assert_eq!(Ext::of(".png"), None);
        assert_eq!(Ext::of("folder"), None);
        assert!(Held { dir: 0, ext: Ext::Png } < Held { dir: 0, ext: Ext::Svg });
    }

    /// **Whatever this machine has installed**, against the names a file manager
    /// draws. A fixture proves the algorithm; only a real theme proves that
    /// `index.theme` as distributions actually write it parses — Adwaita and
    /// breeze disagree about `Type`, about `ScaledDirectories` and about whether
    /// the generic marks are `png` or `svg`.
    ///
    /// Skipped where no theme is installed, which is
    /// [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md)'s shape
    /// and not a pass by omission: the assertion below fails the build on any
    /// machine that has one.
    #[test]
    fn an_installed_theme_answers_the_names_a_file_manager_draws() {
        let bases = super::icon_dirs();
        for name in ["Adwaita", "breeze", "Yaru"] {
            let Ok(theme) = Theme::load_from(name, &bases) else { continue };
            assert!(theme.holds() > 0, "{name} loaded with nothing in it");
            let found = theme.find("folder", 16, 1);
            assert!(found.is_some(), "{name} has no folder at 16 pixels");
            for size in [16u32, 24, 32, 48] {
                for scale in [1u32, 2] {
                    let Some(path) = theme.find("folder", size, scale) else { continue };
                    assert!(path.is_file(), "{name} named {path:?}, which is not there");
                }
            }
        }
    }

    #[test]
    fn the_icon_directories_are_absolute_and_most_specific_first() {
        let dirs = super::icon_dirs();
        assert!(!dirs.is_empty());
        assert!(dirs.iter().all(|path| path.is_absolute()), "{dirs:?}");
        assert!(dirs.iter().all(|path| path.ends_with("icons") || path.ends_with(".icons")));
        assert!(Path::new("/usr/share/icons").is_absolute());
    }
}

/// **`index.theme` is a file we did not write** — ADR 0042 puts it on the fuzzing
/// list rather than outside it. A theme package is not a download directory, but
/// it is not ours either, and this parser reads every one installed on the
/// machine before the first icon is drawn.
///
/// The name and the subdirectory each go through [`plain`] and [`under`] on the
/// way to a path, so a mutated group header that is `../..` is refused here and
/// not by whatever the join lands on.
#[test]
fn parser_campaign_icons() {
    let seeds: &[&[u8]] = &[
        b"[Icon Theme]\nName=T\nInherits=a,b\nDirectories=16x16/places\n\n\
          [16x16/places]\nSize=16\nType=Fixed\n",
        b"[Icon Theme]\nDirectories=48x48/apps\nScaledDirectories=48x48@2x/apps\n\n\
          [48x48/apps]\nSize=48\nType=Scalable\nMinSize=1\nMaxSize=512\n\n\
          [48x48@2x/apps]\nSize=48\nScale=2\nThreshold=8\n",
        b"[Icon Theme]\n",
        &[0; 48],
    ];
    crate::fuzz_support::campaign("index.theme", seeds, |bytes| {
        let text = String::from_utf8_lossy(bytes);
        let index = Ini::of(&text);
        for group in ["Icon Theme", "16x16/places", "48x48@2x/apps", "", "\u{fffd}"] {
            let _ = index.value(group, "Directories");
            let _ = describe(&index, group);
        }
        // Every name that reaches a path goes through these two, so a mutated
        // header that is a traversal is refused before anything is joined.
        for name in index.value("Icon Theme", "Directories").unwrap_or_default().split(',') {
            let name = name.trim();
            assert!(!under(name) || !name.starts_with('/'), "{name:?} reached a join");
        }
        if let Some(inherits) = index.value("Icon Theme", "Inherits") {
            for name in inherits.split(',') {
                let name = name.trim();
                assert!(!plain(name) || (!name.contains('/') && name != ".."), "{name:?} joined");
            }
        }
    });
}
