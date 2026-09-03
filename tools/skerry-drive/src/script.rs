//! The replay script: a line-oriented format with no cleverness in it.

/// Which pointer button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// The `linux/input-event-codes.h` code.
    pub fn code(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
        }
    }
}

/// One step of a script.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// One button, pressed or released on its own.
    Press {
        /// Which button.
        button: Button,
        /// Down or up.
        down: bool,
    },
    /// `modifiers` are evdev keycodes to hold; `mask` is the same set as the
    /// keymap's modifier bits.
    ///
    /// Both, because a virtual keyboard has to say so twice. Pressing the
    /// modifier's keycode is what a real keyboard does, and the protocol *also*
    /// has an explicit `modifiers` request — and wlroots does not derive the
    /// second from the first. Sending only the keycodes produces a `Ctrl+H` that
    /// arrives at the client as a plain `h`, which is how four of these tests
    /// failed the first time they ran.
    Key {
        modifiers: Vec<u32>,
        mask: u32,
        code: u32,
    },
    Move {
        x: u32,
        y: u32,
    },
    Click {
        button: Button,
        times: u32,
    },
    Drag {
        from: (u32, u32),
        to: (u32, u32),
    },
    Scroll {
        notches: i32,
        /// Which modifier is held across it, if any.
        ///
        /// **Two of them mean something to a wheel and no others do.** `Shift`
        /// is how a program is asked to scroll sideways, and `Ctrl` is how every
        /// image viewer is asked to zoom — including this one, where it is the
        /// only zoom gesture X11 can deliver at all.
        held: Held,
    },
    /// A two-finger touchpad gesture that ends with the fingers leaving the pad.
    ///
    /// `hold` is how long they rest before lifting, which is the difference
    /// between a flick and a drag that stopped — and the only way to test one
    /// against the other without asserting an absolute distance that whatever
    /// scroll factor the compositor applies would decide.
    Flick {
        pixels: i32,
        events: u32,
        hold: u32,
    },
    Sleep {
        milliseconds: u32,
    },
}

/// Parses a whole script, or says which line is wrong.
pub fn parse(source: &str) -> Result<Vec<Action>, String> {
    let mut actions = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(verb) = words.next() else { continue };
        let rest: Vec<&str> = words.collect();
        match step(verb, &rest) {
            Ok(mut produced) => actions.append(&mut produced),
            Err(problem) => return Err(format!("line {}: {problem}", number + 1)),
        }
    }
    Ok(actions)
}

fn step(verb: &str, rest: &[&str]) -> Result<Vec<Action>, String> {
    match verb {
        "key" => {
            let chord = rest.first().ok_or("key needs a chord")?;
            Ok(vec![chord_action(chord)?])
        }
        "text" => {
            let text = rest.join(" ");
            text.chars().map(character_action).collect()
        }
        "move" => {
            let (x, y) = two(rest)?;
            Ok(vec![Action::Move { x: percent(x)?, y: percent(y)? }])
        }
        "click" => {
            let button = rest.first().map_or(Ok(Button::Left), |name| button(name))?;
            Ok(vec![Action::Click { button, times: 1 }])
        }
        "double" => Ok(vec![Action::Click { button: Button::Left, times: 2 }]),
        // Press and release apart, so a drag can be paced. `drag` sends its
        // whole gesture at once, which is right for a rubber band inside one
        // window and wrong for a drag between two applications: the peer answers
        // each position and a gesture that outruns its replies is dropped before
        // it was ever accepted.
        "press" => {
            let button = rest.first().map_or(Ok(Button::Left), |name| button(name))?;
            Ok(vec![Action::Press { button, down: true }])
        }
        "release" => {
            let button = rest.first().map_or(Ok(Button::Left), |name| button(name))?;
            Ok(vec![Action::Press { button, down: false }])
        }
        "drag" => {
            if rest.len() != 4 {
                return Err("drag needs four numbers".into());
            }
            Ok(vec![Action::Drag {
                from: (percent(rest[0])?, percent(rest[1])?),
                to: (percent(rest[2])?, percent(rest[3])?),
            }])
        }
        "scroll" => {
            let amount = rest.first().ok_or("scroll needs a number")?;
            let notches: i32 = amount.parse().map_err(|_| format!("`{amount}` is not a number"))?;
            let held = match rest.get(1) {
                None => Held::Nothing,
                Some(&"shift") => Held::Shift,
                Some(&"ctrl") => Held::Ctrl,
                Some(other) => return Err(format!("`{other}` is not `shift` or `ctrl`")),
            };
            Ok(vec![Action::Scroll { notches, held }])
        }
        "flick" => {
            if rest.len() < 2 || rest.len() > 3 {
                return Err("flick needs pixels, events and optionally a hold".into());
            }
            let pixels = rest[0].parse().map_err(|_| format!("`{}` is not a number", rest[0]))?;
            let events = rest[1].parse().map_err(|_| format!("`{}` is not a number", rest[1]))?;
            let hold = match rest.get(2) {
                Some(word) => word.parse().map_err(|_| format!("`{word}` is not a number"))?,
                None => 0,
            };
            Ok(vec![Action::Flick { pixels, events, hold }])
        }
        "sleep" => {
            let amount = rest.first().ok_or("sleep needs milliseconds")?;
            let milliseconds = amount.parse().map_err(|_| format!("`{amount}` is not a number"))?;
            Ok(vec![Action::Sleep { milliseconds }])
        }
        other => Err(format!("`{other}` is not a step")),
    }
}

fn two<'a>(rest: &[&'a str]) -> Result<(&'a str, &'a str), String> {
    match rest {
        [x, y] => Ok((x, y)),
        _ => Err("needs two numbers".into()),
    }
}

/// A position as a percentage of the output, in the protocol's units.
/// What is held down while the wheel turns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Held {
    Nothing,
    Shift,
    Ctrl,
}

impl Held {
    /// The key to press and the mask to report, or nothing.
    pub(crate) const fn key(self) -> Option<(u32, u32)> {
        match self {
            Self::Nothing => None,
            Self::Shift => Some((EV_LEFTSHIFT, MOD_SHIFT)),
            Self::Ctrl => Some((EV_LEFTCTRL, MOD_CONTROL)),
        }
    }
}

fn percent(text: &str) -> Result<u32, String> {
    let value: f64 = text.parse().map_err(|_| format!("`{text}` is not a number"))?;
    if !(0.0..=100.0).contains(&value) {
        return Err(format!("`{text}` is not a percentage"));
    }
    Ok((value / 100.0 * f64::from(super::EXTENT)) as u32)
}

fn button(name: &str) -> Result<Button, String> {
    match name {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(format!("`{other}` is not a button")),
    }
}

fn chord_action(chord: &str) -> Result<Action, String> {
    let mut modifiers = Vec::new();
    let mut mask = 0;
    let mut code = None;
    for part in chord.split(['+', '-']).filter(|part| !part.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                modifiers.push(EV_LEFTCTRL);
                mask |= MOD_CONTROL;
            }
            "shift" => {
                modifiers.push(EV_LEFTSHIFT);
                mask |= MOD_SHIFT;
            }
            "alt" => {
                modifiers.push(EV_LEFTALT);
                mask |= MOD_ALT;
            }
            "super" | "logo" => {
                modifiers.push(EV_LEFTMETA);
                mask |= MOD_SUPER;
            }
            other => code = Some(keycode(other).ok_or(format!("`{other}` is not a key"))?),
        }
    }
    Ok(Action::Key { modifiers, mask, code: code.ok_or(format!("`{chord}` names no key"))? })
}

/// The upper half of a US keyboard: the character, and the key Shift produces it
/// from. A filename can hold any of them, so a harness that cannot type them
/// cannot test a rename.
const SHIFTED: [(char, &str); 21] = [
    ('~', "`"),
    ('!', "1"),
    ('@', "2"),
    ('#', "3"),
    ('$', "4"),
    ('%', "5"),
    ('^', "6"),
    ('&', "7"),
    ('*', "8"),
    ('(', "9"),
    (')', "0"),
    ('_', "-"),
    ('+', "="),
    ('{', "["),
    ('}', "]"),
    (':', ";"),
    ('"', "'"),
    ('|', "\\"),
    ('<', ","),
    ('>', "."),
    ('?', "/"),
];

fn character_action(character: char) -> Result<Action, String> {
    let shifted = if character.is_ascii_uppercase() {
        Some(character.to_ascii_lowercase().to_string())
    } else {
        SHIFTED
            .iter()
            .find(|(shifted, _)| *shifted == character)
            .map(|(_, unshifted)| (*unshifted).to_owned())
    };
    if let Some(unshifted) = shifted {
        let code = keycode(&unshifted).ok_or(format!("`{character}` cannot be typed"))?;
        return Ok(Action::Key { modifiers: vec![EV_LEFTSHIFT], mask: MOD_SHIFT, code });
    }
    let name = if character == ' ' { "space".to_owned() } else { character.to_string() };
    let code = keycode(&name).ok_or(format!("`{character}` cannot be typed"))?;
    Ok(Action::Key { modifiers: Vec::new(), mask: 0, code })
}

/// The modifier bits of a standard evdev keymap, in the order xkb assigns them:
/// Shift, Lock, Control, Mod1 through Mod5.
pub(crate) const MOD_SHIFT: u32 = 1 << 0;
pub(crate) const MOD_CONTROL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;

pub(crate) const EV_LEFTCTRL: u32 = 29;
pub(crate) const EV_LEFTSHIFT: u32 = 42;
const EV_LEFTALT: u32 = 56;
const EV_LEFTMETA: u32 = 125;

/// Key names to `linux/input-event-codes.h` codes.
///
/// The protocol takes evdev codes, not keysyms: a virtual keyboard supplies a
/// keymap and then presses positions on it. These are the codes for a US layout,
/// which is the keymap the driver installs.
fn keycode(name: &str) -> Option<u32> {
    const LETTERS: [(&str, u32); 26] = [
        ("a", 30),
        ("b", 48),
        ("c", 46),
        ("d", 32),
        ("e", 18),
        ("f", 33),
        ("g", 34),
        ("h", 35),
        ("i", 23),
        ("j", 36),
        ("k", 37),
        ("l", 38),
        ("m", 50),
        ("n", 49),
        ("o", 24),
        ("p", 25),
        ("q", 16),
        ("r", 19),
        ("s", 31),
        ("t", 20),
        ("u", 22),
        ("v", 47),
        ("w", 17),
        ("x", 45),
        ("y", 21),
        ("z", 44),
    ];
    const DIGITS: [(&str, u32); 10] = [
        ("1", 2),
        ("2", 3),
        ("3", 4),
        ("4", 5),
        ("5", 6),
        ("6", 7),
        ("7", 8),
        ("8", 9),
        ("9", 10),
        ("0", 11),
    ];
    const NAMED: [(&str, u32); 23] = [
        ("escape", 1),
        ("esc", 1),
        ("backspace", 14),
        ("tab", 15),
        ("enter", 28),
        ("return", 28),
        ("space", 57),
        ("up", 103),
        ("down", 108),
        ("left", 105),
        ("right", 106),
        ("home", 102),
        ("end", 107),
        ("pageup", 104),
        ("page_up", 104),
        ("pagedown", 109),
        ("page_down", 109),
        ("insert", 110),
        ("delete", 111),
        ("period", 52),
        ("slash", 53),
        ("minus", 12),
        // `KEY_PAUSE`. The one key whose name is what it does, and the one bound
        // to holding a running operation still.
        ("pause", 119),
    ];
    /// Every key a US layout produces without Shift, as the character itself, so
    /// `text my-file.txt` types a filename rather than refusing one. `SHIFTED`
    /// above is the other half of the same keyboard.
    const PUNCTUATION: [(&str, u32); 11] = [
        ("-", 12),
        ("=", 13),
        ("[", 26),
        ("]", 27),
        (";", 39),
        ("'", 40),
        ("`", 41),
        ("\\", 43),
        (",", 51),
        (".", 52),
        ("/", 53),
    ];

    if let Some(number) = name.strip_prefix('f').and_then(|rest| rest.parse::<u32>().ok()) {
        // F1 to F10 are 59 to 68; F11 and F12 are 87 and 88.
        return match number {
            1..=10 => Some(58 + number),
            11 => Some(87),
            12 => Some(88),
            _ => None,
        };
    }
    LETTERS
        .iter()
        .chain(DIGITS.iter())
        .chain(NAMED.iter())
        .chain(PUNCTUATION.iter())
        .find(|(key, _)| *key == name)
        .map(|(_, code)| *code)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Action, Button, EV_LEFTCTRL, EV_LEFTSHIFT, MOD_CONTROL, MOD_SHIFT, parse};

    #[test]
    fn a_chord_becomes_modifiers_and_a_key() {
        let actions = parse("key ctrl+h").expect("parsed");
        assert_eq!(
            actions,
            vec![Action::Key { modifiers: vec![EV_LEFTCTRL], mask: MOD_CONTROL, code: 35 }]
        );
    }

    #[test]
    fn a_function_key_maps_to_the_right_side_of_the_keyboard() {
        let plain = |code| Action::Key { modifiers: vec![], mask: 0, code };
        assert_eq!(parse("key f1").unwrap(), vec![plain(59)]);
        assert_eq!(parse("key f10").unwrap(), vec![plain(68)]);
        assert_eq!(parse("key f12").unwrap(), vec![plain(88)]);
        assert!(parse("key f13").is_err());
    }

    #[test]
    fn text_becomes_one_press_per_character_with_shift_for_capitals() {
        let actions = parse("text aB").expect("parsed");
        assert_eq!(
            actions,
            vec![
                Action::Key { modifiers: vec![], mask: 0, code: 30 },
                Action::Key { modifiers: vec![EV_LEFTSHIFT], mask: MOD_SHIFT, code: 48 },
            ]
        );
    }

    #[test]
    fn a_position_is_a_percentage_of_the_output() {
        assert_eq!(parse("move 0 100").unwrap(), vec![Action::Move { x: 0, y: 10_000 }]);
        assert_eq!(parse("move 50 25").unwrap(), vec![Action::Move { x: 5_000, y: 2_500 }]);
        assert!(parse("move 120 0").is_err(), "a percentage over a hundred is a typo");
        assert!(parse("move 0").is_err());
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let actions = parse("# a comment\n\n  key down   # trailing\n").expect("parsed");
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn a_bad_line_says_which_line_it_is() {
        let problem = parse("key down\nnonsense\n").expect_err("refused");
        assert!(problem.starts_with("line 2:"), "{problem}");
    }

    #[test]
    fn every_step_verb_parses() {
        let script = "\
            key ctrl+shift+c\n\
            text hello world\n\
            move 10 20\n\
            click\n\
            click right\n\
            double\n\
            drag 10 20 80 90\n\
            scroll -3\n\
            sleep 250\n";
        let actions = parse(script).expect("parsed");
        assert!(actions.iter().any(|action| matches!(action, Action::Drag { .. })));
        assert!(actions.contains(&Action::Scroll { notches: -3, held: super::Held::Nothing }));
        assert_eq!(
            parse("scroll 4 shift").as_deref(),
            Ok([Action::Scroll { notches: 4, held: super::Held::Shift }].as_slice()),
            "Shift is held across the notches, not sent as a chord beside them"
        );
        // **And `Ctrl`, which zooms a picture.** It was refused here on the
        // grounds that only Shift means anything to a wheel, which stopped being
        // true when the preview learned to magnify.
        assert_eq!(
            parse("scroll -2 ctrl").as_deref(),
            Ok([Action::Scroll { notches: -2, held: super::Held::Ctrl }].as_slice())
        );
        assert!(parse("scroll 4 alt").is_err(), "and nothing else means anything to a wheel");
        assert_eq!(
            parse("flick 16 20").as_deref(),
            Ok([Action::Flick { pixels: 16, events: 20, hold: 0 }].as_slice()),
            "a flick with no hold lifts the moment it stops moving"
        );
        assert_eq!(
            parse("flick -8 12 300").as_deref(),
            Ok([Action::Flick { pixels: -8, events: 12, hold: 300 }].as_slice())
        );
        assert!(parse("flick 16").is_err(), "pixels alone is not a gesture");
        assert!(actions.contains(&Action::Click { button: Button::Right, times: 1 }));
        assert!(actions.contains(&Action::Sleep { milliseconds: 250 }));
    }
}
