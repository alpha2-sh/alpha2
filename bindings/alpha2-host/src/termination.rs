//! How a running request is stopped: by the client's `cancel` line, or by a
//! request to terminate the process.
//!
//! A `cancel` line naming the request being handled cancels it: the export
//! stops at its next wait, its ordinary abort path removes the partial
//! output, the request is answered `CANCELLED`, and the host serves the next
//! request. A cancel naming a request not read or started yet is kept, and
//! that request is answered `CANCELLED` without running.
//!
//! A request to terminate the process (`SIGTERM`, `SIGINT`, `SIGHUP`, a
//! console control event, or the exit of the process that started `serve`)
//! cancels the running export the same way instead of ending the process where
//! it stands, and the process then exits with the status the signal would
//! have given it. An idle process exits at once. Ended where it stands, the
//! process runs no destructor: the partial output stays on disk (taken over by
//! the next export to that path once its lock is gone,
//! `alpha2_native::PARTIAL_OUTPUT_IN_USE`), decoder surfaces and encoder
//! sessions are never released, and the caller gets no answer. A second
//! signal of the same kind still ends the process immediately, as does
//! `SIGKILL`.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use alpha2_media::CancelToken;
use alpha2_os::Termination;

/// What the process has been asked, and what a request would have to stop.
pub(crate) struct Terminations {
    state: Mutex<State>,
    /// Called when a termination was asked for and the last request it found
    /// being handled has been answered: how the process exits.
    on_idle: fn(Termination),
}

struct State {
    requested: Option<Termination>,
    /// Requests being handled, from their line being read until answered.
    busy: usize,
    /// The cancel tokens of the exports running now.
    running: Vec<(u64, CancelToken)>,
    next: u64,
    /// The id of the request being handled, which a `cancel` line naming it
    /// stops.
    current: Option<String>,
    /// Whether the client cancelled `current`.
    cancelled_current: bool,
    /// Ids of requests cancelled before they were read or started, oldest
    /// first. A cancel for a request that already ended stays until newer ones
    /// push it out ([`MAX_PENDING_CANCELS`]).
    pending: VecDeque<String>,
}

/// The most cancels of requests not started yet that are kept.
const MAX_PENDING_CANCELS: usize = 1024;

impl Terminations {
    pub(crate) const fn new(on_idle: fn(Termination)) -> Self {
        Self {
            state: Mutex::new(State {
                requested: None,
                busy: 0,
                running: Vec::new(),
                next: 0,
                current: None,
                cancelled_current: false,
                pending: VecDeque::new(),
            }),
            on_idle,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `termination` and cancels every running export. Returns whether
    /// the process is idle, with nothing to answer, and can exit now.
    pub(crate) fn request(&self, termination: Termination) -> bool {
        let mut state = self.lock();
        state.requested.get_or_insert(termination);
        for (_, token) in &state.running {
            token.cancel();
        }
        state.busy == 0
    }

    /// The first termination the process was asked for, if any.
    pub(crate) fn requested(&self) -> Option<Termination> {
        self.lock().requested
    }

    /// Marks the request `id` as being handled until the guard is dropped:
    /// from the line being read until it is answered. A cancel that named it
    /// before this is taken now, and the guard says so
    /// ([`RequestGuard::cancelled_before_start`]); one that arrives later
    /// stops it.
    pub(crate) fn begin_request(&'static self, id: &str) -> RequestGuard {
        let mut state = self.lock();
        state.busy += 1;
        let cancelled = take_pending(&mut state.pending, id);
        state.current = Some(id.to_owned());
        state.cancelled_current = cancelled;
        RequestGuard {
            owner: self,
            id: id.to_owned(),
            cancelled_before_start: cancelled,
        }
    }

    /// Whether a cancel named `id`, a request not started yet; the cancel is
    /// taken.
    pub(crate) fn take_pending_cancel(&self, id: &str) -> bool {
        take_pending(&mut self.lock().pending, id)
    }

    /// What a `cancel` line naming `target` does: `running` when `target` is
    /// the request being handled, whose exports are cancelled now (a request
    /// with nothing to cancel, `capabilities` or `conform`, finishes);
    /// otherwise `pending`, kept for a request with that id read or started
    /// later.
    pub(crate) fn cancel_request(&self, target: &str) -> &'static str {
        let mut state = self.lock();
        if state.current.as_deref() == Some(target) {
            state.cancelled_current = true;
            for (_, token) in &state.running {
                token.cancel();
            }
            return "running";
        }
        if state.pending.len() >= MAX_PENDING_CANCELS {
            state.pending.pop_front();
        }
        state.pending.push_back(target.to_owned());
        "pending"
    }

    /// Marks the process as busy for the rest of its life: one that answers
    /// a single request and then exits through `main`, which reports it.
    pub(crate) fn hold_busy(&self) {
        self.lock().busy += 1;
    }

    /// A cancel token for an export, which a termination cancels: at once
    /// when one was already asked for.
    pub(crate) fn export_cancel(&'static self) -> ExportCancel {
        let token = CancelToken::default();
        let mut state = self.lock();
        if state.requested.is_some() || state.cancelled_current {
            token.cancel();
        }
        let id = state.next;
        state.next += 1;
        state.running.push((id, token.clone()));
        ExportCancel {
            token,
            id,
            owner: self,
        }
    }

    /// Waits until no request is being handled, for at most `limit`.
    fn wait_idle(&self, limit: Duration) {
        let started = Instant::now();
        while self.lock().busy != 0 && started.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Removes `id` from `pending`; whether it was there.
fn take_pending(pending: &mut VecDeque<String>, id: &str) -> bool {
    match pending.iter().position(|cancelled| cancelled == id) {
        Some(at) => {
            pending.remove(at);
            true
        }
        None => false,
    }
}

/// A request being handled; see [`Terminations::begin_request`].
pub(crate) struct RequestGuard {
    owner: &'static Terminations,
    id: String,
    cancelled_before_start: bool,
}

impl RequestGuard {
    /// Whether a cancel named the request before it started: it is answered
    /// `CANCELLED` without running.
    pub(crate) const fn cancelled_before_start(&self) -> bool {
        self.cancelled_before_start
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut state = self.owner.lock();
        state.busy = state.busy.saturating_sub(1);
        if state.current.as_deref() == Some(self.id.as_str()) {
            state.current = None;
            state.cancelled_current = false;
        }
        let idle = (state.busy == 0).then_some(state.requested).flatten();
        drop(state);
        if let Some(termination) = idle {
            (self.owner.on_idle)(termination);
        }
    }
}

/// A running export's cancel token, which a termination reaches until the
/// export ends.
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
        self.owner.lock().running.retain(|(id, _)| *id != self.id);
    }
}

static TERMINATIONS: Terminations = Terminations::new(exit_on_termination);

fn exit_on_termination(termination: Termination) {
    crate::exit_unflushed(termination.exit_code());
}

/// The process's terminations.
pub(crate) fn terminations() -> &'static Terminations {
    &TERMINATIONS
}

/// The termination this process was asked for, if any.
pub(crate) fn requested() -> Option<Termination> {
    TERMINATIONS.requested()
}

/// A cancel token for an export, which a termination cancels.
pub(crate) fn export_cancel() -> ExportCancel {
    TERMINATIONS.export_cancel()
}

/// A failed export's `message`, or, once the client cancelled the request or a
/// termination was asked for, the cancel it is: whatever the cancel stopped
/// the export at, a wait or the frame loop, and whatever that reported, the
/// export was cancelled.
pub(crate) fn cancelled_by_request_or_termination(message: String) -> String {
    if crate::protocol::is_cancelled(&message) {
        return message;
    }
    let state = TERMINATIONS.lock();
    if let Some(termination) = state.requested {
        return crate::protocol::cancelled(format!(
            "native export was cancelled: the host was asked to terminate ({})",
            termination.name()
        ));
    }
    match (&state.current, state.cancelled_current) {
        (Some(id), true) => crate::protocol::cancelled(format!(
            "native export was cancelled: the client cancelled request {id}"
        )),
        _ => message,
    }
}

/// Turns termination requests into cancellation, for the rest of the
/// process's life.
///
/// # Errors
/// When the operating system refuses the handler.
pub(crate) fn install() -> std::io::Result<()> {
    alpha2_os::on_termination(|termination| {
        // A client may have closed the pipe already: nothing below may depend on this line being
        // written.
        let _ = writeln!(
            std::io::stderr(),
            "alpha2-host: {} received; cancelling the running request",
            termination.name()
        );
        if TERMINATIONS.request(termination) {
            exit_on_termination(termination);
        }
        // The system ends the process when this returns: give the export the time it has to stop
        // and remove its partial.
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

    fn leaked() -> &'static Terminations {
        Box::leak(Box::new(Terminations::new(record_idle)))
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
        // A fallback path, or a pipelined request, that starts after it is cancelled from the
        // start.
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
        // A fallback path the request starts after the cancel is cancelled from the start.
        assert!(terminations.export_cancel().is_cancelled());
        assert_eq!(terminations.requested(), None, "a cancel is no termination");
        drop(running);
        drop(request);
        // The next request is not cancelled by the last one's cancel.
        let next = terminations.begin_request("next");
        assert!(!next.cancelled_before_start());
        assert!(!terminations.export_cancel().is_cancelled());
        drop(next);
        // Nor is a request that reuses its id after it ended.
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
    fn an_idle_process_can_exit_at_once() {
        let terminations = leaked();
        assert!(terminations.request(sigterm()));
    }
}
