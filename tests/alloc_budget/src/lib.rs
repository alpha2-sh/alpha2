//! Allocation budgets for alpha2's hot paths.
//!
//! The tests in `tests/` count heap allocations made by one timeline
//! evaluation and one audio block, after warm-up, on the calling thread only,
//! and fail when a count rises above its budget. Counts are deterministic, so
//! unlike times they can gate CI. A change that lowers a count lowers the
//! budget with it.
