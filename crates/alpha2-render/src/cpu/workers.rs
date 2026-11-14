//! The CPU renderer's threads. Each pass splits rows into bands, each written by one thread, so
//! results do not depend on the thread count.

/// Approximate pixels per band: enough to amortize scheduling, small enough to stay in cache.
const BAND_PIXELS: usize = 16_384;

/// The threads one CPU renderer draws with.
pub(crate) struct Workers {
    #[cfg(not(target_arch = "wasm32"))]
    pool: Option<rayon::ThreadPool>,
    threads: usize,
    /// Rows of every band in place of [`band_rows`], so tests can move band seams.
    fixed_rows: Option<usize>,
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
    /// `threads` workers (one means the calling thread); falls back to the calling thread if the
    /// pool cannot start.
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
            Self {
                pool,
                threads,
                fixed_rows: None,
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = threads;
            Self {
                threads: 1,
                fixed_rows: None,
            }
        }
    }

    /// As [`Self::new`], with every band `rows` rows high (`usize::MAX` for one band).
    #[cfg(test)]
    pub(crate) fn with_band_rows(threads: usize, rows: usize) -> Self {
        Self {
            fixed_rows: Some(rows.max(1)),
            ..Self::new(threads)
        }
    }

    /// Rows per band for an image `width` pixels wide.
    pub(crate) fn band_rows(&self, width: usize) -> usize {
        self.fixed_rows.unwrap_or_else(|| band_rows(width))
    }

    pub(crate) const fn threads(&self) -> usize {
        self.threads
    }

    /// Calls `draw(first_row, rows)` in parallel for each band of whole rows of `pixels`, an image
    /// `width` wide; `first_row` counts from the start of `pixels`.
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
        let rows = self.band_rows(width);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            use rayon::prelude::*;
            pool.install(|| {
                pixels
                    .par_chunks_mut(rows.saturating_mul(width))
                    .enumerate()
                    .for_each(|(band, chunk)| draw(band * rows, chunk));
            });
            return;
        }
        for (band, chunk) in pixels.chunks_mut(rows.saturating_mul(width)).enumerate() {
            draw(band * rows, chunk);
        }
    }

    /// As [`Self::bands`], with `draw(scratch, first_row, rows)` reusing one `init()` scratch per
    /// thread for the bands it draws in this pass.
    ///
    /// Which bands share a scratch depends on scheduling, so `draw` must reset everything it
    /// reads from the scratch at the start of each band.
    pub(crate) fn bands_with<T: Send, S: Send>(
        &self,
        pixels: &mut [T],
        width: usize,
        init: impl Fn() -> S + Sync + Send,
        draw: impl Fn(&mut S, usize, &mut [T]) + Sync + Send,
    ) {
        if width == 0 || pixels.is_empty() {
            return;
        }
        let rows = self.band_rows(width);
        self.chunks_with(pixels, rows.saturating_mul(width), rows, init, draw);
    }

    /// As [`Self::bands_with`] over `items` that each cover `item_pixels` pixels (one row of
    /// chroma, say): bands hold whole items, `draw` gets the index of its first item.
    pub(crate) fn items_with<T: Send, S: Send>(
        &self,
        items: &mut [T],
        item_pixels: usize,
        init: impl Fn() -> S + Sync + Send,
        draw: impl Fn(&mut S, usize, &mut [T]) + Sync + Send,
    ) {
        if item_pixels == 0 || items.is_empty() {
            return;
        }
        let rows = self.band_rows(item_pixels);
        self.chunks_with(items, rows, rows, init, draw);
    }

    /// Calls `draw(scratch, band * step, chunk)` for each `length`-long chunk of `items`.
    #[cfg_attr(target_arch = "wasm32", allow(clippy::unused_self))]
    fn chunks_with<T: Send, S: Send>(
        &self,
        items: &mut [T],
        length: usize,
        step: usize,
        init: impl Fn() -> S + Sync + Send,
        draw: impl Fn(&mut S, usize, &mut [T]) + Sync + Send,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            use rayon::prelude::*;
            use std::sync::{Mutex, TryLockError};
            // The last slot serves a caller outside the pool.
            let slots = (0..=self.threads)
                .map(|_| Mutex::new(None::<S>))
                .collect::<Vec<_>>();
            pool.install(|| {
                items
                    .par_chunks_mut(length)
                    .enumerate()
                    .for_each(|(band, chunk)| {
                        let slot = rayon::current_thread_index()
                            .map_or(self.threads, |index| index.min(self.threads));
                        match slots[slot].try_lock() {
                            Ok(mut held) => {
                                let scratch = held.get_or_insert_with(&init);
                                draw(scratch, band * step, chunk);
                            }
                            // A thread that re-entered the pool while drawing: a scratch of its own.
                            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                                draw(&mut init(), band * step, chunk);
                            }
                        }
                    });
            });
            return;
        }
        let mut scratch = init();
        for (band, chunk) in items.chunks_mut(length).enumerate() {
            draw(&mut scratch, band * step, chunk);
        }
    }

    /// Calls `job(index)` for every index below `count`, in parallel and in any order.
    #[cfg_attr(target_arch = "wasm32", allow(clippy::unused_self))]
    pub(crate) fn for_each(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            use rayon::prelude::*;
            pool.install(|| (0..count).into_par_iter().for_each(job));
            return;
        }
        (0..count).for_each(job);
    }

    /// Calls `work(index)` in parallel for every index below `count`, collecting in index order.
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

impl crate::layout_pack::Bands for Workers {
    fn rows_per_band(&self, _chroma_rows: usize, row_pixels: usize) -> usize {
        self.band_rows(row_pixels)
    }

    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        self.for_each(count, job);
    }
}
