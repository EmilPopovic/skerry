//! One list of every action, and the keymap over it.
//!
//! Generic over the context a command runs against, which is the whole trick
//! that lets a level-2 crate hold level-4 behaviour: `sk-app` instantiates
//! `Registry<App>` and the registry never learns what an `App` is.

use std::collections::HashMap;

use sk_model::i18n;
use sk_plat::KeyEvent;

use crate::chord::Chord;

/// Where a command belongs in a menu or a palette.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Group {
    /// Moving around: open, up, back, home.
    Navigate,
    /// Choosing things.
    Select,
    /// How the listing looks and is ordered.
    View,
    /// Acting on files.
    File,
    /// The window itself: minimise, maximise, close.
    ///
    /// A group of its own because it is the only one whose commands are not
    /// about files, and because on a desktop that draws no decorations these
    /// three *are* the title bar — so they have to be findable and bindable
    /// like everything else rather than living in a pointer handler.
    Window,
    /// Finding out what the program can do.
    Help,
}

impl Group {
    /// The catalogue key for this group's heading.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Navigate => "group.navigate",
            Self::Select => "group.select",
            Self::View => "group.view",
            Self::Window => "group.window",
            Self::File => "group.file",
            Self::Help => "group.help",
        }
    }
}

/// A command's position in the registry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommandId(usize);

/// One action, toggle or setting.
pub struct CommandSpec<C> {
    /// Stable identifier. This is what a config file and a keymap refer to, so
    /// it never changes once released, whatever the display name does.
    pub id: &'static str,
    /// Catalogue key for the display name.
    pub name: &'static str,
    /// Where it belongs.
    pub group: Group,
    /// Default bindings, as [`Chord::parse`] accepts them. More than one is
    /// allowed: `Backspace` and `Alt ↑` are both Parent because both are
    /// somebody's muscle memory.
    pub keys: &'static [&'static str],
    /// Whether it can run right now. A command that cannot is shown and dimmed
    /// rather than hidden, because rule 4 is about discovery.
    pub enabled: fn(&C) -> bool,
    /// What it does.
    pub run: fn(&mut C),
}

/// Every command, and the chords that reach them.
pub struct Registry<C> {
    specs: Vec<CommandSpec<C>>,
    by_id: HashMap<&'static str, usize>,
    bindings: Vec<(Chord, usize)>,
    /// Bindings that were refused, and why. Read by a test, not by the program.
    problems: Vec<String>,
}

impl<C> Default for Registry<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Registry<C> {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            specs: Vec::new(),
            by_id: HashMap::new(),
            bindings: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Adds a command and binds its default chords.
    ///
    /// Anything wrong — a duplicate id, an unparseable chord, a chord already
    /// taken — is recorded in [`Registry::problems`] rather than panicking, so a
    /// test can assert the whole set is sound in one place instead of the program
    /// dying on the one machine where a locale changed a key name.
    pub fn add(&mut self, spec: CommandSpec<C>) {
        let index = self.specs.len();
        if self.by_id.insert(spec.id, index).is_some() {
            self.problems.push(format!("`{}` is registered twice", spec.id));
        }
        for text in spec.keys {
            match Chord::parse(text) {
                None => self.problems.push(format!("`{}`: `{text}` is not a chord", spec.id)),
                Some(chord) => {
                    if let Some((_, taken)) =
                        self.bindings.iter().find(|(existing, _)| *existing == chord)
                    {
                        let other = self.specs.get(*taken).map_or("?", |spec| spec.id);
                        self.problems.push(format!("`{}`: `{text}` is already `{other}`", spec.id));
                    } else {
                        self.bindings.push((chord, index));
                    }
                }
            }
        }
        self.specs.push(spec);
    }

    /// Everything wrong with the registered set. Empty in a sound program.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// How many commands are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The command a key press reaches.
    #[must_use]
    pub fn resolve(&self, event: &KeyEvent) -> Option<CommandId> {
        self.bindings
            .iter()
            .find(|(chord, _)| chord.matches(event))
            .map(|(_, index)| CommandId(*index))
    }

    /// The command with this identifier.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<CommandId> {
        self.by_id.get(id).copied().map(CommandId)
    }

    /// Runs a command against `context`, if it is enabled.
    ///
    /// Returns whether it ran. A disabled command reached by its key does
    /// nothing, silently — the same as the menu item being dimmed.
    pub fn run(&self, id: CommandId, context: &mut C) -> bool {
        let Some(spec) = self.specs.get(id.0) else { return false };
        if !(spec.enabled)(context) {
            return false;
        }
        (spec.run)(context);
        true
    }

    /// One command's identifier, display name, keys and group.
    #[must_use]
    pub fn describe(&self, id: CommandId) -> Option<Described<'_>> {
        let spec = self.specs.get(id.0)?;
        Some(Described {
            id: spec.id,
            name: i18n::lookup(spec.name),
            group: spec.group,
            keys: self.keys_for(id.0),
        })
    }

    /// Every command, in registration order.
    ///
    /// This is the single list rule 2 is about: the keyboard overlay reads it,
    /// and the palette, the menus and the config schema will read the same one.
    pub fn iter(&self) -> impl Iterator<Item = Described<'_>> + '_ {
        (0..self.specs.len()).filter_map(|index| self.describe(CommandId(index)))
    }

    /// Whether a command can run right now.
    #[must_use]
    pub fn enabled(&self, id: CommandId, context: &C) -> bool {
        self.specs.get(id.0).is_some_and(|spec| (spec.enabled)(context))
    }

    fn keys_for(&self, index: usize) -> String {
        let mut out = String::new();
        for (chord, bound) in &self.bindings {
            if *bound != index {
                continue;
            }
            if !out.is_empty() {
                out.push_str("  ");
            }
            out.push_str(&chord.display());
        }
        out
    }
}

/// A command, ready to show to a person.
#[derive(Clone, Debug)]
pub struct Described<'a> {
    /// The stable identifier.
    pub id: &'static str,
    /// The display name, resolved through the catalogue.
    pub name: &'a str,
    /// Which group it belongs to.
    pub group: Group,
    /// Its keys, as a person reads them.
    pub keys: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{CommandSpec, Group, Registry};
    use sk_plat::{Key, KeyEvent, Modifiers, Named};

    #[derive(Default)]
    struct Context {
        count: usize,
        allowed: bool,
    }

    fn spec(id: &'static str, keys: &'static [&'static str]) -> CommandSpec<Context> {
        CommandSpec {
            id,
            name: "column.name",
            group: Group::Navigate,
            keys,
            enabled: |_| true,
            run: |context| context.count += 1,
        }
    }

    fn press(key: Key, mods: Modifiers) -> KeyEvent {
        KeyEvent { key, text: None, mods, repeat: false }
    }

    #[test]
    fn a_bound_chord_reaches_its_command_and_runs_it() {
        let mut registry = Registry::new();
        registry.add(spec("one", &["ctrl+a"]));
        assert!(registry.problems().is_empty());

        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        let id = registry.resolve(&press(Key::Character('a'), ctrl)).expect("resolved");
        let mut context = Context::default();
        assert!(registry.run(id, &mut context));
        assert_eq!(context.count, 1);
    }

    #[test]
    fn one_command_may_have_several_keys_and_shows_all_of_them() {
        let mut registry = Registry::new();
        registry.add(spec("parent", &["backspace", "alt+up"]));
        let id = registry.find("parent").expect("found");
        let described = registry.describe(id).expect("described");
        assert!(described.keys.contains("Backspace"), "{}", described.keys);
        assert!(described.keys.contains("Alt ↑"), "{}", described.keys);

        let alt = Modifiers { alt: true, ..Modifiers::default() };
        assert_eq!(registry.resolve(&press(Key::Named(Named::Up), alt)), Some(id));
        let plain = Modifiers::default();
        assert_eq!(registry.resolve(&press(Key::Named(Named::Backspace), plain)), Some(id));
    }

    /// A registry that silently drops a conflict is a registry where two
    /// features fight over a key and the loser looks broken.
    #[test]
    fn a_conflicting_binding_is_recorded_and_the_first_one_keeps_the_key() {
        let mut registry = Registry::new();
        registry.add(spec("first", &["ctrl+a"]));
        registry.add(spec("second", &["ctrl+a"]));
        assert_eq!(registry.problems().len(), 1, "{:?}", registry.problems());
        assert!(registry.problems()[0].contains("already"));
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert_eq!(registry.resolve(&press(Key::Character('a'), ctrl)), registry.find("first"));
    }

    #[test]
    fn a_duplicate_identifier_and_an_unparseable_chord_are_both_recorded() {
        let mut registry = Registry::new();
        registry.add(spec("one", &["ctrl+a"]));
        registry.add(spec("one", &["ctrl+b"]));
        registry.add(spec("two", &["notakey"]));
        assert_eq!(registry.problems().len(), 2, "{:?}", registry.problems());
    }

    #[test]
    fn a_disabled_command_does_not_run_and_says_so() {
        let mut registry = Registry::new();
        registry.add(CommandSpec {
            id: "guarded",
            name: "column.name",
            group: Group::File,
            keys: &["f8"],
            enabled: |context: &Context| context.allowed,
            run: |context| context.count += 1,
        });
        let id = registry.find("guarded").expect("found");
        let mut context = Context::default();
        assert!(!registry.run(id, &mut context));
        assert_eq!(context.count, 0);
        assert!(!registry.enabled(id, &context));

        context.allowed = true;
        assert!(registry.enabled(id, &context));
        assert!(registry.run(id, &mut context));
        assert_eq!(context.count, 1);
    }

    #[test]
    fn iterating_yields_every_command_once() {
        let mut registry = Registry::new();
        for id in ["a", "b", "c"] {
            registry.add(spec(id, &[]));
        }
        let ids: Vec<_> = registry.iter().map(|described| described.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(registry.len(), 3);
    }
}
