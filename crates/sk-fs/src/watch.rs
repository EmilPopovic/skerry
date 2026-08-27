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
//!   one `node_modules` tree costs 34% of it. Not a constraint yet — this watches
//!   one directory — and the reason `Watcher` retargets a single watch instead of
//!   accumulating them.
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Something in it was created, deleted, renamed or written.
    ///
    /// Deliberately not "what" — the caller re-reads. A listing is 5 ms for ten
    /// thousand entries and applying a per-event delta correctly is where every
    /// file manager's refresh bugs live.
    Touched,
    /// The kernel dropped events. Everything must be re-read.
    Overflowed,
    /// The directory itself was deleted or unmounted.
    Vanished,
    /// The mount table changed: something was mounted or unmounted anywhere on
    /// the machine.
    ///
    /// Nothing to do with the watched directory, and on this thread because it is
    /// the same `poll` and the same wake-up. A second thread for one descriptor
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

/// Watches one directory at a time, on its own thread.
pub struct Watcher {
    wanted: Arc<Mutex<Option<PathBuf>>>,
    /// Written by the thread, read by the caller.
    armed: Arc<AtomicBool>,
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
        let armed = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new().name("skerry-watch".to_owned()).spawn({
            let wanted = Arc::clone(&wanted);
            let armed = Arc::clone(&armed);
            let stopping = Arc::clone(&stopping);
            move || {
                let mut state = Thread {
                    inotify,
                    mounts: mount_table(),
                    control: thread_control,
                    wanted,
                    armed,
                    stopping,
                    changes,
                    wake,
                    watch: None,
                };
                state.run();
            }
        })?;

        Ok(Self { wanted, armed, control, stopping, thread: Some(thread) })
    }

    /// Watches `path` instead of whatever was being watched.
    pub fn watch(&self, path: PathBuf) {
        if let Ok(mut wanted) = self.wanted.lock() {
            *wanted = Some(path);
        }
        self.poke();
    }

    /// Whether a watch is currently established.
    ///
    /// `false` on a filesystem inotify does not cover — NFS gives no event and no
    /// error, so this is the only way a caller can know the listing is not being
    /// kept fresh.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
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
    control: OwnedFd,
    wanted: Arc<Mutex<Option<PathBuf>>>,
    armed: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    changes: Sender<Change>,
    wake: WakeHandle,
    watch: Option<i32>,
}

impl Thread {
    fn run(&mut self) {
        while !self.stopping.load(Ordering::Acquire) {
            self.retarget();
            let inotify = self.inotify.as_fd();
            let control = self.control.as_fd();
            let mut fds = Vec::with_capacity(3);
            fds.push(PollFd::new(&inotify, PollFlags::IN));
            fds.push(PollFd::new(&control, PollFlags::IN));
            let table = self.mounts.as_ref().map(AsFd::as_fd);
            if let Some(table) = table.as_ref() {
                fds.push(PollFd::new(table, PollFlags::PRI));
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
            let remounted = fds.get(2).is_some_and(|fd| !fd.revents().is_empty());
            let touched = fds[0].revents().contains(PollFlags::IN);
            // The borrows of the two descriptors end here, which is why the two
            // answers are taken before either handler runs.
            drop(fds);
            if remounted {
                self.mounted();
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
        let _ = rustix::fs::seek(table.as_fd(), rustix::fs::SeekFrom::Start(0));
        let mut buffer = [0u8; 4096];
        while matches!(rustix::io::read(table.as_fd(), &mut buffer), Ok(read) if read > 0) {}
        if self.changes.send(Change::Mounts).is_ok() {
            self.wake.wake();
        }
    }

    /// Moves the watch if the caller has asked for a different directory.
    fn retarget(&mut self) {
        let Ok(mut wanted) = self.wanted.lock() else { return };
        let Some(path) = wanted.take() else { return };
        drop(wanted);
        self.disarm();
        // The one syscall in this file that can block on a hung mount, which is
        // why it happens here and not on the frame loop.
        match inotify::add_watch(self.inotify.as_fd(), &path, watched_events()) {
            Ok(descriptor) => {
                self.watch = Some(descriptor);
                self.armed.store(true, Ordering::Release);
            }
            // A directory inotify cannot cover — NFS, or one that vanished
            // between the read and the watch. Reported as "not armed" rather
            // than as an error, because the listing is still valid.
            Err(_) => self.armed.store(false, Ordering::Release),
        }
    }

    fn disarm(&mut self) {
        if let Some(descriptor) = self.watch.take() {
            let _ = inotify::remove_watch(self.inotify.as_fd(), descriptor);
        }
        self.armed.store(false, Ordering::Release);
    }

    /// Reads everything queued, gathers a coalescing window, and sends one
    /// notification for the whole burst.
    fn drain(&mut self) {
        // Uninitialized because `Reader` fills it from the kernel and hands back
        // only the bytes it wrote. 8 KiB is about 250 events per read.
        let mut buffer = [std::mem::MaybeUninit::<u8>::uninit(); 8192];
        let mut worst = None;
        let deadline = std::time::Instant::now() + COALESCE;

        loop {
            let mut reader = inotify::Reader::new(self.inotify.as_fd(), &mut buffer);
            // `Errno::AGAIN` on a non-blocking descriptor is the end of the
            // queue, and it is the only way out of this loop.
            while let Ok(event) = reader.next() {
                // Only events for the watch we currently hold. Retargeting leaves
                // an `IN_IGNORED` for the old descriptor queued behind the new
                // watch, and treating it as ours would classify it as `Vanished`
                // and tear down the watch that was just established.
                if Some(event.wd()) == self.watch {
                    worst = Some(worse_of(worst, classify(&event)));
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

        if let Some(change) = worst {
            if change == Change::Vanished {
                self.disarm();
            }
            if self.changes.send(change).is_ok() {
                self.wake.wake();
            } else {
                // Nobody is listening any more.
                self.stopping.store(true, Ordering::Release);
            }
        }
    }
}

fn classify(event: &inotify::Event<'_>) -> Change {
    let flags = event.events();
    if flags.contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
        return Change::Overflowed;
    }
    // `IGNORED` is the kernel saying the watch is gone, which it sends after a
    // `DELETE_SELF` and also after an unmount — the one case with no event of
    // its own in `WatchFlags`.
    if flags.contains(inotify::ReadFlags::DELETE_SELF)
        || flags.contains(inotify::ReadFlags::MOVE_SELF)
        || flags.contains(inotify::ReadFlags::IGNORED)
    {
        return Change::Vanished;
    }
    Change::Touched
}

/// The more serious of two changes.
///
/// A burst that contains both a write and an unmount is an unmount: acting on the
/// lesser one would re-read a directory that is not there.
fn worse_of(left: Option<Change>, right: Change) -> Change {
    let rank = |change: Change| match change {
        // Never reaches this function: it comes from the mount table and not
        // from an inotify event, so it is not part of a directory's burst.
        Change::Mounts | Change::Touched => 0,
        Change::Overflowed => 1,
        Change::Vanished => 2,
    };
    match left {
        Some(left) if rank(left) >= rank(right) => left,
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

    use super::{Change, Watcher, can_watch, worse_of};
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
    fn settle(watcher: &Watcher, wanted: bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if watcher.armed() == wanted {
                return true;
            }
            std::thread::yield_now();
        }
        watcher.armed() == wanted
    }

    #[test]
    fn a_created_file_is_reported_once_for_the_whole_burst() {
        let root = fixture("create");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");
        watcher.watch(root.clone());

        assert!(settle(&watcher, true), "the watch was never established");

        // Ten files in one burst: one notification, not ten.
        for index in 0..10 {
            std::fs::write(root.join(format!("file-{index}")), b"x").unwrap();
        }
        assert_eq!(wait_for(&changes), Some(Change::Touched));
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
        watcher.watch(root.clone());
        assert!(settle(&watcher, true));

        std::fs::remove_dir_all(&root).unwrap();
        // The burst may carry the child deletions too; what matters is that the
        // most serious thing in it wins.
        let mut seen = wait_for(&changes);
        while seen == Some(Change::Touched) {
            seen = changes.recv_timeout(Duration::from_millis(500)).ok();
        }
        assert_eq!(seen, Some(Change::Vanished));
        assert!(settle(&watcher, false), "the watch outlived the directory");
    }

    #[test]
    fn retargeting_stops_reporting_the_old_directory() {
        let first = fixture("first");
        let second = fixture("second");
        let waker = Waker::new().expect("eventfd");
        let (sender, changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");

        watcher.watch(first.clone());
        assert!(settle(&watcher, true));
        watcher.watch(second.clone());

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
        assert_eq!(wait_for(&changes), Some(Change::Touched));

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// The NFS case, which is the reason `armed` is public: a directory inotify
    /// cannot cover produces no error and no events, and the only way a caller
    /// learns that the listing is a snapshot is by asking.
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

    #[test]
    fn a_directory_that_cannot_be_watched_reports_not_armed_rather_than_failing() {
        let root = fixture("unwatchable");
        let waker = Waker::new().expect("eventfd");
        let (sender, _changes) = mpsc::channel();
        let watcher = Watcher::new(sender, waker.handle()).expect("watcher");

        // Arm on something real first, so that observing `false` afterwards is
        // evidence the retarget happened rather than evidence nothing has.
        watcher.watch(root.clone());
        assert!(settle(&watcher, true));
        watcher.watch(PathBuf::from("/nonexistent-skerry-watch"));
        assert!(settle(&watcher, false), "a watch on a missing directory looked established");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_more_serious_change_wins_a_burst() {
        assert_eq!(worse_of(None, Change::Touched), Change::Touched);
        assert_eq!(worse_of(Some(Change::Touched), Change::Vanished), Change::Vanished);
        assert_eq!(worse_of(Some(Change::Vanished), Change::Touched), Change::Vanished);
        assert_eq!(worse_of(Some(Change::Touched), Change::Overflowed), Change::Overflowed);
        assert_eq!(worse_of(Some(Change::Overflowed), Change::Vanished), Change::Vanished);
    }

    #[test]
    fn a_local_directory_can_be_watched_and_the_probe_says_so() {
        assert!(can_watch(Path::new("/tmp")));
        assert!(can_watch(Path::new("/usr/lib")));
    }
}
