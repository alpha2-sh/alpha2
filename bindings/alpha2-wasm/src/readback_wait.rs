//! How an export readback waits for its mapping.
//!
//! WebGL signals a fence only between tasks, and wgpu reports the mapping
//! only from a poll, so the wait polls, yields a task, and polls again.
//! Browser WebGPU reports the mapping from its own event loop; its wait polls
//! nothing and only looks. A yield can take far longer than asked: a hidden
//! page's timers wake once a second or a minute, and a frozen page or a
//! suspended machine wakes when it wakes. So the wait counts active time: each
//! turn credits the time since the last one, but at most [`MAX_CREDIT_MS`],
//! and the wait times out once the credit passes the timeout. A page frozen
//! for a minute costs one turn's credit, not the deadline. The credit is
//! never read before a poll: the last thing a wait does before it times out
//! is find, by polling, that the mapping is still not done.

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

/// The most active time one turn of a wait is credited with: a little over a
/// hidden page's one-second timer, so throttled timers still count, while a
/// freeze or a sleep, however long, counts as one turn.
pub(crate) const MAX_CREDIT_MS: f64 = 1_100.0;

/// Waits up to `timeout_ms` of active time for the mapping, polling before
/// every check.
pub(crate) async fn wait_for_mapping<W: MappingWait>(
    wait: &mut W,
    timeout_ms: f64,
) -> Result<Waited, W::Error> {
    let mut last = wait.now_ms();
    let mut credited = 0.0;
    loop {
        wait.poll();
        if wait.done() {
            return Ok(Waited::Done);
        }
        if wait.lost() {
            return Ok(Waited::Lost);
        }
        let now = wait.now_ms();
        credited += (now - last).clamp(0.0, MAX_CREDIT_MS);
        last = now;
        if credited > timeout_ms {
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
        /// Yields that take other than `yield_ms`, in order.
        jumps: std::collections::VecDeque<f64>,
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
                jumps: std::collections::VecDeque::new(),
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
            let took = self.jumps.pop_front().unwrap_or(self.yield_ms);
            self.clock.set(self.clock.get() + took);
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
    fn a_frozen_clock_jump_is_credited_at_most_once_per_turn() {
        // Turns of 4 ms, but the third yield lasts 20 s (the page was frozen): the freeze counts
        // 1.1 s, so a 2 s timeout is not reached by it, and the GPU, done at 20.2 s, is found done.
        let mut wait = Fake::new(20_200.0, 4.0);
        wait.jumps.extend([4.0, 4.0, 20_000.0]);
        assert_eq!(run(&mut wait, 2_000.0), Waited::Done);
        // A mapping that never finishes still times out, after 2 s of credited turns.
        let mut never = Fake::new(f64::INFINITY, 4.0);
        never.jumps.extend([4.0, 4.0, 20_000.0]);
        assert_eq!(run(&mut never, 2_000.0), Waited::TimedOut);
        // Polls at 0, 4 and 8 ms and after the freeze (1108 ms credited), then 224 turns of 4 ms
        // to pass 2 s.
        assert_eq!(never.polls, 4 + 224);
    }

    #[test]
    fn steady_turns_time_out_at_the_deadline() {
        // A turn of 500 ms (a throttled timer) is credited in full: 30 s pass in 61 turns.
        let mut wait = Fake::new(f64::INFINITY, 500.0);
        assert_eq!(run(&mut wait, 30_000.0), Waited::TimedOut);
        assert_eq!(wait.polls, 62);
        // A turn past the credit counts 1.1 s, so the same timeout takes 28 turns of 60 s.
        let mut slow = Fake::new(f64::INFINITY, 60_000.0);
        assert_eq!(run(&mut slow, 30_000.0), Waited::TimedOut);
        assert_eq!(slow.polls, 29);
    }

    /// Browser WebGPU: the mapping finishes on the event loop, and a poll does nothing.
    struct EventLoopFake {
        clock: f64,
        done_at: f64,
        polls: u32,
    }

    impl MappingWait for EventLoopFake {
        type Yield = std::future::Ready<Result<(), ()>>;
        type Error = ();
        fn now_ms(&self) -> f64 {
            self.clock
        }
        fn poll(&mut self) {
            self.polls += 1;
        }
        fn done(&self) -> bool {
            self.clock >= self.done_at
        }
        fn lost(&self) -> bool {
            false
        }
        fn yield_task(&mut self) -> Self::Yield {
            self.clock += 250.0;
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn webgpu_wait_times_out_without_polling() {
        let mut never = EventLoopFake {
            clock: 0.0,
            done_at: f64::INFINITY,
            polls: 0,
        };
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let mut future = std::pin::pin!(wait_for_mapping(&mut never, 1_000.0));
        assert_eq!(
            future.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(Waited::TimedOut))
        );
        let mut later = EventLoopFake {
            clock: 0.0,
            done_at: 600.0,
            polls: 0,
        };
        let mut future = std::pin::pin!(wait_for_mapping(&mut later, 1_000.0));
        assert_eq!(
            future.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(Waited::Done))
        );
    }

    #[test]
    fn a_lost_device_ends_the_wait() {
        let mut wait = Fake::new(f64::INFINITY, 4.0);
        wait.lost_at = Some(8.0);
        assert_eq!(run(&mut wait, 30_000.0), Waited::Lost);
    }
}
