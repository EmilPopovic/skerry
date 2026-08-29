//! What the platform layer reports upward, in logical pixels.
//!
//! Every position here is logical and fractional. The renderer multiplies by the
//! scale the compositor sent; nothing above this crate ever sees a physical
//! coordinate, which is what stops a hit test and a drawn rectangle from
//! disagreeing at 125%.

/// A position or a delta, in logical pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Point {
    /// Rightward.
    pub x: f32,
    /// Downward.
    pub y: f32,
}

impl Point {
    /// A point at `(x, y)`.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A size in logical pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Size {
    /// Across.
    pub width: f32,
    /// Down.
    pub height: f32,
}

impl Size {
    /// A size of `width` by `height`.
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// Which modifiers were held when an event happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    /// Control.
    pub ctrl: bool,
    /// Shift.
    pub shift: bool,
    /// Alt, which xkb calls `Mod1`.
    pub alt: bool,
    /// The Super or Windows key, which xkb calls `Mod4`.
    pub logo: bool,
}

impl Modifiers {
    /// Whether no modifier at all is held.
    #[must_use]
    pub const fn none(self) -> bool {
        !self.ctrl && !self.shift && !self.alt && !self.logo
    }

    /// Whether Control is held and nothing else is.
    #[must_use]
    pub const fn only_ctrl(self) -> bool {
        self.ctrl && !self.shift && !self.alt && !self.logo
    }

    /// Whether Control and Shift are held and nothing else is.
    #[must_use]
    pub const fn ctrl_shift(self) -> bool {
        self.ctrl && self.shift && !self.alt && !self.logo
    }

    /// Whether Shift is held and nothing else is.
    ///
    /// Its own question because Shift is the one modifier that also changes what
    /// a key *types*: a caller asking "is this a bare press" has to exclude it,
    /// and a caller asking "is this a Shift chord" has to require it alone.
    #[must_use]
    pub const fn only_shift(self) -> bool {
        self.shift && !self.ctrl && !self.alt && !self.logo
    }
}

/// A key identified by what it means rather than where it is.
///
/// A keysym, not a scancode, because a keymap is the user's choice: `Dvorak`
/// puts `Delete`'s neighbour somewhere else, and a shortcut bound to a position
/// is a shortcut that moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A key with a name and no character: `Escape`, `Left`, `F3`.
    Named(Named),
    /// A key that produced a character, already case-folded by the keymap.
    Character(char),
    /// A keysym we have no name for. Carried rather than dropped so a keymap
    /// binding can still reach it.
    Unknown(u32),
}

/// The keys a file manager binds that are not characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(missing_docs, reason = "each variant is the key of that name")]
pub enum Named {
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Menu,
    /// A function key, one-based: `Function(1)` is F1.
    Function(u8),
}

/// One key press or release.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    /// What was pressed.
    pub key: Key,
    /// The text it produced, if any. Separate from `key` because a key both
    /// binds a shortcut and types a character, and an IME can produce text with
    /// no key at all.
    pub text: Option<String>,
    /// What was held down with it.
    pub mods: Modifiers,
    /// Whether this is a repeat rather than a fresh press.
    pub repeat: bool,
}

/// A pointer button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    /// Usually the left one.
    Primary,
    /// Usually the right one.
    Secondary,
    /// The wheel, pressed.
    Middle,
    /// Back, forward, or anything else the device reports.
    Other(u32),
}

/// What happened, in the order it happened.
#[derive(Clone, PartialEq, Debug)]
pub enum Event {
    /// The window has a new size or a new scale.
    ///
    /// `physical` is what the renderer's surface must be, exactly:
    /// `logical * scale_120 / 120`, rounded up. Spike A rendered visibly fuzzy
    /// text for a day because the surface was the wrong size by four pixels and
    /// the compositor resampled it.
    Resized {
        /// The size to lay out and hit-test in.
        logical: Size,
        /// The size to allocate the surface at, in device pixels.
        physical: (u32, u32),
        /// The compositor's scale as a numerator over 120. 150 is 125%.
        scale_120: u32,
    },
    /// The user asked to close the window.
    CloseRequested,
    /// Keyboard focus arrived or left.
    Focused(bool),
    /// A key was pressed.
    KeyDown(KeyEvent),
    /// A key was released.
    KeyUp(KeyEvent),
    /// The pointer moved to `position`.
    PointerMoved {
        /// Where it is now.
        position: Point,
        /// What is held.
        mods: Modifiers,
    },
    /// The pointer left the window.
    PointerLeft,
    /// A button went down or up.
    Button {
        /// Which button.
        button: Button,
        /// Down or up.
        pressed: bool,
        /// Where the pointer was.
        position: Point,
        /// 1 for a single click, 2 for a double, 3 for a triple. Counted here
        /// because the threshold is a platform convention, not a widget's
        /// business, and because getting it wrong turns six drags into six
        /// double-clicks — which is what happened in Spike D.
        clicks: u8,
        /// What is held.
        mods: Modifiers,
    },
    /// The wheel or a touchpad scrolled.
    Scroll {
        /// Logical pixels, positive down and right.
        delta: Point,
        /// Whether this came from a touchpad, which sends continuous deltas
        /// worth animating, rather than a wheel, which sends notches.
        precise: bool,
        /// What is held.
        mods: Modifiers,
    },
    /// A drag from another application arrived over the window.
    DragEntered {
        /// Where the pointer is, in logical pixels.
        position: Point,
    },
    /// It moved. The window says what it would do with it through
    /// [`Window::accept_drag`](crate::Window::accept_drag).
    DragMoved {
        /// Where the pointer is now.
        position: Point,
    },
    /// It left without dropping, or the source cancelled it.
    DragLeft,
    /// It was dropped. The payload is read with
    /// [`Window::take_drop`](crate::Window::take_drop), which hands back a
    /// descriptor rather than bytes for the same reason the clipboard does.
    Dropped {
        /// Where it landed.
        position: Point,
    },
    /// A touchpad swipe finished, having travelled far enough to mean something.
    ///
    /// One event per gesture rather than a stream of deltas: a swipe is a
    /// discrete intent, and the accumulation and the threshold are the same on
    /// both display servers because they live in one place.
    Swiped {
        /// Which way it went.
        direction: Swipe,
        /// How many fingers were on the pad. A three-finger swipe and a
        /// four-finger swipe are different gestures to a person.
        fingers: u32,
    },
    /// A touchpad pinch passed another step, in or out.
    ///
    /// Emitted while the fingers move, because a zoom that arrives when the hand
    /// lifts is a zoom that cannot be aimed.
    Pinched {
        /// Whether the fingers moved apart.
        out: bool,
    },
    /// A background job finished and wants the frame loop to look.
    Woken,
}

/// Which way a swipe went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(missing_docs, reason = "each variant is the direction of that name")]
pub enum Swipe {
    Left,
    Right,
    Up,
    Down,
}
