//! The CPU renderer: its catalog of built-ins, its controls, and its
//! result cache.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::image::CpuImage;
use super::kernels::{CpuKernel, CpuLut, kernel};
use super::workers::Workers;
use crate::result_cache::{CachedPicture, ResultCache};
use crate::{
    BlendSpace, CacheInvalidation, EffectCatalog, EffectDef, EffectId, GraphError, Lut3dData,
    RenderCacheKey, RenderControls, RenderError, RenderGraph, RenderQualityControls,
    ResultCacheStats,
};

impl CachedPicture for Arc<CpuImage> {
    fn cached_bytes(&self) -> u64 {
        self.retained_bytes()
    }
}

/// One built-in the CPU renderer draws.
pub(crate) struct CatalogEntry {
    pub(crate) definition: EffectDef,
    pub(crate) reads_time: bool,
    pub(crate) kernel: &'static dyn CpuKernel,
}

/// Counts of what the CPU renderer has drawn, for metrics.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CpuRenderStats {
    /// Graphs drawn.
    pub graphs: u64,
    /// Nodes drawn (not found in the result cache).
    pub nodes: u64,
    /// Time spent drawing graphs, in microseconds.
    pub render_micros: u64,
    /// Sources presented: decoded frames converted to pictures.
    pub presented_sources: u64,
    /// Bytes of the largest picture set one graph held at once.
    pub image_high_water_bytes: u64,
}

#[derive(Default)]
pub(crate) struct StatCounters {
    pub(crate) graphs: AtomicU64,
    pub(crate) nodes: AtomicU64,
    pub(crate) render_micros: AtomicU64,
    pub(crate) presented_sources: AtomicU64,
    pub(crate) image_high_water_bytes: AtomicU64,
}

/// A renderer that draws [`RenderGraph`]s on the CPU, with no GPU and no
/// graphics driver.
///
/// It draws every built-in effect, transition and pass of the GPU
/// [`crate::Renderer`], to the same contract: the same graph, the same
/// stages, and pictures stored as the GPU's half-float intermediate stores
/// them. Effects registered at run time as WGSL have no CPU implementation;
/// a graph that draws one fails with [`RenderError::UnknownEffect`].
///
/// Its output is the same for any thread count and on every target.
pub struct CpuRenderer {
    pub(crate) workers: Workers,
    pub(crate) blend_space: BlendSpace,
    quality: Mutex<RenderQualityControls>,
    cache: Mutex<ResultCache<Arc<CpuImage>>>,
    catalog: HashMap<EffectId, CatalogEntry>,
    internal: HashSet<EffectId>,
    resources: HashMap<String, Arc<CpuLut>>,
    /// Vector coverage kept between frames, as the GPU's vector pass keeps it.
    pub(crate) coverage: Mutex<alpha2_geometry::raster::CoverageCache>,
    pub(crate) stats: StatCounters,
    frames: AtomicU64,
    repeated: AtomicU64,
}

impl std::fmt::Debug for CpuRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CpuRenderer")
            .field("threads", &self.workers.threads())
            .field("blend_space", &self.blend_space)
            .finish_non_exhaustive()
    }
}

impl CpuRenderer {
    /// A CPU renderer drawing on `threads` threads (one draws on the
    /// calling thread), with every built-in registered.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        let mut catalog = HashMap::new();
        let mut internal = HashSet::new();
        for (definition, is_internal) in crate::builtins::internal_effect_definitions()
            .into_iter()
            .map(|definition| (definition, true))
            .chain(
                crate::builtin_effect_definitions()
                    .into_iter()
                    .chain(crate::builtin_transition_definitions())
                    .map(|definition| (definition, false)),
            )
        {
            let Some(kernel) = kernel(&definition.id, definition.version) else {
                continue;
            };
            let id = EffectId::new(definition.id.clone(), definition.version);
            if is_internal {
                internal.insert(id.clone());
            }
            catalog.insert(
                id,
                CatalogEntry {
                    reads_time: crate::registry::body_reads_time(&definition.wgsl_body),
                    definition,
                    kernel,
                },
            );
        }
        Self {
            workers: Workers::new(threads),
            blend_space: BlendSpace::default(),
            quality: Mutex::new(RenderQualityControls::default()),
            cache: Mutex::new(ResultCache::default()),
            catalog,
            internal,
            resources: HashMap::new(),
            coverage: Mutex::new(alpha2_geometry::raster::CoverageCache::default()),
            stats: StatCounters::default(),
            frames: AtomicU64::new(0),
            repeated: AtomicU64::new(0),
        }
    }

    /// The renderer with composites blending in `blend_space` unless a
    /// graph sets its own.
    #[must_use]
    pub const fn with_blend_space(mut self, blend_space: BlendSpace) -> Self {
        self.blend_space = blend_space;
        self
    }

    /// Threads the renderer draws on.
    #[must_use]
    pub const fn threads(&self) -> usize {
        self.workers.threads()
    }

    /// The quality controls passes take now.
    #[must_use]
    pub fn quality_controls(&self) -> RenderQualityControls {
        *self.quality.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every built-in effect and transition the GPU registers that this
    /// renderer cannot draw, as `(id, version)`. Empty: every built-in has a
    /// CPU kernel.
    #[must_use]
    pub fn unsupported_builtins() -> Vec<(String, u32)> {
        crate::builtin_effect_definitions()
            .into_iter()
            .chain(crate::builtin_transition_definitions())
            .chain(crate::builtins::internal_effect_definitions())
            .filter(|definition| kernel(&definition.id, definition.version).is_none())
            .map(|definition| (definition.id, definition.version))
            .collect()
    }

    /// Keeps up to `budget_bytes` of drawn node results for reuse across
    /// graphs, as [`crate::Renderer::set_result_cache_budget`] does.
    pub fn set_result_cache_budget(&self, budget_bytes: u64) {
        self.lock_cache().set_budget(budget_bytes);
    }

    /// The result cache's counters.
    #[must_use]
    pub fn result_cache_stats(&self) -> ResultCacheStats {
        self.lock_cache().stats()
    }

    /// Drops every cached result.
    pub fn clear_result_cache(&self) {
        self.lock_cache().clear(CacheInvalidation::Explicit);
    }

    /// What the renderer has drawn so far.
    #[must_use]
    pub fn stats(&self) -> CpuRenderStats {
        CpuRenderStats {
            graphs: self.stats.graphs.load(Ordering::Relaxed),
            nodes: self.stats.nodes.load(Ordering::Relaxed),
            render_micros: self.stats.render_micros.load(Ordering::Relaxed),
            presented_sources: self.stats.presented_sources.load(Ordering::Relaxed),
            image_high_water_bytes: self.stats.image_high_water_bytes.load(Ordering::Relaxed),
        }
    }

    /// Frames presented and frames that repeated the one before, as
    /// [`RenderControls::record_frame_outcome`] counted them.
    #[must_use]
    pub fn frame_outcomes(&self) -> (u64, u64) {
        (
            self.frames.load(Ordering::Relaxed),
            self.repeated.load(Ordering::Relaxed),
        )
    }

    /// Registers or replaces the 3D lookup table `id`, as
    /// [`crate::Renderer::register_lut3d`] does.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidEffectResource`] for an empty identity
    /// or an invalid cube.
    pub fn register_lut3d(
        &mut self,
        id: impl Into<String>,
        data: &Lut3dData,
    ) -> Result<(), RenderError> {
        let id = id.into();
        if id.is_empty() || !data.is_valid(u32::MAX) {
            return Err(RenderError::InvalidEffectResource);
        }
        // Stored as the GPU's half-float texture stores it.
        let values = data
            .values
            .iter()
            .map(|rgb| rgb.map(|value| half::f16::from_f32(value).to_f32()))
            .collect();
        self.resources.insert(
            id,
            Arc::new(CpuLut {
                edge: data.edge_length,
                domain_min: data.domain_min,
                domain_max: data.domain_max,
                values,
            }),
        );
        self.lock_cache().clear(CacheInvalidation::EffectResources);
        Ok(())
    }

    /// Removes every resource and clears the cached results.
    pub fn clear_effect_resources(&mut self) {
        self.resources.clear();
        self.lock_cache().clear(CacheInvalidation::EffectResources);
    }

    /// Lets go of the cached results, which are kept only for reuse, and
    /// returns the bytes they held.
    pub fn release_idle_memory(&self) -> u64 {
        let mut cache = self.lock_cache();
        let bytes = cache.stats().bytes;
        cache.clear(CacheInvalidation::Explicit);
        bytes
    }

    /// Removes the resource `id` and returns whether it existed.
    pub fn unregister_effect_resource(&mut self, id: &str) -> bool {
        let removed = self.resources.remove(id).is_some();
        if removed {
            self.lock_cache().clear(CacheInvalidation::EffectResources);
        }
        removed
    }

    pub(crate) fn resource(&self, id: &str) -> Option<&Arc<CpuLut>> {
        self.resources.get(id)
    }

    pub(crate) fn entry(&self, id: &EffectId) -> Option<&CatalogEntry> {
        self.catalog.get(id)
    }

    pub(crate) fn lock_cache(&self) -> MutexGuard<'_, ResultCache<Arc<CpuImage>>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EffectCatalog for CpuRenderer {
    fn effect_definition(&self, id: &EffectId) -> Option<&EffectDef> {
        self.catalog.get(id).map(|entry| &entry.definition)
    }

    fn effect_reads_time(&self, id: &EffectId) -> bool {
        if let Some(passes) = crate::gaussian_blur::expanded_passes(id.as_str()) {
            return passes
                .iter()
                .any(|pass| self.effect_reads_time(&EffectId::from_static(pass, 1)));
        }
        self.catalog.get(id).is_none_or(|entry| entry.reads_time)
    }

    fn project_effect_id(&self, id: &str, version: u32) -> Option<&EffectId> {
        let key = EffectId::new(id.to_owned(), version);
        self.catalog
            .get_key_value(&key)
            .map(|(id, _)| id)
            .filter(|id| !self.internal.contains(*id))
    }
}

impl RenderControls for CpuRenderer {
    fn set_quality_controls(&self, max_filter_samples: u32, motion_blur_subframes: u32) {
        let next = RenderQualityControls {
            max_filter_samples: max_filter_samples
                .clamp(1, RenderQualityControls::MAX_FILTER_SAMPLES),
            motion_blur_subframes: motion_blur_subframes.max(1),
        };
        let mut quality = self.quality.lock().unwrap_or_else(PoisonError::into_inner);
        if *quality != next {
            *quality = next;
            drop(quality);
            // Results drawn with other controls are other pictures.
            self.lock_cache().clear(CacheInvalidation::Quality);
        }
    }

    /// The CPU renderer keeps no pool of pictures to budget: a picture's
    /// memory goes back to the allocator when its last reader is done.
    fn set_texture_budget_bytes(&self, _budget_bytes: u64) {}

    fn content_epoch(&self) -> u64 {
        self.lock_cache().clears()
    }

    fn record_frame_outcome(&self, presented: bool, repeated: bool) {
        if presented {
            self.frames.fetch_add(1, Ordering::Relaxed);
        }
        if repeated {
            self.repeated.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn graph_cache_key(&self, graph: &RenderGraph<'_>) -> Result<RenderCacheKey, GraphError> {
        let quality = self.quality_controls();
        let plan = graph.compile_with_quality(quality)?;
        Ok(crate::graph_cache::cpu_renderer_cache_key(
            plan.cache_key,
            graph.blend_space.unwrap_or(self.blend_space).shader_value(),
            quality,
        ))
    }
}
