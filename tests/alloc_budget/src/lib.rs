//! Allocation budgets for alpha2's hot paths.
//!
//! Tests count heap allocations on the calling thread after warm-up. Counts are deterministic, so
//! they can gate CI; lower a budget when a change lowers its count.
