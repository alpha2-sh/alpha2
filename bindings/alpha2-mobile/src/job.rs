//! A run on a library thread and the outcome its handle awaits, settled exactly once.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use alpha2_media::CancelToken;

use crate::error::Alpha2Error;

struct State<T> {
    outcome: Option<Result<T, Alpha2Error>>,
    taken: bool,
    wakers: Vec<Waker>,
}

/// The cancel token and the outcome of one export or extraction.
pub(crate) struct Job<T> {
    /// What the job is, as error text names it (`export`, `extraction`).
    what: &'static str,
    cancel: CancelToken,
    state: Mutex<State<T>>,
}

impl<T: Send + 'static> Job<T> {
    pub(crate) fn new(what: &'static str) -> Arc<Self> {
        Arc::new(Self {
            what,
            cancel: CancelToken::default(),
            state: Mutex::new(State {
                outcome: None,
                taken: false,
                wakers: Vec::new(),
            }),
        })
    }

    /// The token the job's work checks.
    pub(crate) const fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Runs `body` on a thread named `name` and settles the job with its result. A panic in
    /// `body` settles it as `INTERNAL` with the panic's message, after `body`'s values dropped.
    ///
    /// # Errors
    /// `RESOURCE_LIMIT` when the thread does not start.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        name: &str,
        body: impl FnOnce() -> Result<T, Alpha2Error> + Send + 'static,
    ) -> Result<(), Alpha2Error> {
        let job = Arc::clone(self);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
                    .unwrap_or_else(|payload| {
                        Err(Alpha2Error::Failed {
                            details: None,
                            code: "INTERNAL".into(),
                            reason: format!(
                                "the {} thread panicked: {}",
                                job.what,
                                alpha2_host_core::panic_text(&*payload)
                            ),
                        })
                    });
                job.finish(outcome);
            })
            .map(drop)
            .map_err(|error| Alpha2Error::Failed {
                details: None,
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the {} thread: {error}", self.what),
            })
    }

    /// Stores `outcome` unless one was stored already, then wakes the waiting futures with the
    /// lock released.
    pub(crate) fn finish(&self, outcome: Result<T, Alpha2Error>) {
        let wakers = {
            let mut state = self.lock();
            if state.outcome.is_some() || state.taken {
                return;
            }
            state.outcome = Some(outcome);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Whether the job has settled, its outcome taken or not.
    pub(crate) fn is_finished(&self) -> bool {
        let state = self.lock();
        state.outcome.is_some() || state.taken
    }

    /// Resolves with the outcome once the job settles; every later wait gets `INVALID_INPUT`.
    pub(crate) const fn outcome(&self) -> Outcome<'_, T> {
        Outcome { job: self }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The future [`Job::outcome`] returns.
pub(crate) struct Outcome<'a, T> {
    job: &'a Job<T>,
}

impl<T: Send + 'static> Future for Outcome<'_, T> {
    type Output = Result<T, Alpha2Error>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.job.lock();
        if let Some(outcome) = state.outcome.take() {
            state.taken = true;
            return Poll::Ready(outcome);
        }
        if state.taken {
            return Poll::Ready(Err(Alpha2Error::invalid(format!(
                "this {}'s outcome was already taken",
                self.job.what
            ))));
        }
        if !state
            .wakers
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            state.wakers.push(context.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    use std::time::Duration;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll<T: Send + 'static>(job: &Job<T>, waker: &Waker) -> Poll<Result<T, Alpha2Error>> {
        let mut future = job.outcome();
        Pin::new(&mut future).poll(&mut Context::from_waker(waker))
    }

    /// Runs `body` on a thread and fails when it has not returned within `seconds`.
    fn within<R: Send + 'static>(seconds: u64, body: impl FnOnce() -> R + Send + 'static) -> R {
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(body());
        });
        wait.recv_timeout(Duration::from_secs(seconds))
            .expect("returned in time")
    }

    #[test]
    fn a_panicking_body_settles_the_job_as_internal_with_its_message() {
        let outcome = within(30, || {
            let job = Job::<u32>::new("export");
            job.spawn("alpha2-test", || panic!("the body broke"))
                .expect("spawned");
            pollster::block_on(job.outcome())
        });
        match outcome {
            Err(Alpha2Error::Failed { code, reason, .. }) => {
                assert_eq!(code, "INTERNAL");
                assert!(reason.contains("the body broke"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_body_that_returns_settles_the_job_with_its_result() {
        let job = Job::<u32>::new("export");
        job.spawn("alpha2-test", || Ok(7)).expect("spawned");
        assert_eq!(pollster::block_on(job.outcome()).ok(), Some(7));
        assert!(job.is_finished());
    }

    /// Polls its job again from `wake`, as an executor that runs the task inline does.
    struct PollsAgain(Arc<Job<u32>>, AtomicUsize);

    impl Wake for PollsAgain {
        fn wake(self: Arc<Self>) {
            let ready = poll(&self.0, Waker::noop()).is_ready();
            self.1.store(usize::from(ready) + 1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_waker_that_polls_again_does_not_deadlock() {
        let woken = within(10, || {
            let job = Job::<u32>::new("export");
            let waker = Arc::new(PollsAgain(Arc::clone(&job), AtomicUsize::new(0)));
            assert!(poll(&job, &Waker::from(Arc::clone(&waker))).is_pending());
            job.finish(Ok(1));
            waker.1.load(Ordering::SeqCst)
        });
        assert_eq!(woken, 2, "the poll from inside wake saw the outcome");
    }

    #[test]
    fn of_two_waiting_futures_one_gets_the_outcome_and_the_other_already_taken() {
        let job = Job::<u32>::new("extraction");
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut first = job.outcome();
        let mut second = job.outcome();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut first).poll(&mut context).is_pending());
        assert!(Pin::new(&mut second).poll(&mut context).is_pending());
        job.finish(Ok(3));
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "one waker for one task");
        let Poll::Ready(got) = Pin::new(&mut first).poll(&mut context) else {
            panic!("pending after finish");
        };
        assert_eq!(got.ok(), Some(3));
        let Poll::Ready(again) = Pin::new(&mut second).poll(&mut context) else {
            panic!("pending after the outcome was taken");
        };
        match again {
            Err(Alpha2Error::InvalidInput { problems }) => {
                assert_eq!(problems, ["this extraction's outcome was already taken"]);
            }
            other => panic!("{other:?}"),
        }
        job.finish(Ok(4));
        assert!(job.is_finished());
    }

    #[test]
    fn finishing_one_job_does_not_wake_another_jobs_waker() {
        let finished = Job::<u32>::new("export");
        let waiting = Job::<u32>::new("export");
        let count = Arc::new(Count::default());
        assert!(poll(&waiting, &Waker::from(Arc::clone(&count))).is_pending());
        finished.finish(Ok(1));
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        waiting.finish(Ok(2));
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
    }
}
