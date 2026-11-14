//! Stopping a running request, by a `cancel` line or a request to terminate the process.
//!
//! A cancel stops the export at its next wait; the normal abort path removes the partial output,
//! the request is answered `CANCELLED`, and the host serves the next. A cancel for a request not
//! yet read or started is kept until that request arrives.
//!
//! `SIGTERM`, `SIGINT`, `SIGHUP`, a console control event, or the exit of the process that started
//! `serve` cancels the running export the same way, and the process then exits with the signal's
//! status. Ending in place would run no destructors: the partial output would stay on disk,
//! decoder surfaces and encoder sessions would leak, and the caller would get no answer. A second
//! signal of the same kind, or `SIGKILL`, still ends the process immediately.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use alpha2_media::CancelToken;
use alpha2_os::Termination;

pub(crate) struct Terminations {
    state: Mutex<State>,
    /// Called when a termination was requested and the last request being handled is answered.
    on_idle: fn(Termination),
    /// Called when a termination's grace runs out, whatever still runs.
    on_deadline: fn(Termination),
    deadline_started: std::sync::atomic::AtomicBool,
}

struct State {
    requested: Option<Termination>,
    /// Requests from their frame being read until answered.
    busy: usize,
    /// Export cancel tokens and the request each belongs to, if known.
    running: Vec<(u64, Option<u64>, CancelToken)>,
    next: u64,
    /// Requests being handled: their sequence number, id and whether a cancel named them.
    active: Vec<(u64, String, bool)>,
    /// Ids cancelled before they were read or started, oldest first. A cancel for a request that
    /// already ended stays until newer ones push it out.
    pending: VecDeque<String>,
}

const MAX_PENDING_CANCELS: usize = 1024;

thread_local! {
    /// The request this thread handles, set by [`Terminations::begin_request`].
    static CURRENT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

impl Terminations {
    pub(crate) const fn new(on_idle: fn(Termination), on_deadline: fn(Termination)) -> Self {
        Self {
            state: Mutex::new(State {
                requested: None,
                busy: 0,
                running: Vec::new(),
                next: 0,
                active: Vec::new(),
                pending: VecDeque::new(),
            }),
            on_idle,
            on_deadline,
            deadline_started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Once per process: calls `on_deadline` after `grace` on a thread of its own, so a request
    /// that cannot stop, or an output nobody reads, cannot keep the process alive.
    pub(crate) fn start_deadline(&'static self, termination: Termination, grace: Duration) {
        if self
            .deadline_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("alpha2-termination-deadline".to_owned())
            .spawn(move || {
                std::thread::sleep(grace);
                (self.on_deadline)(termination);
            });
        if spawned.is_err() {
            self.deadline_started
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cancels every running export. Returns whether the process is idle and can exit now.
    pub(crate) fn request(&self, termination: Termination) -> bool {
        let mut state = self.lock();
        state.requested.get_or_insert(termination);
        for (_, _, token) in &state.running {
            token.cancel();
        }
        state.busy == 0
    }

    pub(crate) fn requested(&self) -> Option<Termination> {
        self.lock().requested
    }

    /// Held until the guard drops, on the thread that handles the request. A cancel that named
    /// `id` earlier is taken now ([`RequestGuard::cancelled_before_start`]).
    pub(crate) fn begin_request(&'static self, id: &str) -> RequestGuard {
        let mut state = self.lock();
        state.busy += 1;
        let cancelled = take_pending(&mut state.pending, id);
        let sequence = state.next;
        state.next += 1;
        state.active.push((sequence, id.to_owned(), cancelled));
        drop(state);
        CURRENT.with(|current| current.set(Some(sequence)));
        RequestGuard {
            owner: self,
            sequence,
            cancelled_before_start: cancelled,
        }
    }

    pub(crate) fn take_pending_cancel(&self, id: &str) -> bool {
        take_pending(&mut self.lock().pending, id)
    }

    /// `running` when a request with id `target` is being handled (a request with nothing to
    /// cancel finishes); otherwise `pending`, kept for a later request with that id.
    pub(crate) fn cancel_request(&self, target: &str) -> &'static str {
        let mut state = self.lock();
        let State {
            active, running, ..
        } = &mut *state;
        let mut found = false;
        for (sequence, id, cancelled) in active.iter_mut() {
            if id == target {
                *cancelled = true;
                found = true;
                cancel_tokens_of(running, Some(*sequence));
            }
        }
        if found {
            return "running";
        }
        if state.pending.len() >= MAX_PENDING_CANCELS {
            state.pending.pop_front();
        }
        state.pending.push_back(target.to_owned());
        "pending"
    }

    /// For a cause the caller already recorded as the failure (the Linux memory watchdog), so the
    /// export reports that cause, not a cancel. Cancels every export when `request` is unknown.
    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    pub(crate) fn cancel_running_request(&self, request: Option<u64>) {
        let state = self.lock();
        match request {
            Some(_) => cancel_tokens_of(&state.running, request),
            None => {
                for (_, _, token) in &state.running {
                    token.cancel();
                }
            }
        }
    }

    /// For a process that answers a single request and then exits through `main`.
    pub(crate) fn hold_busy(&self) {
        self.lock().busy += 1;
    }

    /// Belongs to the request this thread handles. Already cancelled when a termination was asked
    /// for or a cancel named the request.
    pub(crate) fn export_cancel(&'static self) -> ExportCancel {
        let token = CancelToken::default();
        let request = current_request();
        let mut state = self.lock();
        let cancelled = request.is_some_and(|request| {
            state
                .active
                .iter()
                .any(|(sequence, _, cancelled)| *sequence == request && *cancelled)
        });
        if state.requested.is_some() || cancelled {
            token.cancel();
        }
        let id = state.next;
        state.next += 1;
        state.running.push((id, request, token.clone()));
        ExportCancel {
            token,
            id,
            owner: self,
        }
    }

    fn wait_idle(&self, limit: Duration) {
        let started = Instant::now();
        while self.lock().busy != 0 && started.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Tokens of no known request are cancelled with any request, as they may be its.
fn cancel_tokens_of(running: &[(u64, Option<u64>, CancelToken)], request: Option<u64>) {
    for (_, owner, token) in running {
        if owner.is_none() || *owner == request {
            token.cancel();
        }
    }
}

/// The request the calling thread handles, if it handles one.
pub(crate) fn current_request() -> Option<u64> {
    CURRENT.with(std::cell::Cell::get)
}

fn take_pending(pending: &mut VecDeque<String>, id: &str) -> bool {
    match pending.iter().position(|cancelled| cancelled == id) {
        Some(at) => {
            pending.remove(at);
            true
        }
        None => false,
    }
}

pub(crate) struct RequestGuard {
    owner: &'static Terminations,
    sequence: u64,
    cancelled_before_start: bool,
}

impl RequestGuard {
    pub(crate) const fn cancelled_before_start(&self) -> bool {
        self.cancelled_before_start
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            if current.get() == Some(self.sequence) {
                current.set(None);
            }
        });
        let mut state = self.owner.lock();
        state.busy = state.busy.saturating_sub(1);
        state
            .active
            .retain(|(sequence, _, _)| *sequence != self.sequence);
        let idle = (state.busy == 0).then_some(state.requested).flatten();
        drop(state);
        if let Some(termination) = idle {
            (self.owner.on_idle)(termination);
        }
    }
}

pub(crate) struct ExportCancel {
    token: CancelToken,
    id: u64,
    owner: &'static Terminations,
}

impl std::ops::Deref for ExportCancel {
    type Target = CancelToken;

    fn deref(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for ExportCancel {
    fn drop(&mut self) {
        self.owner
            .lock()
            .running
            .retain(|(id, _, _)| *id != self.id);
    }
}

static TERMINATIONS: Terminations = Terminations::new(exit_on_termination, exit_at_deadline);

fn exit_on_termination(termination: Termination) {
    crate::lifecycle::announce_termination();
    crate::exit_unflushed(termination.exit_code());
}

/// Writes and flushes nothing: a blocked writer may hold either stream.
fn exit_at_deadline(termination: Termination) {
    alpha2_os::exit_without_handlers(termination.exit_code());
}

/// How long a termination waits for running requests: past the stall limit the host itself calls a
/// step stuck. After the parent's exit nobody reads the answers, so only a healthy stop is awaited.
fn grace(termination: Termination) -> Duration {
    #[cfg(test)]
    if let Some(grace) = *TEST_GRACE.lock().unwrap_or_else(PoisonError::into_inner) {
        return grace;
    }
    if termination.parent_exited() {
        PARENT_EXIT_GRACE
    } else {
        crate::config::configured_stall_timeout().saturating_add(SIGNAL_GRACE_MARGIN)
    }
}

const PARENT_EXIT_GRACE: Duration = Duration::from_secs(5);
const SIGNAL_GRACE_MARGIN: Duration = Duration::from_secs(5);

/// A short grace for the serve children of the tests.
#[cfg(test)]
pub(crate) static TEST_GRACE: Mutex<Option<Duration>> = Mutex::new(None);

pub(crate) fn terminations() -> &'static Terminations {
    &TERMINATIONS
}

pub(crate) fn requested() -> Option<Termination> {
    TERMINATIONS.requested()
}

pub(crate) fn export_cancel() -> ExportCancel {
    TERMINATIONS.export_cancel()
}

/// Once the request was cancelled or a termination asked for, the failure is reported as the
/// cancel, whatever the export stopped at.
pub(crate) fn cancelled_by_request_or_termination(message: String) -> String {
    if crate::protocol::is_cancelled(&message) {
        return message;
    }
    let request = current_request();
    let state = TERMINATIONS.lock();
    if let Some(termination) = state.requested {
        return crate::protocol::cancelled(format!(
            "native export was cancelled: the host was asked to terminate ({})",
            termination.name()
        ));
    }
    match state
        .active
        .iter()
        .find(|(sequence, _, cancelled)| Some(*sequence) == request && *cancelled)
    {
        Some((_, id, _)) => crate::protocol::cancelled(format!(
            "native export was cancelled: the client cancelled request {id}"
        )),
        None => message,
    }
}

/// # Errors
/// When the operating system refuses the handler.
pub(crate) fn install() -> std::io::Result<()> {
    alpha2_os::on_termination(|termination| {
        TERMINATIONS.start_deadline(termination, grace(termination));
        // A client may have closed the pipe already, so nothing may depend on this line.
        let _ = writeln!(
            std::io::stderr(),
            "alpha2-host: {} received; cancelling the running request",
            termination.name()
        );
        if TERMINATIONS.request(termination) {
            exit_on_termination(termination);
        }
        // The system ends the process when this returns, so give the export time to stop and
        // remove its partial.
        if termination.closing() {
            TERMINATIONS.wait_idle(Duration::from_secs(4));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static IDLE: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    fn record_idle(termination: Termination) {
        IDLE.lock().expect("idle").push(termination.name());
    }

    static DEADLINES: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    fn record_deadline(termination: Termination) {
        DEADLINES
            .lock()
            .expect("deadlines")
            .push(termination.exit_code());
    }

    fn leaked() -> &'static Terminations {
        Box::leak(Box::new(Terminations::new(record_idle, record_deadline)))
    }

    fn sigterm() -> Termination {
        Termination::new("SIGTERM", 143, false)
    }

    #[test]
    fn a_termination_cancels_every_running_export_and_any_that_starts_after_it() {
        let terminations = leaked();
        let request = terminations.begin_request("export");
        let running = terminations.export_cancel();
        let ended = terminations.export_cancel();
        drop(ended);
        assert!(!running.is_cancelled());
        assert!(
            !terminations.request(sigterm()),
            "a process answering a request is not idle"
        );
        assert!(running.is_cancelled());
        assert_eq!(terminations.requested(), Some(sigterm()));
        assert!(terminations.export_cancel().is_cancelled());
        drop(running);
        assert!(IDLE.lock().expect("idle").is_empty());
        drop(request);
        assert_eq!(*IDLE.lock().expect("idle"), ["SIGTERM"]);
        assert!(terminations.request(sigterm()), "idle once answered");
    }

    #[test]
    fn a_cancel_stops_the_request_it_names_and_only_that_request() {
        let terminations = leaked();
        let request = terminations.begin_request("export");
        let running = terminations.export_cancel();
        assert_eq!(terminations.cancel_request("another"), "pending");
        assert!(!running.is_cancelled());
        assert_eq!(terminations.cancel_request("export"), "running");
        assert!(running.is_cancelled());
        assert!(terminations.export_cancel().is_cancelled());
        assert_eq!(terminations.requested(), None, "a cancel is no termination");
        drop(running);
        drop(request);
        let next = terminations.begin_request("next");
        assert!(!next.cancelled_before_start());
        assert!(!terminations.export_cancel().is_cancelled());
        drop(next);
        assert!(
            !terminations
                .begin_request("export")
                .cancelled_before_start()
        );
    }

    #[test]
    fn a_cancel_for_a_request_not_started_yet_is_kept_for_it() {
        let terminations = leaked();
        assert_eq!(terminations.cancel_request("later"), "pending");
        let later = terminations.begin_request("later");
        assert!(later.cancelled_before_start());
        assert!(terminations.export_cancel().is_cancelled());
        drop(later);
        assert!(!terminations.take_pending_cancel("later"), "taken once");
        for index in 0..=MAX_PENDING_CANCELS {
            terminations.cancel_request(&index.to_string());
        }
        assert!(!terminations.take_pending_cancel("0"), "the oldest goes");
        assert!(terminations.take_pending_cancel("1"));
    }

    #[test]
    fn a_deadline_ends_a_busy_process_once_whatever_runs() {
        // Its own idle hook: the shared one is asserted on by tests that run beside it.
        fn ignore_idle(_: Termination) {}
        let terminations: &'static Terminations =
            Box::leak(Box::new(Terminations::new(ignore_idle, record_deadline)));
        let _request = terminations.begin_request("stuck");
        assert!(!terminations.request(sigterm()));
        terminations.start_deadline(sigterm(), Duration::from_millis(50));
        terminations.start_deadline(sigterm(), Duration::from_millis(50));
        let started = Instant::now();
        while DEADLINES.lock().expect("deadlines").is_empty() {
            assert!(started.elapsed() < Duration::from_secs(10), "no deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*DEADLINES.lock().expect("deadlines"), [143]);
    }

    #[test]
    fn the_parent_exit_takes_the_short_grace() {
        let parent = Termination::new("PARENT_EXITED", 143, false);
        assert!(parent.parent_exited());
        assert_eq!(grace(parent), PARENT_EXIT_GRACE);
        assert!(!sigterm().parent_exited());
        assert_eq!(
            grace(sigterm()),
            crate::config::configured_stall_timeout() + SIGNAL_GRACE_MARGIN
        );
    }

    #[test]
    fn an_idle_process_can_exit_at_once() {
        let terminations = leaked();
        assert!(terminations.request(sigterm()));
    }
}
