// SPDX-License-Identifier: Apache-2.0
//! The colour somebody gave a folder.
//!
//! **A choice about one folder, and only that one.** The per-folder view records
//! in [`crate::views`] inherit down a subtree — setting a grid on `~/Pictures`
//! makes the albums under it grids — and a colour must not: colouring
//! `~/Projects` red so it stands out would turn every folder inside it red and
//! the colour would stop meaning anything. So the two live in the same file and
//! in different lists, and the difference is written down here because it is the
//! kind of thing a later reader would "fix".
//!
//! Six colours, from `Palette::folders`. What they *mean* is nobody's business
//! but the person's: Finder and Drive both ship a palette and no vocabulary, and
//! a file manager that named them "important" and "later" would be wrong for
//! everybody who meant something else.

use sk_ui::Palette;

/// One of the six.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(missing_docs, reason = "each variant is the colour of that name")]
pub enum Colour {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl Colour {
    /// Every colour, in the order they are offered.
    ///
    /// Spectrum order, so the menu reads as a palette rather than as a list of
    /// words in the order somebody typed them.
    pub const ALL: [Self; 6] =
        [Self::Red, Self::Orange, Self::Yellow, Self::Green, Self::Blue, Self::Purple];

    /// What it is written as in the file.
    ///
    /// **Never translated.** It is on disk, and a record whose meaning depends on
    /// the locale of the session that wrote it is a record another session cannot
    /// read — the same rule `copy` in a duplicated filename follows.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Purple => "purple",
        }
    }

    /// The colour a token names, if it names one this build has.
    ///
    /// A file written by a build with a seventh colour reads as no record for
    /// that folder, which is the same thing a file written before the feature
    /// existed reads as.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|colour| colour.token() == token)
    }

    /// The command that gives a folder this colour.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Red => "view.colour_red",
            Self::Orange => "view.colour_orange",
            Self::Yellow => "view.colour_yellow",
            Self::Green => "view.colour_green",
            Self::Blue => "view.colour_blue",
            Self::Purple => "view.colour_purple",
        }
    }

    /// Its catalogue key.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Red => "action.colour_red",
            Self::Orange => "action.colour_orange",
            Self::Yellow => "action.colour_yellow",
            Self::Green => "action.colour_green",
            Self::Blue => "action.colour_blue",
            Self::Purple => "action.colour_purple",
        }
    }

    /// What it is drawn in.
    #[must_use]
    pub const fn ink(self, palette: &Palette) -> sk_gpu::Color {
        let folders = palette.folders;
        match self {
            Self::Red => folders.red,
            Self::Orange => folders.orange,
            Self::Yellow => folders.yellow,
            Self::Green => folders.green,
            Self::Blue => folders.blue,
            Self::Purple => folders.purple,
        }
    }
}

/// The command that takes a folder's colour away.
pub const NONE: &str = "view.colour_none";

/// The colour a command sets, if it is one of the seven.
///
/// **Read by the menu**, which draws each row's swatch from it: a list of colour
/// names with no colours on it is a menu a person has to try rather than read,
/// and the swatch is the whole difference. `None` for every other command, and
/// for the one that clears — which has no colour by definition.
#[must_use]
pub fn of_command(id: &str) -> Option<Colour> {
    Colour::ALL.into_iter().find(|colour| colour.command() == id)
}

#[cfg(test)]
mod tests {
    use super::{Colour, NONE, of_command};

    /// Every colour has a token, a command and a name of its own, and the
    /// tokens round-trip.
    #[test]
    fn each_colour_is_its_own_token_command_and_name() {
        let mut tokens: Vec<&str> = Colour::ALL.iter().map(|it| it.token()).collect();
        let count = tokens.len();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), count, "two colours share a token");

        for colour in Colour::ALL {
            assert_eq!(Colour::from_token(colour.token()), Some(colour));
            assert_eq!(of_command(colour.command()), Some(colour));
            assert!(colour.command().starts_with("view.colour_"));
            assert_ne!(colour.command(), NONE, "a colour claims the clearing command");
        }
        assert_eq!(Colour::from_token("magenta"), None, "a colour this build lacks");
        assert_eq!(of_command(NONE), None);
        assert_eq!(of_command("file.trash"), None);
    }
}
