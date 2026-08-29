//! One job pool for the process, and the eventfd that lets a finished job wake
//! the frame loop.
//!
//! [ADR 0005](../../../docs/adr/0005-no-async-runtime.md) is the reason there is
//! exactly one: a second scheduler — a `rayon` global, an async runtime's
//! blocking pool — means two schedulers oversubscribing the same cores, which
//! is a latency bug that only shows up under load.
//!
//! Width is `available_parallelism`, not `ncpu - 2`. Spike B measured a
//! million-file walk at **1.626 s** with `ncpu - 2` on a four-core Raspberry Pi
//! 5 against **0.91–1.01 s** anywhere from four workers upward; past `ncpu` the
//! curve is flat enough that the choice is inside the run-to-run noise.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{EventfdFlags, eventfd};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// Which queue a job goes on.
///
/// Per-device queues, not one queue: Spike B's hung-mount arm took **28,000 ms**
/// for a call that costs 4 ms on a healthy device. A parallel lane for a local
/// device and a serial lane for anything rotational or networked keeps one sick
/// device from consuming every worker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    /// Runs as wide as the pool. For local devices that answer concurrent reads.
    Parallel,
    /// At most one job at a time, whatever the pool's width. For rotational
    /// disks, where seeking between concurrent readers is slower than queueing,
    /// and for network mounts, where a stalled request must not take a worker
    /// each.
    Serial,
}

/// A shared flag a job checks and its owner sets.
///
/// Cancellation is cooperative because the alternative is killing a thread
/// mid-syscall. Every long job takes one and checks it per directory, per batch
/// or per extent — never once at the start.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A token that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the job to stop. Idempotent, and safe from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the owner has asked this job to stop.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A shared latch a job waits on and its owner opens.
///
/// **Blocks the worker, and that is the point.** "Pause" implemented by stopping
/// and starting again is not a pause: a tree copy that stopped between files and
/// started over would redo the file it was in the middle of, and one that stopped
/// mid-file would leave a partial nobody asked for. A real pause has the job
/// stand still, holding whatever it has open, until it is let go.
///
/// **At most one job is ever paused**, because at most one mutation is ever in
/// flight — see `sk_app::edit`. A paused job holds one worker of
/// `available_parallelism`, and a design that could pause many would have to
/// count them against the pool's width.
#[derive(Clone, Default, Debug)]
pub struct Pause(Arc<(Mutex<bool>, Condvar)>);

impl Pause {
    /// A latch that is open.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Closes it. The next job to reach a checkpoint stops there.
    pub fn hold(&self) {
        if let Ok(mut held) = self.0.0.lock() {
            *held = true;
        }
    }

    /// Opens it and wakes whatever was waiting.
    pub fn release(&self) {
        if let Ok(mut held) = self.0.0.lock() {
            *held = false;
        }
        self.0.1.notify_all();
    }

    /// Whether it is closed.
    #[must_use]
    pub fn held(&self) -> bool {
        self.0.0.lock().is_ok_and(|held| *held)
    }

    /// Stands still while it is closed, and returns when it opens.
    ///
    /// `cancel` is checked as well as waited on: a queue stopped while it is
    /// paused must not need un-pausing first, which is a state a person can
    /// reach and not get out of.
    pub fn wait(&self, cancel: &Cancel) {
        let Ok(mut held) = self.0.0.lock() else { return };
        while *held && !cancel.is_cancelled() {
            // Timed rather than plain, so a `cancel` that arrives while this is
            // waiting is noticed without the canceller having to know there is a
            // latch to open as well.
            let Ok((next, _)) = self.0.1.wait_timeout(held, Duration::from_millis(50)) else {
                return;
            };
            held = next;
        }
    }
}

type Task = Box<dyn FnOnce(&Cancel) + Send + 'static>;

struct Job {
    task: Task,
    cancel: Cancel,
    lane: Lane,
}

#[derive(Default)]
struct Queues {
    parallel: VecDeque<Job>,
    serial: VecDeque<Job>,
    serial_busy: bool,
    shutdown: bool,
    /// Workers that have returned. Under the same lock as everything else here,
    /// so the count and the wait cannot race.
    left: usize,
}

impl Queues {
    fn take(&mut self) -> Option<Job> {
        if let Some(job) = self.parallel.pop_front() {
            return Some(job);
        }
        if !self.serial_busy
            && let Some(job) = self.serial.pop_front()
        {
            self.serial_busy = true;
            return Some(job);
        }
        None
    }

    fn has_work(&self) -> bool {
        !self.parallel.is_empty() || (!self.serial.is_empty() && !self.serial_busy)
    }
}

struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
    /// Signalled by a worker on its way out, so a dropped pool waits on an
    /// event rather than on a timer — the same rule the frame loop follows.
    left: Condvar,
    /// Jobs that panicked. Surfaced rather than swallowed: one unreadable
    /// directory must not take down a window, but it must also not be invisible.
    panics: AtomicU64,
    completed: AtomicU64,
}

/// The process's job pool.
pub struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Starts a pool one worker per available core.
    #[must_use]
    pub fn new() -> Self {
        Self::with_width(
            std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get),
        )
    }

    /// Starts a pool of exactly `width` workers, at least one.
    #[must_use]
    pub fn with_width(width: usize) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            ready: Condvar::new(),
            left: Condvar::new(),
            panics: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        });
        let workers = (0..width.max(1))
            .filter_map(|index| {
                let shared = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name(format!("skerry-job-{index}"))
                    .spawn(move || worker(&shared))
                    .ok()
            })
            .collect();
        Self { shared, workers }
    }

    /// How many workers are running.
    #[must_use]
    pub fn width(&self) -> usize {
        self.workers.len()
    }

    /// Queues `task` on `lane`, to be run with `cancel`.
    pub fn spawn<F>(&self, lane: Lane, cancel: Cancel, task: F)
    where
        F: FnOnce(&Cancel) + Send + 'static,
    {
        let job = Job { task: Box::new(task), cancel, lane };
        {
            let Ok(mut queues) = self.shared.queues.lock() else {
                // A poisoned queue means a worker panicked while holding the
                // lock, which cannot happen — the lock is never held across a
                // job. Dropping the task loses work rather than aborting the UI.
                return;
            };
            match lane {
                Lane::Parallel => queues.parallel.push_back(job),
                Lane::Serial => queues.serial.push_back(job),
            }
        }
        self.shared.ready.notify_one();
    }

    /// Jobs that finished, and jobs that panicked.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.shared.completed.load(Ordering::Relaxed), self.shared.panics.load(Ordering::Relaxed))
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut queues) = self.shared.queues.lock() {
            queues.shutdown = true;
            // Queued-but-unstarted work is abandoned, not run: a pool being
            // dropped is a process exiting, and a listing nobody will see is
            // not worth the exit latency.
            queues.parallel.clear();
            queues.serial.clear();
        }
        self.shared.ready.notify_all();

        // **A bounded wait, not a join.** A worker inside a blocking syscall
        // cannot be asked to stop: a read from a peer's clipboard pipe that the
        // peer never writes to, a `statx` on a suspended sshfs host, an `exec` of
        // a binary on a stalled mount. Joining meant the window vanished and the
        // process never exited — nothing on screen, nothing to kill but a pid.
        //
        // Past the deadline the workers are left running and `main` returns,
        // which ends the process and them with it. What is given up is the
        // chance to run their destructors, and what they own is a closure and a
        // file descriptor the kernel closes anyway.
        let width = self.workers.len();
        if let Ok(queues) = self.shared.queues.lock() {
            // On the condvar and not on a timer: the wait ends the moment the
            // last worker returns, and the deadline is only the bound.
            let _ = self
                .shared
                .left
                .wait_timeout_while(queues, SHUTDOWN_GRACE, |queues| queues.left < width);
        }
        for worker in self.workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                std::mem::forget(worker);
            }
        }
    }
}

/// How long a dropped pool waits for its workers before leaving them to the
/// process exit.
///
/// Long enough that an ordinary job — a `statx` batch, a directory read — is
/// finished and reaped, short enough that a person closing a window sees it
/// close. Every job in this program checks its cancellation token, so the ones
/// that can stop have stopped by the time this matters.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Workers run at the **same** priority as the frame loop, and that was measured.
///
/// The obvious idea is to nice them down: the pool is as wide as the machine, so
/// while a directory lands every core is inside `statx` and the loop is trying to
/// draw. Tried at nice +5 on a 24-thread machine over 200,000 entries, eight runs
/// each: the worst frame while the listing lands was 2.6–6.2 ms before and
/// 3.8–9.5 ms after, and the only frame ever measured over the 8 ms budget was
/// one of the niced ones. It does not help because the loop is not only competing
/// with the pool, it is *waiting* for it — slowing the workers lengthens the
/// window in which there is anything to compete over.
///
/// Recorded here rather than deleted, so it is not tried a third time.
fn worker(shared: &Arc<Shared>) {
    run(shared);
    if let Ok(mut queues) = shared.queues.lock() {
        queues.left += 1;
    }
    shared.left.notify_all();
}

fn run(shared: &Arc<Shared>) {
    loop {
        let job = {
            let Ok(mut queues) = shared.queues.lock() else { return };
            loop {
                if queues.shutdown {
                    return;
                }
                if let Some(job) = queues.take() {
                    break job;
                }
                let Ok(next) = shared.ready.wait(queues) else { return };
                queues = next;
            }
        };

        let lane = job.lane;
        let Job { task, cancel, .. } = job;
        // Rule 1's other half: a panic in a job is a failed job, not a dead
        // window. Caught here so the pool survives a bad `.desktop` file or an
        // unreadable directory. This is why the release profile unwinds.
        if std::panic::catch_unwind(AssertUnwindSafe(|| task(&cancel))).is_err() {
            shared.panics.fetch_add(1, Ordering::Relaxed);
        }
        shared.completed.fetch_add(1, Ordering::Relaxed);

        if lane == Lane::Serial
            && let Ok(mut queues) = shared.queues.lock()
        {
            queues.serial_busy = false;
            if queues.has_work() {
                shared.ready.notify_one();
            }
        }
    }
}

/// An eventfd the frame loop can wait on alongside the display connection.
///
/// This is what makes "zero CPU at idle" and "a job's result appears the moment
/// it is ready" the same design instead of opposite ones: the loop blocks in
/// `poll` on the display fd and this one, and a finished job writes eight bytes.
/// No timer, no polling interval, no wake-up that had nothing to do.
#[derive(Debug)]
pub struct Waker {
    fd: Arc<OwnedFd>,
}

impl Waker {
    /// Creates the eventfd. Fails only if the process is out of descriptors.
    pub fn new() -> std::io::Result<Self> {
        let fd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self { fd: Arc::new(fd) })
    }

    /// A handle that can be moved into a job.
    #[must_use]
    pub fn handle(&self) -> WakeHandle {
        WakeHandle { fd: Arc::clone(&self.fd) }
    }

    /// The descriptor to poll for readability.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Consumes any pending wake-ups. Returns whether there were any.
    // Draining is the point of the call; the return value is a convenience.
    #[allow(clippy::must_use_candidate)]
    pub fn drain(&self) -> bool {
        let mut buffer = [0u8; 8];
        rustix::io::read(self.fd.as_fd(), &mut buffer).is_ok()
    }
}

/// The sending half of a [`Waker`], cheap to clone into a job.
#[derive(Clone, Debug)]
pub struct WakeHandle {
    fd: Arc<OwnedFd>,
}

impl WakeHandle {
    /// Wakes the frame loop. Safe from any thread, and cheap enough to call per
    /// completed job rather than per batch.
    pub fn wake(&self) {
        // A full counter means a wake-up is already pending and unread, which is
        // exactly the state a wake-up is asking for.
        let _ = rustix::io::write(self.fd.as_fd(), &1u64.to_ne_bytes());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Cancel, Lane, Pause, Pool, SHUTDOWN_GRACE, Waker};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// A held job stands still and goes on when it is let go.
    #[test]
    fn a_paused_job_stops_at_its_checkpoint_and_carries_on() {
        let pause = Pause::new();
        let cancel = Cancel::new();
        let reached = Arc::new(AtomicUsize::new(0));
        pause.hold();

        let running = {
            let (pause, cancel, reached) = (pause.clone(), cancel.clone(), Arc::clone(&reached));
            std::thread::spawn(move || {
                for _ in 0..3 {
                    pause.wait(&cancel);
                    reached.fetch_add(1, Ordering::Release);
                }
            })
        };

        // Long enough that a latch which did not hold would have run all three.
        let a_while = std::time::Duration::from_millis(120);
        std::thread::sleep(a_while); // skerry-allow: no-blocking-ui — time passing is the claim
        assert_eq!(reached.load(Ordering::Acquire), 0, "the job ran through a closed latch");
        pause.release();
        running.join().expect("the job did not finish after the latch opened");
        assert_eq!(reached.load(Ordering::Acquire), 3);
    }

    /// **A queue stopped while paused must stop.** Without this a person can
    /// pause, press cancel, and be left with a job that is waiting on a latch
    /// nothing on screen says is closed.
    #[test]
    fn a_cancel_gets_a_job_out_of_a_pause_nobody_opened() {
        let pause = Pause::new();
        let cancel = Cancel::new();
        pause.hold();
        let running = {
            let (pause, cancel) = (pause.clone(), cancel.clone());
            std::thread::spawn(move || pause.wait(&cancel))
        };
        let a_while = std::time::Duration::from_millis(60);
        std::thread::sleep(a_while); // skerry-allow: no-blocking-ui — time passing is the claim
        assert!(!running.is_finished(), "the latch did not hold");
        cancel.cancel();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !running.is_finished() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(running.is_finished(), "a cancelled job stayed in the pause");
        assert!(pause.held(), "the cancel opened the latch instead of leaving it closed");
    }

    /// A worker inside a blocking syscall cannot be asked to stop, and joining
    /// one meant the window vanished and the process never exited — a peer that
    /// asked for the clipboard and stopped reading was enough. Dropping the pool
    /// has to return whether or not every worker does.
    #[test]
    fn dropping_the_pool_does_not_wait_for_a_job_that_will_never_finish() {
        // Declared before the pool so it outlives it: the job blocks on this
        // channel, and a sender dropped first would release it.
        let (_keep, blocked) = std::sync::mpsc::channel::<()>();
        let started = Instant::now();
        {
            let pool = Pool::with_width(2);
            let stuck = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&stuck);
            pool.spawn(Lane::Parallel, Cancel::new(), move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                // Stands in for a `read` on a pipe nobody writes to: a blocking
                // wait with no timeout that does not check its token, which is
                // the point — it cannot.
                let _ = blocked.recv();
            });
            while stuck.load(Ordering::SeqCst) == 0 {
                std::hint::spin_loop();
            }
        }
        let waited = started.elapsed();
        assert!(waited < SHUTDOWN_GRACE * 4, "dropping the pool took {waited:?}");
        // And it did wait — a pool whose workers all returned is joined, which
        // this deliberately is not.
        assert!(waited >= SHUTDOWN_GRACE, "it did not wait for them at all: {waited:?}");
    }

    #[test]
    fn every_queued_job_runs() {
        let pool = Pool::with_width(4);
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..256 {
            let counter = Arc::clone(&counter);
            pool.spawn(Lane::Parallel, Cancel::new(), move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
        }
        while pool.stats().0 < 256 {
            std::hint::spin_loop();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 256);
    }

    #[test]
    fn the_serial_lane_never_runs_two_jobs_at_once() {
        let pool = Pool::with_width(8);
        let inside = Arc::new(AtomicUsize::new(0));
        let worst = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let inside = Arc::clone(&inside);
            let worst = Arc::clone(&worst);
            pool.spawn(Lane::Serial, Cancel::new(), move |_| {
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                worst.fetch_max(now, Ordering::SeqCst);
                inside.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while pool.stats().0 < 64 {
            std::hint::spin_loop();
        }
        assert_eq!(worst.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panicking_job_is_counted_and_the_pool_survives_it() {
        let pool = Pool::with_width(2);
        pool.spawn(Lane::Parallel, Cancel::new(), |_| panic!("an unreadable directory"));
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        pool.spawn(Lane::Parallel, Cancel::new(), move |_| {
            flag.fetch_add(1, Ordering::Relaxed);
        });
        while pool.stats().0 < 2 {
            std::hint::spin_loop();
        }
        assert_eq!(pool.stats().1, 1, "the panic is reported");
        assert_eq!(ran.load(Ordering::Relaxed), 1, "the next job still ran");
    }

    #[test]
    fn a_cancelled_job_sees_the_flag() {
        let pool = Pool::with_width(1);
        let cancel = Cancel::new();
        cancel.cancel();
        let saw = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&saw);
        pool.spawn(Lane::Parallel, cancel, move |c| {
            if c.is_cancelled() {
                flag.fetch_add(1, Ordering::Relaxed);
            }
        });
        while pool.stats().0 < 1 {
            std::hint::spin_loop();
        }
        assert_eq!(saw.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_waker_reports_a_wake_and_then_reports_nothing() {
        let waker = Waker::new().expect("eventfd");
        assert!(!waker.drain(), "nothing pending yet");
        waker.handle().wake();
        assert!(waker.drain(), "the wake arrived");
        assert!(!waker.drain(), "and was consumed");
    }
}
