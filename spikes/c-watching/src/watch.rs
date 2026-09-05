// SPDX-License-Identifier: Apache-2.0
//! Establishing and holding inotify watches.
//!
//! The interesting property is not latency, it is arithmetic: `max_user_watches`
//! is 65,536 on this machine and a single million-file tree holds 21,338
//! directories. Three such trees exhaust the budget, and the kernel's answer when
//! they do is `ENOSPC` on `inotify_add_watch` — a per-call error, not a signal,
//! so a program that ignores it silently stops seeing changes in the directories
//! it failed to register.

use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::inotify;

/// Everything a listing needs to be told about. `MOVED_FROM`/`MOVED_TO` carry a
/// cookie that pairs them, which is how a rename stays a rename instead of
/// becoming a delete and a create.
pub fn interest() -> inotify::WatchFlags {
    inotify::WatchFlags::CREATE
        | inotify::WatchFlags::DELETE
        | inotify::WatchFlags::MOVED_FROM
        | inotify::WatchFlags::MOVED_TO
        | inotify::WatchFlags::MODIFY
        | inotify::WatchFlags::ATTRIB
        | inotify::WatchFlags::DELETE_SELF
        | inotify::WatchFlags::MOVE_SELF
}

pub struct Watcher {
    fd: OwnedFd,
    /// Watch descriptor to path, so an event can be attributed to a directory.
    /// This is the per-watch memory *we* pay, on top of the kernel's.
    paths: HashMap<i32, PathBuf>,
    pub added: usize,
    pub refused: usize,
    /// The errno that stopped us, if anything did. `ENOSPC` means the watch
    /// budget; `EMFILE` means the instance limit.
    pub stopped_by: Option<rustix::io::Errno>,
}

impl Watcher {
    pub fn new() -> rustix::io::Result<Self> {
        Ok(Self {
            fd: inotify::init(inotify::CreateFlags::NONBLOCK)?,
            paths: HashMap::new(),
            added: 0,
            refused: 0,
            stopped_by: None,
        })
    }

    /// Adds a watch, recording rather than propagating a refusal: running out of
    /// watches is a condition to report and degrade from, not an error to abort
    /// on, because aborting would mean showing the user nothing at all.
    pub fn watch(&mut self, directory: &Path) -> bool {
        match inotify::add_watch(&self.fd, directory, interest()) {
            Ok(wd) => {
                self.paths.insert(wd, directory.to_path_buf());
                self.added += 1;
                true
            }
            Err(errno) => {
                self.refused += 1;
                self.stopped_by.get_or_insert(errno);
                false
            }
        }
    }

    pub fn path_of(&self, wd: i32) -> Option<&Path> {
        self.paths.get(&wd).map(PathBuf::as_path)
    }

    pub fn watch_count(&self) -> usize {
        self.paths.len()
    }

    /// Drains whatever is queued, up to `budget` events, and reports whether the
    /// kernel told us it had dropped some.
    ///
    /// Coalescing is the caller's job: this returns raw events so the harness can
    /// measure the arrival pattern rather than a smoothed version of it.
    pub fn drain(&self, budget: usize, wait: Duration) -> Drained {
        let mut drained = Drained::default();
        let deadline = Instant::now() + wait;
        let mut buffer = vec![MaybeUninit::<u8>::uninit(); 64 * 1024];

        loop {
            let mut reader = inotify::Reader::new(&self.fd, buffer.as_mut_slice());
            loop {
                match reader.next() {
                    Ok(event) => {
                        if event.events().contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
                            drained.overflowed = true;
                        } else {
                            drained.events += 1;
                            if drained.first.is_none() {
                                drained.first = Some(Instant::now());
                            }
                            drained.last = Some(Instant::now());
                            if drained.touched.len() < 4096 {
                                drained.touched.push(event.wd());
                            }
                        }
                        if drained.events >= budget {
                            return drained;
                        }
                    }
                    // Nothing more right now.
                    // WOULDBLOCK and AGAIN are the same errno on Linux.
                    Err(rustix::io::Errno::WOULDBLOCK) => break,
                    Err(_) => break,
                }
            }
            if Instant::now() >= deadline {
                return drained;
            }
            // The watcher thread sleeps on the queue in the product; here it
            // polls, because the measurement is arrival time and a blocking read
            // would hide the gap between events.
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

#[derive(Default)]
pub struct Drained {
    pub events: usize,
    pub overflowed: bool,
    pub first: Option<Instant>,
    pub last: Option<Instant>,
    /// Watch descriptors seen, capped — this is what a targeted rescan would use.
    pub touched: Vec<i32>,
}
