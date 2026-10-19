/// The capability class exposed to renderer code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CapabilityTier {
    /// Full WebGPU-capable adapter.
    TierA,
    /// Conservative downlevel adapter.
    TierB,
}

/// Internal compositing texture format.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IntermediateFormat {
    /// Half-float RGBA storage (the format does not specify a transfer curve).
    Rgba16Float,
    /// Normalized RGBA fallback.
    Rgba8Unorm,
}

/// Adapter facts relevant to rendering decisions.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Capabilities {
    /// Compatibility classification.
    pub tier: CapabilityTier,
    /// Chosen intermediate format.
    pub intermediate_format: IntermediateFormat,
    /// Platform adapter name.
    pub adapter_name: String,
    /// Maximum 2D texture dimension.
    pub max_texture_dimension_2d: u32,
    /// Whether render-pass GPU timestamp queries were enabled on the device.
    pub timestamp_queries: bool,
    /// Backend selected by wgpu.
    pub backend: String,
    /// Adapter driver name.
    pub driver: String,
    /// Adapter driver detail/version when exposed by the platform.
    pub driver_info: String,
    /// Whether the adapter rasterizes on the CPU (lavapipe, llvmpipe,
    /// `SwiftShader`, WARP), where a shader's cost is paid for every pixel it
    /// is run on whatever branches it skips.
    pub software_rasterizer: bool,
}

impl Capabilities {
    /// Creates an explicit capability snapshot for policy simulation and tests.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tier: CapabilityTier,
        intermediate_format: IntermediateFormat,
        adapter_name: impl Into<String>,
        max_texture_dimension_2d: u32,
        timestamp_queries: bool,
        backend: impl Into<String>,
        driver: impl Into<String>,
        driver_info: impl Into<String>,
    ) -> Self {
        Self {
            tier,
            intermediate_format,
            adapter_name: adapter_name.into(),
            max_texture_dimension_2d,
            timestamp_queries,
            backend: backend.into(),
            driver: driver.into(),
            driver_info: driver_info.into(),
            software_rasterizer: false,
        }
    }

    pub(crate) fn from_adapter(adapter: &wgpu::Adapter) -> Self {
        let info = adapter.get_info();
        Self {
            tier: tier_for(&adapter.get_downlevel_capabilities()),
            intermediate_format: intermediate_format_for(
                adapter.get_texture_format_features(wgpu::TextureFormat::Rgba16Float),
            ),
            adapter_name: info.name,
            max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d,
            timestamp_queries: adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY),
            backend: format!("{:?}", info.backend),
            driver: info.driver,
            driver_info: info.driver_info,
            software_rasterizer: info.device_type == wgpu::DeviceType::Cpu,
        }
    }
}

/// The intermediate format for an adapter whose `Rgba16Float` support is
/// `half_float`: half float where pictures can be rendered to it, sampled
/// with filtering, and copied out; 8-bit everywhere else.
///
/// The tier does not enter into it: a WebGL2 or GL adapter with a float or
/// half-float colour buffer is tier B and still gets half float.
fn intermediate_format_for(half_float: wgpu::TextureFormatFeatures) -> IntermediateFormat {
    let required = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC;
    if half_float.allowed_usages.contains(required)
        && half_float
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
    {
        IntermediateFormat::Rgba16Float
    } else {
        IntermediateFormat::Rgba8Unorm
    }
}

/// Tier A for an adapter that meets all of WebGPU, tier B for a downlevel
/// one.
fn tier_for(downlevel: &wgpu::DownlevelCapabilities) -> CapabilityTier {
    if downlevel.is_webgpu_compliant() {
        CapabilityTier::TierA
    } else {
        CapabilityTier::TierB
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::{TextureFormatFeatureFlags as Flags, TextureUsages as Usages};

    fn half_float(allowed_usages: Usages, flags: Flags) -> wgpu::TextureFormatFeatures {
        wgpu::TextureFormatFeatures {
            allowed_usages,
            flags,
        }
    }

    #[test]
    fn half_float_is_the_intermediate_only_where_it_renders_filters_and_copies() {
        let required = Usages::RENDER_ATTACHMENT | Usages::TEXTURE_BINDING | Usages::COPY_SRC;
        assert_eq!(
            intermediate_format_for(half_float(required | Usages::COPY_DST, Flags::FILTERABLE)),
            IntermediateFormat::Rgba16Float
        );
        for missing in [
            Usages::RENDER_ATTACHMENT,
            Usages::TEXTURE_BINDING,
            Usages::COPY_SRC,
        ] {
            assert_eq!(
                intermediate_format_for(half_float(required - missing, Flags::FILTERABLE)),
                IntermediateFormat::Rgba8Unorm,
                "without {missing:?}"
            );
        }
        assert_eq!(
            intermediate_format_for(half_float(required, Flags::empty())),
            IntermediateFormat::Rgba8Unorm,
            "without filtering"
        );
        // No half float at all, as a WebGL2 context without a float colour buffer.
        assert_eq!(
            intermediate_format_for(half_float(Usages::empty(), Flags::empty())),
            IntermediateFormat::Rgba8Unorm
        );
    }

    #[test]
    fn the_tier_follows_webgpu_compliance_alone() {
        assert_eq!(
            tier_for(&wgpu::DownlevelCapabilities::default()),
            CapabilityTier::TierA
        );
        let mut downlevel = wgpu::DownlevelCapabilities::default();
        downlevel
            .flags
            .remove(wgpu::DownlevelFlags::COMPUTE_SHADERS);
        assert_eq!(tier_for(&downlevel), CapabilityTier::TierB);
        let shader_model = wgpu::DownlevelCapabilities {
            shader_model: wgpu::ShaderModel::Sm4,
            ..Default::default()
        };
        assert_eq!(tier_for(&shader_model), CapabilityTier::TierB);
    }
}
