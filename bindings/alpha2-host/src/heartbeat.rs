//! The watch on worker threads that call what may never return.
//!
//! Some `FFmpeg` calls wait with no timeout: Vulkan decode and encode wait on their fences with
//! `UINT64_MAX`, and a muxer's `write(2)` to a dead hard-mounted share retries forever. Nothing
//! can interrupt such a call, so a stuck worker is found by its heartbeat, reported, and left
//! behind instead of joined.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// When a worker last began an operation, and whether it is doing work at all.
///
/// Decode shards beat before every decoder operation; the encode worker before every command and
/// every encoder or muxer call inside it. Between beats a worker does one operation, which is
/// bounded unless `FFmpeg` blocks inside it.
#[derive(Debug)]
pub struct WorkerHeartbeat {
    /// On the awake clock: a system sleep is not time spent inside an operation.
    epoch: alpha2_os::AwakeInstant,
    /// Nanoseconds from `epoch` to the start of the latest operation.
    last: AtomicU64,
    busy: AtomicBool,
    /// Once set, any of the worker's `FFmpeg` calls may wait on the GPU (freeing a picture does).
    hardware: AtomicBool,
    /// Set only during a hardware open; a failed open leaves nothing on the GPU, so only success
    /// sets `hardware`.
    opening_hardware: AtomicBool,
    activity: Mutex<Option<Activity>>,
    /// Which worker was left running; the process-wide flag only says some was.
    abandoned: AtomicBool,
    /// Stalls are not reproducible on test machines, so tests inject them.
    #[cfg(test)]
    pub stall: Option<InjectedStall>,
    #[cfg(test)]
    operations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    stalled: AtomicBool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub enum InjectedStall {
    AfterOperations(usize),
    OnClose,
    /// Where a worker drops what it held, after its last result.
    OnTeardown,
    PanicAfterOperations(usize),
    /// A long call that keeps going outlasts the stall limit on any build.
    // A provider open is the slow call it stands in for.
    #[cfg_attr(not(feature = "extensions"), allow(dead_code))]
    SlowProgress(Duration),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Activity {
    pub what: String,
    pub provider: bool,
}

pub struct HeartbeatWork(Arc<WorkerHeartbeat>);

// Only a hardware decode session opens one.
#[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
pub struct HardwareOpen<'a>(&'a WorkerHeartbeat);

#[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
impl HardwareOpen<'_> {
    pub fn opened(self) {
        self.0.opened_hardware();
    }
}

impl Drop for HardwareOpen<'_> {
    fn drop(&mut self) {
        self.0.opening_hardware.store(false, Ordering::Release);
    }
}

/// A shard's heartbeat as provider calls report to it: waits for another shard's call to the same
/// source are idle, work of their own beats.
pub struct ProviderHeartbeat<'a>(pub &'a Arc<WorkerHeartbeat>);

impl alpha2_host_core::providers::ProviderWatch for ProviderHeartbeat<'_> {
    type Waiting = HeartbeatIdle;

    fn waiting(&self) -> HeartbeatIdle {
        self.0.idle()
    }

    fn beat(&self) {
        self.0.beat();
    }
}

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
            epoch: alpha2_os::AwakeInstant::now(),
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
    #[cfg(test)]
    pub fn stalling(stall: Option<InjectedStall>) -> Self {
        Self {
            stall,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    pub fn beat(&self) {
        self.mark();
        self.inject();
    }

    /// The current operation produced something; unlike [`Self::beat`] it starts no new one.
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

    #[cfg(test)]
    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// The guard also drops on unwind, so a failed worker is not left looking busy and reported
    /// as stuck.
    pub fn begin_work(self: &Arc<Self>) -> HeartbeatWork {
        let work = HeartbeatWork(Arc::clone(self));
        // Busy only after the time is marked, so the watch never pairs it with the previous
        // operation's start.
        self.mark();
        self.busy.store(true, Ordering::Release);
        self.inject();
        work
    }

    /// Closing is watched too: freeing hardware pictures waits on the GPU, and an encoder flushes
    /// and the muxer writes its trailer.
    pub fn begin_close(self: &Arc<Self>) -> HeartbeatWork {
        let work = self.begin_work();
        #[cfg(test)]
        if matches!(self.stall, Some(InjectedStall::OnClose)) {
            self.stall_forever();
        }
        work
    }

    /// Dropping what a worker holds can wait on the GPU inside `FFmpeg`, so it is watched too.
    pub fn begin_teardown(self: &Arc<Self>) -> HeartbeatWork {
        let work = self.begin_work();
        #[cfg(test)]
        if matches!(self.stall, Some(InjectedStall::OnTeardown)) {
            self.stall_forever();
        }
        work
    }

    #[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
    pub fn begin_hardware_open(&self) -> HardwareOpen<'_> {
        self.opening_hardware.store(true, Ordering::Release);
        HardwareOpen(self)
    }

    pub fn opened_hardware(&self) {
        self.hardware.store(true, Ordering::Release);
    }

    pub fn holds_hardware(&self) -> bool {
        self.hardware.load(Ordering::Acquire) || self.opening_hardware.load(Ordering::Acquire)
    }

    pub fn set_activity(&self, activity: Option<Activity>) {
        *self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = activity;
    }

    pub fn activity(&self) -> Option<Activity> {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A wait for something outside the worker is not its own stall; work resumes as a new
    /// operation.
    pub fn idle(self: &Arc<Self>) -> HeartbeatIdle {
        let was_busy = self.busy.swap(false, Ordering::AcqRel);
        HeartbeatIdle {
            beat: Arc::clone(self),
            was_busy,
        }
    }

    pub fn stalled_for(&self) -> Option<Duration> {
        if !self.busy.load(Ordering::Acquire) {
            return None;
        }
        let started = Duration::from_nanos(self.last.load(Ordering::Acquire));
        Some(self.epoch.elapsed().saturating_sub(started))
    }

    pub fn stuck_past(&self, limit: Duration) -> Option<Duration> {
        self.stalled_for()
            .filter(|stalled_for| *stalled_for > limit)
    }
}

/// A host blocked on a full queue waits on this rather than in a blocking send, so it wakes when
/// the worker takes one and can notice a worker that stopped.
#[derive(Default)]
pub struct CommandProgress {
    taken: Mutex<u64>,
    changed: Condvar,
    waiting: AtomicBool,
    /// Set under `taken`'s lock so waiters wake at once instead of at their timeout.
    stopped: AtomicBool,
}

impl CommandProgress {
    pub fn set_waiting(&self, waiting: bool) {
        self.waiting.store(waiting, Ordering::Release);
    }

    pub fn waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    pub fn current(&self) -> u64 {
        *self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn took_one(&self) {
        let mut taken = self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *taken = taken.wrapping_add(1);
        self.changed.notify_all();
    }

    pub fn mark_stopped(&self) {
        let _taken = self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn wait_past(&self, seen: u64, timeout: Duration) {
        let taken = self
            .taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.changed
                .wait_timeout_while(taken, timeout, |taken| {
                    *taken == seen && !self.stopped.load(Ordering::Acquire)
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// Marks a worker's [`CommandProgress`] stopped however its thread ends.
pub struct WorkerExit(pub Arc<CommandProgress>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.mark_stopped();
    }
}

/// Only bounds how late a stall is noticed; waits end early on their event.
pub const STALL_POLL: Duration = Duration::from_millis(250);

const JOIN_POLL: Duration = Duration::from_millis(5);

static THREADS_ABANDONED: AtomicBool = AtomicBool::new(false);

/// Such a process cannot be reused or exit normally: its threads may be inside the GPU driver,
/// whose exit handlers then wait for them.
pub fn worker_threads_abandoned() -> bool {
    THREADS_ABANDONED.load(Ordering::Acquire)
}

pub enum Joined<T> {
    Finished(T),
    Panicked,
    Left,
}

/// Joins `join` once it finishes, or leaves it running.
///
/// A cancelled worker finishes within one operation; one that does not is stuck in a call no
/// cancellation reaches, and joining would hang the caller. The wait ends when the thread
/// finishes, when its operation passes `limit`, or after `deadline`, whichever is first. A thread
/// left running keeps what it holds and marks each of `heartbeats`. Unlike [`join_or_abandon`] it
/// does not mark the process.
pub fn join_or_leave<T>(
    join: JoinHandle<T>,
    heartbeats: &[Arc<WorkerHeartbeat>],
    limit: Duration,
    deadline: Option<Duration>,
) -> Joined<T> {
    // Bounded by its own deadline, so the request's watch does not count it.
    let _idle = request_idle();
    let started = alpha2_os::AwakeInstant::now();
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

/// For a worker that calls into `FFmpeg` or a provider: a thread left running also marks the
/// process ([`worker_threads_abandoned`]), which exits after its answer. Returns false when the
/// thread was left or had panicked.
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

enum WatchedEvent<T> {
    /// A progress report, not a beat.
    Progress(u64),
    /// Sent after the worker dropped everything else it held.
    Done(T),
}

pub struct Watched<'a> {
    pub what: &'static str,
    /// [`run_watched`] holds it busy while the body runs.
    pub heartbeat: Arc<WorkerHeartbeat>,
    pub limit: Duration,
    /// Set here when the worker is found stuck or a progress report fails.
    pub cancel: &'a alpha2_media::CancelToken,
}

/// Runs `body` on its own thread under `watched.heartbeat` and waits for it here.
///
/// A failed progress report cancels the work. A worker inside one operation past `watched.limit`
/// will not come back: this returns `stalled`'s error, cancels, and leaves the thread
/// ([`join_or_abandon`]). The wait is bounded either way.
///
/// # Errors
/// A worker that cannot start, panicked, stalled, or whose progress report failed.
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
        beat_request();
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

/// Which side of the host a request's current step is on; a stall is answered with its code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestSide {
    /// Reading media: conform, decoding, audio windows, the loudness pass.
    Input,
    /// Rendering and reading back.
    Gpu,
    /// Encoder probes and opens, encoding, the mux, the publish, the answer.
    Output,
    /// A native plugin call.
    Plugin,
    /// Work on the request's own data, such as `compose` and `edit`.
    Compute,
}

impl RequestSide {
    const ALL: [Self; 5] = [
        Self::Input,
        Self::Gpu,
        Self::Output,
        Self::Plugin,
        Self::Compute,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::Input => "DECODE_STALLED",
            Self::Gpu => "GPU_TIMEOUT",
            Self::Output => "ENCODE_STALLED",
            Self::Plugin => "PLUGIN_LIMIT",
            Self::Compute => "INTERNAL",
        }
    }

    const fn index(self) -> u8 {
        match self {
            Self::Input => 0,
            Self::Gpu => 1,
            Self::Output => 2,
            Self::Plugin => 3,
            Self::Compute => 4,
        }
    }
}

/// The watch on a request's own thread: busy from its start to its answer, beating where a loop
/// turns or a bounded wait returns, idle while it waits on what is not its own work.
pub struct RequestWatch {
    heartbeat: Arc<WorkerHeartbeat>,
    pub id: String,
    /// The request's termination sequence, `None` on the command line.
    pub sequence: Option<u64>,
    pub export: bool,
    side: std::sync::atomic::AtomicU8,
    step: Mutex<&'static str>,
    phase: std::sync::atomic::AtomicU8,
    answered: AtomicBool,
}

impl RequestWatch {
    pub fn side(&self) -> RequestSide {
        RequestSide::ALL[usize::from(self.side.load(Ordering::Acquire)).min(4)]
    }

    pub fn step(&self) -> &'static str {
        *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn phase(&self) -> crate::failure::ExportPhase {
        crate::failure::ExportPhase::ALL[usize::from(self.phase.load(Ordering::Acquire)).min(7)]
    }

    /// True for the one caller that may answer the request: its thread or the supervisor.
    pub fn claim_answer(&self) -> bool {
        !self.answered.swap(true, Ordering::AcqRel)
    }

    fn set(&self, side: RequestSide, step: &'static str) {
        self.side.store(side.index(), Ordering::Release);
        *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = step;
    }
}

static REQUEST_WATCHES: Mutex<Vec<Arc<RequestWatch>>> = Mutex::new(Vec::new());

thread_local! {
    static CURRENT_REQUEST: std::cell::RefCell<Option<Arc<RequestWatch>>> =
        const { std::cell::RefCell::new(None) };
}

fn request_watches() -> std::sync::MutexGuard<'static, Vec<Arc<RequestWatch>>> {
    REQUEST_WATCHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Watches the calling thread as request `id` until the guard drops: it is at work from now on.
pub fn watch_request(
    id: &str,
    sequence: Option<u64>,
    export: bool,
    side: RequestSide,
    step: &'static str,
) -> RequestWatchGuard {
    let watch = Arc::new(RequestWatch {
        heartbeat: Arc::new(WorkerHeartbeat::default()),
        id: id.to_owned(),
        sequence,
        export,
        side: std::sync::atomic::AtomicU8::new(side.index()),
        step: Mutex::new(step),
        phase: std::sync::atomic::AtomicU8::new(0),
        answered: AtomicBool::new(false),
    });
    let work = watch.heartbeat.begin_work();
    request_watches().push(Arc::clone(&watch));
    CURRENT_REQUEST.with(|current| current.replace(Some(Arc::clone(&watch))));
    let core = alpha2_host_core::request_watch::watch_request(beat_request);
    let bands = {
        let heartbeat = Arc::clone(&watch.heartbeat);
        alpha2_render::report_band_progress(Arc::new(move || heartbeat.progressed()))
    };
    RequestWatchGuard {
        watch,
        _work: work,
        _core: core,
        _bands: bands,
    }
}

pub struct RequestWatchGuard {
    watch: Arc<RequestWatch>,
    _work: HeartbeatWork,
    _core: alpha2_host_core::request_watch::RequestWatchGuard,
    _bands: alpha2_render::BandProgress,
}

impl std::ops::Deref for RequestWatchGuard {
    type Target = RequestWatch;

    fn deref(&self) -> &RequestWatch {
        &self.watch
    }
}

impl Drop for RequestWatchGuard {
    fn drop(&mut self) {
        request_watches().retain(|watch| !Arc::ptr_eq(watch, &self.watch));
        CURRENT_REQUEST.with(|current| {
            let mine = current
                .borrow()
                .as_ref()
                .is_some_and(|watch| Arc::ptr_eq(watch, &self.watch));
            if mine {
                current.replace(None);
            }
        });
    }
}

/// A step of the request this thread handles goes on.
pub fn beat_request() {
    CURRENT_REQUEST.with(|current| {
        if let Some(watch) = current.borrow().as_ref() {
            watch.heartbeat.beat();
        }
    });
}

/// A new step of the request this thread handles starts, on `side`.
pub fn request_step(side: RequestSide, step: &'static str) {
    CURRENT_REQUEST.with(|current| {
        if let Some(watch) = current.borrow().as_ref() {
            watch.set(side, step);
            watch.heartbeat.beat();
        }
    });
    #[cfg(test)]
    if *INJECT_REQUEST_STALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        == Some(step)
    {
        loop {
            thread::park();
        }
    }
}

/// Parks a request's thread for good at the step of this name, as a read that never returns.
#[cfg(test)]
pub static INJECT_REQUEST_STALL: Mutex<Option<&'static str>> = Mutex::new(None);

/// An export of this thread's request enters `phase` ([`crate::failure::FailureContext`]).
pub fn request_phase(phase: crate::failure::ExportPhase) {
    use crate::failure::ExportPhase;
    let side = match phase {
        ExportPhase::Prepare | ExportPhase::Decode => RequestSide::Input,
        ExportPhase::Upload | ExportPhase::Render | ExportPhase::Readback => RequestSide::Gpu,
        ExportPhase::Encode | ExportPhase::Mux | ExportPhase::Cleanup => RequestSide::Output,
    };
    CURRENT_REQUEST.with(|current| {
        if let Some(watch) = current.borrow().as_ref() {
            let index = ExportPhase::ALL.iter().position(|known| *known == phase);
            watch.phase.store(
                u8::try_from(index.unwrap_or(0)).unwrap_or(0),
                Ordering::Release,
            );
            watch.set(side, phase.as_str());
            watch.heartbeat.beat();
        }
    });
}

/// The request this thread handles waits on what is not its own work until the guard drops.
pub fn request_idle() -> Option<HeartbeatIdle> {
    CURRENT_REQUEST.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|watch| watch.heartbeat.idle())
    })
}

/// How long a request may stay in one step: three stall limits, since CPU-bound steps on the
/// request thread have no measured ceiling on slow machines.
pub fn request_stall_limit() -> Duration {
    #[cfg(test)]
    if let Some(limit) = *TEST_REQUEST_STALL_LIMIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return limit;
    }
    crate::config::configured_stall_timeout().saturating_mul(3)
}

/// A short request stall limit for the serve children of the tests.
#[cfg(test)]
pub static TEST_REQUEST_STALL_LIMIT: Mutex<Option<Duration>> = Mutex::new(None);

static REQUEST_THREAD_LEFT: AtomicBool = AtomicBool::new(false);

/// Marks the process as holding a request thread that will not come back, nor free its slot.
pub fn leave_request_thread() {
    REQUEST_THREAD_LEFT.store(true, Ordering::Release);
    THREADS_ABANDONED.store(true, Ordering::Release);
}

pub fn request_thread_left() -> bool {
    REQUEST_THREAD_LEFT.load(Ordering::Acquire)
}

/// Once per process: a thread that looks every [`STALL_POLL`] for a request in one step past
/// [`request_stall_limit`] and hands the first caller of its answer to `stalled`.
pub fn supervise_requests(stalled: fn(&RequestWatch, Duration)) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = thread::Builder::new()
        .name("alpha2-watch".to_owned())
        .spawn(move || {
            loop {
                thread::sleep(STALL_POLL);
                let limit = request_stall_limit();
                let watches = request_watches().clone();
                for watch in watches {
                    if let Some(stalled_for) = watch.heartbeat.stuck_past(limit)
                        && watch.claim_answer()
                    {
                        stalled(&watch, stalled_for);
                    }
                }
            }
        });
    if let Err(error) = spawned {
        STARTED.store(false, Ordering::Release);
        alpha2_os::diagnostic!("alpha2-host: a request that never returns goes unnoticed: {error}");
    }
}
