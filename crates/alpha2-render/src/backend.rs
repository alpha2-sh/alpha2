use crate::{EffectDef, EffectId, Renderer};

/// Effect information a graph builder needs without drawing. The GPU [`Renderer`] and the
/// [`crate::CpuRenderer`] answer from the same [`EffectDef`]s, so a frame compiles to the same
/// graph on either.
pub trait EffectCatalog {
    /// The definition registered as `id`.
    fn effect_definition(&self, id: &EffectId) -> Option<std::sync::Arc<EffectDef>>;

    /// Whether `id`'s output can change with frame time alone; `true` for unknown effects.
    fn effect_reads_time(&self, id: &EffectId) -> bool;

    /// The identity a project's effect `id`@`version` draws under; `None` if this renderer cannot
    /// draw it or it is internal.
    fn project_effect_id(&self, id: &str, version: u32) -> Option<EffectId>;
}

impl EffectCatalog for Renderer {
    fn effect_definition(&self, id: &EffectId) -> Option<std::sync::Arc<EffectDef>> {
        Renderer::effect_definition(self, id)
    }

    fn effect_reads_time(&self, id: &EffectId) -> bool {
        Renderer::effect_reads_time(self, id)
    }

    fn project_effect_id(&self, id: &str, version: u32) -> Option<EffectId> {
        Renderer::project_effect_id(self, id, version)
    }
}

/// Controls a frame runner sets and reads on either renderer.
pub trait RenderControls {
    /// Sets the filter sample budget (possibly capped lower) and motion blur subframe count.
    fn set_quality_controls(&self, max_filter_samples: u32, motion_blur_subframes: u32);

    /// Sets the byte budget of pooled pictures kept for reuse.
    fn set_texture_budget_bytes(&self, budget_bytes: u64);

    /// Changes whenever earlier results may differ from what the same graph draws now.
    fn content_epoch(&self) -> u64;

    /// Records whether a frame was presented and whether it repeated the previous one.
    fn record_frame_outcome(&self, presented: bool, repeated: bool);

    /// A deterministic key for what `graph` draws on this renderer.
    ///
    /// # Errors
    /// Returns [`crate::GraphError::MissingOutput`] when no output was selected.
    fn graph_cache_key(
        &self,
        graph: &crate::RenderGraph<'_>,
    ) -> Result<crate::RenderCacheKey, crate::GraphError>;
}

impl RenderControls for Renderer {
    fn set_quality_controls(&self, max_filter_samples: u32, motion_blur_subframes: u32) {
        Renderer::set_quality_controls(self, max_filter_samples, motion_blur_subframes);
    }

    fn set_texture_budget_bytes(&self, budget_bytes: u64) {
        Renderer::set_texture_budget_bytes(self, budget_bytes);
    }

    fn content_epoch(&self) -> u64 {
        Renderer::content_epoch(self)
    }

    fn record_frame_outcome(&self, presented: bool, repeated: bool) {
        Renderer::record_frame_outcome(self, presented, repeated);
    }

    fn graph_cache_key(
        &self,
        graph: &crate::RenderGraph<'_>,
    ) -> Result<crate::RenderCacheKey, crate::GraphError> {
        Renderer::graph_cache_key(self, graph)
    }
}
