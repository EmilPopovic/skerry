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
//! Two protocols, both wlroots': `zwp_virtual_keyboard_manager_v1` and
//! `zwlr_virtual_pointer_manager_v1`. Measured on this machine, `KWin` advertises
//! neither — so this fixture is Hyprland-only, and the portability answer for
//! `KWin` has to come from running the product there by hand.

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

mod script;

use script::{Action, Button};

/// Where the pointer's absolute coordinates are expressed against.
///
/// The virtual-pointer protocol takes a position and an extent rather than a
/// pixel, so a script says "40% across" and does not have to know the output.
const EXTENT: u32 = 10_000;

fn main() -> std::process::ExitCode {
    let mut display = None;
    let mut path = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--display" => display = arguments.next(),
            "--help" | "-h" => {
                println!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            other => path = Some(other.to_owned()),
        }
    }
    if let Some(display) = display {
        // SAFETY: single-threaded, before any thread is started.
        unsafe { std::env::set_var("WAYLAND_DISPLAY", display) };
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

    match Driver::connect() {
        Ok(mut driver) => {
            driver.play(&actions);
            std::process::ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("skerry-drive: {problem}");
            std::process::ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
skerry-drive [--display SOCKET] [SCRIPT]

Replays input into a compositor. The script is read from SCRIPT or from standard
input. Point it at a nested compositor: synthetic input goes wherever the focus
is.

  key ctrl+shift+c      press and release a chord
  text hello            type a string
  move 50 30            move the pointer, in percent of the output
  click                 press and release the left button
  click right           any of left, right, middle
  double                two clicks inside the double-click window
  drag 10 20 80 90      press at the first point, move to the second, release
  scroll -3             wheel notches, negative is up
  sleep 250             milliseconds";

struct Driver {
    queue: EventQueue<State>,
    state: State,
    keyboard: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    time: u32,
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

        Ok(Self { queue, state, keyboard, pointer, time: 1 })
    }

    fn play(&mut self, actions: &[Action]) {
        for action in actions {
            match action {
                Action::Key { modifiers, mask, code } => self.chord(modifiers, *mask, *code),
                Action::Move { x, y } => self.move_to(*x, *y),
                Action::Click { button, times } => self.click(*button, *times),
                Action::Drag { from, to } => self.drag(*from, *to),
                Action::Scroll { notches } => self.scroll(*notches),
                Action::Sleep { milliseconds } => {
                    self.flush();
                    std::thread::sleep(Duration::from_millis(u64::from(*milliseconds)));
                }
            }
            self.flush();
        }
        // A last round trip, so the compositor has seen everything before the
        // process exits and its virtual devices are destroyed.
        let _ = self.queue.roundtrip(&mut self.state);
    }

    /// Advances the clock.
    ///
    /// Milliseconds, monotonically. The click counter under test decides a double
    /// click from these timestamps, so a driver that sent the same one twice
    /// would be testing its own bug.
    fn tick(&mut self) -> u32 {
        self.time = self.time.wrapping_add(8);
        self.time
    }

    fn chord(&mut self, modifiers: &[u32], mask: u32, code: u32) {
        for held in modifiers {
            let time = self.tick();
            self.keyboard.key(time, *held, 1);
        }
        // And say so explicitly. `zwp_virtual_keyboard_v1` does not derive the
        // modifier state from the keycodes it was sent, so a client that only
        // presses `Ctrl` sees a plain `h` arrive at the far end.
        if mask != 0 {
            self.keyboard.modifiers(mask, 0, 0, 0);
        }
        let time = self.tick();
        self.keyboard.key(time, code, 1);
        let time = self.tick();
        self.keyboard.key(time, code, 0);
        if mask != 0 {
            self.keyboard.modifiers(0, 0, 0, 0);
        }
        for held in modifiers.iter().rev() {
            let time = self.tick();
            self.keyboard.key(time, *held, 0);
        }
    }

    fn move_to(&mut self, x: u32, y: u32) {
        let time = self.tick();
        self.pointer.motion_absolute(time, x, y, EXTENT, EXTENT);
        self.pointer.frame();
    }

    fn click(&mut self, button: Button, times: u32) {
        for _ in 0..times {
            let time = self.tick();
            self.pointer.button(time, button.code(), wl_pointer::ButtonState::Pressed);
            self.pointer.frame();
            let time = self.tick();
            self.pointer.button(time, button.code(), wl_pointer::ButtonState::Released);
            self.pointer.frame();
        }
    }

    fn drag(&mut self, from: (u32, u32), to: (u32, u32)) {
        self.move_to(from.0, from.1);
        let time = self.tick();
        self.pointer.button(time, Button::Left.code(), wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        // Several steps, because a rubber band is a gesture and one jump is a
        // click that happened to end somewhere else.
        for step in 1..=8i64 {
            // Signed, because a drag upward has a negative delta and the
            // unsigned version wrapped to four billion — which is a driver bug
            // that reads exactly like a rubber band that selects nothing.
            let along = |start: u32, end: u32| {
                (i64::from(start) + (i64::from(end) - i64::from(start)) * step / 8).max(0) as u32
            };
            self.move_to(along(from.0, to.0), along(from.1, to.1));
        }
        let time = self.tick();
        self.pointer.button(time, Button::Left.code(), wl_pointer::ButtonState::Released);
        self.pointer.frame();
    }

    /// One wheel notch per step, as a discrete axis with a source.
    ///
    /// **`axis` alone produced nothing.** Hyprland forwarded a bare
    /// `virtual_pointer.axis` to the client as `wl_pointer.axis` with a value of
    /// **zero**, and `axis_value120` zero beside it, so twelve notches scrolled
    /// nothing at all — and the scenario that was supposed to cover this returned
    /// early on a fixture smaller than the window, so nothing ever failed.
    /// `axis_discrete` with `axis_source` is what a real wheel sends and what the
    /// compositor translates.
    fn scroll(&mut self, notches: i32) {
        for _ in 0..notches.abs() {
            let time = self.tick();
            let steps = if notches < 0 { -1 } else { 1 };
            let distance = f64::from(steps) * 15.0;
            self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
            self.pointer.axis_discrete(time, wl_pointer::Axis::VerticalScroll, distance, steps);
            self.pointer.frame();
        }
    }

    fn flush(&mut self) {
        let _ = self.queue.flush();
        let _ = self.queue.dispatch_pending(&mut self.state);
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
