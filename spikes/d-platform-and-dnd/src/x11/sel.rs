// SPDX-License-Identifier: MIT OR Apache-2.0
//! ICCCM selections: owning `CLIPBOARD` and `PRIMARY`, answering `TARGETS`, and
//! the `INCR` protocol for anything that will not fit in one request.
//!
//! `INCR` is the part everyone skips. A file manager reaches it easily — a
//! selection of a few thousand files is a megabyte of `text/uri-list` — and a
//! target that does not implement it receives a truncated list or nothing at all.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, GetPropertyReply, Property,
    PropMode, SelectionNotifyEvent, SelectionRequestEvent, Window,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

use super::X11;

/// ICCCM says to switch to `INCR` above a quarter of the maximum request size.
/// Fixing it at 256 KB instead makes the threshold a property of our code rather
/// than of whether BIG-REQUESTS happened to be negotiated, which is what a test
/// needs.
pub const INCR_THRESHOLD: usize = 256 * 1024;

pub struct Payload {
    pub target: Atom,
    pub bytes: Vec<u8>,
}

/// One in-flight incremental send. The owner cannot write the next chunk until the
/// requestor deletes the property, so this is a state machine and not a loop.
struct IncrSend {
    requestor: Window,
    property: Atom,
    target: Atom,
    bytes: Vec<u8>,
    sent: usize,
}

pub struct Owner<'a> {
    x: &'a X11,
    pub selection: Atom,
    payloads: Vec<Payload>,
    incr: Vec<IncrSend>,
    pub requests: Vec<String>,
    pub refused: Vec<String>,
    pub incr_chunks: usize,
}

impl<'a> Owner<'a> {
    pub fn take(x: &'a X11, selection: Atom, payloads: Vec<Payload>) -> Option<Self> {
        let time = x.server_time();
        x.conn.set_selection_owner(x.window, selection, time).ok()?;
        x.conn.flush().ok()?;
        let owner = x.conn.get_selection_owner(selection).ok()?.reply().ok()?.owner;
        if owner != x.window {
            return None;
        }
        Some(Self {
            x,
            selection,
            payloads,
            incr: Vec::new(),
            requests: Vec::new(),
            refused: Vec::new(),
            incr_chunks: 0,
        })
    }

    pub fn targets(&self) -> Vec<Atom> {
        let mut v = vec![self.x.atoms.TARGETS, self.x.atoms.TIMESTAMP];
        v.extend(self.payloads.iter().map(|p| p.target));
        v
    }

    /// Answer whatever arrives for `duration`. Returns the number of requests
    /// served, so a check can tell "nobody asked" from "we refused".
    pub fn serve(&mut self, duration: Duration) -> usize {
        let end = Instant::now() + duration;
        let mut served = 0;
        let mut put_back = Vec::new();
        while Instant::now() < end {
            match self.x.poll() {
                Some(Event::SelectionRequest(req)) => {
                    if self.handle(&req) {
                        served += 1;
                    }
                }
                Some(Event::PropertyNotify(e)) => self.continue_incr(e.window, e.atom, e.state),
                Some(other) => put_back.push(other),
                None => {
                    let _ = self.x.conn.flush();
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        for e in put_back {
            self.x.defer(e);
        }
        served
    }

    fn handle(&mut self, req: &SelectionRequestEvent) -> bool {
        let a = &self.x.atoms;
        self.requests.push(self.x.atom_name(req.target));
        let property = if req.property == 0 { req.target } else { req.property };

        let ok = if req.target == a.TARGETS {
            let targets = self.targets();
            self.x
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    property,
                    Atom::from(AtomEnum::ATOM),
                    &targets,
                )
                .is_ok()
        } else if req.target == a.TIMESTAMP {
            self.x
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    property,
                    Atom::from(AtomEnum::INTEGER),
                    &[req.time],
                )
                .is_ok()
        } else if let Some(p) = self.payloads.iter().find(|p| p.target == req.target) {
            if p.bytes.len() > INCR_THRESHOLD {
                self.begin_incr(req.requestor, property, req.target, p.bytes.clone())
            } else {
                self.x
                    .conn
                    .change_property8(PropMode::REPLACE, req.requestor, property, req.target, &p.bytes)
                    .is_ok()
            }
        } else {
            self.refused.push(self.x.atom_name(req.target));
            false
        };

        // ICCCM: a refusal is a SelectionNotify with property None. Silence is a
        // protocol violation, and the requestor waits for its full timeout.
        let notify = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: if ok { property } else { 0 },
        };
        let _ = self.x.conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
        let _ = self.x.conn.flush();
        ok
    }

    fn begin_incr(&mut self, requestor: Window, property: Atom, target: Atom, bytes: Vec<u8>) -> bool {
        // The requestor has to be watched for the property deletions that drive
        // the transfer; without this the first chunk is also the last.
        let _ = self.x.conn.change_window_attributes(
            requestor,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE),
        );
        let total = bytes.len() as u32;
        let ok = self
            .x
            .conn
            .change_property32(PropMode::REPLACE, requestor, property, self.x.atoms.INCR, &[total])
            .is_ok();
        self.incr.push(IncrSend { requestor, property, target, bytes, sent: 0 });
        ok
    }

    fn continue_incr(&mut self, window: Window, atom: Atom, state: Property) {
        if std::env::var_os("SPIKE_D_TRACE").is_some() {
            eprintln!(
                "    owner: PropertyNotify window {window:#x} atom {atom} state {state:?}; \
                 {} transfers in flight",
                self.incr.len()
            );
        }
        if state != Property::DELETE {
            return;
        }
        let chunk = INCR_THRESHOLD / 4;
        let mut finished = Vec::new();
        for (i, s) in self.incr.iter_mut().enumerate() {
            if s.requestor != window || s.property != atom {
                continue;
            }
            let end = (s.sent + chunk).min(s.bytes.len());
            let slice = &s.bytes[s.sent..end];
            let _ = self.x.conn.change_property8(
                PropMode::REPLACE,
                s.requestor,
                s.property,
                s.target,
                slice,
            );
            self.incr_chunks += 1;
            if slice.is_empty() {
                finished.push(i);
            }
            s.sent = end;
        }
        for i in finished.into_iter().rev() {
            self.incr.remove(i);
        }
        let _ = self.x.conn.flush();
    }

    pub fn release(&self) {
        let _ = self.x.conn.set_selection_owner(0u32, self.selection, x11rb::CURRENT_TIME);
        let _ = self.x.conn.flush();
    }
}

pub struct Received {
    pub bytes: Vec<u8>,
    pub incr: bool,
    pub chunks: usize,
    /// Longest gap between two turns of our own loop while the transfer was in
    /// flight — rule 1's number, on the X11 side.
    pub worst_gap_ms: f64,
}

/// Request a selection and read it, following `INCR` if the owner starts one.
pub fn request(x: &X11, selection: Atom, target: Atom, timeout: Duration) -> Option<Received> {
    let property = x.atoms._SKERRY_SELECTION_IN;
    let _ = x.conn.delete_property(x.window, property);
    // ICCCM says a requestor should use the timestamp of the event that caused
    // the request. `SPIKE_D_CURRENT_TIME` exists to test whether a particular
    // owner cares, because a timestamp an owner rejects is indistinguishable from
    // an owner that never answers.
    let time = if std::env::var_os("SPIKE_D_CURRENT_TIME").is_some() {
        x11rb::CURRENT_TIME
    } else {
        x.server_time()
    };
    x.conn.convert_selection(x.window, selection, target, property, time).ok()?;
    x.conn.flush().ok()?;

    let mut worst = 0.0f64;
    let mut last = Instant::now();
    let end = Instant::now() + timeout;
    let mut notified = false;
    let mut refused = false;
    let mut put_back = Vec::new();
    while Instant::now() < end {
        let now = Instant::now();
        worst = worst.max((now - last).as_secs_f64() * 1000.0);
        last = now;
        match x.poll() {
            Some(Event::SelectionNotify(e)) if e.requestor == x.window => {
                refused = e.property == 0;
                notified = true;
                break;
            }
            Some(other) => {
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    eprintln!("    trace: {other:?}");
                }
                put_back.push(other);
            }
            None => std::thread::sleep(Duration::from_micros(500)),
        }
    }
    for e in put_back {
        x.defer(e);
    }
    if refused {
        return Some(Received { bytes: Vec::new(), incr: false, chunks: 0, worst_gap_ms: worst });
    }
    if !notified {
        return None;
    }

    let head = get(x, property, false)?;
    if head.type_ != x.atoms.INCR {
        let bytes = head.value;
        let _ = x.conn.delete_property(x.window, property);
        return Some(Received { bytes, incr: false, chunks: 1, worst_gap_ms: worst });
    }

    // INCR: deleting the property is the acknowledgement that unblocks the next
    // chunk, and a zero-length chunk is the end.
    let mut bytes = Vec::new();
    let mut chunks = 0usize;
    let _ = x.conn.delete_property(x.window, property);
    let _ = x.conn.flush();
    // Everything queued for this property predates the delete and is therefore
    // about the header, not about a chunk.
    x.forget_property_notifies(x.window, property);
    let end = Instant::now() + timeout;
    // Deferring back into the queue this loop polls from would spin on the same
    // events forever; unrelated events go into a local list and are put back once.
    let mut put_back = Vec::new();
    while Instant::now() < end {
        let now = Instant::now();
        worst = worst.max((now - last).as_secs_f64() * 1000.0);
        last = now;
        match x.poll() {
            Some(Event::PropertyNotify(e))
                if e.window == x.window && e.atom == property && e.state == Property::NEW_VALUE =>
            {
                let part = get(x, property, false)?;
                let n = part.value.len();
                let _ = x.conn.delete_property(x.window, property);
                let _ = x.conn.flush();
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    eprintln!("    reader: chunk of {n} bytes, {} so far", bytes.len());
                }
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&part.value);
                chunks += 1;
            }
            Some(other) => {
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    if let Event::PropertyNotify(p) = &other {
                        eprintln!(
                            "    reader: not ours — window {:#x} (we are {:#x}) atom {} (want {}) state {:?}",
                            p.window, x.window, p.atom, property, p.state
                        );
                    }
                }
                put_back.push(other);
            }
            None => std::thread::sleep(Duration::from_micros(200)),
        }
    }
    for e in put_back {
        x.defer(e);
    }
    Some(Received { bytes, incr: true, chunks, worst_gap_ms: worst })
}

fn get(x: &X11, property: Atom, delete: bool) -> Option<GetPropertyReply> {
    x.conn
        .get_property(delete, x.window, property, AtomEnum::ANY, 0, u32::MAX / 4)
        .ok()?
        .reply()
        .ok()
}

pub fn client_message(window: Window, kind: Atom, data: [u32; 5]) -> ClientMessageEvent {
    ClientMessageEvent::new(32, window, kind, data)
}
