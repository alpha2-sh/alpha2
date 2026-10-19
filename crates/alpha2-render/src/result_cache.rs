//! Rendered node results kept across graph executions.
//!
//! A graph node's cache key names everything its pixels depend on: its
//! operation, parameters, runtime, size, and its inputs' keys, down to its
//! sources. That key is complete only when every source under the node has a
//! key its host vouches for ([`crate::RenderGraph::add_source_with_key`]); a
//! source added without one gets a key unique to its graph, so a node above it
//! can never be found again. Only nodes a caller names as candidates
//! ([`crate::RenderGraph::set_cache_intent`]) are stored, and only those with
//! complete keys: a stored result is exactly the texture a live render would
//! produce, never an approximation.
//!
//! What a key leaves out, the cache is cleared on: removing an effect (a new
//! registration under its id and version could draw differently), uploading,
//! replacing, or removing an effect resource, and changing quality controls.

use std::collections::HashMap;

use crate::RenderedTexture;
use crate::graph::RenderCacheKey;

/// Whether a node's result may be kept and reused.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum CacheIntent {
    /// Keep it when its key is complete, evicting it like any other result.
    #[default]
    Auto,
    /// Keep it when its key is complete, evicting it only after every
    /// unpinned result.
    Pin,
    /// Never read or keep it, nor anything it is drawn from or drawn into:
    /// for looking at a live render with certainty.
    Bypass,
}

/// Whether a node's result can be cached, and if not, why.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CacheEligibility {
    /// Its key names everything it depends on.
    Eligible,
    /// A source under it has no stable key, so its key is unique to this
    /// graph.
    TransientSource,
    /// It, something it is drawn from, or something it is drawn into asks
    /// for [`CacheIntent::Bypass`].
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
    /// The browser reported a GPU error for earlier work, which may have
    /// come from a kept result (see [`crate::Renderer::gpu_error_reports`]).
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
    /// Candidate nodes that could not be cached: a transient source or a
    /// bypass under or over them.
    pub ineligible: u64,
    /// Results stored.
    pub inserts: u64,
    /// Results dropped to stay within the budget.
    pub evictions: u64,
    /// Nodes not encoded because a result above them was found.
    pub skipped_nodes: u64,
    /// Clears by reason: effects, effect resources, quality, explicit, and
    /// a GPU error reported for earlier work.
    pub invalidations: [u64; 5],
}

struct Entry {
    texture: RenderedTexture,
    bytes: u64,
    pinned: bool,
    last_used: u64,
}

/// Kept results by renderer-level key, bounded by bytes.
#[derive(Default)]
pub(crate) struct ResultCache {
    entries: HashMap<u128, Entry>,
    clock: u64,
    stats: ResultCacheStats,
}

impl std::fmt::Debug for ResultCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResultCache")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl ResultCache {
    pub(crate) const fn enabled(&self) -> bool {
        self.stats.budget_bytes > 0
    }

    pub(crate) const fn stats(&self) -> ResultCacheStats {
        self.stats
    }

    pub(crate) fn set_budget(&mut self, budget_bytes: u64) {
        self.stats.budget_bytes = budget_bytes;
        self.evict_to(budget_bytes);
    }

    /// A kept result, marked as just used and pinned as the node that asks
    /// for it now is: a cache intent is not part of the key, so a clip pinned
    /// or unpinned after its picture was kept finds the same entry.
    pub(crate) fn get(&mut self, key: RenderCacheKey, pinned: bool) -> Option<RenderedTexture> {
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

    /// Keeps `texture` under `key`, evicting least recently used results
    /// (unpinned before pinned) to make room. A result larger than the whole
    /// budget is not kept.
    pub(crate) fn insert(&mut self, key: RenderCacheKey, texture: &RenderedTexture, pinned: bool) {
        let bytes = texture_bytes(texture);
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
