//! How a WebGL export readback waits for its mapping.
//!
//! WebGL signals a fence only between tasks, and wgpu reports the mapping
//! only from a poll, so the wait polls, yields a task, and polls again. A
//! yield can take far longer than asked: a hidden page's timers wake once a
//! second or a minute, and a suspended machine wakes when it wakes. The
//! deadline is therefore never read before a poll: the last thing a wait
//! does before it times out is find, by polling, that the mapping is still
//! not done.

use std::future::Future;

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// The mapping finished.
    Done,
    /// The device was lost first; a lost device never finishes a mapping.
    Lost,
    /// A poll after the deadline still found the mapping unfinished.
    TimedOut,
}

/// What a wait reads and drives: a clock in milliseconds, the mapping, the
/// device, and a yield to the event loop.
pub(crate) trait MappingWait {
    type Yield: Future<Output = Result<(), Self::Error>>;
    type Error;

    fn now_ms(&self) -> f64;
    fn poll(&mut self);
    fn done(&self) -> bool;
    fn lost(&self) -> bool;
    fn yield_task(&mut self) -> Self::Yield;
}

/// Waits up to `timeout_ms` for the mapping, polling before every check.
pub(crate) async fn wait_for_mapping<W: MappingWait>(
    wait: &mut W,
    timeout_ms: f64,
) -> Result<Waited, W::Error> {
    let deadline = wait.now_ms() + timeout_ms;
    loop {
        wait.poll();
        if wait.done() {
            return Ok(Waited::Done);
        }
        if wait.lost() {
            return Ok(Waited::Lost);
        }
        if wait.now_ms() > deadline {
            return Ok(Waited::TimedOut);
        }
        wait.yield_task().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A GPU that finishes `gpu_ms` after the wait starts, and an event loop
    /// whose every yield takes `yield_ms` (a throttled timer, a sleep).
    struct Fake {
        clock: Rc<Cell<f64>>,
        gpu_ms: f64,
        yield_ms: f64,
        mapped: bool,
        lost_at: Option<f64>,
        polls: u32,
    }

    impl Fake {
        fn new(gpu_ms: f64, yield_ms: f64) -> Self {
            Self {
                clock: Rc::new(Cell::new(0.0)),
                gpu_ms,
                yield_ms,
                mapped: false,
                lost_at: None,
                polls: 0,
            }
        }
    }

    impl MappingWait for Fake {
        type Yield = std::future::Ready<Result<(), ()>>;
        type Error = ();

        fn now_ms(&self) -> f64 {
            self.clock.get()
        }

        // As WebGL: the mapping is reported only by a poll that runs after the GPU finished.
        fn poll(&mut self) {
            self.polls += 1;
            if self.clock.get() >= self.gpu_ms {
                self.mapped = true;
            }
        }

        fn done(&self) -> bool {
            self.mapped
        }

        fn lost(&self) -> bool {
            self.lost_at.is_some_and(|at| self.clock.get() >= at)
        }

        fn yield_task(&mut self) -> Self::Yield {
            self.clock.set(self.clock.get() + self.yield_ms);
            std::future::ready(Ok(()))
        }
    }

    // Every yield of the fake is ready, so one poll runs the wait to its end.
    fn run(wait: &mut Fake, timeout_ms: f64) -> Waited {
        let mut future = std::pin::pin!(wait_for_mapping(wait, timeout_ms));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(waited) => waited.expect("the fake never fails"),
            std::task::Poll::Pending => panic!("the fake's yields are ready"),
        }
    }

    #[test]
    fn a_yield_longer_than_the_deadline_still_finds_a_finished_mapping() {
        // A hidden page's timer wakes a minute later, while the GPU took 5 ms.
        let mut wait = Fake::new(5.0, 60_000.0);
        assert_eq!(run(&mut wait, 30_000.0), Waited::Done);
    }

    #[test]
    fn a_mapping_that_never_finishes_times_out_after_a_poll_past_the_deadline() {
        let mut wait = Fake::new(f64::INFINITY, 4.0);
        assert_eq!(run(&mut wait, 30.0), Waited::TimedOut);
        // Polls at 0, 4, ..., 32 ms: the last one is past the deadline.
        assert_eq!(wait.polls, 9);
    }

    #[test]
    fn a_short_wait_polls_until_the_gpu_finishes() {
        let mut wait = Fake::new(10.0, 4.0);
        assert_eq!(run(&mut wait, 30_000.0), Waited::Done);
        assert_eq!(wait.polls, 4);
    }

    #[test]
    fn a_lost_device_ends_the_wait() {
        let mut wait = Fake::new(f64::INFINITY, 4.0);
        wait.lost_at = Some(8.0);
        assert_eq!(run(&mut wait, 30_000.0), Waited::Lost);
    }
}
