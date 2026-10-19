//! A one-shot completion signal that a wgpu callback sets and an async
//! export awaits.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct State {
    done: bool,
    wakers: Vec<Waker>,
}

/// Send-safe bridge from wgpu's completion callback to a browser promise.
///
/// Clones share one state: [`Completion::finish`] on any clone completes every
/// clone, and a finish that runs before the first poll is not lost.
#[derive(Clone, Default)]
pub(crate) struct Completion(Arc<Mutex<State>>);

impl Completion {
    /// Marks the work done and wakes every task waiting on it.
    pub(crate) fn finish(&self) {
        let wakers = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.done = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn is_done(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .done
    }
}

impl Future for Completion {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.done {
            Poll::Ready(())
        } else {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Wake;

    struct Signal(AtomicBool);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn completion_handles_callbacks_before_and_after_poll() {
        for finish_first in [true, false] {
            let mut completion = Completion::default();
            let signal = Arc::new(Signal(AtomicBool::new(false)));
            let waker = Waker::from(signal.clone());
            let mut context = Context::from_waker(&waker);
            if finish_first {
                completion.finish();
            } else {
                assert_eq!(Pin::new(&mut completion).poll(&mut context), Poll::Pending);
                completion.finish();
                assert!(signal.0.load(Ordering::SeqCst));
            }
            assert_eq!(
                Pin::new(&mut completion).poll(&mut context),
                Poll::Ready(())
            );
        }
    }
}
