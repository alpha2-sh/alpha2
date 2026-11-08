//! Public Rust facade for the alpha2 media engine.
//!
//! This crate re-exports the platform-neutral project, timeline, rendering, playback and export
//! APIs. Host-specific media I/O lives in `alpha2-native` or `alpha2-wasm`.
//!
//! The default API names no `wgpu` type. Open a device with [`GpuContext::request_product`],
//! supply source frames as [`SourceView`]s through a [`FrameProvider`] (or upload them with
//! [`Renderer::upload_rgba8`]), and receive [`RenderedTexture`]s and readbacks from [`Player`]
//! and [`Exporter`].
//!
//! # `wgpu` interop
//!
//! The `wgpu-interop` feature exposes the items that adopt or hand out `wgpu` handles and
//! re-exports `wgpu` as `alpha2::gpu::wgpu`. That surface follows `wgpu`'s major version; the
//! rest of the API does not.

/// Audio model and deterministic DSP primitives.
pub use alpha2_audio as audio;
/// Playback, evaluation, and export orchestration.
pub use alpha2_engine as engine;
/// Deterministic vector geometry and coverage for shapes.
pub use alpha2_geometry as geometry;
/// GPU adapter and device capability boundary.
pub use alpha2_gpu as gpu;
/// Host-independent media vocabulary and decode/encode contracts.
pub use alpha2_media as media;
/// Versioned project document types.
pub use alpha2_model as model;
/// Renderer, effects, and render-graph APIs.
pub use alpha2_render as render;
/// Exact media-time and frame-rate primitives.
pub use alpha2_time as time;

pub use alpha2_engine::extract;
pub use alpha2_engine::{
    EngineProject, ExportStep, Exporter, FrameProvider, FrameRenderer, Player, QualityProfile,
};
pub use alpha2_gpu::{
    GpuAcquisitionStep, GpuContext, GpuDiagnostic, GpuDiagnosticHandler, GpuDiagnosticLog,
    GpuDiagnosticSnapshot, GpuError, GpuErrorKind,
};
pub use alpha2_model::{CURRENT_SCHEMA_VERSION, Project};
pub use alpha2_render::{RenderSize, RenderedTexture, Renderer, SourceView};
pub use alpha2_time::{FrameRate, MediaTime, TimeRange};
