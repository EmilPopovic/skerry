// SPDX-License-Identifier: MIT OR Apache-2.0
//! `text-input-v3`: the client half of an input method.
//!
//! **What this is for.** Compose sequences need no protocol and are handled in
//! `crate::input` for both display servers. This is the other kind: a Chinese,
//! Japanese or Korean input method is a *separate process* that shows a
//! candidate window, and the compositor is the go-between. Without it those
//! keyboards type nothing at all into this window.
//!
//! **Three rules the protocol states and a client can get wrong silently:**
//!
//! * The events between one `done` and the next are **one change**, applied in
//!   the order `delete_surrounding_text`, `commit_string`, `preedit_string` —
//!   not the order they arrived in. They leave here as one
//!   [`TextChange`](crate::event::TextChange) so a caller cannot.
//! * `done` carries the number of `commit` requests the compositor has seen. A
//!   mismatch does **not** mean the change is stale and droppable — the
//!   specification says the client "must proceed with evaluating and applying
//!   the changes as normal, except it should not change the current state of the
//!   object". Reading it the other way is a client that silently types nothing,
//!   which is what the first draft of this file did and what the input-method
//!   peer caught on its first run.
//! * An enabled text input is a request to *show* an input method. Enabled at
//!   start-up, it would put a candidate window over a file list nobody is typing
//!   into — so it is enabled when a field takes the keyboard and disabled when
//!   it gives it back.

use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};

use crate::event::{Event, TextChange};

use super::State;

/// The text input, and the change being assembled for the next `done`.
pub(super) struct TextInput {
    object: zwp_text_input_v3::ZwpTextInputV3,
    /// How many `commit` requests have been sent, which is the serial the next
    /// `done` must carry to be about the current text.
    serial: u32,
    /// Whether the compositor has said this surface has the text-input focus.
    ///
    /// **Nothing may be sent before it does.** `enter` is the compositor
    /// offering the conversation and `leave` is it taking the offer back; a
    /// client that enabled without one gets a `leave` for its trouble, which is
    /// what the first draft of this did and why it went deaf the moment an input
    /// method appeared.
    entered: bool,
    /// Whether `enable` has been sent since the last `enter`.
    enabled: bool,
    /// What has arrived since the last `done`.
    pending: TextChange,
    /// Where the caret was last reported to be, so an unchanged answer sends
    /// nothing.
    area: Option<TextArea>,
    /// Where it should be next, when that could not be sent yet.
    wanted: Option<TextArea>,
    /// Whether the last `done` matched the commits sent.
    ///
    /// The other half of the serial rule: pending state "should be sent and
    /// committed after receiving a `done` event with a matching serial", so a
    /// cursor rectangle raised while the compositor is behind waits here.
    settled: bool,
}

/// Where the text being edited is, so the candidate window can sit beside it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct TextArea {
    /// The caret's left edge, in surface-local logical pixels.
    pub x: i32,
    /// Its top edge.
    pub y: i32,
    /// How wide it is.
    pub width: i32,
    /// How tall it is.
    pub height: i32,
}

impl TextInput {
    pub(super) fn new(
        manager: &zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        seat: &wl_seat::WlSeat,
        handle: &QueueHandle<State>,
    ) -> Self {
        Self {
            object: manager.get_text_input(seat, handle, ()),
            serial: 0,
            entered: false,
            enabled: false,
            pending: TextChange::default(),
            area: None,
            wanted: None,
            settled: true,
        }
    }

    /// Says whether a field has the keyboard, and where its caret is.
    ///
    /// Idempotent: called every frame with the same answer, it sends nothing.
    /// The protocol counts every `commit`, and a commit per frame would make the
    /// serial race the input method's replies.
    pub(super) fn focus(&mut self, area: Option<TextArea>) {
        // Nothing may be said before the compositor has offered the
        // conversation. The wanted area is kept, and the next `enter` sends it.
        if !self.entered {
            self.wanted = area;
            return;
        }
        match (self.enabled, area) {
            (false, Some(area)) => {
                self.enabled = true;
                self.object.enable();
                self.wanted = Some(area);
                // Straight through: enabling is a state change of the client's
                // own, and holding the first cursor rectangle back would leave
                // the candidate window in the corner until the caret moved.
                self.flush();
            }
            (true, Some(area)) => {
                self.wanted = Some(area);
                if self.settled {
                    self.flush();
                }
            }
            (true, None) => {
                self.enabled = false;
                self.area = None;
                self.wanted = None;
                self.object.disable();
                self.commit();
            }
            (false, None) => {}
        }
    }

    /// Sends the cursor rectangle the caller asked for, if it is new.
    fn flush(&mut self) {
        let Some(area) = self.wanted.take() else { return };
        if self.area == Some(area) {
            return;
        }
        self.area = Some(area);
        self.object.set_cursor_rectangle(area.x, area.y, area.width, area.height);
        self.commit();
    }

    fn commit(&mut self) {
        self.object.commit();
        self.serial = self.serial.wrapping_add(1);
        self.settled = false;
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(input) = state.text_input.as_mut() else { return };
        match event {
            zwp_text_input_v3::Event::PreeditString { text, cursor_begin, cursor_end } => {
                input.pending.preedit = text.unwrap_or_default();
                // Negative means "hide the cursor", which is a method saying it
                // has no opinion about where inside the preedit the caret is.
                input.pending.preedit_cursor = (cursor_begin >= 0 && cursor_end >= 0)
                    .then(|| (cursor_begin.unsigned_abs(), cursor_end.unsigned_abs()));
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                input.pending.commit = text.unwrap_or_default();
            }
            zwp_text_input_v3::Event::DeleteSurroundingText { before_length, after_length } => {
                input.pending.delete = (before_length, after_length);
            }
            zwp_text_input_v3::Event::Done { serial } => {
                let change = std::mem::take(&mut input.pending);
                // **Applied whatever the serial says.** The specification is
                // explicit that a mismatch does not make the change stale: what
                // it withholds is the client's *own* pending state, which waits
                // for a `done` that matches.
                //
                // **Untested, and it says so.** The scenario cannot arrange for
                // the compositor to answer a stale commit, so the branch below
                // rests on the specification's wording and not on a run. What is
                // tested is the ordinary path, and that a resize mid-composition
                // does not lose the commit.
                input.settled = serial == input.serial;
                if input.settled {
                    input.flush();
                }
                if !change.is_empty() {
                    state.events.push_back(Event::Composing(change));
                }
            }
            // **A `leave` disables the text input, and the client has to enable
            // it again.** Both events are the compositor saying which surface
            // the input method follows, and an input method appearing while a
            // field is open produces a `leave` and then an `enter` — measured,
            // with Hyprland. A client that treated them as nothing enabled once
            // at the start and was deaf from the moment an input method was
            // started.
            //
            // Forgotten rather than re-enabled here: `focus` runs every pass and
            // will put it back on the next one, which is the same self-healing
            // shape the directory watcher uses.
            // The conversation is offered and withdrawn by these two, and an
            // input method starting while a field is open produces both —
            // measured, with Hyprland. Everything is forgotten rather than
            // re-established here: `focus` runs every pass and puts it back on
            // the next one, which is the self-healing shape the directory
            // watcher uses for the same reason.
            zwp_text_input_v3::Event::Enter { .. } => {
                input.entered = true;
                input.enabled = false;
                input.area = None;
                input.pending = TextChange::default();
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                input.entered = false;
                input.enabled = false;
                input.area = None;
                input.pending = TextChange::default();
            }
            _ => {}
        }
    }
}
