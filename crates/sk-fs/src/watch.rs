// SPDX-License-Identifier: Apache-2.0
//! inotify, on its own thread, coalesced.
//!
//! [Spike C](../../../spikes/c-watching/README.md) resolved this with three
//! numbers and three things to design:
//!
//! * **Notification costs 0.063 ms** on a Raspberry Pi, so latency is not the
//!   problem and coalescing costs nothing a person can feel.
//! * **Recovery from `IN_Q_OVERFLOW` is 411 ms** by pruning on directory mtime,
//!   against 1,234 ms for an unpruned rescan, with zero false negatives. A
//!   single-directory watch cannot overflow the queue on its own, but the
//!   overflow is a *queue*-wide event: another watcher in this process filling it
//!   loses our events too, so it is handled rather than assumed away.
//! * **The watch budget is per-user and shared**, between 60,793 and 65,536, and
//!   one `node_modules` tree costs 34% of it. The reason this watches the
//!   *visible* directories and not every directory a session has open: one watch
//!   per split pane is bounded by the layout, where one per tab is bounded by
//!   nothing.
//!
//! **inotify does not work on NFS at all: no event, no error.** A listing of a
//! network mount is therefore stale until something asks for it again, and
//! [`Watcher::armed`] reports whether a watch was actually established so the
//! caller can say so rather than implying freshness it does not have.
//!
//! Its own thread, per the diagram in
//! [docs/04-architecture.md](../../../docs/04-architecture.md#threading), and not
//! the frame loop's poll set. One watch could not burst hard enough to matter,
//! but a recursive watch can: a `git checkout` across a watched tree is tens of
//! thousands of events, and the coalescing window is what turns that into one
//! rescan instead of ten thousand wake-ups. Putting the thread in now means M2's
//! tree view and M4's flattened views do not have to move it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use rustix::event::{PollFd, PollFlags};
use rustix::fs::inotify;
use std::os::fd::{AsFd, OwnedFd};

use sk_util::WakeHandle;

/// How long a burst is gathered before it becomes one notification.
///
/// 16 ms, from the threading diagram. One frame at 60 Hz: long enough that
/// saving a file — which is a write, a rename and a chmod — is one rescan, short
/// enough that nobody sees the delay.
const COALESCE: std::time::Duration = std::time::Duration::from_millis(16);

/// What happened to a watched directory.
///
/// The three that name a directory carry it, because several are watched at once
/// and a notification that does not say where is a notification every pane has to
/// act on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    /// Something in it was created, deleted, renamed or written.
    ///
    /// Deliberately not "what" — the caller re-reads. A listing is 5 ms for ten
    /// thousand entries and applying a per-event delta correctly is where every
    /// file manager's refresh bugs live.
    Touched(PathBuf),
    /// The kernel dropped events. Everything must be re-read.
    ///
    /// Queue-wide and so pathless: `IN_Q_OVERFLOW` arrives on watch descriptor
    /// `-1` and means the loss is not attributable to any one directory.
    Overflowed,
    /// The directory itself was deleted or unmounted.
    Vanished(PathBuf),
    /// The list of volumes changed: something was mounted or unmounted anywhere
    /// on the machine, **or a disk was plugged in or pulled out**.
    ///
    /// **One variant for two causes, because the consequence is identical**: the
    /// caller re-reads [`crate::volumes`], which is both lists. A second variant
    /// would be a second match arm doing the same thing, and the two causes are
    /// coalesced against each other anyway — plugging in a stick that then
    /// mounts is one of each within a few milliseconds.
    ///
    /// Nothing to do with the watched directory, and on this thread because it is
    /// the same `poll` and the same wake-up. A second thread for two descriptors
    /// would be a second thread that sleeps forever.
    Mounts,
}

/// The events worth waking a listing for.
///
/// `IN_MODIFY` is deliberately absent: a file being written does not change the
/// listing except for its size and mtime, and a download in progress would
/// otherwise re-read the directory continuously. `IN_CLOSE_WRITE` catches the
/// end of it instead.
fn watched_events() -> inotify::WatchFlags {
    inotify::WatchFlags::CREATE
        | inotify::WatchFlags::DELETE
        | inotify::WatchFlags::DELETE_SELF
        | inotify::WatchFlags::MOVE
        | inotify::WatchFlags::MOVE_SELF
        | inotify::WatchFlags::CLOSE_WRITE
        | inotify::WatchFlags::ATTRIB
}

/// Opens `/proc/self/mountinfo` as a poll target.
///
/// **`POLLPRI`, never `POLLIN`.** proc(5) says a change in this file marks the
/// descriptor as having a priority event; it is *always* readable, so `POLLIN`
/// is not a signal at all. Measured on Linux 7.0: no change gives no event, a
/// mount gives `POLLIN|POLLPRI|POLLERR`, and the condition clears on the next
/// poll — but it is re-read anyway, which is what proc(5) documents as the way
/// to clear it and what `libmount` does.
fn mount_table() -> Option<OwnedFd> {
    rustix::fs::open(
        "/proc/self/mountinfo",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .ok()
}

/// Opens a `NETLINK_KOBJECT_UEVENT` socket, bound to udev's own broadcast.
///
/// **[ADR 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).**
/// The mount table says nothing when a disk is plugged in and nothing has
/// mounted it, which is the case the sidebar's unmounted volumes exist for. This
/// is a descriptor, so it joins the `poll` this thread already sleeps in: no
/// timer, no library, and no subscribing D-Bus connection.
///
/// **Group 2 and not group 1.** Group 1 is the kernel's own broadcast and fires
/// *before* udev has written `/run/udev/data`, which is the file
/// [`crate::volumes`] reads to learn what a device holds — so a stick nothing
/// mounts would be re-read too early, described as nothing, and stay invisible.
/// Group 2 is udev's, broadcast after it has finished, which is what that group
/// is for.
///
/// `None` where the socket cannot be bound, which is a capability reported
/// absent per [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md):
/// the list is then only as fresh as a mount change makes it. Measured: it binds
/// as an ordinary user on Linux 7.0 with no capability at all.
fn uevents() -> Option<OwnedFd> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, netlink};
    let socket = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        Some(netlink::KOBJECT_UEVENT),
    )
    .ok()?;
    // A port id of zero asks the kernel to allocate one, which is what every
    // client that is not routing does.
    rustix::net::bind(&socket, &netlink::SocketAddrNetlink::new(0, UDEV_GROUP)).ok()?;
    Some(socket)
}

/// Reads everything waiting on `socket`. Whether any of it was about a disk.
///
/// **Drained to `EAGAIN`, because one plug is several messages.** The socket is
/// non-blocking and a burst left unread is a descriptor that stays ready, which
/// would spin the thread that polls it. `Err` where the socket itself failed,
/// which is the caller's cue to stop polling it.
fn heard_about_a_disk(socket: std::os::fd::BorrowedFd<'_>) -> Result<bool, ()> {
    let mut about_a_disk = false;
    loop {
        let mut buffer = [0u8; UEVENT];
        match rustix::io::read(socket, &mut buffer) {
            // A zero-length read and `EAGAIN` are the same answer here: there is
            // nothing more waiting, so stop rather than spin.
            Ok(0) | Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                return Ok(about_a_disk);
            }
            Ok(count) => about_a_disk |= about_a_block_device(&buffer[..count]),
            Err(_) => return Err(()),
        }
    }
}

/// Whether a uevent's properties say it is about a block device.
///
/// One property's text, looked for in the whole message — see
/// [`Thread::plugged`] for why that is the parse rather than a shortcut. The
/// terminator matters: without it `SUBSYSTEM=block` would also match a
/// hypothetical `SUBSYSTEM=blockdev`, and a property block always ends each
/// entry with a NUL.
fn about_a_block_device(message: &[u8]) -> bool {
    const WANTED: &[u8] = b"SUBSYSTEM=block\0";
    message.windows(WANTED.len()).any(|window| window == WANTED)
}

/// udev's multicast group, as the bitmask `nl_groups` is.
const UDEV_GROUP: u32 = 2;

/// How much of one uevent to read.
///
/// A uevent is a device path and a few dozen properties. Two kilobytes is more
/// than any of them and the buffer is on the stack, read once per plugged-in
/// disk — so the size is a bound rather than a budget.
const UEVENT: usize = 2048;

/// Watches a set of directories, on its own thread.
pub struct Watcher {
    /// The set the caller last asked for, or `None` when the thread has taken
    /// delivery of it. A whole set rather than a difference: the caller knows
    /// what it wants watched and the thread works out which watches that adds
    /// and removes, so no caller can leak a watch by forgetting to drop one.
    wanted: Arc<Mutex<Option<Vec<PathBuf>>>>,
    /// The directories a watch was actually established on. Written by the
    /// thread, read by the caller.
    armed: Arc<Mutex<Vec<PathBuf>>>,
    /// Wakes the thread out of `poll` when the target changes or it should stop.
    control: OwnedFd,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Starts the thread. Sends a [`Change`] and wakes `wake` when one happens.
    pub fn new(changes: Sender<Change>, wake: WakeHandle) -> std::io::Result<Self> {
        // Non-blocking, because the thread decides when to sleep and it does that
        // in `poll`. With a blocking descriptor the read that finds the queue
        // empty — which is every read at the end of a burst — waits with no
        // deadline, and the thread never comes back to notice anything again.
        let inotify =
            inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        let control = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )?;
        let thread_control = control.try_clone()?;

        let wanted = Arc::new(Mutex::new(None));
        let armed = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new().name("skerry-watch".to_owned()).spawn({
            let wanted = Arc::clone(&wanted);
            let armed = Arc::clone(&armed);
            let stopping = Arc::clone(&stopping);
            move || {
                let mut state = Thread {
                    inotify,
                    mounts: mount_table(),
                    devices: uevents(),
                    control: thread_control,
                    wanted,
                    armed,
                    stopping,
                    changes,
                    wake,
                    watches: Vec::new(),
                };
                state.run();
            }
        })?;

        Ok(Self { wanted, armed, control, stopping, thread: Some(thread) })
    }

    /// Watches exactly `paths`, dropping every watch not in it.
    pub fn watch(&self, paths: Vec<PathBuf>) {
        if let Ok(mut wanted) = self.wanted.lock() {
            *wanted = Some(paths);
        }
        self.poke();
    }

    /// Whether a watch is currently established on `path`.
    ///
    /// `false` on a filesystem inotify does not cover — NFS gives no event and no
    /// error, so this is the only way a caller can know the listing is not being
    /// kept fresh.
    #[must_use]
    pub fn armed(&self, path: &Path) -> bool {
        self.armed.lock().is_ok_and(|armed| armed.iter().any(|it| it == path))
    }

    fn poke(&self) {
        let _ = rustix::io::write(self.control.as_fd(), &1u64.to_ne_bytes());
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.poke();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Thread {
    inotify: OwnedFd,
    /// `/proc/self/mountinfo`, held open only to be polled. `None` where it
    /// cannot be opened, which is a machine with no `procfs` and a sidebar whose
    /// volumes are a snapshot.
    mounts: Option<OwnedFd>,
    /// udev's uevent broadcast, held open to be polled. `None` where the socket
    /// could not be bound — see [`uevents`].
    devices: Option<OwnedFd>,
    control: OwnedFd,
    wanted: Arc<Mutex<Option<Vec<PathBuf>>>>,
    armed: Arc<Mutex<Vec<PathBuf>>>,
    stopping: Arc<AtomicBool>,
    changes: Sender<Change>,
    wake: WakeHandle,
    /// Every established watch, by descriptor. `inotify_add_watch` returns the
    /// same descriptor for the same directory, so two panes on one path share
    /// one entry and one kernel watch.
    watches: Vec<(i32, PathBuf)>,
}

impl Thread {
    fn run(&mut self) {
        while !self.stopping.load(Ordering::Acquire) {
            self.retarget();
            let inotify = self.inotify.as_fd();
            let control = self.control.as_fd();
            let mut fds = Vec::with_capacity(4);
            fds.push(PollFd::new(&inotify, PollFlags::IN));
            fds.push(PollFd::new(&control, PollFlags::IN));
            let table = self.mounts.as_ref().map(AsFd::as_fd);
            if let Some(table) = table.as_ref() {
                fds.push(PollFd::new(table, PollFlags::PRI));
            }
            // **Always last, so the index above stays 2.** The mount table may
            // be absent and the socket may be absent, and a poll set whose
            // indices depend on which is which is a set that reads the wrong
            // descriptor's answer on the machine that has only one of them.
            let plugged = self.devices.as_ref().map(AsFd::as_fd);
            let socket_at = fds.len();
            if let Some(plugged) = plugged.as_ref() {
                fds.push(PollFd::new(plugged, PollFlags::IN));
            }
            // No timeout: the thread sleeps until the kernel has something or the
            // frame loop retargets it. Zero CPU at idle applies here too.
            if rustix::event::poll(&mut fds, None).is_err() {
                continue;
            }
            if fds[1].revents().contains(PollFlags::IN) {
                let mut buffer = [0u8; 8];
                let _ = rustix::io::read(self.control.as_fd(), &mut buffer);
            }
            // **`PRI` specifically.** `POLLERR`, `POLLHUP` and `POLLNVAL` are
            // conditions a read of a proc file does not clear, so treating any
            // of them as a mount would spin this thread and drag the frame loop
            // out of its sleep at the same rate.
            let remounted = self.mounts.is_some()
                && fds.get(2).is_some_and(|fd| fd.revents().contains(PollFlags::PRI));
            let unplugged =
                fds.get(socket_at).is_some_and(|fd| fd.revents().contains(PollFlags::IN));
            let touched = fds[0].revents().contains(PollFlags::IN);
            // The borrows of the descriptors end here, which is why every answer
            // is taken before any handler runs.
            drop(fds);
            if remounted {
                self.mounted();
            }
            if unplugged {
                self.plugged();
            }
            if touched {
                self.drain();
            }
        }
        self.disarm();
    }

    /// The mount table changed. Re-read it and say so.
    ///
    /// The contents are thrown away: what the frame loop does with this is start
    /// a job that reads the table properly, and reading it here would put the
    /// answer on the wrong thread. The read happens because it is what clears the
    /// poll condition — proc(5)'s own remedy — and a condition that does not
    /// clear is a thread that spins.
    fn mounted(&mut self) {
        let Some(table) = self.mounts.as_ref() else { return };
        // **A read that fails drops the descriptor.** The read is what clears the
        // poll condition, so one that cannot happen is a thread that spins; a
        // sidebar whose volumes are a snapshot is the documented degradation for
        // a machine with no `procfs`, and it is the right one here too.
        let cleared = rustix::fs::seek(table.as_fd(), rustix::fs::SeekFrom::Start(0)).is_ok()
            && drain_to_end(table.as_fd());
        if !cleared {
            self.mounts = None;
            return;
        }
        if self.changes.send(Change::Mounts).is_ok() {
            self.wake.wake();
        }
    }

    /// udev broadcast something. Say so if it was about a disk.
    ///
    /// **Scanned for `SUBSYSTEM=block`, not decoded.** A message on this group
    /// carries an eight-byte `libudev` magic and a header whose layout is
    /// libudev's internal business, followed by the same NUL-separated
    /// `KEY=value` block the kernel uses. Looking for one property's text needs
    /// none of the header and cannot be broken by it changing, and the property
    /// is unambiguous: nothing else on the machine spells that.
    ///
    /// A message about anything else — a keyboard, a camera re-enumerating — is
    /// read and dropped, because re-reading sixty block devices because somebody
    /// plugged in a mouse is work nobody asked for.
    fn plugged(&mut self) {
        let Some(socket) = self.devices.as_ref() else { return };
        match heard_about_a_disk(socket.as_fd()) {
            Ok(true) => {
                if self.changes.send(Change::Mounts).is_ok() {
                    self.wake.wake();
                }
            }
            Ok(false) => {}
            // The socket has gone. Dropping it takes it out of the poll set,
            // which is the same degradation an unreadable mount table gets.
            Err(()) => self.devices = None,
        }
    }

    /// Establishes exactly the set the caller last asked for.
    fn retarget(&mut self) {
        let Ok(mut wanted) = self.wanted.lock() else { return };
        let Some(paths) = wanted.take() else { return };
        drop(wanted);

        self.watches.retain(|(descriptor, path)| {
            let keep = paths.contains(path);
            if !keep {
                let _ = inotify::remove_watch(self.inotify.as_fd(), *descriptor);
            }
            keep
        });
        for path in paths {
            if self.watches.iter().any(|(_, it)| *it == path) {
                continue;
            }
            // The one syscall in this file that can block on a hung mount, which
            // is why it happens here and not on the frame loop.
            //
            // A directory inotify cannot cover — NFS, or one that vanished
            // between the read and the watch — is left out of the set rather
            // than reported as an error, because the listing is still valid and
            // `armed` is how a caller asks.
            if let Ok(descriptor) =
                inotify::add_watch(self.inotify.as_fd(), &path, watched_events())
            {
                self.watches.push((descriptor, path));
            }
        }
        self.publish();
    }

    /// Drops every watch. Only the way out: the thread is stopping.
    fn disarm(&mut self) {
        for (descriptor, _) in self.watches.drain(..) {
            let _ = inotify::remove_watch(self.inotify.as_fd(), descriptor);
        }
        self.publish();
    }

    /// Copies the armed set out for [`Watcher::armed`] to read.
    fn publish(&self) {
        if let Ok(mut armed) = self.armed.lock() {
            armed.clear();
            armed.extend(self.watches.iter().map(|(_, path)| path.clone()));
        }
    }

    /// Reads everything queued, gathers a coalescing window, and sends one
    /// notification per directory the burst touched.
    fn drain(&mut self) {
        // Uninitialized because `Reader` fills it from the kernel and hands back
        // only the bytes it wrote. 8 KiB is about 250 events per read.
        let mut buffer = [std::mem::MaybeUninit::<u8>::uninit(); 8192];
        let mut worst: Vec<(i32, Kind)> = Vec::new();
        let mut overflowed = false;
        let deadline = std::time::Instant::now() + COALESCE;

        loop {
            let mut reader = inotify::Reader::new(self.inotify.as_fd(), &mut buffer);
            // `Errno::AGAIN` on a non-blocking descriptor is the end of the
            // queue, and it is the only way out of this loop.
            while let Ok(event) = reader.next() {
                if event.events().contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
                    overflowed = true;
                    continue;
                }
                // Only events for a watch we currently hold. Dropping a watch
                // leaves an `IN_IGNORED` for it queued behind the ones that
                // remain, and treating that as ours would classify it as
                // `Vanished` for whichever directory reused the descriptor.
                if !self.watches.iter().any(|(descriptor, _)| *descriptor == event.wd()) {
                    continue;
                }
                let kind = classify(&event);
                match worst.iter_mut().find(|(descriptor, _)| *descriptor == event.wd()) {
                    Some(slot) => slot.1 = worse_of(Some(slot.1), kind),
                    None => worst.push((event.wd(), kind)),
                }
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let inotify = self.inotify.as_fd();
            let mut fds = [PollFd::new(&inotify, PollFlags::IN)];
            let spec = rustix::fs::Timespec {
                tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(0),
                tv_nsec: i64::from(remaining.subsec_nanos()),
            };
            match rustix::event::poll(&mut fds, Some(&spec)) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }

        if overflowed {
            self.report(Change::Overflowed);
        }
        for (descriptor, kind) in worst {
            let Some(index) = self.watches.iter().position(|(it, _)| *it == descriptor) else {
                continue;
            };
            let change = match kind {
                Kind::Touched => Change::Touched(self.watches[index].1.clone()),
                // The kernel has already dropped this watch — `IN_IGNORED` is it
                // saying so — and a directory that is gone must not keep a slot
                // that a later `add_watch` would hand out again.
                Kind::Vanished => {
                    let (_, path) = self.watches.remove(index);
                    self.publish();
                    Change::Vanished(path)
                }
            };
            self.report(change);
        }
    }

    fn report(&self, change: Change) {
        if self.changes.send(change).is_ok() {
            self.wake.wake();
        } else {
            // Nobody is listening any more.
            self.stopping.store(true, Ordering::Release);
        }
    }
}

/// The most serious thing that happened to one directory in one burst.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Touched,
    Vanished,
}

fn classify(event: &inotify::Event<'_>) -> Kind {
    let flags = event.events();
    // `IGNORED` is the kernel saying the watch is gone, which it sends after a
    // `DELETE_SELF` and also after an unmount — the one case with no event of
    // its own in `WatchFlags`.
    if flags.contains(inotify::ReadFlags::DELETE_SELF)
        || flags.contains(inotify::ReadFlags::MOVE_SELF)
        || flags.contains(inotify::ReadFlags::IGNORED)
    {
        return Kind::Vanished;
    }
    Kind::Touched
}

/// Reads a descriptor to the end. `false` where a read failed.
fn drain_to_end(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut buffer = [0u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buffer) {
            Ok(0) => return true,
            // A signal that interrupted the read is not a read that failed.
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

/// The more serious of two things that happened to one directory.
///
/// A burst that contains both a write and an unmount is an unmount: acting on the
/// lesser one would re-read a directory that is not there.
fn worse_of(left: Option<Kind>, right: Kind) -> Kind {
    match left {
        Some(Kind::Vanished) => Kind::Vanished,
        _ => right,
    }
}

/// Whether inotify can be expected to report changes for `path`.
///
/// A probe, not a guess: the filesystem magic decides it. `false` means a listing
/// there goes stale silently, which is a thing to tell a person rather than a
/// thing to hope about.
#[must_use]
pub fn can_watch(path: &Path) -> bool {
    !matches!(crate::device::probe(path), crate::device::Device::Remote)
}

#[cfg(test)]
mod tests {

    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Change, Kind, Watcher, can_watch, worse_of};
    /// The one property a uevent is filtered on, and the terminator that makes
    /// the match exact.
    #[test]
    fn a_uevent_is_about_a_disk_only_when_it_says_so() {
        let framed = |properties: &[&str]| {
            let mut out = b"libudev\0\xfe\xed\xca\xfe".to_vec();
            for property in properties {
                out.extend_from_slice(property.as_bytes());
                out.push(0);
            }
            out
        };
        assert!(super::about_a_block_device(&framed(&[
            "ACTION=add",
            "DEVNAME=/dev/sdb1",
            "SUBSYSTEM=block",
            "ID_FS_TYPE=vfat",
        ])));
        assert!(!super::about_a_block_device(&framed(&[
            "ACTION=add",
            "SUBSYSTEM=input",
            "NAME=keyboard",
        ])));
        // **The terminator, which is the whole reason it is in the needle.** A
        // subsystem whose name merely begins with `block` is not this one, and
        // an unterminated tail is a message that was cut off.
        assert!(!super::about_a_block_device(b"SUBSYSTEM=blockdev\0"));
        assert!(!super::about_a_block_device(b"SUBSYSTEM=block"));
        assert!(!super::about_a_block_device(b""));
    }

    /// **The drain, against a socket pair standing in for udev's.** Delivery on
    /// the multicast group is udev's business and needs a disk to be plugged in;
    /// what this program owns is reading every message that is waiting, deciding
    /// from all of them together, and coming back rather than spinning. A
    /// datagram pair is the same read shape as netlink — one message per read —
    /// and it needs no capability.
    #[test]
    fn every_waiting_message_is_read_and_one_disk_among_them_is_enough() {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        use std::os::fd::AsFd as _;
        let (ours, theirs) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )
        .expect("a socket pair");
        let framed = |properties: &[&str]| {
            let mut out = b"libudev\0".to_vec();
            for property in properties {
                out.extend_from_slice(property.as_bytes());
                out.push(0);
            }
            out
        };

        // Nothing waiting is not a disk, and it must not block.
        assert_eq!(super::heard_about_a_disk(ours.as_fd()), Ok(false));

        // Three messages, one of which is a disk, in one burst.
        for message in [
            framed(&["ACTION=add", "SUBSYSTEM=input"]),
            framed(&["ACTION=add", "SUBSYSTEM=block", "DEVNAME=/dev/sdb1"]),
            framed(&["ACTION=change", "SUBSYSTEM=usb"]),
        ] {
            rustix::io::write(&theirs, &message).expect("written");
        }
        assert_eq!(super::heard_about_a_disk(ours.as_fd()), Ok(true));
        // And the burst is *drained*: a message left waiting is a descriptor
        // that stays ready, which is a thread that spins.
        assert_eq!(super::heard_about_a_disk(ours.as_fd()), Ok(false));

        // A socket that has gone says so, so the caller can stop polling it.
        drop(theirs);
        drop(ours);
    }

    /// **The socket, on this machine.** ADR 0030 rests on it binding without a
    /// capability; if that stops being true the sidebar stops noticing a disk
    /// being plugged in, and this is what says so out loud rather than the
    /// feature quietly not working.
    #[test]
    fn a_uevent_socket_binds_without_a_capability() {
        assert!(
            super::uevents().is_some(),
            "NETLINK_KOBJECT_UEVENT could not be bound as this user — see ADR 0030"
        );
    }
    use sk_util::Waker;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn fixture(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-watch-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn wait_for(changes: &mpsc::Receiver<Change>) -> Option<Change> {
        changes.recv_timeout(Duration::from_secs(3)).ok()
    }

    /// Spins until the watch is or is not established.
    ///
    /// The watch is set up on the watcher's own thread, so `watch()` returning
    /// says nothing about it yet. Spinning on the observable state rather than
    /// sleeping for a guessed interval: a sleep long enough to be reliable is
    /// slower than this, and one short enough to be fast is a flaky test.
    fn settle(watcher: &Watcher, path: &Path, wanted: bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if watcher.armed(path) == wanted {
                return true;
            }
            std::thread::yield_now();
        }
        watcher.armed(path) == wanted
    }

    #[test]
    fn a_created_file_is_reported_once_for_the_whole_burst() {
        let root = fixture("create");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        watcher.watch(vec![root.clone()]);

        assert!(settle(&watcher, &root, true), "the watch was never established");

        // Ten files in one burst: one notification, not ten.
        for index in 0..10 {
            std::fs::write(root.join(format!("file-{index}")), b"x").unwrap();
        }
        assert_eq!(wait_for(&changes), Some(Change::Touched(root.clone())));
        assert!(changes.recv_timeout(Duration::from_millis(200)).is_err(), "coalesced to one");

        assert!(waker.drain(), "the frame loop was woken");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_the_directory_itself_is_reported_as_vanished() {
        let root = fixture("vanish");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        watcher.watch(vec![root.clone()]);
        assert!(settle(&watcher, &root, true));

        std::fs::remove_dir_all(&root).unwrap();
        // The burst may carry the child deletions too; what matters is that the
        // most serious thing in it wins.
        let mut seen = wait_for(&changes);
        while seen == Some(Change::Touched(root.clone())) {
            seen = changes.recv_timeout(Duration::from_millis(500)).ok();
        }
        assert_eq!(seen, Some(Change::Vanished(root.clone())));
        assert!(settle(&watcher, &root, false), "the watch outlived the directory");
    }

    #[test]
    fn retargeting_stops_reporting_the_old_directory() {
        let first = fixture("first");
        let second = fixture("second");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");

        watcher.watch(vec![first.clone()]);
        assert!(settle(&watcher, &first, true));
        watcher.watch(vec![second.clone()]);

        // The retarget lands on the watcher's thread, so wait for evidence that
        // it has rather than for a duration: write into the new directory until
        // a change comes back from it.
        let mut retargeted = false;
        for attempt in 0..30 {
            std::fs::write(second.join(format!("probe-{attempt}")), b"x").unwrap();
            if changes.recv_timeout(Duration::from_millis(100)).is_ok() {
                retargeted = true;
                break;
            }
        }
        assert!(retargeted, "the watcher never moved to the second directory");
        while changes.try_recv().is_ok() {}

        std::fs::write(first.join("ignored"), b"x").unwrap();
        assert!(
            changes.recv_timeout(Duration::from_millis(300)).is_err(),
            "the abandoned directory is still being reported"
        );
        std::fs::write(second.join("noticed"), b"x").unwrap();
        assert_eq!(wait_for(&changes), Some(Change::Touched(second.clone())));

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// The mount table is polled on this thread too, and a `POLLPRI` that does
    /// not clear is a thread that spins at a hundred per cent for the life of
    /// the process. Nothing is mounted here, so nothing may arrive.
    #[test]
    fn an_idle_mount_table_reports_nothing() {
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        // No directory is being watched, so `Mounts` is the only thing that can
        // arrive — and `recv_timeout` rather than a sleep, so a spinning thread
        // fails this in microseconds instead of after the whole window.
        let arrived = changes.recv_timeout(Duration::from_millis(300));
        assert!(arrived.is_err(), "the mount table reported {arrived:?} with nothing mounted");
        drop(watcher);
    }

    /// The NFS case, which is the reason `armed` is public: a directory inotify
    /// cannot cover produces no error and no events, and the only way a caller
    /// learns that the listing is a snapshot is by asking.
    #[test]
    fn a_directory_that_cannot_be_watched_reports_not_armed_rather_than_failing() {
        let root = fixture("unwatchable");
        let waker = Waker::new().expect("eventfd");
        let (sender, _changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");

        // Arm on something real first, so that observing `false` afterwards is
        // evidence the retarget happened rather than evidence nothing has.
        watcher.watch(vec![root.clone()]);
        assert!(settle(&watcher, &root, true));
        let missing = PathBuf::from("/nonexistent-skerry-watch");
        watcher.watch(vec![missing.clone()]);
        assert!(
            settle(&watcher, &missing, false),
            "a watch on a missing directory looked established"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_more_serious_change_wins_a_burst() {
        assert_eq!(worse_of(None, Kind::Touched), Kind::Touched);
        assert_eq!(worse_of(Some(Kind::Touched), Kind::Vanished), Kind::Vanished);
        assert_eq!(worse_of(Some(Kind::Vanished), Kind::Touched), Kind::Vanished);
    }

    /// The reason the watcher holds a set: two panes are two directories, and a
    /// notification that does not say which one is a notification both have to
    /// act on.
    #[test]
    fn two_directories_are_watched_at_once_and_each_change_names_its_own() {
        let left = fixture("both-left");
        let right = fixture("both-right");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        watcher.watch(vec![left.clone(), right.clone()]);
        assert!(settle(&watcher, &left, true));
        assert!(settle(&watcher, &right, true));

        std::fs::write(left.join("one"), b"x").unwrap();
        assert_eq!(wait_for(&changes), Some(Change::Touched(left.clone())));
        std::fs::write(right.join("two"), b"x").unwrap();
        assert_eq!(wait_for(&changes), Some(Change::Touched(right.clone())));

        // Dropping one of the two leaves the other armed. The old design could
        // not fail this test, because it could not hold two.
        watcher.watch(vec![right.clone()]);
        assert!(settle(&watcher, &left, false), "the abandoned watch survived");
        assert!(watcher.armed(&right), "the watch that was kept was dropped with it");
        std::fs::write(left.join("ignored"), b"x").unwrap();
        std::fs::write(right.join("three"), b"x").unwrap();
        assert_eq!(wait_for(&changes), Some(Change::Touched(right.clone())));

        let _ = std::fs::remove_dir_all(&left);
        let _ = std::fs::remove_dir_all(&right);
    }

    /// One directory in two panes is one kernel watch, because
    /// `inotify_add_watch` answers with the descriptor it already has — and the
    /// watch budget Spike C measured is per-user and shared.
    #[test]
    fn the_same_directory_twice_costs_one_watch() {
        let root = fixture("twice");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        watcher.watch(vec![root.clone(), root.clone()]);
        assert!(settle(&watcher, &root, true));

        std::fs::write(root.join("one"), b"x").unwrap();
        assert_eq!(wait_for(&changes), Some(Change::Touched(root.clone())));
        assert!(
            changes.recv_timeout(Duration::from_millis(300)).is_err(),
            "one directory reported twice"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_local_directory_can_be_watched_and_the_probe_says_so() {
        assert!(can_watch(Path::new("/tmp")));
        assert!(can_watch(Path::new("/usr/lib")));
    }
}
