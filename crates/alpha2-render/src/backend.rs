use crate::{EffectDef, EffectId, Renderer};

/// What a renderer can tell a graph builder about the effects it draws,
/// without drawing anything: the GPU [`Renderer`]'s registry and the
/// [`crate::CpuRenderer`]'s kernels answer from the same [`EffectDef`]s, so
/// a frame compiles to the same graph whichever renderer draws it.
pub trait EffectCatalog {
    /// The definition registered as `id`.
    fn effect_definition(&self, id: &EffectId) -> Option<&EffectDef>;

    /// Whether what `id` draws can change with the frame time alone. An
    /// effect that is not registered answers `true`.
    fn effect_reads_time(&self, id: &EffectId) -> bool;

    /// The identity a project's effect `id` at `version` is drawn under;
    /// `None` when this renderer cannot draw it, or it is an internal pass.
    fn project_effect_id(&self, id: &str, version: u32) -> Option<&EffectId>;
}

impl EffectCatalog for Renderer {
    fn effect_definition(&self, id: &EffectId) -> Option<&EffectDef> {
        Renderer::effect_definition(self, id)
    }

    fn effect_reads_time(&self, id: &EffectId) -> bool {
        Renderer::effect_reads_time(self, id)
    }

    fn project_effect_id(&self, id: &str, version: u32) -> Option<&EffectId> {
        Renderer::project_effect_id(self, id, version)
    }
}

/// The controls a frame runner sets and reads on whichever renderer draws
/// its frames: quality, the memory budget, and what has changed since.
pub trait RenderControls {
    /// Sets the filter sample budget and the motion blur subframe count
    /// passes take; the renderer may cap the budget lower.
    fn set_quality_controls(&self, max_filter_samples: u32, motion_blur_subframes: u32);

    /// Sets the bytes of pooled pictures the renderer keeps for reuse.
    fn set_texture_budget_bytes(&self, budget_bytes: u64);

    /// A count that changes whenever results drawn earlier may no longer be
    /// what the same graph draws now.
    fn content_epoch(&self) -> u64;

    /// Records whether a frame was presented, and whether it repeated the
    /// one before.
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
