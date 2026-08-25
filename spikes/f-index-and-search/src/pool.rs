//! One job pool, owned rather than imported.
//!
//! [ADR 0005](../../../docs/adr/0005-no-async-runtime.md) allows one pool for
//! the process and no async runtime, so measuring this design with `rayon`
//! would measure a design we are not allowed to ship. It is `std::thread`, a
//! channel and a cancellation flag — which is roughly what `sk-util` will hold,
//! minus the per-device queues that Spike B has no second device to justify.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

/// Set once and read by every worker between jobs. A hung job cannot be
/// interrupted mid-syscall — that is the whole difficulty of a dead mount — so
/// cancellation means "stop taking new work and stop caring about the result",
/// not "unblock the thread".
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A cheap, cloneable handle for submitting work from inside a job. Needed
/// because a parallel tree walk is jobs that spawn jobs, and a `&Pool` cannot
/// cross into a `'static` closure.
#[derive(Clone)]
pub struct Submitter {
    sender: mpsc::Sender<Job>,
    queued: Arc<AtomicU64>,
}

impl Submitter {
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        let _ = self.sender.send(Box::new(job));
    }
}

pub struct Pool {
    sender: Option<mpsc::Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    pub queued: Arc<AtomicU64>,
    pub completed: Arc<AtomicU64>,
}

impl Pool {
    /// `ncpu - 2`: one core left for the UI thread and one for the watcher, per
    /// the threading diagram in 04-architecture.
    pub fn new(cancel: Cancel) -> Self {
        let width = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(2);
        Self::with_width(width, cancel)
    }

    pub fn with_width(width: usize, cancel: Cancel) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let completed = Arc::new(AtomicU64::new(0));

        let workers = (0..width)
            .map(|_| {
                let receiver = receiver.clone();
                let completed = completed.clone();
                let cancel = cancel.clone();
                std::thread::spawn(move || loop {
                    // The lock is held only to take a job, never to run one.
                    let job = match receiver.lock() {
                        Ok(guard) => guard.recv(),
                        Err(_) => return,
                    };
                    match job {
                        Ok(job) => {
                            if cancel.is_cancelled() {
                                continue;
                            }
                            job();
                            completed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => return,
                    }
                })
            })
            .collect();

        Self {
            sender: Some(sender),
            workers,
            queued: Arc::new(AtomicU64::new(0)),
            completed,
        }
    }

    pub fn width(&self) -> usize {
        self.workers.len()
    }

    /// A handle jobs can carry. Cloning it does not extend the pool's lifetime;
    /// the workers stop when `drain` drops the pool's own sender and every
    /// submitter has gone with the jobs that held it.
    pub fn submitter(&self) -> Option<Submitter> {
        self.sender.as_ref().map(|sender| Submitter {
            sender: sender.clone(),
            queued: self.queued.clone(),
        })
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(sender) = &self.sender {
            self.queued.fetch_add(1, Ordering::Relaxed);
            let _ = sender.send(Box::new(job));
        }
    }

    pub fn outstanding(&self) -> u64 {
        self.queued.load(Ordering::Relaxed) - self.completed.load(Ordering::Relaxed)
    }

    /// Drops the sender so workers see the channel close, then joins them.
    /// Deliberately *not* called on the hung-mount path: a worker blocked in a
    /// dead `readdir` never returns, and pretending otherwise would hide the
    /// thing being measured.
    pub fn drain(&mut self) {
        self.sender = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
