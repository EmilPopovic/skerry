// SPDX-License-Identifier: MIT OR Apache-2.0
//! An input method, for testing the window's `text-input-v3` half.
//!
//! **The only way that code can be tested at all.** A real input method is a
//! separate process that the compositor talks to; without one, everything the
//! window does with `zwp_text_input_v3` is a protocol conversation with nobody,
//! and a client that dropped every event would look exactly like one that works.
//!
//! It speaks `zwp_input_method_v2`, which is what ibus, fcitx and every wlroots
//! input method use. It waits for the window to enable its text input, sends a
//! preedit, holds it, then commits — which is the shape of typing one character
//! of Japanese, and the shape a scenario can sample twice.

use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2, zwp_input_method_v2,
};

/// What to type, and for how long the preedit is held before it commits.
pub(crate) struct Typing {
    pub(crate) preedit: String,
    pub(crate) commit: String,
    pub(crate) hold: Duration,
}

/// The peer's state while it runs.
struct Method {
    /// How many `done` events have arrived, which is the serial a `commit` must
    /// carry. The protocol counts them, so a peer that sent a constant would
    /// have every commit ignored — silently.
    serial: u32,
    /// Whether the window has a field with the keyboard.
    active: bool,
    /// Whether the compositor refused: another input method already has the
    /// seat, which is one per seat.
    unavailable: bool,
}

/// Prints every global the compositor advertises, with its version.
///
/// "Why did that not work" needs an answer that does not need a second run: an
/// input method that finds no manager and a window that finds no `text-input`
/// are the same one-line question.
pub(crate) fn list_globals() -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, _queue) = registry_queue_init::<Method>(&connection)
        .map_err(|error| format!("wl_registry: {error}"))?;
    for global in globals.contents().clone_list() {
        // skerry-allow: log-prefix — the answer to `--globals`, not a log line
        println!("{} v{}", global.interface, global.version);
    }
    Ok(())
}

/// Types `what` into whatever window has a text input enabled.
///
/// Returns the reason it could not, or nothing if it did.
pub(crate) fn type_into_the_window(what: &Typing) -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, mut queue) = registry_queue_init::<Method>(&connection)
        .map_err(|error| format!("wl_registry: {error}"))?;
    let handle = queue.handle();
    let seat: wl_seat::WlSeat =
        globals.bind(&handle, 1..=9, ()).map_err(|_| "no wl_seat".to_owned())?;
    let manager: zwp_input_method_manager_v2::ZwpInputMethodManagerV2 = globals
        .bind(&handle, 1..=1, ())
        .map_err(|_| "the compositor offers no zwp_input_method_manager_v2".to_owned())?;
    let method = manager.get_input_method(&seat, &handle, ());

    let mut state = Method { serial: 0, active: false, unavailable: false };
    // Waits for the window to enable its text input. A method that sent a
    // preedit before that would be sending it to a compositor with nowhere to
    // put it, and the failure would look like the window ignoring it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.active && Instant::now() < deadline {
        queue.blocking_dispatch(&mut state).map_err(|error| error.to_string())?;
        if state.unavailable {
            return Err("another input method already has the seat".to_owned());
        }
    }
    if !state.active {
        return Err("no window enabled a text input within five seconds".to_owned());
    }

    if !what.preedit.is_empty() {
        let end = i32::try_from(what.preedit.len()).unwrap_or(0);
        method.set_preedit_string(what.preedit.clone(), end, end);
        method.commit(state.serial);
        let _ = connection.flush();
        let until = Instant::now() + what.hold;
        while Instant::now() < until {
            queue.roundtrip(&mut state).map_err(|error| error.to_string())?;
            std::thread::yield_now();
        }
    }
    if !what.commit.is_empty() {
        // The preedit is cleared by the same commit that replaces it: an empty
        // preedit string is how the protocol says "nothing pending", and a
        // method that left the old one would show the text twice.
        method.set_preedit_string(String::new(), 0, 0);
        method.commit_string(what.commit.clone());
        method.commit(state.serial);
        let _ = connection.flush();
        queue.roundtrip(&mut state).map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for Method {
    fn event(
        state: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => state.active = true,
            zwp_input_method_v2::Event::Deactivate => state.active = false,
            zwp_input_method_v2::Event::Done => state.serial = state.serial.wrapping_add(1),
            zwp_input_method_v2::Event::Unavailable => state.unavailable = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Method {
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

impl Dispatch<wl_seat::WlSeat, ()> for Method {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_input_method_manager_v2::ZwpInputMethodManagerV2, ()> for Method {
    fn event(
        _: &mut Self,
        _: &zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        _: zwp_input_method_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Unused, and required: `registry_queue_init` needs the state to be able to
/// receive from the registry it built.
#[expect(dead_code, reason = "the bound registry_queue_init requires")]
type Bound = GlobalList;
