//! What a runtime can do, in one shape shared by the native host and the browser bridge.

use std::collections::BTreeMap;

use alpha2_gpu::CapabilityTier;
use alpha2_render::Renderer;
use serde::Serialize;

use crate::PlaybackPolicy;

/// The report's schema name.
pub const CAPABILITY_SCHEMA: &str = "alpha2-capabilities";
/// The report's schema version.
pub const CAPABILITY_VERSION: u32 = 2;

/// Where the engine runs.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// A native host process.
    Native,
    /// A browser page.
    Browser,
}

/// What a runtime can do.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityReport {
    /// [`CAPABILITY_SCHEMA`].
    pub schema: &'static str,
    /// [`CAPABILITY_VERSION`].
    pub version: u32,
    /// Where the engine runs.
    pub runtime: Runtime,
    /// The GPU it renders with.
    pub gpu: GpuReport,
    /// How it handles color.
    pub color: ColorReport,
    /// How its real-time playback behaves.
    pub playback: PlaybackReport,
    /// Host-specific video decode description, or `null` when the browser page decodes.
    pub decode: serde_json::Value,
    /// Containers, codecs and encoders; `null` when only known at export.
    pub encode: serde_json::Value,
    /// Named features, each available or not, with why not.
    pub features: BTreeMap<String, Feature>,
    /// Host-specific adapter choice under `ALPHA2_GPU_ADAPTER` and what each candidate supports;
    /// `null` where no choice was made (browser, CPU renderer).
    pub adapters: serde_json::Value,
    /// Host-specific machine and process memory; `null` where not read (browser).
    pub memory: serde_json::Value,
}

/// The GPU the renderer drives.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuReport {
    /// The adapter's name.
    pub adapter: String,
    /// The graphics backend.
    pub backend: String,
    /// The driver's name.
    pub driver: String,
    /// The driver's version and details, where the platform gives them.
    pub driver_info: String,
    /// The DX12 shader compiler (`FXC`, `DXC <path>`, ...); empty elsewhere.
    pub shader_compiler: String,
    /// `a` (an adapter meeting all of WebGPU) or `b` (a downlevel one).
    pub tier: &'static str,
    /// Intermediate texture format, independent of tier: `rgba16float` where the adapter
    /// renders, filters and copies it, else `rgba8unorm`.
    pub intermediate_format: &'static str,
    /// The largest 2D texture side.
    pub max_texture_dimension_2d: u32,
    /// Whether passes are timed on the GPU.
    pub timestamp_queries: bool,
    /// The idle-texture pool's byte budget.
    pub texture_budget_bytes: u64,
    /// The adapter's device memory.
    pub memory: GpuMemoryReport,
}

/// The device memory the adapter may use.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuMemoryReport {
    /// The budget the graphics API reports (DXGI local segment, Vulkan `VK_EXT_memory_budget`
    /// device-local heaps, Metal recommended working set), or `None`. Idle pools use a quarter.
    pub budget_bytes: Option<u64>,
}

/// How color is handled.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorReport {
    /// The working space: display-referred linear BT.709, 1.0 at SDR white.
    pub working_space: &'static str,
    /// Whether PQ and HLG sources are tone-mapped into it.
    pub hdr_sources: bool,
    /// Whether output can be HDR (always false; exports are SDR BT.709).
    pub hdr_output: bool,
    /// Whether linear float sources (`OpenEXR`) keep values beyond SDR until compositing, which
    /// needs a half-float intermediate.
    pub extended_range_sources: bool,
}

/// How real-time playback behaves.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackReport {
    /// The policies it runs.
    pub policies: Vec<PlaybackPolicy>,
    /// The one it runs unless asked otherwise.
    pub default_policy: PlaybackPolicy,
    /// Whether sound plays at rates other than 1x (always false).
    pub audio_at_non_unit_rate: bool,
}

/// One feature: whether it is available, and why not.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Feature {
    /// Whether the feature works here.
    pub available: bool,
    /// Why not, when it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Feature {
    /// An available feature.
    #[must_use]
    pub const fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    /// A feature that is not available, and why.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

impl CapabilityReport {
    /// The report of `renderer` on `runtime`, with decode, encode and features left to the host.
    #[must_use]
    pub fn new(runtime: Runtime, renderer: &Renderer) -> Self {
        let capabilities = renderer.gpu_capabilities();
        let half = matches!(
            capabilities.intermediate_format,
            alpha2_gpu::IntermediateFormat::Rgba16Float
        );
        Self {
            schema: CAPABILITY_SCHEMA,
            version: CAPABILITY_VERSION,
            runtime,
            gpu: GpuReport {
                adapter: capabilities.adapter_name.clone(),
                backend: capabilities.backend.clone(),
                driver: capabilities.driver.clone(),
                driver_info: capabilities.driver_info.clone(),
                shader_compiler: capabilities.shader_compiler.clone(),
                tier: match capabilities.tier {
                    CapabilityTier::TierA => "a",
                    CapabilityTier::TierB => "b",
                    _ => "unknown",
                },
                intermediate_format: if half { "rgba16float" } else { "rgba8unorm" },
                max_texture_dimension_2d: capabilities.max_texture_dimension_2d,
                timestamp_queries: capabilities.timestamp_queries,
                texture_budget_bytes: renderer.diagnostics().textures.budget_bytes,
                memory: GpuMemoryReport {
                    budget_bytes: renderer.gpu_memory_budget(),
                },
            },
            color: ColorReport {
                working_space: "linear_bt709_sdr",
                hdr_sources: true,
                hdr_output: false,
                extended_range_sources: half,
            },
            playback: PlaybackReport {
                policies: PlaybackPolicy::ALL.to_vec(),
                default_policy: PlaybackPolicy::default(),
                audio_at_non_unit_rate: false,
            },
            decode: serde_json::Value::Null,
            encode: serde_json::Value::Null,
            features: BTreeMap::new(),
            adapters: serde_json::Value::Null,
            memory: serde_json::Value::Null,
        }
    }

    /// The report for the CPU renderer on `runtime`: tier A with a half-float intermediate.
    #[must_use]
    pub fn cpu(runtime: Runtime, renderer: &alpha2_render::CpuRenderer) -> Self {
        Self {
            schema: CAPABILITY_SCHEMA,
            version: CAPABILITY_VERSION,
            runtime,
            gpu: GpuReport {
                adapter: format!("alpha2 CPU renderer ({} threads)", renderer.threads()),
                backend: "cpu".to_owned(),
                driver: "alpha2".to_owned(),
                driver_info: env!("CARGO_PKG_VERSION").to_owned(),
                shader_compiler: String::new(),
                tier: "a",
                intermediate_format: "rgba16float",
                max_texture_dimension_2d: alpha2_render::RenderSize::MAX_DIMENSION,
                timestamp_queries: false,
                texture_budget_bytes: 0,
                memory: GpuMemoryReport::default(),
            },
            color: ColorReport {
                working_space: "linear_bt709_sdr",
                hdr_sources: true,
                hdr_output: false,
                extended_range_sources: true,
            },
            playback: PlaybackReport {
                policies: PlaybackPolicy::ALL.to_vec(),
                default_policy: PlaybackPolicy::default(),
                audio_at_non_unit_rate: false,
            },
            decode: serde_json::Value::Null,
            encode: serde_json::Value::Null,
            features: BTreeMap::new(),
            adapters: serde_json::Value::Null,
            memory: serde_json::Value::Null,
        }
    }

    /// Records a feature.
    #[must_use]
    pub fn with_feature(mut self, name: &str, feature: Feature) -> Self {
        self.features.insert(name.to_owned(), feature);
        self
    }
}
