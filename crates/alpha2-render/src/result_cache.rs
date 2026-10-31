//! Rendered node results kept across graph executions.
//!
//! A node's key covers its operation, parameters, runtime, size and its inputs' keys down to
//! the sources. It is complete only when every source beneath has a host-vouched key
//! ([`crate::RenderGraph::add_source_with_key`]); unkeyed sources get a per-graph key. Only
//! candidate nodes ([`crate::RenderGraph::set_cache_intent`]) with complete keys are stored, so
//! a hit is exactly what a live render would produce.
//!
//! Anything the key leaves out clears the cache when it changes: effect removal, effect
//! resource changes, and quality controls.

use std::collections::HashMap;

use crate::RenderedTexture;
use crate::graph::RenderCacheKey;

/// Whether a node's result may be kept and reused.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum CacheIntent {
    /// Keep when the key is complete; evict like any other result.
    #[default]
    Auto,
    /// Keep when the key is complete; evict only after every unpinned result.
    Pin,
    /// Never read or keep it, its inputs, or its dependents; for inspecting a live render.
    Bypass,
}

/// Whether a node's result can be cached, and if not, why.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CacheEligibility {
    /// Its key names everything it depends on.
    Eligible,
    /// A source beneath it has no stable key.
    TransientSource,
    /// It, an input, or a dependent asks for [`CacheIntent::Bypass`].
    Bypassed,
}

/// Why cached results were dropped all at once.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CacheInvalidation {
    /// An effect was registered or removed.
    Effects,
    /// An effect resource was uploaded, replaced, or removed.
    EffectResources,
    /// Filter or temporal sampling controls changed.
    Quality,
    /// The caller cleared it.
    Explicit,
    /// The browser reported a GPU error for earlier work (see
    /// [`crate::Renderer::gpu_error_reports`]).
    GpuError,
}

/// Result-cache counters since the renderer was created.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ResultCacheStats {
    /// Bytes results may occupy; zero disables the cache.
    pub budget_bytes: u64,
    /// Bytes the kept results occupy.
    pub bytes: u64,
    /// Results kept.
    pub entries: u64,
    /// Of those, pinned.
    pub pinned: u64,
    /// Candidate nodes found in the cache.
    pub hits: u64,
    /// Candidate nodes with complete keys that were not.
    pub misses: u64,
    /// Candidates that could not be cached (transient source or bypass).
    pub ineligible: u64,
    /// Results stored.
    pub inserts: u64,
    /// Results dropped to stay within the budget.
    pub evictions: u64,
    /// Nodes not encoded because a result above them was found.
    pub skipped_nodes: u64,
    /// Clears by reason: effects, effect resources, quality, explicit, GPU error.
    pub invalidations: [u64; 5],
}

struct Entry<T> {
    texture: T,
    bytes: u64,
    pinned: bool,
    last_used: u64,
}

/// A picture the result cache can keep, cloned out on a hit and charged against the budget.
pub(crate) trait CachedPicture: Clone {
    fn cached_bytes(&self) -> u64;
}

impl CachedPicture for RenderedTexture {
    fn cached_bytes(&self) -> u64 {
        texture_bytes(self)
    }
}

pub(crate) struct ResultCache<T = RenderedTexture> {
    entries: HashMap<u128, Entry<T>>,
    clock: u64,
    /// Clears for any reason, even when empty; each changes pixels without changing graph keys.
    clears: u64,
    stats: ResultCacheStats,
}

impl<T> Default for ResultCache<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            clears: 0,
            stats: ResultCacheStats::default(),
        }
    }
}

impl<T> std::fmt::Debug for ResultCache<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResultCache")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl<T: CachedPicture> ResultCache<T> {
    pub(crate) const fn enabled(&self) -> bool {
        self.stats.budget_bytes > 0
    }

    pub(crate) const fn stats(&self) -> ResultCacheStats {
        self.stats
    }

    pub(crate) const fn clears(&self) -> u64 {
        self.clears
    }

    pub(crate) fn set_budget(&mut self, budget_bytes: u64) {
        self.stats.budget_bytes = budget_bytes;
        self.evict_to(budget_bytes);
    }

    /// A kept result, marked used and re-pinned per the current request (intent is not part of the
    /// key).
    pub(crate) fn get(&mut self, key: RenderCacheKey, pinned: bool) -> Option<T> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(&key.value())?;
        entry.last_used = clock;
        if entry.pinned != pinned {
            entry.pinned = pinned;
            if pinned {
                self.stats.pinned += 1;
            } else {
                self.stats.pinned -= 1;
            }
        }
        Some(entry.texture.clone())
    }

    /// Keeps `texture` under `key`, evicting least recently used results (unpinned first). A
    /// result larger than the whole budget is not kept.
    pub(crate) fn insert(&mut self, key: RenderCacheKey, texture: &T, pinned: bool) {
        let bytes = texture.cached_bytes();
        if bytes > self.stats.budget_bytes {
            return;
        }
        self.remove(key.value());
        self.evict_to(self.stats.budget_bytes - bytes);
        self.clock += 1;
        self.entries.insert(
            key.value(),
            Entry {
                texture: texture.clone(),
                bytes,
                pinned,
                last_used: self.clock,
            },
        );
        self.stats.bytes += bytes;
        self.stats.entries += 1;
        self.stats.pinned += u64::from(pinned);
        self.stats.inserts += 1;
    }

    pub(crate) fn clear(&mut self, reason: CacheInvalidation) {
        self.clears += 1;
        if !self.entries.is_empty() {
            self.stats.invalidations[reason as usize] += 1;
        }
        self.entries.clear();
        self.stats.bytes = 0;
        self.stats.entries = 0;
        self.stats.pinned = 0;
    }

    pub(crate) fn record(&mut self, hits: u64, misses: u64, ineligible: u64, skipped: u64) {
        self.stats.hits += hits;
        self.stats.misses += misses;
        self.stats.ineligible += ineligible;
        self.stats.skipped_nodes += skipped;
    }

    fn evict_to(&mut self, bytes: u64) {
        while self.stats.bytes > bytes {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| (entry.pinned, entry.last_used))
                .map(|(key, _)| *key)
            else {
                return;
            };
            self.remove(victim);
            self.stats.evictions += 1;
        }
    }

    fn remove(&mut self, key: u128) {
        if let Some(entry) = self.entries.remove(&key) {
            self.stats.bytes -= entry.bytes;
            self.stats.entries -= 1;
            self.stats.pinned -= u64::from(entry.pinned);
        }
    }
}

fn texture_bytes(texture: &RenderedTexture) -> u64 {
    let texel = u64::from(texture.format.block_copy_size(None).unwrap_or(4));
    u64::from(texture.size.width) * u64::from(texture.size.height) * texel
}
