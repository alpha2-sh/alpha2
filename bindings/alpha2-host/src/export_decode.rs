//! How an export decodes its sources, whichever output path it takes.
//!
//! The readback and GPU-resident paths differ in what happens after a frame
//! is rendered, not in how its sources arrive. Both size the decode pool and
//! the codec thread split from the same facts (whether this host may decode
//! on the GPU, whether the encoder competes for the CPU), and both import
//! hardware surfaces through the same frame supply. Deciding that once keeps
//! the GPU output path from assuming hardware decode, so a software-decoded
//! export can still encode from the GPU.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{
    configured_decode_concurrency, configured_decode_lookahead, configured_decoder_sessions,
    configured_reverse_hardware_surfaces, visual_decode_parallelism,
};
use crate::decode::DecodeWorkerConfig;
use crate::frames::NativeGpuFrames;
use crate::host::Host;
use alpha2_model::Project;
use alpha2_native::{CodecThreadAllocation, CodecThreadRequest, codec_thread_allocation};
use alpha2_render::RenderSize;
use alpha2_time::TimeRange;

/// The decode half of one export.
pub struct ExportDecodePlan {
    /// Whether video sources may decode on the GPU for this export: never
    /// on the synchronous comparison path.
    pub hardware_decode: bool,
    pub decode_lookahead: usize,
    pub decoder_sessions: usize,
    pub codec_threads: CodecThreadAllocation,
    /// Luma samples in the largest video source, which sizes a software
    /// decoder that shares the CPU with a software encoder; zero otherwise.
    pub source_frame_pixels: u64,
    /// Time spent reading source stream parameters for that size.
    pub source_probe: Duration,
    /// Sources an extension provider draws.
    pub providers: Arc<crate::providers::ProviderMedia>,
}

impl ExportDecodePlan {
    /// The plan for a codec split already decided, which the readback path
    /// decides once its encoder has opened.
    pub(crate) fn new(
        split: ExportCodecSplit,
        providers: Arc<crate::providers::ProviderMedia>,
    ) -> Self {
        Self {
            hardware_decode: split.hardware_decode,
            decode_lookahead: split.decode_lookahead,
            decoder_sessions: split.decoder_sessions,
            codec_threads: split.codec_threads,
            source_frame_pixels: split.source_frame_pixels,
            source_probe: split.source_probe,
            providers,
        }
    }
}

/// The thread half of an [`ExportDecodePlan`]: what the split between decode
/// and the video encoder is, before the decode pool exists.
#[derive(Clone, Copy, Debug)]
pub struct ExportCodecSplit {
    pub hardware_decode: bool,
    pub decode_lookahead: usize,
    pub decoder_sessions: usize,
    pub codec_threads: CodecThreadAllocation,
    pub source_frame_pixels: u64,
    pub source_probe: Duration,
}

impl Host {
    /// Whether this host decodes video on the GPU: it has a shared device and
    /// its decode policy allows using it. A shared device alone does not say
    /// so: a software-decode host keeps one for the GPU-resident export.
    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(clippy::unused_self)
    )]
    pub(crate) fn hardware_decode_ready(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        return self.decode_vulkan().is_some();
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        return self.decode_metal().is_some();
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        return self.decode_d3d12().is_some();
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        false
    }

    /// The video codecs this host decodes through a hardware session, for
    /// conform to tell a stream only hardware decodes from one it cannot read.
    pub(crate) fn hardware_video_decode(&self) -> crate::assets::HardwareVideoDecode {
        use crate::assets::HardwareVideoDecode;
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        return self
            .decode_vulkan()
            .map_or(HardwareVideoDecode::None, |context| {
                HardwareVideoDecode::Codecs(context.decode_codecs().to_vec())
            });
        #[cfg(not(all(target_os = "linux", feature = "hwdecode")))]
        if self.hardware_decode_ready() {
            HardwareVideoDecode::PerStream
        } else {
            HardwareVideoDecode::None
        }
    }

    /// The shared `Metal` device, when this host may decode on it.
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    pub(crate) fn decode_metal(&self) -> Option<alpha2_native_gpu::SharedMetalContext> {
        self.shared_metal
            .clone()
            .filter(|_| self.decode_policy.allows_hardware())
    }

    /// The shared D3D12 device, when this host may decode on it: the policy
    /// allows it, and `wgpu` shows NV12 planes there. A device made for the
    /// GPU-resident export alone exists under `software` too, and must not
    /// turn hardware decode on.
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    pub(crate) fn decode_d3d12(&self) -> Option<alpha2_native_gpu::SharedD3D12Context> {
        self.shared_d3d12
            .clone()
            .filter(|context| self.decode_policy.allows_hardware() && context.decode_planes())
    }

    /// The shared Vulkan device, when this host may decode on it: the policy
    /// allows it, and nothing keeps `FFmpeg` from opening a video session
    /// there, which the host reported when it started.
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    pub(crate) fn decode_vulkan(&self) -> Option<alpha2_native_gpu::SharedVulkanContext> {
        self.shared_vulkan.clone().filter(|context| {
            self.decode_policy.allows_hardware() && context.video_session_hazard().is_none()
        })
    }

    /// The extension-provider sources an export draws, watched by the
    /// failure report.
    pub(crate) fn export_providers(
        &self,
        project: &Project,
        size: RenderSize,
    ) -> Arc<crate::providers::ProviderMedia> {
        let providers = Arc::new(crate::providers::ProviderMedia::new(
            project,
            self.provider_files.clone(),
            (size.width, size.height),
            self.memory.provider_sources.bytes,
        ));
        self.failure.watch_providers(Arc::clone(&providers));
        providers
    }

    /// How an export divides its software codec workers between decode and
    /// a video encoder that does (`software_video_encoder`) or does not run on
    /// the CPU: the encoder that opened, which the readback path knows only
    /// once one has.
    pub(crate) fn export_codec_split(
        &self,
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        range: TimeRange,
        pipeline_enabled: bool,
        software_video_encoder: bool,
    ) -> ExportCodecSplit {
        // The synchronous comparison path (the pipeline off) supplies frames from software
        // decoders on the coordinator, whatever the host may decode on.
        let hardware_decode = pipeline_enabled && self.hardware_decode_ready();
        let decode_lookahead = configured_decode_lookahead(pipeline_enabled);
        let decoder_sessions = configured_decoder_sessions();
        let workload_parallelism = visual_decode_parallelism(project, range);
        // Frame area only sizes a software decode session that shares the budget with a
        // software encoder, so no other run pays to read it; a conformed export already read it.
        let probe_started = Instant::now();
        let source_frame_pixels = if hardware_decode || !software_video_encoder {
            0
        } else {
            crate::host::max_video_source_pixels(
                project,
                assets,
                self.conformed_video_pixels.as_ref(),
            )
        };
        let source_probe = probe_started.elapsed();
        let codec_threads = codec_thread_allocation(CodecThreadRequest {
            decode_concurrency: configured_decode_concurrency(
                pipeline_enabled,
                hardware_decode,
                workload_parallelism,
            )
            .min(decoder_sessions),
            hardware_decode,
            source_frame_pixels,
            software_video_encoder,
        });
        ExportCodecSplit {
            hardware_decode,
            decode_lookahead,
            decoder_sessions,
            codec_threads,
            source_frame_pixels,
            source_probe,
        }
    }

    /// The frame supply that installs decoded sources for the renderer:
    /// hardware surfaces import on the GPU when this host decodes there.
    pub(crate) fn export_gpu_frames(&self, size: RenderSize, project: &Project) -> NativeGpuFrames {
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        let mut gpu_frames = NativeGpuFrames::with_vulkan(size, self.decode_vulkan());
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        let mut gpu_frames = NativeGpuFrames::with_metal(size, self.decode_metal());
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        let mut gpu_frames = NativeGpuFrames::with_d3d12(size, self.decode_d3d12());
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        let mut gpu_frames = NativeGpuFrames::new(size);
        gpu_frames.import_fallback = !self.decode_policy.requires_hardware();
        gpu_frames.color = alpha2_engine::color_policy(&project.settings.color);
        #[cfg(test)]
        if let Some(fault) = crate::frames::INJECT_SURFACE_FAULT.with(std::cell::Cell::take) {
            gpu_frames.inject(fault);
        }
        gpu_frames
    }

    /// The decode pool's configuration for `plan`.
    pub(crate) fn decode_worker_config(&self, plan: &ExportDecodePlan) -> DecodeWorkerConfig {
        DecodeWorkerConfig {
            providers: Arc::clone(&plan.providers),
            decoding: self.failure.decoding(),
            depth: plan.decode_lookahead,
            byte_budget: self.memory.decode_queue.byte_budget(),
            concurrency: plan.codec_threads.decode_concurrency,
            decoder_threads: plan.codec_threads.decoder_threads,
            hardware_decoder_threads: plan.codec_threads.hardware_decoder_threads,
            max_sessions: plan.decoder_sessions,
            reverse_cache_bytes: self.memory.reverse_cache.bytes,
            reverse_hardware_surfaces: configured_reverse_hardware_surfaces(self.decode_policy),
            stall_timeout: crate::config::configured_stall_timeout(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            shared_vulkan: self.decode_vulkan(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            shared_metal: self.decode_metal(),
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            shared_d3d12: self.decode_d3d12(),
            #[cfg(all(
                any(target_os = "linux", target_os = "macos", target_os = "windows"),
                feature = "hwdecode"
            ))]
            hardware_required: self.decode_policy.requires_hardware(),
            #[cfg(test)]
            stall: None,
        }
    }
}
