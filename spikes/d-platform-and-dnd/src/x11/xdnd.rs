// SPDX-License-Identifier: Apache-2.0
//! XDND version 5, as a source and as a target.
//!
//! The protocol is a conversation in `ClientMessage`s with the data itself moving
//! through an ordinary selection named `XdndSelection`. Two things about it do not
//! survive translation to Wayland and are worth stating as code: the target is
//! found by *us*, by walking the window tree under the cursor, and
//! `XdndActionLink` exists here and has no counterpart in
//! `wl_data_device_manager.dnd_action`.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, EventMask, PropMode, Window,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

use super::sel::{self, Payload};
use super::{X11, XDND_VERSION};

pub struct Target {
    pub window: Window,
    pub version: u32,
}

/// The topmost window under the pointer that declares `XdndAware`, found by
/// descending the tree. A drag source has to do this itself: X11 has no notion of
/// "the window that would receive this drop".
pub fn target_under_pointer(x: &X11) -> Option<Target> {
    let ptr = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
    let (mut win, root_x, root_y) = (ptr.child, ptr.root_x, ptr.root_y);
    if win == 0 {
        win = x.root;
    }
    // Descend at most a few levels: a GTK application nests its real target
    // several windows deep, and the XdndAware property may be on any of them.
    let mut best = None;
    let mut current = win;
    for _ in 0..8 {
        if let Some(v) = aware_version(x, current) {
            best = Some(Target { window: current, version: v });
        }
        let reply = x.conn.translate_coordinates(x.root, current, root_x, root_y).ok()?.reply().ok()?;
        if reply.child == 0 {
            break;
        }
        current = reply.child;
    }
    best
}

pub fn aware_version(x: &X11, window: Window) -> Option<u32> {
    let r = x
        .conn
        .get_property(false, window, x.atoms.XdndAware, AtomEnum::ATOM, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let v = r.value32()?.next()?;
    (v > 0).then_some(v)
}

pub struct SourceResult {
    pub target: Option<Window>,
    pub target_version: u32,
    pub accepted: bool,
    pub action: Option<String>,
    pub status_messages: u32,
    pub data_requested: Vec<String>,
    pub finished: bool,
    pub finished_accepted: Option<bool>,
}

/// Drive a whole drag as the source. The pointer is moved by the caller — this
/// only speaks the protocol, which is the half a windowing crate does not do.
pub struct Source<'a> {
    x: &'a X11,
    owner: sel::Owner<'a>,
    types: Vec<Atom>,
    pub result: SourceResult,
    current: Option<Target>,
}

impl<'a> Source<'a> {
    pub fn new(x: &'a X11, payloads: Vec<Payload>, action: Atom) -> Option<Self> {
        let types: Vec<Atom> = payloads.iter().map(|p| p.target).collect();
        // XdndEnter carries three types inline; more than that go in a property
        // the target reads back.
        if types.len() > 3 {
            x.conn
                .change_property32(
                    PropMode::REPLACE,
                    x.window,
                    x.atoms.XdndTypeList,
                    Atom::from(AtomEnum::ATOM),
                    &types,
                )
                .ok()?;
        }
        x.conn
            .change_property32(
                PropMode::REPLACE,
                x.window,
                x.atoms.XdndActionList,
                Atom::from(AtomEnum::ATOM),
                &[action],
            )
            .ok()?;
        let owner = sel::Owner::take(x, x.atoms.XdndSelection, payloads)?;
        Some(Self {
            x,
            owner,
            types,
            current: None,
            result: SourceResult {
                target: None,
                target_version: 0,
                accepted: false,
                action: None,
                status_messages: 0,
                data_requested: Vec::new(),
                finished: false,
                finished_accepted: None,
            },
        })
    }

    pub fn enter(&mut self, target: Target) {
        let a = &self.x.atoms;
        let more = u32::from(self.types.len() > 3);
        let mut data = [self.x.window, (XDND_VERSION << 24) | more, 0, 0, 0];
        for (i, t) in self.types.iter().take(3).enumerate() {
            data[2 + i] = *t;
        }
        let _ = self.x.conn.send_event(
            false,
            target.window,
            EventMask::NO_EVENT,
            sel::client_message(target.window, a.XdndEnter, data),
        );
        self.result.target = Some(target.window);
        self.result.target_version = target.version;
        self.current = Some(target);
        let _ = self.x.conn.flush();
    }

    pub fn position(&mut self, root_x: i16, root_y: i16, action: Atom, now: u32) {
        let Some(t) = &self.current else { return };
        let packed = ((root_x as u32) << 16) | (root_y as u32 & 0xffff);
        let _ = self.x.conn.send_event(
            false,
            t.window,
            EventMask::NO_EVENT,
            sel::client_message(
                t.window,
                self.x.atoms.XdndPosition,
                [self.x.window, 0, packed, now, action],
            ),
        );
        let _ = self.x.conn.flush();
    }

    pub fn drop(&mut self, now: u32) {
        let Some(t) = &self.current else { return };
        let _ = self.x.conn.send_event(
            false,
            t.window,
            EventMask::NO_EVENT,
            sel::client_message(t.window, self.x.atoms.XdndDrop, [self.x.window, 0, now, 0, 0]),
        );
        let _ = self.x.conn.flush();
    }

    pub fn leave(&mut self) {
        let Some(t) = &self.current else { return };
        let _ = self.x.conn.send_event(
            false,
            t.window,
            EventMask::NO_EVENT,
            sel::client_message(t.window, self.x.atoms.XdndLeave, [self.x.window, 0, 0, 0, 0]),
        );
        let _ = self.x.conn.flush();
    }

    /// Answer `XdndStatus`, serve the selection request the drop produces, and
    /// collect `XdndFinished`.
    pub fn pump(&mut self, dur: Duration) {
        let end = Instant::now() + dur;
        while Instant::now() < end {
            match self.x.poll() {
                Some(Event::ClientMessage(m)) => {
                    let a = &self.x.atoms;
                    let d = m.data.as_data32();
                    if m.type_ == a.XdndStatus {
                        self.result.status_messages += 1;
                        self.result.accepted = d[1] & 1 == 1;
                        if d[4] != 0 {
                            self.result.action = Some(self.x.atom_name(d[4]));
                        }
                    } else if m.type_ == a.XdndFinished {
                        self.result.finished = true;
                        // Version 5 adds an accepted flag; older targets send
                        // nothing, and treating a missing flag as a refusal is a
                        // real interop bug in the wild.
                        self.result.finished_accepted =
                            (self.result.target_version >= 5).then_some(d[1] & 1 == 1);
                    }
                }
                // Selection traffic belongs to the owner half below, which runs
                // on every turn of this loop.
                Some(other) => self.x.defer(other),
                None => std::thread::sleep(Duration::from_millis(1)),
            }
            // Selection requests are answered by the owner half, which also
            // handles the INCR state machine.
            let before = self.owner.requests.len();
            self.owner.serve(Duration::from_millis(4));
            for r in &self.owner.requests[before..] {
                self.result.data_requested.push(r.clone());
            }
        }
    }
}

pub struct DropReceived {
    pub source: Option<Window>,
    pub version: u32,
    pub types: Vec<String>,
    pub action: Option<String>,
    pub position_messages: u32,
    pub bytes: Vec<u8>,
    pub chosen: Option<String>,
    pub worst_gap_ms: f64,
}

/// Act as a target: answer every `XdndPosition` with an `XdndStatus`, convert the
/// selection on `XdndDrop`, and send `XdndFinished`.
pub fn receive(x: &X11, prefer: &[Atom], wait: Duration) -> Option<DropReceived> {
    let a = &x.atoms;
    let mut got = DropReceived {
        source: None,
        version: 0,
        types: Vec::new(),
        action: None,
        position_messages: 0,
        bytes: Vec::new(),
        chosen: None,
        worst_gap_ms: 0.0,
    };
    let mut type_atoms: Vec<Atom> = Vec::new();
    let mut source: Option<Window> = None;
    let end = Instant::now() + wait;
    let mut last = Instant::now();

    while Instant::now() < end {
        let now = Instant::now();
        got.worst_gap_ms = got.worst_gap_ms.max((now - last).as_secs_f64() * 1000.0);
        last = now;
        match x.poll() {
            Some(Event::ClientMessage(m)) => {
                let d = m.data.as_data32();
                if m.type_ == a.XdndEnter {
                    source = Some(d[0]);
                    got.source = source;
                    got.version = d[1] >> 24;
                    type_atoms = if d[1] & 1 == 1 {
                        x.conn
                            .get_property(false, d[0], a.XdndTypeList, AtomEnum::ATOM, 0, 64)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .and_then(|r| r.value32().map(|v| v.collect()))
                            .unwrap_or_default()
                    } else {
                        d[2..5].iter().copied().filter(|t| *t != 0).collect()
                    };
                    got.types = type_atoms.iter().map(|t| x.atom_name(*t)).collect();
                } else if m.type_ == a.XdndPosition {
                    got.position_messages += 1;
                    got.action = Some(x.atom_name(d[4]));
                    let accept = type_atoms.iter().any(|t| prefer.contains(t));
                    let action = if accept { a.XdndActionCopy } else { 0 };
                    let _ = x.conn.send_event(
                        false,
                        d[0],
                        EventMask::NO_EVENT,
                        sel::client_message(
                            d[0],
                            a.XdndStatus,
                            // l[2] and l[3] are a rectangle in which the source
                            // need not send further positions; zero means "keep
                            // telling me", which is what a list view wants.
                            [x.window, u32::from(accept), 0, 0, action],
                        ),
                    );
                    let _ = x.conn.flush();
                } else if m.type_ == a.XdndLeave {
                    source = None;
                } else if m.type_ == a.XdndDrop {
                    let chosen = prefer.iter().find(|t| type_atoms.contains(t)).copied();
                    if let Some(t) = chosen {
                        got.chosen = Some(x.atom_name(t));
                        if let Some(r) = sel::request(x, a.XdndSelection, t, Duration::from_secs(3)) {
                            got.bytes = r.bytes;
                            got.worst_gap_ms = got.worst_gap_ms.max(r.worst_gap_ms);
                        }
                    }
                    if let Some(src) = source.or(Some(d[0])) {
                        let _ = x.conn.send_event(
                            false,
                            src,
                            EventMask::NO_EVENT,
                            sel::client_message(
                                src,
                                a.XdndFinished,
                                [x.window, u32::from(!got.bytes.is_empty()), a.XdndActionCopy, 0, 0],
                            ),
                        );
                        let _ = x.conn.flush();
                    }
                    return Some(got);
                }
            }
            Some(other) => x.defer(other),
            None => std::thread::sleep(Duration::from_micros(500)),
        }
    }
    (got.source.is_some()).then_some(got)
}
