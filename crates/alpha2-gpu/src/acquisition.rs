//! One device acquisition at a time in a process, for test harnesses.
//!
//! Parallel test threads that create instances, enumerate adapters and open devices at once can
//! crash some Vulkan loaders and the NVIDIA EGL driver. Hosts acquire once on one thread, so the
//! `serial-acquisition` feature (enabled by test dev-dependencies) serializes acquisitions only
//! in tests. Without the feature the guard is free.
//!
//! Unloading drivers also races: a driver can leave per-thread data whose destructor lives in
//! the unloaded library, so a thread exiting while the last instance drops can crash. The first
//! turn therefore creates an instance on the first product backend that lives until process
//! exit, keeping drivers and layers loaded. The instance builder keeps one more per backend set
//! not yet covered, so GL/EGL libraries load only in processes that use GL.

/// While alive, keeps other threads out of acquisitions guarded by [`serialize_acquisition`].
/// The holding thread may re-enter.
#[must_use = "acquisitions are serialized only while the guard lives"]
pub struct AcquisitionGuard {
    #[cfg(all(feature = "serial-acquisition", not(target_arch = "wasm32")))]
    _held: held::Held,
}

impl std::fmt::Debug for AcquisitionGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AcquisitionGuard")
    }
}

/// Waits until no other thread holds an [`AcquisitionGuard`] and returns one.
///
/// Hold it while creating a wgpu instance, Vulkan instance or device outside
/// [`crate::GpuContext`]'s constructors. Without the `serial-acquisition` feature it returns at
/// once.
pub fn serialize_acquisition() -> AcquisitionGuard {
    AcquisitionGuard {
        #[cfg(all(feature = "serial-acquisition", not(target_arch = "wasm32")))]
        _held: held::Held::take(),
    }
}

#[cfg(all(feature = "serial-acquisition", not(target_arch = "wasm32")))]
mod held {
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::thread::ThreadId;

    static TURN: Mutex<Option<(ThreadId, usize)>> = Mutex::new(None);
    static FREED: Condvar = Condvar::new();

    pub(super) struct Held;

    impl Held {
        pub(super) fn take() -> Self {
            let me = std::thread::current().id();
            let mut turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                match &mut *turn {
                    None => {
                        *turn = Some((me, 1));
                        break;
                    }
                    Some((owner, count)) if *owner == me => {
                        *count += 1;
                        return Self;
                    }
                    Some(_) => turn = FREED.wait(turn).unwrap_or_else(PoisonError::into_inner),
                }
            }
            drop(turn);
            // Keep a process-lifetime instance, created before the caller's own.
            crate::instance::keep(crate::product_backends()[0]);
            Self
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let mut turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((_, count)) = &mut *turn {
                *count -= 1;
                if *count == 0 {
                    *turn = None;
                    FREED.notify_all();
                }
            }
        }
    }
}

#[cfg(all(test, feature = "serial-acquisition", not(target_arch = "wasm32")))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::serialize_acquisition;

    #[test]
    fn one_thread_holds_the_turn_at_a_time_and_may_take_it_again() {
        let inside = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let inside = Arc::clone(&inside);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let _outer = serialize_acquisition();
                        let _inner = serialize_acquisition();
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0);
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no two threads held the turn");
        }
    }
}
