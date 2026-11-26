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
    pub(crate) layout_pack: OnceLock<CachedLayoutPackPipeline>,
    /// Test-only: the full-screen quad.
    #[cfg(test)]
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
}

#[derive(Debug)]
pub(crate) struct CachedSampledPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
}

#[derive(Debug)]
pub(crate) struct CachedCompositePipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
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

/// Packs frames into the integer planes of an output layout (layout-pack.wesl).
#[derive(Debug)]
pub(crate) struct CachedLayoutPackPipeline {
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline_layout: wgpu::PipelineLayout,
    pub(crate) shader: wgpu::ShaderModule,
    pub(crate) programs: [OnceLock<wgpu::RenderPipeline>; LayoutPackPass::COUNT],
}

impl CachedLayoutPackPipeline {
    fn program_count(&self) -> usize {
        self.programs
            .iter()
            .filter(|program| program.get().is_some())
            .count()
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum LayoutPackPass {
    Luma { ten_bit: bool, alpha: bool },
    Chroma { ten_bit: bool, interleaved: bool },
}

impl LayoutPackPass {
    const COUNT: usize = 8;

    pub(crate) fn of_layout(layout: alpha2_media::PixelLayout) -> [Self; 2] {
        let ten_bit = layout.bit_depth == alpha2_media::BitDepth::Ten;
        [
            Self::Luma {
                ten_bit,
                alpha: layout.alpha,
            },
            Self::Chroma {
                ten_bit,
                interleaved: layout.planes == alpha2_media::PlaneLayout::P010,
            },
        ]
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Luma { ten_bit, alpha } => (ten_bit as usize) * 2 + alpha as usize,
            Self::Chroma {
                ten_bit,
                interleaved,
            } => 4 + (ten_bit as usize) * 2 + interleaved as usize,
        }
    }

    pub(crate) const fn entry_point(self) -> &'static str {
        match self {
            Self::Luma { alpha: false, .. } => "luma_fs",
            Self::Luma { alpha: true, .. } => "luma_alpha_fs",
            Self::Chroma {
                interleaved: false, ..
            } => "chroma_fs",
            Self::Chroma {
                interleaved: true, ..
            } => "chroma_interleaved_fs",
        }
    }

    pub(crate) fn target_formats(self) -> Vec<wgpu::TextureFormat> {
        let (ten_bit, count, pair) = match self {
            Self::Luma { ten_bit, alpha } => (ten_bit, 1 + usize::from(alpha), false),
            Self::Chroma {
                ten_bit,
                interleaved,
            } => (ten_bit, 2 - usize::from(interleaved), interleaved),
        };
        let format = match (ten_bit, pair) {
            (false, false) => wgpu::TextureFormat::R8Uint,
            (false, true) => wgpu::TextureFormat::Rg8Uint,
            (true, false) => wgpu::TextureFormat::R16Uint,
            (true, true) => wgpu::TextureFormat::Rg16Uint,
        };
        vec![format; count]
    }
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
        #[cfg(test)]
        let quad = usize::from(self.quad.get().is_some());
        #[cfg(not(test))]
        let quad = 0;
        usize::from(self.composite.get().is_some())
            + quad
            + usize::from(self.yuv.get().is_some())
            + usize::from(self.yuv16.get().is_some())
            + usize::from(self.rgba_present.get().is_some())
            + usize::from(self.readback_pack.get().is_some())
            + usize::from(self.yuv_readback.get().is_some())
            + self
                .layout_pack
                .get()
                .map_or(0, CachedLayoutPackPipeline::program_count)
            + usize::from(self.yuv_repack.get().is_some())
            + self
                .vector
                .get()
                .map_or(0, crate::vector::CachedVectorPipeline::pipeline_count)
    }
}
