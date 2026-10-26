//! Player and exporter public API.
//!
//! # `wgpu` interop
//!
//! The API names no `wgpu` type by default: a host lends source frames as
//! [`alpha2_render::SourceView`]s through [`FrameProvider`], and frames come
//! back as [`alpha2_render::RenderedTexture`]s and readbacks. Rendering
//! straight into a hardware encoder's NV12 planes takes `wgpu` textures and
//! is interop, documented with the `wgpu-interop` feature and following
//! `wgpu`'s major version. Without the feature those items are hidden.

mod audio;
mod capabilities;
mod downmix;
mod error;
mod error_code;
mod exporter;
mod frame;
mod generated;
mod layout;
mod passthrough;
mod playback;
mod player;
mod policy;
mod quality;
mod schedule;
mod snapshot;
mod text;

pub use audio::{
    AudioContinuity, AudioNeed, AudioRenderStatus, AudioRenderer, MAX_AUDIO_BLOCK,
    MAX_SETTLING_SECONDS,
};
pub use capabilities::{
    CAPABILITY_SCHEMA, CAPABILITY_VERSION, CapabilityReport, ColorReport, Feature, GpuReport,
    PlaybackReport, Runtime,
};
pub use error::EngineError;
pub use error_code::{ErrorCode, gpu_error_of, gpu_fault_of};
pub use exporter::{
    ExportAudioWindow, ExportFrameNeeds, ExportPlaneStep, ExportProgress, ExportStep,
    ExportStepTimings, ExportTextureStep, Exporter, MAX_EXPORT_FRAMES, export_audio_window,
};
pub use frame::{
    FrameGraph, FrameNv12Render, FrameProvider, FrameRender, FrameRenderer, FrameYuv420pRender,
    GpuFrameRenderer, RenderReport, check_frame_effects,
};
pub use layout::{LAYOUT_WRAP_SLACK, LayoutBounds, NodeBounds, PreparedLayout, measure_layout};
pub use passthrough::{PassthroughAudio, PassthroughPlan, PassthroughSegment, passthrough_plan};
pub use playback::{ClockStep, PlaybackClock, PlaybackPolicy};
pub use player::{PlaybackFrame, Player};
pub use policy::{MissingFramePolicy, color_policy};
pub use quality::QualityProfile;
pub use snapshot::EngineProject;
pub use text::{
    CanvasRect, FramesWithText, TextBounds, TextError, TextPixels, TextTexture, TextTextures,
    TextUpload, layout_key, measure_text, text_key,
};
