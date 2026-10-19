//! A request to terminate the process (`SIGTERM`, `SIGINT`, a console
//! control event) cancels the running export instead of ending the process
//! where it stands.
//!
//! Ended where it stands, the process runs no destructor: the partial output
//! stays on disk (taken over by the next export to that path once its lock is
//! gone, `alpha2_native::PARTIAL_OUTPUT_IN_USE`), decoder surfaces and encoder
//! sessions are never released, and the caller gets no answer. Cancelled, the
//! export stops at its next wait, its ordinary abort path removes the partial,
//! the request is answered `CANCELLED`, and the process then exits with the
//! status the signal would have given it. An idle process exits at once. A
//! second signal of the same kind still ends the process immediately, as does
//! `SIGKILL`; `@alpha2-sh/node` sends that 5 s after `SIGTERM`.

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

#[derive(Default)]
struct State {
    requested: Option<Termination>,
    /// Requests being handled, from their line being read until answered.
    busy: usize,
    /// The cancel tokens of the exports running now.
    running: Vec<(u64, CancelToken)>,
    next: u64,
}

impl Terminations {
    pub(crate) const fn new(on_idle: fn(Termination)) -> Self {
        Self {
            state: Mutex::new(State {
                requested: None,
                busy: 0,
                running: Vec::new(),
                next: 0,
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

    /// Marks a request as being handled until the guard is dropped: from the
    /// line being read until it is answered.
    pub(crate) fn begin_request(&'static self) -> RequestGuard {
        self.lock().busy += 1;
        RequestGuard { owner: self }
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
        if state.requested.is_some() {
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

/// A request being handled; see [`Terminations::begin_request`].
pub(crate) struct RequestGuard {
    owner: &'static Terminations,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut state = self.owner.lock();
        state.busy = state.busy.saturating_sub(1);
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

/// A failed export's `message`, or, once a termination was asked for, the
/// cancel it is: whatever the cancel stopped the export at, a wait or the
/// frame loop, and whatever that reported, the export was cancelled.
pub(crate) fn cancelled_by_termination(message: String) -> String {
    match requested() {
        Some(termination) if !crate::protocol::is_cancelled(&message) => {
            crate::protocol::cancelled(format!(
                "native export was cancelled: the host was asked to terminate ({})",
                termination.name()
            ))
        }
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
        let request = terminations.begin_request();
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
    fn an_idle_process_can_exit_at_once() {
        let terminations = leaked();
        assert!(terminations.request(sigterm()));
    }
}
