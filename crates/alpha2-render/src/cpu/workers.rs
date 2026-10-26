//! The CPU renderer's threads: every pass splits its rows into bands and
//! draws them in parallel, each band written by one thread, so the result
//! does not depend on how many there are.

/// Pixels a band holds, about: enough to amortize scheduling, few enough
/// that a band's rows stay in a core's cache while it draws them.
const BAND_PIXELS: usize = 16_384;

/// The threads one CPU renderer draws with.
pub(crate) struct Workers {
    #[cfg(not(target_arch = "wasm32"))]
    pool: Option<rayon::ThreadPool>,
    threads: usize,
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Workers")
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

impl Workers {
    /// `threads` workers of its own (one draws on the calling thread). A
    /// pool that cannot start draws on the calling thread alone.
    pub(crate) fn new(threads: usize) -> Self {
        let threads = threads.max(1);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let pool = (threads > 1)
                .then(|| {
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(threads)
                        .thread_name(|index| format!("alpha2-cpu-render-{index}"))
                        .build()
                        .ok()
                })
                .flatten();
            let threads = if pool.is_some() { threads } else { 1 };
            Self { pool, threads }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = threads;
            Self { threads: 1 }
        }
    }

    pub(crate) const fn threads(&self) -> usize {
        self.threads
    }

    /// Calls `draw(first_row, rows)` for each band of `pixels`, an image
    /// `width` pixels wide stored row after row, in parallel. Each band is a
    /// run of whole rows; `first_row` counts rows from the start of
    /// `pixels`.
    // On wasm there is no pool: every band runs on the calling thread.
    #[cfg_attr(target_arch = "wasm32", allow(clippy::unused_self))]
    pub(crate) fn bands<T: Send>(
        &self,
        pixels: &mut [T],
        width: usize,
        draw: impl Fn(usize, &mut [T]) + Sync + Send,
    ) {
        if width == 0 || pixels.is_empty() {
            return;
        }
        let rows = band_rows(width);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            use rayon::prelude::*;
            pool.install(|| {
                pixels
                    .par_chunks_mut(rows * width)
                    .enumerate()
                    .for_each(|(band, chunk)| wide(|| draw(band * rows, chunk)));
            });
            return;
        }
        for (band, chunk) in pixels.chunks_mut(rows * width).enumerate() {
            wide(|| draw(band * rows, chunk));
        }
    }

    /// Calls `work(index)` for every index below `count`, in parallel, and
    /// collects the results in index order.
    #[cfg_attr(target_arch = "wasm32", allow(clippy::unused_self))]
    pub(crate) fn map<T: Send>(
        &self,
        count: usize,
        work: impl Fn(usize) -> T + Sync + Send,
    ) -> Vec<T> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            use rayon::prelude::*;
            return pool.install(|| (0..count).into_par_iter().map(&work).collect());
        }
        (0..count).map(work).collect()
    }
}

/// Rows per band for an image `width` pixels wide.
fn band_rows(width: usize) -> usize {
    (BAND_PIXELS / width).clamp(1, 64)
}

/// Runs `work` compiled for the widest vector instructions this processor
/// has: AVX2 on x86-64 where it is present. The arithmetic is the same IEEE
/// operations in the same order either way (Rust never contracts a multiply
/// and an add, and FMA is not enabled), so the bits are too; only how many
/// lanes run at once differs.
#[inline]
fn wide<R>(work: impl FnOnce() -> R) -> R {
    #[cfg(target_arch = "x86_64")]
    if avx2::available() {
        // SAFETY: the processor reports AVX2 and F16C, the only features `run` enables.
        #[allow(unsafe_code)]
        return unsafe { avx2::run(work) };
    }
    work()
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::sync::OnceLock;

    pub(super) fn available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("f16c")
        })
    }

    /// `work`, inlined into a function compiled with AVX2.
    #[allow(unsafe_code)]
    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn run<R>(work: impl FnOnce() -> R) -> R {
        work()
    }
}
