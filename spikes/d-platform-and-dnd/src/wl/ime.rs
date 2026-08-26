//! `zwp_text_input_v3` on the application side, and `zwp_input_method_v2` on the
//! other side of the same connection, because this machine has no input method
//! installed and the bar says CJK text composes into a field.
//!
//! Acting as both ends proves our client half end to end: the compositor routes a
//! real preedit and a real commit through its own text-input implementation. It
//! does not prove fcitx5 or ibus interop, which is recorded as owed rather than
//! implied.

use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2, zwp_input_method_v2,
};

use super::App;

/// Hiragana in, kanji out — the shape of every Japanese input method, and the
/// reason a text field cannot simply append keystrokes.
pub const PREEDIT: &str = "にほんご";
pub const COMMIT: &str = "日本語";

#[derive(Default)]
pub struct ImeState {
    pub text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    pub method: Option<zwp_input_method_v2::ZwpInputMethodV2>,

    pub entered: bool,
    pub preedit: String,
    pub committed: String,
    pub preedit_cursor: (i32, i32),
    pub done_serial: u32,

    pub method_active: bool,
    pub method_serial: u32,
    pub method_unavailable: bool,
}

impl ImeState {
    pub fn enable(
        &mut self,
        mgr: &zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<App>,
    ) {
        let ti = mgr.get_text_input(seat, qh, ());
        ti.enable();
        ti.set_content_type(
            zwp_text_input_v3::ContentHint::None,
            zwp_text_input_v3::ContentPurpose::Normal,
        );
        // Where the compositor may put a candidate popup. A rename field that
        // does not send this gets the popup in the corner of the screen.
        ti.set_cursor_rectangle(16, 96, 320, 24);
        ti.commit();
        self.text_input = Some(ti);
    }

    pub fn bind_method(
        &mut self,
        mgr: &zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<App>,
    ) {
        self.method = Some(mgr.get_input_method(seat, qh, ()));
    }

    /// The input-method half of a composition: a preedit, then the commit that
    /// replaces it. `commit(serial)` is what makes either visible.
    pub fn compose(&self) {
        let Some(m) = &self.method else { return };
        m.set_preedit_string(PREEDIT.into(), 0, PREEDIT.len() as i32);
        m.commit(self.method_serial);
    }

    pub fn finish(&self) {
        let Some(m) = &self.method else { return };
        m.set_preedit_string(String::new(), 0, 0);
        m.commit_string(COMMIT.into());
        m.commit(self.method_serial);
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { .. } => app.ime.entered = true,
            zwp_text_input_v3::Event::Leave { .. } => app.ime.entered = false,
            zwp_text_input_v3::Event::PreeditString { text, cursor_begin, cursor_end } => {
                app.ime.preedit = text.unwrap_or_default();
                app.ime.preedit_cursor = (cursor_begin, cursor_end);
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                app.ime.committed.push_str(&text.unwrap_or_default());
            }
            zwp_text_input_v3::Event::Done { serial } => {
                app.ime.done_serial = serial;
                app.paint.preedit_chars = app.ime.preedit.chars().count();
                app.paint.committed_chars = app.ime.committed.chars().count();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => app.ime.method_active = true,
            zwp_input_method_v2::Event::Deactivate => app.ime.method_active = false,
            zwp_input_method_v2::Event::Done => app.ime.method_serial += 1,
            zwp_input_method_v2::Event::Unavailable => app.ime.method_unavailable = true,
            _ => {}
        }
    }
}
