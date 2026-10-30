//! One device acquisition at a time in a process, for test harnesses.
//!
//! A test binary runs its tests on parallel threads, and each may make its
//! own instance, enumerate adapters and open a device at the same moment.
//! Vulkan loaders and drivers do not all survive that: the 1.4.321 loader can
//! crash naming a new device's objects while another thread enumerates
//! physical devices, and the NVIDIA EGL driver can abort enumerating GL
//! adapters beside another enumeration. A host makes its device once, on one
//! thread, so only tests meet this; the `serial-acquisition` feature, which
//! the workspace's test builds turn on as a dev-dependency, makes those
//! acquisitions take turns. Without the feature the guard is free.
//!
//! Turns cover making instances and devices, not dropping them or the exit
//! of a thread that used them, and those race too. When a process's last
//! instance goes, the loader unloads the drivers, and a driver can leave
//! per-thread data whose destructor lives in what was unloaded. The NVIDIA
//! 580 Vulkan driver's EGL library does, so a test thread that exits as
//! another test drops the last instance (or makes the next one, which loads
//! and unloads every driver to list extensions) dies with SIGSEGV. The
//! loader's own dispatch has crashed the same way while another thread
//! unloaded a layer. So the first turn of a process also
//! makes an instance the process keeps until it exits, as a host keeps its
//! one: every driver and layer the tests' instances load stays loaded, and
//! their unloads become reference counts. That instance is on the first
//! product backend group (DX12, Metal or Vulkan); the instance builder keeps
//! one more for each backend set no kept instance covers, so the GL/EGL
//! libraries load only in a process that makes a GL instance. A driver that
//! offers no adapter is still loaded and unloaded around each instance; it
//! holds nothing a thread keeps.

/// While alive, keeps every other thread of the process out of the
/// acquisitions that take [`serialize_acquisition`]: instance creation,
/// adapter requests and enumeration, and device creation. The thread holding
/// one may take another (an acquisition inside another's callback).
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
/// Callers that make a wgpu instance, a Vulkan instance or a device outside
/// [`crate::GpuContext`]'s constructors (which take it themselves) hold it
/// for as long as that takes. Without the `serial-acquisition` feature it
/// returns at once.
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

    /// The thread holding the turn and how many guards it holds.
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
            // Made inside the turn, as any instance is, and before the caller makes its own.
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
