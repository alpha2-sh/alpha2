//! GPU compositing public API.
//!
//! # `wgpu` interop
//!
//! By default the API names no `wgpu` type: sources enter as [`SourceView`]s and results leave
//! as [`RenderedTexture`]s and readbacks. Items taking or returning `wgpu` handles are
//! documented only with the `wgpu-interop` feature, track `wgpu`'s major version, and are
//! otherwise hidden and not part of this crate's API.

mod backend;
mod blend;
mod builtins;
mod call_budget;
mod canary;
mod catalog;
mod color;
mod composite;
mod cpu;
mod diagnostics;
mod effect;
mod effect_cost;
mod effect_resource;
mod error;
mod frame_gpu_time;
pub use frame_gpu_time::FrameGpuTimes;
pub use present::SurfacePresenter;
mod gaussian_blur;
mod generator;
mod gpu_errors;
mod graph;
mod graph_cache;
mod graph_execute;
mod graph_plan;
mod graph_submit;
pub mod hdr;
mod layout_pack;
mod loop_bound;
mod masking;
mod paint_wgsl;
mod pipeline;
mod pool;
mod present;
mod quad;
mod readback;
mod registry;
pub use registry::{CompiledEffect, MAX_EFFECT_INPUTS, PendingEffect};
mod renderer;
mod repack;
mod result_cache;
mod retire;
mod robustness;
mod runtime_effect;
mod settle;
mod shader_inventory;
mod submission_clock;
mod submission_planner;
pub use submission_planner::{FrameCost, FrameSubmissions};
mod target;
#[cfg(test)]
mod tests;
mod uniform;
mod vector;
mod yuv;

/// Shared ownership of the renderer's pools. Not atomic on wasm, where wgpu objects are neither
/// `Send` nor `Sync`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Shared<T> = std::sync::Arc<T>;
#[cfg(target_arch = "wasm32")]
pub(crate) type Shared<T> = std::rc::Rc<T>;

pub use backend::{EffectCatalog, RenderControls};
pub use blend::{BlendMode, BlendSpace};
pub use builtins::{
    CHROMA_KEY_ID, GAUSSIAN_BLUR_ID, GLOW_ID, LUT_3D_ID, builtin_effect_definitions,
    builtin_transition_definitions, chroma_key_definition, gaussian_blur_definition,
    glow_definition, lut_3d_definition,
};
pub use catalog::{builtin_catalog, builtin_catalog_document, builtin_catalog_json};
pub use color::{
    Color, ColorDiagnostic, ColorError, ColorPolicy, SdrOutputTransfer, YuvSample,
    convert_yuv_to_linear_rgba, encode_linear_rgb, is_hdr,
};
pub use composite::{CompositeLayer, LayerTransform};
pub use cpu::{
    CpuImage, CpuRenderStats, CpuRenderer, CpuSource, RgbaPixels, YuvPlanes, pack_cpu_image,
};
pub use diagnostics::{
    AdapterDiagnostics, BenchmarkMetadata, GpuSubmissionStats, PassTiming, PipelineCreation,
    PipelineCreationStats, RenderDiagnostics, TimingSource,
};
pub use effect::{
    DiagnosticSeverity, EffectDef, EffectId, EffectKind, EffectParams, EffectResourceKind,
    EffectResources, EffectRuntime, InputSpec, ParamSpec, ParamType, ParamUnit, ParamValue,
    ResourceSpec, ShaderDiagnostic, ShaderDiagnostics, UniformField, UniformLayout,
};
pub use effect_resource::{Lut3dData, LutParseError};
pub use error::{
    DEFAULT_GPU_WAIT_TIMEOUT, GPU_DEVICE_LOST_MESSAGE, GPU_TIMEOUT_MESSAGE,
    GPU_WORK_DROPPED_MESSAGE, ReadbackFailure, RenderError,
};
pub use gaussian_blur::blur_sigma_pixels;
pub use generator::{
    FILL_PAINT_PARAMS, GENERATOR_EFFECT_ID, MAX_GENERATOR_STOPS, PAINT_KINDS, PAINT_SPREADS,
    PaintParams, generator_definition,
};
pub use gpu_errors::GpuErrorReports;
pub use graph::{
    EffectInputs, GraphError, GraphLayer, RenderCacheKey, RenderGraph, RenderNodeId, RenderNodeKind,
};
pub use graph_plan::RenderPlan;
pub use graph_submit::FrameSubmitTimings;
pub use masking::{
    MASK_COMBINE_EFFECT_IDS, MASK_COMBINE_NAMES, MASK_MODES, MAX_SHAPE_MASKS, MaskCombineNames,
    TRACK_MATTE_EFFECT_ID, mask_combine_definition, mask_combine_definitions,
    track_matte_definition,
};
pub use pipeline::{PipelineCacheStats, WarmupReport};
pub use pool::{ReadbackBufferPoolStats, TexturePoolStats, UploadTexture, UploadTexturePoolStats};
pub use readback::{
    EncodedNv12Pack, Nv12PlaneTargets, PlaneTarget, Rgba8Readback, StagingPlane, Yuv420pPlanes,
    Yuv420pReadback, Yuv420pTargets, yuv420p_staging_bytes,
};
pub use renderer::{LossClassifier, RenderQualityControls, Renderer};
pub use repack::RepackSource;
pub use result_cache::{CacheEligibility, CacheIntent, CacheInvalidation, ResultCacheStats};
pub use robustness::{
    CapabilityDiagnostic, DegradationDecision, ResourceBudget, filter_sample_limit,
    resolve_degradation,
};
pub use shader_inventory::{builtin_wgsl_modules, effect_wgsl};
pub use submission_clock::{FrameMark, SubmissionReport};
pub use target::{RenderSize, RenderedTexture, SourceView};
pub use uniform::UniformBufferPoolStats;
pub use yuv::{
    ExternalTextureLease, GpuVideoPlanes, PendingSourceWork, PreparedSourceTexture, SourceAlpha,
    SourcePlane, SourcePresentation, Yuv420Frame, YuvFrame,
};
