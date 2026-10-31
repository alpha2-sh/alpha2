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
    /// Whether `Rgba16Float` render targets blend in hardware, so pictures
    /// can be composited in linear light by blending alone.
    pub half_float_blendable: bool,
    /// The DX12 shader compiler the device's pipelines are compiled with
    /// (`FXC`, `DXC <path>`, ...); empty on other backends.
    pub shader_compiler: String,
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
        shader_compiler: impl Into<String>,
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
            half_float_blendable: intermediate_format == IntermediateFormat::Rgba16Float,
            shader_compiler: shader_compiler.into(),
        }
    }

    /// One line naming the device: `<adapter> (<backend>, driver <driver> <driver_info>,
    /// <compiler>)`, empty parts left out.
    #[must_use]
    pub fn summary(&self) -> String {
        let driver = [self.driver.as_str(), self.driver_info.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let mut parts = vec![self.backend.clone()];
        if !driver.is_empty() {
            parts.push(format!("driver {driver}"));
        }
        if !self.shader_compiler.is_empty() {
            parts.push(self.shader_compiler.clone());
        }
        format!("{} ({})", self.adapter_name, parts.join(", "))
    }

    /// The facts of `device`, opened on `adapter`: what the device was opened
    /// with (timestamp queries, texture size), not what the adapter offers;
    /// format facts come from the adapter.
    pub(crate) fn from_device(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let info = adapter.get_info();
        let shader_compiler = if info.backend == wgpu::Backend::Dx12 {
            crate::dx12_shader_compiler().to_string()
        } else {
            String::new()
        };
        let half_float = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba16Float);
        let software_rasterizer = software_adapter(&info);
        Self {
            tier: tier_for(&adapter.get_downlevel_capabilities()),
            intermediate_format: intermediate_format_for(half_float),
            half_float_blendable: half_float
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                && half_float
                    .flags
                    .contains(wgpu::TextureFormatFeatureFlags::BLENDABLE),
            adapter_name: info.name,
            max_texture_dimension_2d: device.limits().max_texture_dimension_2d,
            timestamp_queries: device.features().contains(wgpu::Features::TIMESTAMP_QUERY),
            backend: format!("{:?}", info.backend),
            driver: info.driver,
            driver_info: info.driver_info,
            software_rasterizer,
            shader_compiler,
        }
    }
}

/// Renderer names that mean a software rasterizer: wgpu-hal's list for GL
/// (`mesa offscreen`, `swiftshader`, `llvmpipe`) and WARP, which a browser
/// reaches through ANGLE as a GL renderer named "Microsoft Basic Render
/// Driver" that wgpu-hal does not flag.
const SOFTWARE_RENDERER_NAMES: [&str; 4] = [
    "mesa offscreen",
    "swiftshader",
    "llvmpipe",
    "microsoft basic render",
];

/// Whether `info` names a software adapter: one the backend flags as a CPU
/// (`DeviceType::Cpu`, a browser's fallback adapter) or whose name is a
/// software rasterizer's ([`software_renderer_name`]). Product selection and
/// the capability report both decide by this rule.
#[must_use]
pub fn software_adapter(info: &wgpu::AdapterInfo) -> bool {
    info.device_type == wgpu::DeviceType::Cpu || software_renderer_name(&info.name)
}

/// Whether a renderer or adapter name is a software rasterizer's, compared
/// without case: a browser's unmasked WebGL renderer string qualifies as the
/// adapter name does.
#[must_use]
pub fn software_renderer_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SOFTWARE_RENDERER_NAMES
        .iter()
        .any(|software| name.contains(software))
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
    fn software_adapters_are_named_by_type_or_renderer() {
        let info = |name: &str, device_type| wgpu::AdapterInfo {
            name: name.to_owned(),
            vendor: 0,
            device: 0,
            device_type,
            device_pci_bus_id: String::new(),
            driver: String::new(),
            driver_info: String::new(),
            backend: wgpu::Backend::Gl,
            subgroup_min_size: 0,
            subgroup_max_size: 0,
            transient_saves_memory: None,
            limit_bucket: None,
        };
        assert!(software_adapter(&info("", wgpu::DeviceType::Cpu)));
        assert!(software_adapter(&info(
            "ANGLE (Microsoft, Microsoft Basic Render Driver (0x0000008C) Direct3D11 vs_5_0 \
             ps_5_0, D3D11)",
            wgpu::DeviceType::Other
        )));
        assert!(software_adapter(&info(
            "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0x0000C0DE)), \
             SwiftShader driver)",
            wgpu::DeviceType::Other
        )));
        assert!(!software_adapter(&info(
            "ANGLE (AMD, AMD Radeon Graphics (0x0000164E) Direct3D11 vs_5_0 ps_5_0, D3D11)",
            wgpu::DeviceType::Other
        )));
        assert!(!software_adapter(&info(
            "Intel(R) UHD Graphics",
            wgpu::DeviceType::IntegratedGpu
        )));
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
