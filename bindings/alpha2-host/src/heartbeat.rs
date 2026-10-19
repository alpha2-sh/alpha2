//! The watch on worker threads that call what may never return: decode
//! shards, the encode worker, a packet copy, and the render cache's writer.
//!
//! Some `FFmpeg` calls wait with no timeout: Vulkan decode and encode wait on
//! their execution fences with `UINT64_MAX`, and a muxer's `write(2)` to a
//! hard-mounted share that stopped answering is retried forever. Nothing can
//! interrupt such a call, so a worker stuck in one is found by its heartbeat,
//! reported, and left behind instead of joined.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// When a worker last began an `FFmpeg` operation, and whether it is doing
/// work at all.
///
/// A decode shard beats before every decoder operation: opening a session,
/// seeking, taking the next frame, closing. The encode worker beats before
/// every command and before every encoder or muxer call inside it. Between
/// beats a worker does one operation, which is bounded unless `FFmpeg` blocks
/// inside a call, and that it does forever where a hardware decoder waits for
/// a surface that never comes back, or a hardware encoder for work that never
/// completes. The host reads the beat to turn that into a failure instead of
/// an export that never ends.
#[derive(Debug)]
pub struct WorkerHeartbeat {
    epoch: Instant,
    /// Nanoseconds from `epoch` to the start of the latest operation.
    last: AtomicU64,
    busy: AtomicBool,
    /// Set once the worker opened a hardware session. From then on it may
    /// hold GPU resources, and any of its `FFmpeg` calls may wait on the GPU:
    /// freeing a picture does.
    hardware: AtomicBool,
    /// Set while the worker is inside a hardware session's open, which
    /// negotiates with the GPU. An open that fails leaves nothing on it, so
    /// this is cleared when the open returns and only a success sets
    /// `hardware`.
    opening_hardware: AtomicBool,
    /// What the worker is reading now, for a stall report to name.
    activity: Mutex<Option<Activity>>,
    /// Set when the worker's thread was left running ([`join_or_abandon`]).
    /// The process-wide flag says some worker was; this says which one.
    abandoned: AtomicBool,
    /// Where the worker blocks forever, as a stuck `FFmpeg` call does. Stalls
    /// are not reproducible on the machines that run the tests.
    #[cfg(test)]
    pub stall: Option<InjectedStall>,
    #[cfg(test)]
    operations: std::sync::atomic::AtomicUsize,
    /// Set as the worker blocks at its injected stall.
    #[cfg(test)]
    stalled: AtomicBool,
}

/// Where a test makes a worker block forever, panic, or slow down.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub enum InjectedStall {
    /// At the operation after this many.
    AfterOperations(usize),
    /// While closing: a decode shard closing its decoders, the encode worker
    /// finishing its output.
    OnClose,
    /// Panics at the operation after this many, as a defect in a decoder or
    /// provider does.
    PanicAfterOperations(usize),
    /// Takes this long over each step of progress, as a slow machine does: a
    /// long call that keeps going outlasts the stall limit on any build.
    // A provider open is the slow call it stands in for.
    #[cfg_attr(not(feature = "extensions"), allow(dead_code))]
    SlowProgress(Duration),
}

/// What a decode shard is reading, as a stall report names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Activity {
    /// The source, clip, and source time, in words.
    pub what: String,
    /// Whether an extension provider, not `FFmpeg`, does the work.
    pub provider: bool,
}

/// Work a worker is inside: see [`WorkerHeartbeat::begin_work`].
pub struct HeartbeatWork(Arc<WorkerHeartbeat>);

/// A hardware session's open in progress: see
/// [`WorkerHeartbeat::begin_hardware_open`].
// Only a hardware decode session opens one.
#[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
pub struct HardwareOpen<'a>(&'a WorkerHeartbeat);

#[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
impl HardwareOpen<'_> {
    /// The open succeeded: the worker holds a hardware session.
    pub fn opened(self) {
        self.0.opened_hardware();
    }
}

impl Drop for HardwareOpen<'_> {
    fn drop(&mut self) {
        self.0.opening_hardware.store(false, Ordering::Release);
    }
}

/// A wait outside the worker: see [`WorkerHeartbeat::idle`].
pub struct HeartbeatIdle {
    beat: Arc<WorkerHeartbeat>,
    was_busy: bool,
}

impl Drop for HeartbeatIdle {
    fn drop(&mut self) {
        if self.was_busy {
            // Marked before busy, as `begin_work` does, so the watch never pairs busy with the time
            // before the wait.
            self.beat.mark();
            self.beat.busy.store(true, Ordering::Release);
        }
    }
}

impl Drop for HeartbeatWork {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

impl Default for WorkerHeartbeat {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            last: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            hardware: AtomicBool::new(false),
            opening_hardware: AtomicBool::new(false),
            activity: Mutex::new(None),
            abandoned: AtomicBool::new(false),
            #[cfg(test)]
            stall: None,
            #[cfg(test)]
            operations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            stalled: AtomicBool::new(false),
        }
    }
}

impl WorkerHeartbeat {
    /// A heartbeat whose worker blocks forever where `stall` says.
    #[cfg(test)]
    pub fn stalling(stall: Option<InjectedStall>) -> Self {
        Self {
            stall,
            ..Self::default()
        }
    }

    /// Whether this worker's thread was left running, stuck in `FFmpeg`.
    #[cfg(test)]
    pub fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    /// An operation starts now.
    pub fn beat(&self) {
        self.mark();
        self.inject();
    }

    /// The operation in progress produced something (a seek decoded one
    /// more frame): it is alive, not blocked. Unlike [`Self::beat`] it starts
    /// no new operation.
    pub fn progressed(&self) {
        self.mark();
        #[cfg(test)]
        if let Some(InjectedStall::SlowProgress(step)) = self.stall {
            thread::sleep(step);
        }
    }

    fn mark(&self) {
        self.last.store(
            u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }

    /// Blocks or panics here if the test said to.
    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    const fn inject(&self) {}

    #[cfg(test)]
    fn inject(&self) {
        match self.stall {
            Some(InjectedStall::AfterOperations(limit))
                if self.operations.fetch_add(1, Ordering::Relaxed) >= limit =>
            {
                self.stall_forever();
            }
            Some(InjectedStall::PanicAfterOperations(limit))
                if self.operations.fetch_add(1, Ordering::Relaxed) >= limit =>
            {
                panic!("injected decoder panic");
            }
            _ => {}
        }
    }

    #[cfg(test)]
    fn stall_forever(&self) -> ! {
        self.stalled.store(true, Ordering::Release);
        loop {
            thread::park();
        }
    }

    /// Whether the worker has blocked at its injected stall.
    #[cfg(test)]
    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// The worker is doing work until the guard drops, which a panic's unwind
    /// does too: a worker that failed is not left looking busy, to be
    /// reported as stuck.
    pub fn begin_work(self: &Arc<Self>) -> HeartbeatWork {
        let work = HeartbeatWork(Arc::clone(self));
        // Busy only after the time is marked, so the watch never pairs it with the previous
        // operation's start; an injected stall comes after both, as a stuck call does.
        self.mark();
        self.busy.store(true, Ordering::Release);
        self.inject();
        work
    }

    /// The worker starts closing what it holds, which is watched like any
    /// operation: a hardware decode session frees pictures, and that waits on
    /// the GPU inside `FFmpeg`; an encoder flushes and the muxer writes its
    /// trailer.
    pub fn begin_close(self: &Arc<Self>) -> HeartbeatWork {
        let work = self.begin_work();
        #[cfg(test)]
        if matches!(self.stall, Some(InjectedStall::OnClose)) {
            self.stall_forever();
        }
        work
    }

    /// The worker starts opening a hardware session; it holds hardware from
    /// now on only if [`HardwareOpen::opened`] says the open succeeded.
    #[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
    pub fn begin_hardware_open(&self) -> HardwareOpen<'_> {
        self.opening_hardware.store(true, Ordering::Release);
        HardwareOpen(self)
    }

    /// The worker opened a hardware session.
    pub fn opened_hardware(&self) {
        self.hardware.store(true, Ordering::Release);
    }

    /// Whether the worker has opened a hardware session, or is opening one.
    pub fn holds_hardware(&self) -> bool {
        self.hardware.load(Ordering::Acquire) || self.opening_hardware.load(Ordering::Acquire)
    }

    /// The worker now reads what `activity` names, until the next call.
    pub fn set_activity(&self, activity: Option<Activity>) {
        *self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = activity;
    }

    /// What the worker said it reads now.
    pub fn activity(&self) -> Option<Activity> {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The worker waits for something outside it until the guard drops: no
    /// operation of its own is in progress, so the wait is not a stall of
    /// its own. Work resumes as a new operation.
    pub fn idle(self: &Arc<Self>) -> HeartbeatIdle {
        let was_busy = self.busy.swap(false, Ordering::AcqRel);
        HeartbeatIdle {
            beat: Arc::clone(self),
            was_busy,
        }
    }

    /// How long the worker has been inside its current operation, while it is
    /// doing work.
    pub fn stalled_for(&self) -> Option<Duration> {
        if !self.busy.load(Ordering::Acquire) {
            return None;
        }
        let started = Duration::from_nanos(self.last.load(Ordering::Acquire));
        Some(self.epoch.elapsed().saturating_sub(started))
    }

    /// Whether the worker has been inside one operation for longer than
    /// `limit`.
    pub fn stuck_past(&self, limit: Duration) -> Option<Duration> {
        self.stalled_for()
            .filter(|stalled_for| *stalled_for > limit)
    }
}

/// How many commands a worker has taken off its queue.
///
/// A host blocked on a full queue waits on this rather than in a blocking
/// send, so it wakes as soon as the worker takes one and can also notice a
/// worker that has stopped taking any.
#[derive(Default)]
pub struct CommandProgress {
    taken: Mutex<u64>,
    changed: Condvar,
}

impl CommandProgress {
    /// The count so far, to pass to [`Self::wait_past`] as `seen`.
    pub fn current(&self) -> u64 {
        *self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called by the worker as it takes a command; wakes every waiter.
    pub fn took_one(&self) {
        let mut taken = self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *taken = taken.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Waits until the worker has taken a command since `seen`, or `timeout`.
    pub fn wait_past(&self, seen: u64, timeout: Duration) {
        let taken = self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.changed
                .wait_timeout_while(taken, timeout, |taken| *taken == seen)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// How often a host waiting on a worker checks it for a stall. A wait ends
/// early on the event it waits for; this only bounds how late a stall is
/// noticed.
pub const STALL_POLL: Duration = Duration::from_millis(250);

/// How often a worker being dropped checks whether its thread has finished.
const JOIN_POLL: Duration = Duration::from_millis(5);

/// Set once a worker left its thread running, stuck in `FFmpeg`.
static THREADS_ABANDONED: AtomicBool = AtomicBool::new(false);

/// Whether this process holds worker threads stuck in `FFmpeg`.
///
/// Such a process cannot be used again, and cannot exit normally either: its
/// threads may be inside the GPU driver, whose exit handlers then wait for
/// them.
pub fn worker_threads_abandoned() -> bool {
    THREADS_ABANDONED.load(Ordering::Acquire)
}

/// How [`join_or_leave`] ended.
pub enum Joined<T> {
    /// The thread finished and returned this.
    Finished(T),
    /// The thread panicked.
    Panicked,
    /// The thread is still running and was left so.
    Left,
}

/// Joins `join` once it finishes, or leaves it running.
///
/// A cancelled worker finishes within one operation. One that does not is stuck inside a call no
/// cancellation reaches (`FFmpeg`, an extension provider, or a filesystem that stopped answering),
/// and joining it would hang the caller too. The wait ends when the thread finishes, when its
/// current operation has run past `limit` (`heartbeats` say so), or after `deadline` from now,
/// whichever is first; a worker already reported stuck passes `Duration::ZERO`. A worker that beats
/// before everything it does that may block passes no deadline: only a stall ends its wait early. A
/// thread left running keeps what it holds, so nothing it may use is destroyed under it, and marks
/// each of `heartbeats`. Unlike [`join_or_abandon`] it does not mark the process: a caller whose
/// worker holds nothing of the GPU's or `FFmpeg`'s decides what a thread left running means.
pub fn join_or_leave<T>(
    join: JoinHandle<T>,
    heartbeats: &[Arc<WorkerHeartbeat>],
    limit: Duration,
    deadline: Option<Duration>,
) -> Joined<T> {
    let started = Instant::now();
    while !join.is_finished()
        && deadline.is_none_or(|deadline| started.elapsed() < deadline)
        && heartbeats
            .iter()
            .all(|beat| beat.stuck_past(limit).is_none())
    {
        thread::sleep(JOIN_POLL);
    }
    if !join.is_finished() {
        for beat in heartbeats {
            beat.abandoned.store(true, Ordering::Release);
        }
        return Joined::Left;
    }
    join.join().map_or(Joined::Panicked, Joined::Finished)
}

/// [`join_or_leave`], waiting at most `limit`, for a worker that calls into `FFmpeg` or an
/// extension provider: a thread left running also marks the process
/// ([`worker_threads_abandoned`]), which exits after its answer. Returns false when the thread was
/// left or had panicked.
pub fn join_or_abandon(
    join: JoinHandle<()>,
    heartbeats: &[Arc<WorkerHeartbeat>],
    limit: Duration,
    what: &str,
) -> bool {
    match join_or_leave(join, heartbeats, limit, Some(limit)) {
        Joined::Finished(()) => true,
        Joined::Left => {
            THREADS_ABANDONED.store(true, Ordering::Release);
            let inside = if heartbeats.iter().any(|beat| {
                beat.stalled_for().is_some()
                    && beat.activity().is_some_and(|activity| activity.provider)
            }) {
                "an extension provider"
            } else {
                "FFmpeg"
            };
            alpha2_os::diagnostic!(
                "alpha2 native {what} left running: it is stuck inside {inside}"
            );
            false
        }
        Joined::Panicked => {
            alpha2_os::diagnostic!("alpha2 native {what} panicked during cleanup");
            false
        }
    }
}

/// What a [`run_watched`] worker tells the thread watching it.
enum WatchedEvent<T> {
    /// It reached this much of its work: a progress report, not a beat.
    Progress(u64),
    /// It finished with this, having dropped everything else it held.
    Done(T),
}

/// A worker [`run_watched`] starts: what it is called in a report, its
/// heartbeat, and how long one of its operations may run.
pub struct Watched<'a> {
    /// The worker's name in a diagnostic, such as `"packet copy"`.
    pub what: &'static str,
    /// Beaten by the worker before each operation that may block forever;
    /// [`run_watched`] holds it busy while the body runs.
    pub heartbeat: Arc<WorkerHeartbeat>,
    /// See [`crate::config::configured_stall_timeout`].
    pub limit: Duration,
    /// The export's cancel, which the body checks between operations; set
    /// here when the worker is found stuck or a progress report fails.
    pub cancel: &'a alpha2_media::CancelToken,
}

/// Runs `body` on a thread of its own, under `watched.heartbeat`, and waits
/// for it on this one, as the render path waits on its encode worker.
///
/// `body` beats before each operation that may block forever and reports its
/// progress through the function it is given, which this thread hands to
/// `progress`. A failed report cancels the work, and its error is returned
/// once the worker stops. When the worker has been inside one operation for
/// longer than `watched.limit`, nothing it does will come back: this thread
/// returns `stalled`'s error for how long it has been there, cancels, and
/// leaves the thread running ([`join_or_abandon`]). A termination's cancel
/// stops a live worker at its next operation; a stuck one is found by the
/// same stall check, so the wait is bounded either way.
///
/// # Errors
/// A worker that cannot start, panicked, stalled, or whose progress report
/// failed.
pub fn run_watched<T: Send + 'static>(
    watched: &Watched<'_>,
    body: impl FnOnce(&dyn Fn(u64)) -> T + Send + 'static,
    progress: &mut dyn FnMut(u64) -> Result<(), String>,
    stalled: &dyn Fn(Duration) -> String,
) -> Result<T, String> {
    let (events, received) = std::sync::mpsc::channel();
    let beat = Arc::clone(&watched.heartbeat);
    let join = thread::Builder::new()
        .name(format!("alpha2-{}", watched.what.replace(' ', "-")))
        .spawn(move || {
            let report = events.clone();
            let work = beat.begin_work();
            let result = body(&move |reached| {
                let _ = report.send(WatchedEvent::Progress(reached));
            });
            drop(work);
            let _ = events.send(WatchedEvent::Done(result));
        })
        .map_err(|error| format!("failed to start the {}: {error}", watched.what))?;
    let mut failed = None;
    loop {
        match received.recv_timeout(STALL_POLL) {
            Ok(WatchedEvent::Progress(reached)) => {
                if failed.is_none()
                    && let Err(error) = progress(reached)
                {
                    failed = Some(error);
                    watched.cancel.cancel();
                }
            }
            Ok(WatchedEvent::Done(result)) => {
                join_or_abandon(
                    join,
                    std::slice::from_ref(&watched.heartbeat),
                    watched.limit,
                    watched.what,
                );
                return failed.map_or(Ok(result), Err);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(stalled_for) = watched.heartbeat.stuck_past(watched.limit) {
                    let error = failed.unwrap_or_else(|| stalled(stalled_for));
                    watched.cancel.cancel();
                    join_or_abandon(
                        join,
                        std::slice::from_ref(&watched.heartbeat),
                        Duration::ZERO,
                        watched.what,
                    );
                    return Err(error);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = join.join();
                return Err(format!(
                    "{}the {} panicked",
                    crate::protocol::internal_tag(),
                    watched.what
                ));
            }
        }
    }
}
