use std::sync::OnceLock;

/// Current pipeline-cache occupancy.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PipelineCacheStats {
    /// Eagerly registered runtime-effect pipelines.
    pub runtime_effects: usize,
    /// Lazily initialized built-in pipelines.
    pub builtins: usize,
}

/// Work completed by a graph warmup request.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WarmupReport {
    /// Runtime effect pipelines verified as present.
    pub runtime_effects: usize,
    /// Built-in pipelines required and initialized.
    pub builtins: usize,
}

#[derive(Debug, Default)]
pub(crate) struct PipelineCache {
    pub(crate) composite: OnceLock<CachedCompositePipeline>,
    pub(crate) quad: OnceLock<CachedPipeline>,
    pub(crate) readback_pack: OnceLock<CachedPipeline>,
    pub(crate) rgba_present: OnceLock<CachedSampledPipeline>,
    pub(crate) yuv_readback: OnceLock<CachedYuvReadbackPipeline>,
    pub(crate) yuv: OnceLock<CachedYuvPipeline>,
    pub(crate) yuv16: OnceLock<CachedYuvPipeline>,
    pub(crate) yuv_repack: OnceLock<CachedYuvRepackPipeline>,
    pub(crate) vector: OnceLock<crate::vector::CachedVectorPipeline>,
}

#[derive(Debug)]
pub(crate) struct CachedPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
}

#[derive(Debug)]
pub(crate) struct CachedYuvPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) sampler: wgpu::Sampler,
}

#[derive(Debug)]
pub(crate) struct CachedSampledPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) sampler: wgpu::Sampler,
}

#[derive(Debug)]
pub(crate) struct CachedCompositePipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    pub(crate) shader: wgpu::ShaderModule,
    /// One pipeline per composite program (loop, then one-layer), built on first use.
    pub(crate) programs: [std::sync::OnceLock<wgpu::RenderPipeline>; 2],
}

#[derive(Debug)]
pub(crate) struct CachedYuvReadbackPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) y_pipeline: wgpu::RenderPipeline,
    pub(crate) uv_pipeline: wgpu::RenderPipeline,
    /// Writes both chroma channels into one half-size `Rg8Unorm` plane, the NV12 layout hardware
    /// encoders expect.
    pub(crate) uv_nv12_pipeline: wgpu::RenderPipeline,
}

/// Plane-for-plane copies of decoded 8-bit 4:2:0 frames into NV12 targets.
#[derive(Debug)]
pub(crate) struct CachedYuvRepackPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    /// Writes an `R8Unorm` luma target.
    pub(crate) luma: wgpu::RenderPipeline,
    /// Writes an `Rg8Unorm` chroma target from an interleaved UV plane.
    pub(crate) interleaved_chroma: wgpu::RenderPipeline,
    /// Writes an `Rg8Unorm` chroma target from separate U and V planes.
    pub(crate) planar_chroma: wgpu::RenderPipeline,
}

impl PipelineCache {
    pub(crate) fn builtin_count(&self) -> usize {
        usize::from(self.composite.get().is_some())
            + [&self.quad]
                .into_iter()
                .filter(|pipeline| pipeline.get().is_some())
                .count()
            + usize::from(self.yuv.get().is_some())
            + usize::from(self.yuv16.get().is_some())
            + usize::from(self.rgba_present.get().is_some())
            + usize::from(self.readback_pack.get().is_some())
            + usize::from(self.yuv_readback.get().is_some())
            + usize::from(self.yuv_repack.get().is_some())
            + self
                .vector
                .get()
                .map_or(0, crate::vector::CachedVectorPipeline::pipeline_count)
    }
}
