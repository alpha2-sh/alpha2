//! Stable, host-readable renderer diagnostics.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alpha2_gpu::{Capabilities, CapabilityTier};

use crate::{
    PipelineCacheStats, ReadbackBufferPoolStats, TexturePoolStats, UniformBufferPoolStats,
    UploadTexturePoolStats,
};

/// Source of one pass duration.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TimingSource {
    /// Hardware timestamp query resolved by wgpu.
    GpuTimestamp,
    /// Wall-clock time spent encoding one pass on the CPU.
    CpuEncoding,
}

/// Most recent timing for one named render pass.
///
/// Pass timing is opt-in; see [`crate::Renderer::set_pass_timing_enabled`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PassTiming {
    /// Stable pass label.
    pub label: &'static str,
    /// Duration in nanoseconds.
    pub nanoseconds: u64,
    /// Measurement source.
    pub source: TimingSource,
}

/// Adapter metadata recorded beside reproducible benchmark samples.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterDiagnostics {
    /// Platform adapter name.
    pub name: String,
    /// Selected graphics backend.
    pub backend: String,
    /// Driver name.
    pub driver: String,
    /// Driver version/details.
    pub driver_info: String,
    /// Capability tier driving deterministic degradation.
    pub tier: CapabilityTier,
    /// Whether hardware timestamps are active.
    pub timestamp_queries: bool,
}

/// Snapshot of render performance, cache, memory, and scheduling behavior.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderDiagnostics {
    /// Reproducibility metadata.
    pub adapter: AdapterDiagnostics,
    /// Latest bounded pass timing history. Empty unless pass timing is enabled.
    pub passes: Vec<PassTiming>,
    /// Intermediate texture allocation/reuse counters.
    pub textures: TexturePoolStats,
    /// Source-upload texture allocation/reuse and live-byte counters.
    pub upload_textures: UploadTexturePoolStats,
    /// Mapped readback-buffer allocation/reuse and live-byte counters.
    pub readback_buffers: ReadbackBufferPoolStats,
    /// Submission uniform-arena allocation/reuse and live-byte counters.
    pub uniform_buffers: UniformBufferPoolStats,
    /// Runtime and built-in pipeline cache counts.
    pub pipelines: PipelineCacheStats,
    /// Kept graph results: budget, occupancy, hits, misses, invalidations.
    pub result_cache: crate::ResultCacheStats,
    /// Frames presented by the caller.
    pub presented_frames: u64,
    /// Requests that could not render a new frame.
    pub dropped_frames: u64,
    /// Presentations that held a previous frame.
    pub repeated_frames: u64,
    /// Accumulated host decoder wait time.
    pub decoder_wait: Duration,
    /// Active maximum filter samples: what the passes take, which may be
    /// less than the caller asked for (see [`Self::degradations`]).
    pub max_filter_samples: u32,
    /// Active temporal subframe count.
    pub motion_blur_subframes: u32,
    /// Fallbacks applied to what the caller asked for: a filter sample
    /// budget reduced to what the passes take.
    pub degradations: Vec<crate::CapabilityDiagnostic>,
    /// Non-source nodes encoded by completed graph executions.
    pub graph_nodes: u64,
    /// Actual render passes encoded by completed graph executions.
    pub graph_passes: u64,
    /// Queue submissions issued by completed graph executions.
    pub graph_submissions: u64,
    /// Queue submissions issued by source presentation/conversion.
    pub source_submissions: u64,
    /// Source passes that converted a decoded picture into the working
    /// color space: YUV to RGB, or an RGBA presentation.
    pub source_conversions: u64,
    /// Decoded 4:2:0 pictures copied plane for plane into NV12 targets, with
    /// no color conversion and no graph.
    pub yuv_repacks: u64,
    /// Queue submissions issued by output packing and readback.
    pub readback_submissions: u64,
    /// Queue submissions combining graph execution with output readback.
    pub frame_submissions: u64,
    /// Actual render passes across graph, source, and readback work.
    pub render_passes: u64,
    /// GPU copy commands encoded for output readback.
    pub copy_operations: u64,
    /// Logical texel bytes copied by output readback commands.
    pub copy_bytes: u64,
}

/// Reproducible metadata envelope written beside benchmark measurements.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchmarkMetadata {
    /// Caller-defined project or fixture identity.
    pub project: String,
    /// Version of the alpha2 render crate.
    pub alpha2_version: &'static str,
    /// Adapter and driver identity.
    pub adapter: AdapterDiagnostics,
    /// Output width and height.
    pub dimensions: (u32, u32),
    /// Output scale used by the caller.
    pub resolution_scale: f32,
    /// Filter sample budget.
    pub max_filter_samples: u32,
    /// Temporal sample count.
    pub motion_blur_subframes: u32,
    /// Texture byte budget.
    pub texture_budget_bytes: u64,
}

/// Entries retained while pass timing is enabled.
const PASS_HISTORY: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct DiagnosticsState {
    passes: VecDeque<PassTiming>,
    /// Which run of pass timing a record belongs to: advanced each time
    /// timing is turned off, so a GPU timestamp read that lands later is
    /// dropped instead of recorded into a history timing no longer keeps.
    timing_epoch: u64,
    presented_frames: u64,
    dropped_frames: u64,
    repeated_frames: u64,
    decoder_wait: Duration,
    graph_nodes: u64,
    graph_passes: u64,
    graph_submissions: u64,
    source_submissions: u64,
    source_conversions: u64,
    yuv_repacks: u64,
    readback_submissions: u64,
    frame_submissions: u64,
    render_passes: u64,
    copy_operations: u64,
    copy_bytes: u64,
}

pub(crate) type SharedDiagnostics = Arc<Mutex<DiagnosticsState>>;

#[derive(Copy, Clone)]
pub(crate) struct ResourceDiagnostics {
    pub(crate) textures: TexturePoolStats,
    pub(crate) upload_textures: UploadTexturePoolStats,
    pub(crate) readback_buffers: ReadbackBufferPoolStats,
    pub(crate) uniform_buffers: UniformBufferPoolStats,
    pub(crate) pipelines: PipelineCacheStats,
    pub(crate) result_cache: crate::ResultCacheStats,
}

/// The run of pass timing a pass encoded now belongs to (see
/// [`crate::Renderer::pass_timing_epoch`]).
pub(crate) fn timing_epoch(state: &SharedDiagnostics) -> u64 {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .timing_epoch
}

/// Records one pass duration, unless timing was turned off since the pass
/// was encoded in timing run `epoch`. Callers take the epoch only while the
/// renderer's opt-in flag is on, so this is never reached on the default
/// path.
pub(crate) fn record_pass(
    state: &SharedDiagnostics,
    epoch: u64,
    label: &'static str,
    duration: Duration,
    source: TimingSource,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.timing_epoch != epoch {
        return;
    }
    if state.passes.len() == PASS_HISTORY {
        state.passes.pop_front();
    }
    state.passes.push_back(PassTiming {
        label,
        nanoseconds: u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        source,
    });
}

pub(crate) fn record_graph_work(
    state: &SharedDiagnostics,
    nodes: u64,
    render_passes: u64,
    standalone_submission: bool,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.graph_nodes = state.graph_nodes.saturating_add(nodes);
    state.graph_passes = state.graph_passes.saturating_add(render_passes);
    if standalone_submission {
        state.graph_submissions = state.graph_submissions.saturating_add(1);
    }
    state.render_passes = state.render_passes.saturating_add(render_passes);
}

pub(crate) fn record_source_work(
    state: &SharedDiagnostics,
    render_passes: u64,
    standalone_submission: bool,
    conversion: bool,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.source_conversions = state
        .source_conversions
        .saturating_add(u64::from(conversion));
    if standalone_submission {
        state.source_submissions = state.source_submissions.saturating_add(1);
    }
    state.render_passes = state.render_passes.saturating_add(render_passes);
}

pub(crate) fn record_readback_work(
    state: &SharedDiagnostics,
    render_passes: u64,
    copy_operations: u64,
    copy_bytes: u64,
    standalone_submission: bool,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if standalone_submission {
        state.readback_submissions = state.readback_submissions.saturating_add(1);
    }
    state.render_passes = state.render_passes.saturating_add(render_passes);
    state.copy_operations = state.copy_operations.saturating_add(copy_operations);
    state.copy_bytes = state.copy_bytes.saturating_add(copy_bytes);
}

pub(crate) fn record_frame_submission(state: &SharedDiagnostics) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.frame_submissions = state.frame_submissions.saturating_add(1);
}

/// Drops retained pass history when timing is turned off, and ends the
/// timing run, so records of passes encoded before arrive to nothing.
pub(crate) fn end_timing_run(state: &SharedDiagnostics) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.passes.clear();
    state.timing_epoch = state.timing_epoch.wrapping_add(1);
}

pub(crate) fn snapshot(
    state: &SharedDiagnostics,
    capabilities: &Capabilities,
    resources: &ResourceDiagnostics,
    max_filter_samples: u32,
    motion_blur_subframes: u32,
    degradations: Vec<crate::CapabilityDiagnostic>,
) -> RenderDiagnostics {
    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    RenderDiagnostics {
        adapter: AdapterDiagnostics {
            name: capabilities.adapter_name.clone(),
            backend: capabilities.backend.clone(),
            driver: capabilities.driver.clone(),
            driver_info: capabilities.driver_info.clone(),
            tier: capabilities.tier,
            timestamp_queries: capabilities.timestamp_queries,
        },
        passes: state.passes.iter().copied().collect(),
        textures: resources.textures,
        upload_textures: resources.upload_textures,
        readback_buffers: resources.readback_buffers,
        uniform_buffers: resources.uniform_buffers,
        pipelines: resources.pipelines,
        result_cache: resources.result_cache,
        presented_frames: state.presented_frames,
        dropped_frames: state.dropped_frames,
        repeated_frames: state.repeated_frames,
        decoder_wait: state.decoder_wait,
        max_filter_samples,
        motion_blur_subframes,
        degradations,
        graph_nodes: state.graph_nodes,
        graph_passes: state.graph_passes,
        graph_submissions: state.graph_submissions,
        source_submissions: state.source_submissions,
        source_conversions: state.source_conversions,
        yuv_repacks: state.yuv_repacks,
        readback_submissions: state.readback_submissions,
        frame_submissions: state.frame_submissions,
        render_passes: state.render_passes,
        copy_operations: state.copy_operations,
        copy_bytes: state.copy_bytes,
    }
}

pub(crate) fn record_yuv_repack(state: &SharedDiagnostics) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.yuv_repacks = state.yuv_repacks.saturating_add(1);
}

pub(crate) fn record_frame(state: &SharedDiagnostics, presented: bool, repeated: bool) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.presented_frames += u64::from(presented);
    state.dropped_frames += u64::from(!presented);
    state.repeated_frames += u64::from(repeated);
}

pub(crate) fn record_decoder_wait(state: &SharedDiagnostics, duration: Duration) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.decoder_wait = state.decoder_wait.saturating_add(duration);
}
