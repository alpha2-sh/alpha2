//! Coverage kept between frames: a path that does not change is not
//! rasterized again.

use super::TileCoverage;
use crate::{IRect, StableHash};
use alpha2_model::FillRule;
use kurbo::BezPath;
use std::collections::HashMap;
use std::sync::Arc;

/// The key of `path`'s coverage under `rule` in `clip`: its bits, so only
/// the same path in the same place shares it.
#[must_use]
pub fn coverage_key(path: &BezPath, rule: FillRule, clip: IRect) -> u128 {
    let mut hash = StableHash::new();
    hash.bytes(b"coverage");
    hash.path(path);
    hash.u32(match rule {
        FillRule::Nonzero => 0,
        FillRule::Evenodd => 1,
    });
    for edge in [clip.x0, clip.y0, clip.x1, clip.y1] {
        hash.u32(edge.cast_unsigned());
    }
    hash.finish()
}

/// Coverage by key, least recently used first out, within a byte budget.
#[derive(Debug)]
pub struct CoverageCache {
    budget: usize,
    bytes: usize,
    clock: u64,
    entries: HashMap<u128, (Arc<TileCoverage>, u64)>,
    hits: u64,
    misses: u64,
}

/// The default budget: 64 MiB of tiles.
pub const DEFAULT_COVERAGE_BUDGET: usize = 64 * 1024 * 1024;

impl Default for CoverageCache {
    fn default() -> Self {
        Self::new(DEFAULT_COVERAGE_BUDGET)
    }
}

impl CoverageCache {
    /// An empty cache holding at most `budget` bytes.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            bytes: 0,
            clock: 0,
            entries: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// The coverage kept under `key`, now the most recently used.
    pub fn get(&mut self, key: u128) -> Option<Arc<TileCoverage>> {
        self.clock += 1;
        let clock = self.clock;
        if let Some((coverage, used)) = self.entries.get_mut(&key) {
            *used = clock;
            self.hits += 1;
            Some(Arc::clone(coverage))
        } else {
            self.misses += 1;
            None
        }
    }

    /// Keeps `coverage` under `key`, dropping the least recently used until
    /// it fits. Coverage larger than the whole budget is not kept.
    pub fn insert(&mut self, key: u128, coverage: Arc<TileCoverage>) {
        let size = coverage.heap_bytes() + size_of::<TileCoverage>();
        if size > self.budget {
            return;
        }
        self.clock += 1;
        if let Some((old, _)) = self.entries.insert(key, (coverage, self.clock)) {
            self.bytes -= old.heap_bytes() + size_of::<TileCoverage>();
        }
        self.bytes += size;
        while self.bytes > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some((evicted, _)) = self.entries.remove(&oldest) {
                self.bytes -= evicted.heap_bytes() + size_of::<TileCoverage>();
            }
        }
    }

    /// Drops everything kept.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Bytes kept.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Entries kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lookups that found coverage, and ones that did not.
    #[must_use]
    pub const fn hit_counts(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}
