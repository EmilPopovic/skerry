// SPDX-License-Identifier: MIT OR Apache-2.0
//! Replays keyboard and pointer input into a compositor.
//!
//! The other half of an end-to-end test of the real window: `skerry` draws and
//! this presses the keys. It talks to the compositor rather than to `skerry`, so
//! what it exercises is the whole path — `wl_keyboard`, `xkbcommon`, the click
//! counter, the scroll coalescer, the frame loop — and not a mock of it.
//!
//! **It is pointed at a nested compositor, never at the session a person is
//! using.** Synthetic input goes wherever the focus is; run against a live
//! desktop it would type into whatever window happened to be focused. `tools/gui`
//! starts a nested Hyprland with its own socket for exactly this reason.
//!
//! **Two backends, because the product has two.** On Wayland it is
//! `zwp_virtual_keyboard_manager_v1` and `zwlr_virtual_pointer_manager_v1`, both
//! wlroots'; measured on this machine, `KWin` advertises neither, so that half of
//! the fixture is Hyprland-only. On X11 it is `XTEST`, which every X server has
//! and which **needs no seat** — that is what lets the same scenarios run under a
//! bare `Xvfb`, and it is the reason the GUI suite can reach CI at all.
//!
//! The script is played by one function over a [`Input`] trait rather than twice.
//! A scenario that pressed a different sequence on each backend would be two
//! tests wearing one name, and the differences between the two servers are
//! exactly what this is here to find.

use std::io::Write as _;
use std::os::fd::AsFd;
use std::time::Duration;

use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

mod ime;
mod script;

use script::{Action, Button};

/// Where the pointer's absolute coordinates are expressed against.
///
/// The virtual-pointer protocol takes a position and an extent rather than a
/// pixel, so a script says "40% across" and does not have to know the output.
const EXTENT: u32 = 10_000;

fn main() -> std::process::ExitCode {
    let mut display = None;
    let mut backend = None;
    let mut paste = None;
    let mut close = false;
    let mut resize = None;
    let mut dpi = None;
    let mut query_dpi = false;
    let mut ime: Option<Vec<String>> = None;
    let mut globals = false;
    let mut path = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--display" => display = arguments.next(),
            "--x11" => backend = Some(Backend::X11),
            "--paste" => paste = arguments.next(),
            "--close" => close = true,
            "--resize" => resize = arguments.next(),
            "--xft-dpi" => dpi = arguments.next(),
            "--query-dpi" => query_dpi = true,
            // Three words: what to show while composing, what to commit, and how
            // many milliseconds to hold the preedit for.
            "--ime" => ime = Some(arguments.by_ref().take(3).collect()),
            "--globals" => globals = true,
            "--wayland" => backend = Some(Backend::Wayland),
            "--help" | "-h" => {
                println!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            other => path = Some(other.to_owned()),
        }
    }
    // Which server, decided the way the product decides it — so a scenario that
    // names a display gets the driver that matches it.
    let backend = backend.unwrap_or_else(|| {
        if display.as_deref().is_some_and(|name| name.starts_with(':'))
            || std::env::var_os("WAYLAND_DISPLAY").is_none_or(|name| name.is_empty())
        {
            Backend::X11
        } else {
            Backend::Wayland
        }
    });
    if let Some(display) = display {
        let name = match backend {
            Backend::Wayland => "WAYLAND_DISPLAY",
            Backend::X11 => "DISPLAY",
        };
        // SAFETY: single-threaded, before any thread is started.
        unsafe { std::env::set_var(name, display) };
    }

    if globals {
        return match ime::list_globals() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(problem) => {
                eprintln!("skerry-drive: {problem}");
                std::process::ExitCode::from(2)
            }
        };
    }

    if let Some(words) = ime {
        let [preedit, commit, hold] = <[String; 3]>::try_from(words).unwrap_or_else(|words| {
            let mut words = words;
            words.resize(3, String::new());
            [words[0].clone(), words[1].clone(), words[2].clone()]
        });
        let hold = Duration::from_millis(hold.parse().unwrap_or(300));
        return match ime::type_into_the_window(&ime::Typing { preedit, commit, hold }) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(problem) => {
                eprintln!("skerry-drive: {problem}");
                std::process::ExitCode::from(2)
            }
        };
    }

    if let Some(code) = one_shot(&OneShot { dpi, query_dpi, resize, close, paste }) {
        return code;
    }

    let source = match path {
        Some(path) => std::fs::read_to_string(&path).unwrap_or_default(),
        None => std::io::read_to_string(std::io::stdin()).unwrap_or_default(),
    };
    let actions = match script::parse(&source) {
        Ok(actions) => actions,
        Err(problem) => {
            eprintln!("skerry-drive: {problem}");
            return std::process::ExitCode::from(2);
        }
    };

    let connected: Result<Box<dyn Input>, String> = match backend {
        Backend::Wayland => Driver::connect().map(|it| Box::new(it) as Box<dyn Input>),
        Backend::X11 => XTest::connect().map(|it| Box::new(it) as Box<dyn Input>),
    };
    match connected {
        Ok(mut input) => {
            play(input.as_mut(), &actions);
            std::process::ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("skerry-drive: {problem}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The one-shot commands: each does its thing and the process exits.
///
/// Together rather than inline in `main`, which is otherwise argument parsing
/// and a playback loop with five early returns wedged between them.
struct OneShot {
    dpi: Option<String>,
    query_dpi: bool,
    resize: Option<String>,
    close: bool,
    paste: Option<String>,
}

/// Runs whichever one-shot was asked for, or `None` to carry on and play a script.
fn one_shot(asked: &OneShot) -> Option<std::process::ExitCode> {
    /// A `Result` becomes an exit code, with the reason on stderr.
    fn done(outcome: Result<(), String>) -> std::process::ExitCode {
        match outcome {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(problem) => {
                eprintln!("skerry-drive: {problem}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    if asked.query_dpi {
        return Some(done(query_xft_dpi().map(|dpi| println!("{dpi}"))));
    }
    if let Some(dpi) = &asked.dpi {
        return Some(done(set_xft_dpi(dpi)));
    }
    if let Some(size) = &asked.resize {
        return Some(done(resize_window(size)));
    }
    if asked.close {
        return Some(done(ask_to_close()));
    }
    if let Some(mime) = &asked.paste {
        return Some(done(read_selection(mime).map(|bytes| {
            use std::io::Write as _;
            let _ = std::io::stdout().write_all(&bytes);
        })));
    }
    None
}

/// Which display server to drive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    Wayland,
    X11,
}

const USAGE: &str = "\
skerry-drive [--display SOCKET] [--x11|--wayland] [SCRIPT]

Replays input into a display server. The script is read from SCRIPT or from
standard input. Point it at a nested compositor or a private X server: synthetic
input goes wherever the focus is, and against a live session that is whatever
window a person was using.

The backend is guessed from --display — a name starting with a colon is an X
display — and can be said outright.

  --paste MIME          print the X11 clipboard as MIME and exit
  --close               ask the X11 window to close, the way a title bar would
  --resize WxH          resize the X11 window, as a window manager would
  --xft-dpi N           set Xft.dpi in the root resource database
  --globals             print every Wayland global the compositor advertises
  --ime PREEDIT COMMIT HOLD_MS
                   act as an input method: show PREEDIT, hold it, then commit
  --query-dpi           print the Xft.dpi the root resource database holds
  press / release       one button, apart, so a drag can be paced
  key ctrl+shift+c      press and release a chord
  text hello            type a string
  move 50 30            move the pointer, in percent of the output
  click                 press and release the left button
  click right           any of left, right, middle
  double                two clicks inside the double-click window
  triple                three of them, which is what selects a line
  drag 10 20 80 90      press at the first point, move to the second, release
  scroll -3 [shift|ctrl] wheel notches, negative is up; the modifier held across them
  flick 16 20 [HOLD]    a touchpad gesture of EVENTS deltas of PIXELS, then the
                   fingers leave — after resting HOLD ms, if given
  sleep 250             milliseconds";

/// What a backend has to be able to do. Deliberately primitive: every decision
/// about *ordering* — how a chord is pressed, how many steps a drag takes — is in
/// [`play`] and is therefore the same on both.
trait Input {
    /// Presses or releases one evdev keycode.
    fn key(&mut self, code: u32, down: bool);
    /// States the modifier mask, where the protocol needs it said separately.
    fn modifiers(&mut self, _mask: u32) {}
    /// Moves the pointer, in [`EXTENT`]ths of the output.
    fn move_to(&mut self, x: u32, y: u32);
    /// Presses or releases one button.
    fn button(&mut self, button: Button, down: bool);
    /// One wheel notch, negative being up.
    fn notch(&mut self, up: bool);
    /// One touchpad delta, from a finger source. Positive is down.
    ///
    /// Defaulted to a diagnostic rather than to nothing, because a backend that
    /// cannot express a finger has to say so: X11 core events are wheel buttons
    /// with no source and no end, so a scenario needing this skips there rather
    /// than passing against a gesture that never happened.
    fn finger(&mut self, _pixels: f64) {
        eprintln!("skerry-drive: this backend cannot send a finger-source scroll");
    }
    /// The fingers leaving the pad, which is what ends the gesture.
    fn finger_stop(&mut self) {
        eprintln!("skerry-drive: this backend cannot end a scroll gesture");
    }
    /// Sends everything queued.
    fn flush(&mut self);
    /// A last round trip before the process exits.
    fn finish(&mut self) {}
}

/// Replays a script through any backend.
fn play(input: &mut dyn Input, actions: &[Action]) {
    for action in actions {
        match action {
            Action::Key { modifiers, mask, code } => {
                for held in modifiers {
                    input.key(*held, true);
                }
                input.modifiers(*mask);
                input.key(*code, true);
                input.key(*code, false);
                input.modifiers(0);
                for held in modifiers.iter().rev() {
                    input.key(*held, false);
                }
            }
            Action::Move { x, y } => input.move_to(*x, *y),
            Action::Press { button, down } => input.button(*button, *down),
            Action::Click { button, times } => {
                for _ in 0..*times {
                    input.button(*button, true);
                    input.button(*button, false);
                }
            }
            Action::Drag { from, to } => {
                input.move_to(from.0, from.1);
                input.button(Button::Left, true);
                // Several steps, because a rubber band is a gesture and one jump
                // is a click that happened to end somewhere else.
                for step in 1..=8i64 {
                    // Signed, because a drag upward has a negative delta and the
                    // unsigned version wrapped to four billion — a driver bug
                    // that reads exactly like a rubber band selecting nothing.
                    let along = |start: u32, end: u32| {
                        (i64::from(start) + (i64::from(end) - i64::from(start)) * step / 8).max(0)
                            as u32
                    };
                    input.move_to(along(from.0, to.0), along(from.1, to.1));
                }
                input.button(Button::Left, false);
            }
            Action::Scroll { notches, held } => {
                // Held *across* the notches rather than pressed beside them: the
                // program reads the modifier state on the scroll event, so a chord
                // sent before it would have been released by the time it mattered.
                if let Some((code, mask)) = held.key() {
                    input.key(code, true);
                    input.modifiers(mask);
                }
                for _ in 0..notches.abs() {
                    input.notch(*notches < 0);
                }
                if let Some((code, _)) = held.key() {
                    input.modifiers(0);
                    input.key(code, false);
                }
            }
            // **Paced in real time, not only in the timestamps.** The client
            // measures a finger's speed from the server's clock, and a compositor
            // is free to restamp what a virtual pointer sends with its own — so a
            // burst of twenty events sent in one microsecond would arrive as
            // twenty events in the same millisecond and have no speed at all.
            Action::Flick { pixels, events, hold } => {
                for _ in 0..*events {
                    input.finger(f64::from(*pixels));
                    input.flush();
                    std::thread::sleep(Duration::from_millis(8));
                }
                std::thread::sleep(Duration::from_millis(u64::from(*hold)));
                input.finger_stop();
            }
            Action::Sleep { milliseconds } => {
                input.flush();
                std::thread::sleep(Duration::from_millis(u64::from(*milliseconds)));
            }
        }
        input.flush();
    }
    input.finish();
}

struct Driver {
    queue: EventQueue<State>,
    state: State,
    keyboard: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    time: u32,
    /// When this process started, so a timestamp is a real elapsed millisecond.
    started: std::time::Instant,
}

#[derive(Default)]
struct State;

impl Driver {
    fn connect() -> Result<Self, String> {
        let connection =
            Connection::connect_to_env().map_err(|error| format!("connect: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| format!("registry: {error}"))?;
        let handle = queue.handle();

        let seat: wl_seat::WlSeat = bind(&globals, &handle, 1..=9).ok_or("no wl_seat")?;
        let keyboards: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 =
            bind(&globals, &handle, 1..=1).ok_or("no zwp_virtual_keyboard_manager_v1")?;
        let pointers: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 =
            bind(&globals, &handle, 1..=2).ok_or("no zwlr_virtual_pointer_manager_v1")?;

        let keyboard = keyboards.create_virtual_keyboard(&seat, &handle, ());
        let pointer = pointers.create_virtual_pointer(Some(&seat), &handle, ());

        let mut state = State;
        // A virtual keyboard must supply the keymap its keycodes are read
        // against, before it may send a single key.
        let (fd, size) = keymap()?;
        keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1 as u32, fd.as_fd(), size);
        queue.roundtrip(&mut state).map_err(|error| format!("roundtrip: {error}"))?;

        Ok(Self { queue, state, keyboard, pointer, time: 1, started: std::time::Instant::now() })
    }

    /// Advances the clock.
    ///
    /// Milliseconds, monotonically, and **real**.
    ///
    /// The click counter under test decides a double click from these timestamps,
    /// so a driver that sent the same one twice would be testing its own bug —
    /// hence monotonic, and hence the `max` below.
    ///
    /// **A fixed step of eight per call was the other half of that, and it was
    /// wrong.** It made every *duration* the client measures a fiction. A `flick`
    /// that rested three hundred milliseconds before lifting reported a lift eight
    /// milliseconds after the last movement, so the client measured a finger still
    /// moving and threw the list — and the scenario comparing a flick against a
    /// drag that stopped saw the same distance twice and had no way to say why.
    /// A velocity is a distance over a time, and a driver that fakes the time
    /// cannot test one.
    fn tick(&mut self) -> u32 {
        let now = self.started.elapsed().as_millis() as u32;
        self.time = now.max(self.time.wrapping_add(1));
        self.time
    }

    /// One touchpad delta: a finger source, and a distance the only way one can
    /// be delivered.
    ///
    /// **`axis` alone arrives as zero**, which is the same limitation
    /// [`Driver::wheel`] records above and works around: Hyprland forwards a bare
    /// `virtual_pointer.axis` to the client with a value of zero. So the distance
    /// has to ride on `axis_discrete`'s notch count, and a finger source beside a
    /// notch count is a pair no real touchpad sends.
    ///
    /// What that costs is one thing and what it keeps is the rest. The *distance*
    /// is synthetic — one notch, resolved by the client to `WHEEL_NOTCH` — and
    /// everything downstream of it is the real path: `axis_source` is what makes
    /// the client track 1:1 rather than ease, `axis_stop` is what ends the gesture,
    /// the timestamps are what the velocity is measured from, and the fling is the
    /// fling. The scenario is differential for exactly this reason: it compares a
    /// lift at speed against a lift at rest, so what the distance happens to be
    /// does not enter the assertion.
    ///
    /// The protocol has no `axis_value120`, so there is no third option to prefer.
    ///
    /// **Both events, because only one of them carries a clock.** `axis_value120`
    /// and `axis_discrete` have no `time` field in the protocol at all — a
    /// timestamp reaches the client on `axis` and on `axis_stop` and nowhere else.
    /// With the discrete event alone every frame of a gesture arrived stamped zero,
    /// so the client had twenty samples with no span between them, measured a
    /// velocity of nothing, and threw nothing: the flick moved the list exactly as
    /// far as a drag that stopped. A real touchpad always sends `axis`, so this is
    /// the harness catching up with the device rather than the client being lenient.
    fn delta(&mut self, pixels: f64) {
        let time = self.tick();
        self.pointer.axis_source(wl_pointer::AxisSource::Finger);
        self.pointer.axis_discrete(time, wl_pointer::Axis::VerticalScroll, pixels, 1);
        self.pointer.frame();
    }

    /// The fingers leaving the pad: `axis_stop`, in its own frame.
    fn lift(&mut self) {
        let time = self.tick();
        self.pointer.axis_source(wl_pointer::AxisSource::Finger);
        self.pointer.axis_stop(time, wl_pointer::Axis::VerticalScroll);
        self.pointer.frame();
    }

    /// One wheel notch, as a discrete axis with a source.
    ///
    /// **`axis` alone produced nothing.** Hyprland forwarded a bare
    /// `virtual_pointer.axis` to the client as `wl_pointer.axis` with a value of
    /// **zero**, and `axis_value120` zero beside it, so twelve notches scrolled
    /// nothing at all — and the scenario that was supposed to cover this returned
    /// early on a fixture smaller than the window, so nothing ever failed.
    /// `axis_discrete` with `axis_source` is what a real wheel sends and what the
    /// compositor translates.
    fn wheel(&mut self, up: bool) {
        let time = self.tick();
        let steps = if up { -1 } else { 1 };
        let distance = f64::from(steps) * 15.0;
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.axis_discrete(time, wl_pointer::Axis::VerticalScroll, distance, steps);
        self.pointer.frame();
    }
}

impl Input for Driver {
    fn key(&mut self, code: u32, down: bool) {
        let time = self.tick();
        self.keyboard.key(time, code, u32::from(down));
    }

    /// Said explicitly, because `zwp_virtual_keyboard_v1` does not derive the
    /// modifier state from the keycodes it was sent — a client that only presses
    /// `Ctrl` sees a plain `h` arrive at the far end.
    fn modifiers(&mut self, mask: u32) {
        self.keyboard.modifiers(mask, 0, 0, 0);
    }

    fn move_to(&mut self, x: u32, y: u32) {
        let time = self.tick();
        self.pointer.motion_absolute(time, x, y, EXTENT, EXTENT);
        self.pointer.frame();
    }

    fn button(&mut self, button: Button, down: bool) {
        let time = self.tick();
        let state =
            if down { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        self.pointer.button(time, button.code(), state);
        self.pointer.frame();
    }

    fn notch(&mut self, up: bool) {
        self.wheel(up);
    }

    fn finger(&mut self, pixels: f64) {
        self.delta(pixels);
    }

    fn finger_stop(&mut self) {
        self.lift();
    }

    fn flush(&mut self) {
        let _ = self.queue.flush();
        let _ = self.queue.dispatch_pending(&mut self.state);
    }

    fn finish(&mut self) {
        // So the compositor has seen everything before the process exits and its
        // virtual devices are destroyed.
        let _ = self.queue.roundtrip(&mut self.state);
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.pointer.destroy();
        self.keyboard.destroy();
        let _ = self.queue.roundtrip(&mut self.state);
    }
}

/// A US keymap in a descriptor, for the virtual keyboard to be read against.
fn keymap() -> Result<(std::fs::File, u32), String> {
    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "evdev",
        "pc105",
        "us",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or("could not compile a keymap")?;
    let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);

    let fd = rustix::fs::memfd_create("skerry-drive-keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|error| format!("memfd: {error}"))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes()).map_err(|error| format!("write: {error}"))?;
    // The trailing NUL the protocol expects, and which every compositor reads.
    file.write_all(&[0]).map_err(|error| format!("write: {error}"))?;
    file.flush().ok();
    Ok((file, text.len() as u32 + 1))
}

fn bind<I>(
    globals: &GlobalList,
    handle: &QueueHandle<State>,
    version: std::ops::RangeInclusive<u32>,
) -> Option<I>
where
    I: Proxy + 'static,
    State: Dispatch<I, ()>,
{
    globals.bind(handle, version, ()).ok()
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($object:path),* $(,)?) => { $(
        impl Dispatch<$object, ()> for State {
            fn event(
                _: &mut Self,
                _: &$object,
                _: <$object as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )* };
}

ignore_events!(
    wl_seat::WlSeat,
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
);

// ---------------------------------------------------------------------------
// X11
// ---------------------------------------------------------------------------

/// Synthetic input through `XTEST`.
///
/// **No seat and no compositor.** That is the whole reason this exists beside
/// the Wayland driver: `XTEST` is a core X extension, every server has it, and a
/// bare `Xvfb` on a machine with no login session answers it — which is what
/// lets the GUI scenarios run somewhere a Wayland compositor cannot start.
///
/// Keycodes arrive here as evdev, because that is what the script and the
/// Wayland protocol use. X11 keycodes are the same physical keys offset by
/// eight, which is the one conversion in this file.
struct XTest {
    connection: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
    /// The screen in pixels, so a script's percentage becomes a coordinate.
    size: (u32, u32),
}

impl XTest {
    fn connect() -> Result<Self, String> {
        use x11rb::connection::Connection as _;
        let (connection, screen) =
            x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
        let present =
            x11rb::protocol::xproto::ConnectionExt::query_extension(&connection, b"XTEST")
                .map_err(|error| format!("query XTEST: {error}"))?
                .reply()
                .map_err(|error| format!("query XTEST: {error}"))?
                .present;
        if !present {
            return Err("this X server has no XTEST extension".to_owned());
        }
        let screen = connection.setup().roots.get(screen).ok_or("no such X screen")?;
        Ok(Self {
            root: screen.root,
            size: (u32::from(screen.width_in_pixels), u32::from(screen.height_in_pixels)),
            connection,
        })
    }

    fn fake(&mut self, kind: u8, detail: u8, x: i16, y: i16) {
        let _ = x11rb::protocol::xtest::fake_input(
            &self.connection,
            kind,
            detail,
            // No delay: the script's own `sleep` is the timing, and a delay here
            // would be a second one nobody asked for.
            0,
            self.root,
            x,
            y,
            0,
        );
    }
}

/// X11 event codes, from the core protocol.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// X11 button numbers for the wheel. It predates wheels, so they are buttons.
const WHEEL_UP: u8 = 4;
const WHEEL_DOWN: u8 = 5;

impl Input for XTest {
    fn key(&mut self, code: u32, down: bool) {
        // evdev to X11: the same physical keys, offset by eight. Getting this
        // wrong shifts the whole keyboard by one row, which is what it looked
        // like the first time.
        let keycode = u8::try_from(code + 8).unwrap_or(0);
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0);
    }

    /// **A round trip, not a no-op.** The X server tracks modifier state from the
    /// keycodes it was sent — that part needs no help — but `play` presses the
    /// modifier and the key back to back, and a reply here is what proves the
    /// press has been *processed* before the key it modifies is sent. Requests
    /// are answered in order, so a reply to this one means every request before
    /// it has been applied.
    ///
    /// Without it `Ctrl+V` arrived as a bare `v` about once in twenty, which
    /// lands in the type-ahead buffer and leaves no trace anywhere: no notice, no
    /// job, and every mode in the window reading clean.
    fn modifiers(&mut self, _mask: u32) {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let _ = self.connection.get_input_focus().map(x11rb::cookie::Cookie::reply);
    }

    fn move_to(&mut self, x: u32, y: u32) {
        let across = |value: u32, extent: u32| {
            i16::try_from(u64::from(value) * u64::from(extent) / u64::from(EXTENT))
                .unwrap_or(i16::MAX)
        };
        self.fake(MOTION_NOTIFY, 0, across(x, self.size.0), across(y, self.size.1));
    }

    fn button(&mut self, button: Button, down: bool) {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        self.fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, detail, 0, 0);
    }

    fn notch(&mut self, up: bool) {
        let detail = if up { WHEEL_UP } else { WHEEL_DOWN };
        // Press and release, because that is what a real wheel sends and what
        // the client counts one notch from.
        self.fake(BUTTON_PRESS, detail, 0, 0);
        self.fake(BUTTON_RELEASE, detail, 0, 0);
    }

    fn flush(&mut self) {
        use x11rb::connection::Connection as _;
        let _ = self.connection.flush();
    }

    fn finish(&mut self) {
        use x11rb::connection::Connection as _;
        // A round trip, so everything has reached the server before the process
        // exits.
        let _ = x11rb::protocol::xproto::ConnectionExt::get_input_focus(&self.connection)
            .map(x11rb::cookie::Cookie::reply);
        let _ = self.connection.flush();
    }
}

/// Reads the `CLIPBOARD` selection as `mime`, as a separate X client.
///
/// The point of a clipboard is that **another application** can read it, so a
/// scenario that asked the window what it thinks it offered would pass with the
/// selection never taken. `wl-paste` is that other application on Wayland;
/// neither `xclip` nor `xsel` is a given on a machine, and this binary already
/// speaks X11, so it is that other application here.
///
/// `INCR` is handled: the reason it exists is a selection larger than one
/// request, and `Ctrl+A` over a large directory is exactly that.
fn read_selection(mime: &str) -> Result<Vec<u8>, String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    let (connection, screen) =
        x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
    let root = connection.setup().roots.get(screen).ok_or("no such X screen")?.root;
    let intern = |name: &str| -> Result<u32, String> {
        connection
            .intern_atom(false, name.as_bytes())
            .map_err(|error| format!("intern {name}: {error}"))?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|error| format!("intern {name}: {error}"))
    };
    // The X11 spelling of the MIME type, matching what the window publishes.
    let target = match mime {
        "text/plain;charset=utf-8" | "text/plain;charset=UTF-8" => intern("UTF8_STRING")?,
        other => intern(other)?,
    };
    let clipboard = intern("CLIPBOARD")?;
    let incr = intern("INCR")?;
    let property = intern("SKERRY_DRIVE_PASTE")?;

    let window = connection.generate_id().map_err(|error| format!("id: {error}"))?;
    let aux = xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE);
    connection
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &aux,
        )
        .map_err(|error| format!("create_window: {error}"))?;
    connection
        .convert_selection(window, clipboard, target, property, x11rb::CURRENT_TIME)
        .map_err(|error| format!("convert_selection: {error}"))?;
    connection.flush().map_err(|error| format!("flush: {error}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut collected = Vec::new();
    let mut incremental = false;
    while std::time::Instant::now() < deadline {
        let Ok(Some(event)) = connection.poll_for_event() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let arrived = match event {
            Event::SelectionNotify(event) if event.property == 0 => {
                return Err(format!("nothing is offered as {mime}"));
            }
            Event::SelectionNotify(_) => true,
            Event::PropertyNotify(event)
                if incremental && event.state == xproto::Property::NEW_VALUE =>
            {
                true
            }
            _ => false,
        };
        if !arrived {
            continue;
        }
        let reply = connection
            .get_property(true, window, property, xproto::AtomEnum::ANY, 0, u32::MAX / 4)
            .map_err(|error| format!("get_property: {error}"))?
            .reply()
            .map_err(|error| format!("get_property: {error}"))?;
        connection.flush().map_err(|error| format!("flush: {error}"))?;
        if reply.type_ == incr {
            incremental = true;
            continue;
        }
        if incremental {
            // A zero-length property ends an `INCR` transfer.
            if reply.value.is_empty() {
                return Ok(collected);
            }
            collected.extend_from_slice(&reply.value);
            continue;
        }
        return Ok(reply.value);
    }
    Err(format!("the owner never answered for {mime}"))
}

/// Asks the `skerry` window to close, the way a window manager's button does.
///
/// A `WM_DELETE_WINDOW` client message and nothing else — **not a signal**. A
/// signal skips every destructor, and the one time this ran for real it turned up
/// a segmentation fault on the way out that a week of signal-killed tests had
/// hidden.
fn ask_to_close() -> Result<(), String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    let (connection, screen) =
        x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
    let root = connection.setup().roots.get(screen).ok_or("no such X screen")?.root;
    let intern = |name: &str| -> Result<u32, String> {
        connection
            .intern_atom(false, name.as_bytes())
            .map_err(|error| format!("intern {name}: {error}"))?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|error| format!("intern {name}: {error}"))
    };
    let protocols = intern("WM_PROTOCOLS")?;
    let delete = intern("WM_DELETE_WINDOW")?;

    let target = find_window(&connection, root).ok_or("no skerry window on this display")?;

    let event = xproto::ClientMessageEvent::new(32, target, protocols, [delete, 0, 0, 0, 0]);
    connection
        .send_event(false, target, xproto::EventMask::NO_EVENT, event)
        .map_err(|error| format!("send_event: {error}"))?;
    // A round trip, not a flush. X11 errors are asynchronous: without something
    // that waits for a reply, a refused `SendEvent` is a message this process
    // exits before ever hearing about.
    connection
        .get_input_focus()
        .map_err(|error| format!("send_event: {error}"))?
        .reply()
        .map_err(|error| format!("the server refused the close message: {error}"))?;
    Ok(())
}

/// Finds the `skerry` window, by the `WM_CLASS` it sets.
///
/// The children of the root are the toplevels: there is no window manager under
/// `Xvfb`, so nothing is reparented and no frame window sits in between.
fn find_window(
    connection: &impl x11rb::connection::Connection,
    root: x11rb::protocol::xproto::Window,
) -> Option<x11rb::protocol::xproto::Window> {
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    let children = connection.query_tree(root).ok()?.reply().ok()?.children;
    children.into_iter().find(|window| {
        connection
            .get_property(
                false,
                *window,
                xproto::AtomEnum::WM_CLASS,
                xproto::AtomEnum::STRING,
                0,
                64,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.value.starts_with(b"skerry\0"))
    })
}

/// Resizes the window, the way a window manager would.
///
/// There is no window manager under `Xvfb`, so without this nothing ever changes
/// the window's size and the whole relayout path — `ConfigureNotify`, the logical
/// size derived from the device one, the columns that shed — is never exercised.
fn resize_window(size: &str) -> Result<(), String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    let (width, height) = size
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
        .ok_or("--resize needs WIDTHxHEIGHT")?;

    let (connection, screen) =
        x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
    let root = connection.setup().roots.get(screen).ok_or("no such X screen")?.root;
    let window = find_window(&connection, root).ok_or("no skerry window on this display")?;
    connection
        .configure_window(window, &xproto::ConfigureWindowAux::new().width(width).height(height))
        .map_err(|error| format!("configure_window: {error}"))?;
    // A round trip and not a flush: this process is about to exit, and a request
    // that has only been queued is a request the server may never see.
    connection
        .get_input_focus()
        .map_err(|error| format!("resize: {error}"))?
        .reply()
        .map_err(|error| format!("the server refused the resize: {error}"))?;
    Ok(())
}

/// Writes `Xft.dpi` into the root window's resource database.
///
/// **Not `xrdb`.** On this machine `xrdb -merge` and `xrdb -load` both exit 0
/// against an `Xvfb` and create no `RESOURCE_MANAGER` property at all, so a
/// harness that used it set nothing and every assertion about scale compared a
/// default against a default. Writing the property is what `xrdb` is for, it is
/// four lines, and it puts exactly the bytes there that the thing under test
/// reads.
fn set_xft_dpi(dpi: &str) -> Result<(), String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::wrapper::ConnectionExt as _;

    let dpi: u32 = dpi.trim().parse().map_err(|_| format!("{dpi} is not a DPI"))?;
    let (connection, screen) =
        x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
    let root = connection.setup().roots.get(screen).ok_or("no such X screen")?.root;
    // **`RetainPermanent` first, or this vanishes the moment the process exits.**
    // An X server frees a client's resources when its connection closes, and a
    // property set on the root is one of them — so writing it, reading it back on
    // the same connection to prove it is there, and then exiting leaves a display
    // with no resource database and a test that compares two defaults. It is what
    // `xrdb` does and the reason it exists rather than being a one-line `xprop`.
    connection
        .set_close_down_mode(xproto::CloseDown::RETAIN_PERMANENT)
        .map_err(|error| format!("set_close_down_mode: {error}"))?
        .check()
        .map_err(|error| format!("the server refused to retain the database: {error}"))?;

    // The X resource database is a text property, one `name:\tvalue` per line.
    let database = format!("Xft.dpi:\t{dpi}\n");
    // `check` and not `flush`: `ChangeProperty` has no reply, so a refusal comes
    // back as an error event that nothing would otherwise look at.
    connection
        .change_property8(
            xproto::PropMode::REPLACE,
            root,
            xproto::AtomEnum::RESOURCE_MANAGER,
            xproto::AtomEnum::STRING,
            database.as_bytes(),
        )
        .map_err(|error| format!("change_property: {error}"))?
        .check()
        .map_err(|error| format!("the server refused the resource database: {error}"))?;
    Ok(())
}

/// Prints the `Xft.dpi` the root resource database holds, or 96 where it has none.
///
/// **Not `xprop`.** `xprop -root RESOURCE_MANAGER` reports `not found` against an
/// `Xvfb` whose root demonstrably holds the property — measured, by writing it
/// and reading it back on the same connection. A harness that asked `xprop` got
/// 96 whatever the truth was, so its scale assertions compared a default with a
/// default and could not fail.
fn query_xft_dpi() -> Result<f64, String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    let (connection, screen) =
        x11rb::connect(None).map_err(|error| format!("X11 connect: {error}"))?;
    let root = connection.setup().roots.get(screen).ok_or("no such X screen")?.root;
    let reply = connection
        .get_property(
            false,
            root,
            xproto::AtomEnum::RESOURCE_MANAGER,
            xproto::AtomEnum::STRING,
            0,
            64 * 1024,
        )
        .map_err(|error| format!("get_property: {error}"))?
        .reply()
        .map_err(|error| format!("get_property: {error}"))?;
    let database = String::from_utf8_lossy(&reply.value);
    Ok(database
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .unwrap_or(96.0))
}
