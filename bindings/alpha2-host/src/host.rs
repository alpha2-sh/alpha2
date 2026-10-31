//! The resident host that serves protocol operations.

use crate::renderer_backend::RendererBackend;
use alpha2_render::CpuRenderer;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::assets::{
    HardwareVideoDecode, ProxyRequest, RequestMedia, ResolvedMedia, blocking_summary, resolve_media,
};
use crate::audio::WindowedAudioSources;
use crate::capture::FrameCapture;
use crate::config::configured_frame_capture;
use crate::config::{
    ResolvedAudioRate, VideoDecodePolicy, codec_thread_policy, configured_cancel_at_frame,
    configured_decode_lookahead, configured_encode_channel_depth, configured_frame_depth,
    configured_pipeline_enabled, decoder_thread_policy, resolve_video_rate_control,
};
use crate::decode::{DecodeScheduler, DecodeWorker};
use crate::effect_request::EffectRequest;
use crate::encode::{
    EncodeCommand, EncodeProgressState, EncodeWorker, FramePool, NativeExportSink, PipelineFailure,
    PooledFrame, drain_encode_worker, encode_event_bytes, handle_encode_worker_event,
};
use crate::export_decode::ExportDecodePlan;
use crate::frames::{NativeFrames, packed_yuv420_bytes};
#[cfg(feature = "hwencode")]
use crate::gpu_export::GpuEncodeOutcome;
use crate::media::{PendingMedia, PendingVideoReadback, ready_pending_media, unpack_baked};
use crate::metrics::{
    ExportMetrics, VideoPipeline, export_metrics_json, export_pipeline_limits, frames_per_second,
};
use crate::protocol::LoudnessRequest;
use crate::protocol::{
    AudioQualityRequest, HostError, RateRequest, Request, ResolvedFormat, VideoQualityRequest,
    code_tag, codec_capabilities_json, decode_capabilities_json,
};
use crate::render_cache::BakedFrame;
use crate::request_inputs::RequestInputs;
use crate::trace::{AudioBlockTrace, ExportTrace, VideoFrameTrace};
use crate::{MAX_PENDING_MEDIA_EVENTS, WORKER_RETRY_INTERVAL, error_message};
use alpha2_assets::MediaPurpose;
use alpha2_audio::TransportState;
use alpha2_engine::{
    AudioRenderer, EngineProject, ExportStep, ExportStepTimings, Exporter, FrameProvider,
    FramesWithText, QualityProfile, TextTextures,
};
use alpha2_gpu::CapabilityTier;
use alpha2_media::{CancelToken, ColorMetadata, EncodeEvent};
use alpha2_model::{MediaFingerprint, MediaKind, Project};
use alpha2_native::{
    AvEncodeConfig, ExistingOutput, NativeAvEncodeSink, NativeVideoCodec, NativeVideoEncodeSink,
    OutputTarget, VideoEncodeConfig, codec_capabilities, ffmpeg_runtime_identity,
};
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
use alpha2_native_gpu::SharedD3D12Context;
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
use alpha2_native_gpu::SharedMetalContext;
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
use alpha2_native_gpu::SharedVulkanContext;
use alpha2_render::{EffectId, Lut3dData, RenderSize, Renderer, builtin_catalog};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use serde_json::{Value, json};

/// Renders the recorded fallback reasons for a strict-mode failure message.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn fallback_summary(metrics: &ExportMetrics) -> String {
    let reasons = metrics
        .hardware_fallbacks
        .entries()
        .map(|(reason, count)| format!("{reason}={count}"))
        .collect::<Vec<_>>();
    if reasons.is_empty() {
        "no fallback reason was recorded".to_owned()
    } else {
        reasons.join(", ")
    }
}

/// A runtime effect the last export request defined: its definition as the
/// request gave it, and why registering it failed, when it did.
pub struct RuntimeEffect {
    definition: String,
    rejected: Option<String>,
}

/// What a runtime effect's registration records on a host that renders on
/// the CPU: it is WGSL, which only a GPU runs.
const CPU_RUNTIME_EFFECT: &str = "the CPU renderer runs no runtime WGSL effects";

/// The resident host: one renderer on one device, answering capability,
/// conform, and export requests one at a time. Per-export state is set when
/// an export begins and cleared when it ends.
pub struct Host {
    pub renderer: crate::renderer_backend::RendererBackend,
    /// Why the host renders where it does, when that was a choice: `auto`
    /// found no hardware GPU, or the GPU host could not start.
    pub renderer_reason: Option<String>,
    /// The built-in effects, registered when the host started.
    pub builtin_effects: HashSet<(String, u32)>,
    /// The runtime effects of the export being run (or the last one): those
    /// its request defined, and no other. See [`Host::register_request_effects`].
    pub runtime_effects: HashMap<(String, u32), RuntimeEffect>,
    /// Decode mode this host was started with.
    pub decode_policy: VideoDecodePolicy,
    /// Why `auto` decode could not set up hardware decode at startup and
    /// runs in software, reported with every export.
    pub hardware_decode_unavailable: Option<String>,
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    pub shared_vulkan: Option<SharedVulkanContext>,
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    pub shared_metal: Option<SharedMetalContext>,
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    pub shared_d3d12: Option<SharedD3D12Context>,
    /// What the adapter choice at start leaves the GPU-resident export to
    /// say: why the renderer's adapter has no shared device, and which other
    /// adapter could encode.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    pub encode_adapters: crate::adapter_choice::EncodeAdapterNotes,
    /// The fonts text draws with, from the export request, and the captions
    /// and text generators drawn with them. Behind a lock because export
    /// stages borrow the host from several threads; only the render loop
    /// takes it.
    pub texts: std::sync::Mutex<TextTextures>,
    /// The files the export being run supplies to its extension sources:
    /// their own files, image sequences' frames, and the fonts.
    pub provider_files: crate::providers::ProviderFiles,
    /// Luma samples in each video source's stored frames, by source ID, as
    /// the conform of the export being run probed them; `None` for an export
    /// not conformed here, whose decode plan probes the files itself.
    pub conformed_video_pixels: Option<HashMap<String, u64>>,
    /// The render cache the export being run asked for, and its snapshot's
    /// identity.
    pub render_cache: Option<(crate::render_cache::RenderCacheRequest, [u8; 32])>,
    /// How the export being run may feed its encoder.
    pub input_policy: crate::config::VideoInputPolicy,
    /// Whether the export being run may copy its source's packets.
    pub strategy: crate::config::ExportStrategy,
    /// The device's latest diagnostics, which a failure report reads.
    pub gpu_log: std::sync::Arc<alpha2_gpu::GpuDiagnosticLog>,
    /// What the export being run is doing, for its failure report.
    pub failure: crate::failure::FailureContext,
    /// The byte budgets of the export being run.
    pub memory: crate::memory::MemoryPolicy,
    /// Blocks this host's drop forever when a test asks, as a drop on a
    /// device that never goes idle blocks.
    #[cfg(test)]
    _drop_stall: crate::teardown::injected::DropStall,
}

/// What an export request asked for, as its failure report records it.
struct ExportRequestFacts<'a> {
    project: &'a str,
    assets: &'a HashMap<String, PathBuf>,
    start_ticks: i64,
    end_ticks: i64,
    frame_rate: RateRequest,
    include_audio: bool,
    media: crate::protocol::ExportMedia,
    video_codec: NativeVideoCodec,
    video_encoder: Option<&'a str>,
    input_policy: crate::config::VideoInputPolicy,
}

/// A project's canvas size, read from its text: the rest of the document is
/// skipped, not parsed.
#[derive(serde::Deserialize)]
struct ProjectCanvas {
    settings: CanvasSize,
}

#[derive(serde::Deserialize)]
struct CanvasSize {
    width: u64,
    height: u64,
}

/// The device facts a render cache identity names: the adapter, its backend
/// and driver, and the DX12 shader compiler, since DXC and FXC compile one
/// pipeline to different bytes.
pub(crate) fn add_adapter_identity(
    identity: &mut crate::render_cache::SnapshotIdentity,
    adapter: &crate::renderer_backend::AdapterIdentity,
) {
    for (label, value) in [
        ("adapter", &adapter.name),
        ("backend", &adapter.backend),
        ("driver", &adapter.driver),
        ("driver-info", &adapter.driver_info),
        ("shader-compiler", &adapter.shader_compiler),
    ] {
        identity.add(label, value.as_bytes());
    }
}

impl Host {
    #[cfg(not(any(
        all(target_os = "linux", feature = "hwdevice"),
        all(target_os = "macos", feature = "hwdevice"),
        all(target_os = "windows", feature = "hwdevice")
    )))]
    pub fn new(
        renderer: Renderer,
        registered: HashSet<(String, u32)>,
        decode_policy: VideoDecodePolicy,
    ) -> Self {
        Self {
            renderer: crate::renderer_backend::RendererBackend::Gpu(Box::new(renderer)),
            renderer_reason: None,
            builtin_effects: registered,
            runtime_effects: HashMap::new(),
            decode_policy,
            hardware_decode_unavailable: None,
            #[cfg(all(target_os = "linux", feature = "hwdevice"))]
            shared_vulkan: None,
            #[cfg(all(target_os = "macos", feature = "hwdevice"))]
            shared_metal: None,
            #[cfg(all(target_os = "windows", feature = "hwdevice"))]
            shared_d3d12: None,
            texts: std::sync::Mutex::new(TextTextures::new()),
            provider_files: crate::providers::ProviderFiles::default(),
            conformed_video_pixels: None,
            render_cache: None,
            input_policy: crate::config::VideoInputPolicy::Auto,
            strategy: crate::config::ExportStrategy::Render,
            gpu_log: std::sync::Arc::default(),
            failure: crate::failure::FailureContext::default(),
            memory: crate::memory::MemoryPolicy::default(),
            #[cfg(test)]
            _drop_stall: crate::teardown::injected::DropStall,
        }
    }

    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    pub fn new_with_metal(
        renderer: Renderer,
        registered: HashSet<(String, u32)>,
        shared_metal: Option<SharedMetalContext>,
        decode_policy: VideoDecodePolicy,
    ) -> Self {
        Self {
            renderer: crate::renderer_backend::RendererBackend::Gpu(Box::new(renderer)),
            renderer_reason: None,
            builtin_effects: registered,
            runtime_effects: HashMap::new(),
            decode_policy,
            hardware_decode_unavailable: None,
            shared_metal,
            texts: std::sync::Mutex::new(TextTextures::new()),
            provider_files: crate::providers::ProviderFiles::default(),
            conformed_video_pixels: None,
            render_cache: None,
            input_policy: crate::config::VideoInputPolicy::Auto,
            strategy: crate::config::ExportStrategy::Render,
            gpu_log: std::sync::Arc::default(),
            failure: crate::failure::FailureContext::default(),
            memory: crate::memory::MemoryPolicy::default(),
            #[cfg(test)]
            _drop_stall: crate::teardown::injected::DropStall,
        }
    }

    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    pub fn new_with_d3d12(
        renderer: Renderer,
        registered: HashSet<(String, u32)>,
        shared_d3d12: Option<SharedD3D12Context>,
        decode_policy: VideoDecodePolicy,
    ) -> Self {
        Self {
            renderer: crate::renderer_backend::RendererBackend::Gpu(Box::new(renderer)),
            renderer_reason: None,
            builtin_effects: registered,
            runtime_effects: HashMap::new(),
            decode_policy,
            hardware_decode_unavailable: None,
            shared_d3d12,
            #[cfg(feature = "hwencode")]
            encode_adapters: crate::adapter_choice::EncodeAdapterNotes::default(),
            texts: std::sync::Mutex::new(TextTextures::new()),
            provider_files: crate::providers::ProviderFiles::default(),
            conformed_video_pixels: None,
            render_cache: None,
            input_policy: crate::config::VideoInputPolicy::Auto,
            strategy: crate::config::ExportStrategy::Render,
            gpu_log: std::sync::Arc::default(),
            failure: crate::failure::FailureContext::default(),
            memory: crate::memory::MemoryPolicy::default(),
            #[cfg(test)]
            _drop_stall: crate::teardown::injected::DropStall,
        }
    }

    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    pub fn new_with_vulkan(
        renderer: Renderer,
        registered: HashSet<(String, u32)>,
        shared_vulkan: Option<SharedVulkanContext>,
        decode_policy: VideoDecodePolicy,
    ) -> Self {
        Self {
            renderer: crate::renderer_backend::RendererBackend::Gpu(Box::new(renderer)),
            renderer_reason: None,
            builtin_effects: registered,
            runtime_effects: HashMap::new(),
            decode_policy,
            hardware_decode_unavailable: None,
            shared_vulkan,
            texts: std::sync::Mutex::new(TextTextures::new()),
            provider_files: crate::providers::ProviderFiles::default(),
            conformed_video_pixels: None,
            render_cache: None,
            input_policy: crate::config::VideoInputPolicy::Auto,
            strategy: crate::config::ExportStrategy::Render,
            gpu_log: std::sync::Arc::default(),
            failure: crate::failure::FailureContext::default(),
            memory: crate::memory::MemoryPolicy::default(),
            #[cfg(test)]
            _drop_stall: crate::teardown::injected::DropStall,
        }
    }

    /// Decode mode, backend, and hardware registration for this host.
    fn decode_capabilities(&self) -> Value {
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        let decode = self.decode_vulkan();
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        let (ready, ten_bit, device_codecs) = (
            decode.is_some(),
            decode
                .as_ref()
                .is_some_and(SharedVulkanContext::supports_ten_bit),
            decode.as_ref().map(SharedVulkanContext::decode_codecs),
        );
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        let decode = self.decode_metal();
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        let (ready, ten_bit, device_codecs) = (
            decode.is_some(),
            decode
                .as_ref()
                .is_some_and(SharedMetalContext::supports_ten_bit),
            // VideoToolbox reports what it can decode per stream, not as a device-wide codec list.
            None,
        );
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        let decode = self.decode_d3d12();
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        let (ready, ten_bit, device_codecs) = (
            decode.is_some(),
            decode
                .as_ref()
                .is_some_and(SharedD3D12Context::supports_ten_bit),
            // D3D12VA checks each stream's profile when its decoder opens.
            None,
        );
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        let (ready, ten_bit, device_codecs) = (false, false, None);
        let mut capabilities =
            decode_capabilities_json(self.decode_policy.as_str(), ready, ten_bit, device_codecs);
        capabilities["hardwareUnavailable"] = json!(self.hardware_decode_unavailable);
        capabilities
    }

    /// Describes the GPU-resident output path, which the encoder list cannot.
    ///
    /// `capabilities.encode.video` lists the encoders the software sinks can
    /// feed, and `h264_vulkan` is deliberately not in it: it takes Vulkan
    /// hardware frames and nothing else, and admitting it there would let a
    /// software export pick an encoder it cannot drive. This report is how a
    /// caller sees that the path exists, asks for it, and tells in advance
    /// whether an export would take it.
    ///
    /// The size matters most. The encoder has a device-imposed minimum coded
    /// extent (160x64 on the adapter this was developed against), and an
    /// export below it falls back to the readback path. That limit is
    /// reported here, read from the device rather than guessed.
    #[cfg_attr(not(feature = "hwencode"), allow(clippy::unused_self))]
    fn gpu_output_path(&self) -> Value {
        #[cfg(feature = "hwencode")]
        let capability = self.gpu_output_capability();
        #[cfg(not(feature = "hwencode"))]
        let capability: Result<std::convert::Infallible, String> =
            Err("this host was built without hardware encode".to_owned());
        match capability {
            #[cfg(feature = "hwencode")]
            Ok(capability) => json!({
                "available": true,
                "reason": Value::Null,
                "backend": capability.backend.as_str(),
                "encoder": capability.encoder,
                "videoCodec": capability.codec.as_str(),
                // `gpu-copy` renders into the backend's planes and copies them into the encoder's
                // surface;
                // `direct-target` renders into the encoder's surface itself.
                "inputLayout": capability.input_layout.as_str(),
                "inputHandoff": capability.handoff.as_str(),
                // Whether pinning this encoder asks for the GPU path. `h264_vulkan` takes Vulkan
                // frames only; `h264_videotoolbox` is also a readback-path encoder.
                "gpuInputOnly": capability.gpu_input_only,
                "sharesRendererDevice": capability.shares_renderer_device,
                "minimumWidth": capability.minimum.0,
                "minimumHeight": capability.minimum.1,
                "maximumWidth": capability.maximum.0,
                "maximumHeight": capability.maximum.1,
                // The limits apply to the coded extent: the output size aligned up to this.
                "widthAlignment": capability.granularity.0,
                "heightAlignment": capability.granularity.1,
                // B pictures need a non-zero L0-for-B or L1 reference count; `disabled` rate
                // control is constant QP. Null where the encoder does not report its reference
                // structure (VideoToolbox).
                "maxPPictureL0References": capability.max_p_picture_l0_references,
                "maxBPictureL0References": capability.max_b_picture_l0_references,
                "maxL1References": capability.max_l1_references,
                "rateControlModes": capability.rate_control_modes,
                // Renderer plane-texture pairs and surfaces still held after release, besides
                // leased ones;
                // `ALPHA2_EXPORT_IN_FLIGHT_BYTES` has to hold both.
                "planeRingSlots": capability.plane_ring_slots,
                "retainedSurfaces": capability.retained_surfaces(),
                // 4:2:0 has no odd chroma plane, and the path refuses rather than rounding.
                "requiresEvenDimensions": true,
                // Every codec the path takes on this device; the fields above describe the first.
                "codecs": self
                    .gpu_output_codecs()
                    .iter()
                    .map(|capability| json!({
                        "videoCodec": capability.codec.as_str(),
                        "encoder": capability.encoder,
                        "minimumWidth": capability.minimum.0,
                        "minimumHeight": capability.minimum.1,
                        "maximumWidth": capability.maximum.0,
                        "maximumHeight": capability.maximum.1,
                        "widthAlignment": capability.granularity.0,
                        "heightAlignment": capability.granularity.1,
                        "maxPPictureL0References": capability.max_p_picture_l0_references,
                        "maxBPictureL0References": capability.max_b_picture_l0_references,
                        "maxL1References": capability.max_l1_references,
                        "rateControlModes": capability.rate_control_modes,
                    }))
                    .collect::<Vec<_>>(),
            }),
            Err(reason) => json!({
                "available": false,
                "reason": reason,
                "backend": Value::Null,
                "encoder": Value::Null,
                "videoCodec": Value::Null,
                "inputLayout": Value::Null,
                "inputHandoff": Value::Null,
                "gpuInputOnly": Value::Null,
                "sharesRendererDevice": Value::Null,
                "minimumWidth": Value::Null,
                "minimumHeight": Value::Null,
                "maximumWidth": Value::Null,
                "maximumHeight": Value::Null,
                "widthAlignment": Value::Null,
                "heightAlignment": Value::Null,
                "planeRingSlots": Value::Null,
                "retainedSurfaces": Value::Null,
                "requiresEvenDimensions": true,
                "codecs": [],
            }),
        }
    }

    /// A host that renders on the CPU: no GPU device, decoding in software.
    /// `reason` says why it was chosen, as results and capabilities report.
    pub fn new_cpu(
        renderer: alpha2_render::CpuRenderer,
        registered: HashSet<(String, u32)>,
        reason: String,
    ) -> Self {
        Self {
            renderer: RendererBackend::Cpu(Box::new(renderer)),
            renderer_reason: Some(reason),
            builtin_effects: registered,
            runtime_effects: HashMap::new(),
            decode_policy: VideoDecodePolicy::Software,
            hardware_decode_unavailable: None,
            #[cfg(all(target_os = "linux", feature = "hwdevice"))]
            shared_vulkan: None,
            #[cfg(all(target_os = "macos", feature = "hwdevice"))]
            shared_metal: None,
            #[cfg(all(target_os = "windows", feature = "hwdevice"))]
            shared_d3d12: None,
            #[cfg(all(target_os = "windows", feature = "hwencode"))]
            encode_adapters: crate::adapter_choice::EncodeAdapterNotes::default(),
            texts: std::sync::Mutex::new(TextTextures::new()),
            provider_files: crate::providers::ProviderFiles::default(),
            conformed_video_pixels: None,
            render_cache: None,
            input_policy: crate::config::VideoInputPolicy::Auto,
            strategy: crate::config::ExportStrategy::Render,
            gpu_log: std::sync::Arc::default(),
            failure: crate::failure::FailureContext::default(),
            memory: crate::memory::MemoryPolicy::default(),
            #[cfg(test)]
            _drop_stall: crate::teardown::injected::DropStall,
        }
    }

    /// The GPU renderer, on a host that draws with one: the paths only a GPU
    /// runs (the GPU-resident export) are reached on no other.
    pub(crate) fn gpu_renderer(&self) -> &Renderer {
        self.renderer
            .gpu()
            .expect("a GPU-only path runs only on a host that draws with a GPU")
    }

    /// What this host can do: its GPU, color handling, playback, decode and
    /// encode paths, and the extension formats it reads.
    pub fn capability_report(&self) -> alpha2_engine::CapabilityReport {
        let mut encode = codec_capabilities_json();
        let gpu_output_path = self.gpu_output_path();
        encode["h264"] = crate::protocol::h264_status_json(&encode["video"], &gpu_output_path);
        encode["gpuOutputPath"] = gpu_output_path;
        let mut report = match &self.renderer {
            RendererBackend::Gpu(renderer) => {
                alpha2_engine::CapabilityReport::new(alpha2_engine::Runtime::Native, renderer)
            }
            RendererBackend::Cpu(renderer) => {
                alpha2_engine::CapabilityReport::cpu(alpha2_engine::Runtime::Native, renderer)
            }
        };
        report.gpu.shader_compiler =
            crate::renderer_backend::describe_shader_compiler(&report.gpu.shader_compiler);
        report.decode = self.decode_capabilities();
        report.encode = encode;
        let cpu = self.renderer.cpu().is_some();
        let ten_bit = self.renderer.intermediate_format() == wgpu::TextureFormat::Rgba16Float;
        report.features.insert(
            "output.tenBit".to_owned(),
            if ten_bit {
                alpha2_engine::Feature::available()
            } else {
                alpha2_engine::Feature::unavailable(
                    "10-bit output needs the rgba16float intermediate; this renderer stores 8 bits",
                )
            },
        );
        report.features.insert(
            "renderer.gpu".to_owned(),
            if cpu {
                alpha2_engine::Feature::unavailable(
                    self.renderer_reason
                        .clone()
                        .unwrap_or_else(|| "this host renders on the CPU".to_owned()),
                )
            } else {
                alpha2_engine::Feature::available()
            },
        );
        report.features.insert(
            "renderer.cpu".to_owned(),
            if cpu {
                alpha2_engine::Feature::available()
            } else {
                alpha2_engine::Feature::unavailable(
                    "this host renders on a GPU; start it with --renderer cpu (ALPHA2_RENDERER=cpu) to render on the CPU",
                )
            },
        );
        // Runtime effects are WGSL, which the CPU renderer cannot run; every built-in it can.
        report.features.insert(
            "effects.runtime".to_owned(),
            if cpu {
                alpha2_engine::Feature::unavailable(CPU_RUNTIME_EFFECT)
            } else {
                alpha2_engine::Feature::available()
            },
        );
        let registry = alpha2_extensions::Registry::with_defaults();
        let names = registry.names();
        for format in ["svg", "animated-image", "lottie", "exr"] {
            report.features.insert(
                format!("extensions.{format}"),
                if names.contains(&format) {
                    alpha2_engine::Feature::available()
                } else {
                    alpha2_engine::Feature::unavailable("this host was built without it")
                },
            );
        }
        // An image sequence has no provider of its own to list.
        report.features.insert(
            "extensions.image-sequence".into(),
            if alpha2_extensions::Registry::reads_image_sequences() {
                alpha2_engine::Feature::available()
            } else {
                alpha2_engine::Feature::unavailable("this host was built without it")
            },
        );
        report
            .with_feature("conform", alpha2_engine::Feature::available())
            .with_feature(
                "proxyGeneration",
                alpha2_engine::Feature::unavailable(
                    "native hosts read proxies they are given; they do not make them",
                ),
            )
    }

    /// Answers one request.
    ///
    /// A panic while handling it, a defect in this host or a library it runs
    /// on the request's input, fails that request alone with `INTERNAL`: the
    /// unwind has dropped what the request held (its decoders, encoders,
    /// partial output), and the host answers the next request.
    pub fn handle(
        &mut self,
        request: Request,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, HostError> {
        let export = matches!(request, Request::Export(_) | Request::Still(_));
        match catch_unwind(AssertUnwindSafe(|| self.handle_request(request, progress))) {
            Ok(result) => result,
            Err(payload) => {
                let error = HostError::new(
                    "INTERNAL",
                    format!(
                        "the host panicked handling the request: {}",
                        crate::failure::panic_message(&*payload)
                    ),
                );
                if !export {
                    return Err(error);
                }
                let error = self.attach_failure(error, None);
                self.end_export();
                Err(error)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: Request,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, HostError> {
        let result = match request {
            Request::Capabilities { .. } => {
                let mut result = self.response_metadata(0.0);
                result["status"] = json!("ready");
                result["capabilities"] = json!(self.capability_report());
                result["effectCatalog"] = builtin_catalog();
                Ok(result)
            }
            Request::Conform(request) => return conform_answer(request, self.decode_policy),
            Request::Export(export) => return self.handle_export(*export, progress),
            Request::Still(still) => return self.handle_still(*still),
        };
        result.map_err(HostError::classify)
    }

    /// Conforms an export request's media, then exports it.
    #[allow(clippy::too_many_lines)]
    fn handle_export(
        &mut self,
        request: crate::protocol::ExportRequest,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, HostError> {
        // Resolved again: the request was checked when it was parsed.
        let format = request.format()?;
        let crate::protocol::ExportRequest {
            project,
            assets,
            proxies,
            frames,
            resources,
            media,
            effect_assets,
            font_files,
            effects,
            start_ticks,
            end_ticks,
            frame_rate,
            include_audio,
            video_encoder,
            video_quality,
            audio_quality,
            loudness,
            render_cache,
            input_policy,
            strategy,
            memory,
            output,
            overwrite,
            ..
        } = request;
        let output = OutputTarget {
            path: output,
            existing: existing_output(overwrite),
        };
        let project = project.text();
        self.begin_export(
            &ExportRequestFacts {
                project,
                assets: &assets,
                start_ticks,
                end_ticks,
                frame_rate,
                include_audio,
                media,
                video_codec: format.video,
                video_encoder: video_encoder.as_deref(),
                input_policy,
            },
            memory,
        )?;
        let PreparedRequest {
            resolved,
            selections,
            released,
            provider_files,
        } = self.prepare_request(
            project,
            &RequestInputs {
                assets: &assets,
                proxies: &proxies,
                frames: &frames,
                resources: &resources,
                effect_assets: &effect_assets,
                font_files: &font_files,
            },
            media,
            &[&output.path],
        )?;
        self.input_policy = input_policy;
        self.strategy = strategy;
        let render_cache_unavailable = render_cache.and_then(|request| {
            self.open_render_cache(
                request,
                project,
                &resolved.paths,
                &provider_files,
                &effect_assets,
                &font_files,
                &effects,
            )
        });
        self.provider_files = provider_files;
        let exported = self
            .set_fonts(&font_files)
            .and_then(|fonts| {
                // The bytes the text store read, once: an SVG's text draws with them, and no
                // provider open reads a font file again.
                self.provider_files.fonts = fonts;
                self.export(
                    project,
                    &resolved.paths,
                    &effect_assets,
                    effects,
                    start_ticks,
                    end_ticks,
                    frame_rate,
                    include_audio,
                    format,
                    video_encoder.as_deref(),
                    video_quality,
                    audio_quality,
                    loudness,
                    &output,
                    progress,
                )
            })
            .map(|mut result| {
                result["media"] = Value::Object(selections);
                result["memoryPolicy"] = self.memory.to_json();
                result["idleBytesReleasedAtStart"] = json!(released);
                let (diagnostics, omitted) = self.failure.provider_diagnostics();
                result["providerDiagnostics"] = diagnostics;
                result["providerDiagnosticsOmitted"] = json!(omitted);
                result["decodeDiagnostics"] = self.failure.decode_diagnostics();
                if let Some(reason) = &render_cache_unavailable {
                    result["renderCache"] = crate::render_cache::unavailable(reason);
                }
                result
            });
        self.end_export();
        exported.map_err(|message| {
            let position = crate::failure::phase_tag(&message);
            let message = crate::termination::cancelled_by_request_or_termination(message);
            let error = HostError::classify(message);
            self.attach_failure(error, position)
        })
    }

    /// The setup `export` and `still` share, once the request's failure
    /// record has begun: refuses an output that is one of the request's
    /// inputs, conforms the media the request reads, checks the files the
    /// project's providers name, gives back what the renderer keeps only for
    /// reuse, and lists the files extension providers read (the fonts once
    /// the text store has read them).
    fn prepare_request(
        &mut self,
        project: &str,
        inputs: &RequestInputs<'_>,
        media: crate::protocol::ExportMedia,
        outputs: &[&Path],
    ) -> Result<PreparedRequest, HostError> {
        for output in outputs {
            inputs
                .refuse_output(output)
                .map_err(|error| self.attach_failure(error, None))?;
        }
        let resolved =
            self.conform_export(project, inputs.assets, inputs.proxies, inputs.frames, media)?;
        Project::from_json(project)
            .map_err(error_message)
            .and_then(|parsed| crate::providers::check_resources(&parsed, inputs.resources))
            .map_err(|error| self.attach_failure(HostError::classify(error), None))?;
        let selections = media_selections(&resolved.report);
        // What the renderer keeps only for reuse goes back before a request renders: a resident
        // host's last request must not crowd this one.
        let released = self
            .renderer
            .release_idle_memory()
            .saturating_add(self.release_idle_decode_copies());
        let provider_files = crate::providers::ProviderFiles {
            originals: resolved.paths.clone(),
            frames: inputs.frames.clone(),
            fonts: Vec::new(),
            resources: inputs.resources.clone(),
        };
        self.conformed_video_pixels = Some(resolved.video_pixels.clone());
        Ok(PreparedRequest {
            resolved,
            selections,
            released,
            provider_files,
        })
    }

    /// Conforms a still request's media, then renders each frame it asks for
    /// to its PNG.
    fn handle_still(&mut self, request: crate::protocol::StillRequest) -> Result<Value, HostError> {
        let crate::protocol::StillRequest {
            project,
            assets,
            proxies,
            frames,
            resources,
            media,
            effect_assets,
            font_files,
            effects,
            size,
            stills,
            overwrite,
            alpha,
            ..
        } = request;
        let project = still_project(project.text(), size)?;
        self.begin_request(crate::memory::MemoryRequest::default(), None)?;
        let outputs = stills
            .iter()
            .map(|still| still.output.as_path())
            .collect::<Vec<_>>();
        let PreparedRequest {
            resolved,
            selections,
            released,
            provider_files,
        } = self.prepare_request(
            &project,
            &RequestInputs {
                assets: &assets,
                proxies: &proxies,
                frames: &frames,
                resources: &resources,
                effect_assets: &effect_assets,
                font_files: &font_files,
            },
            media,
            &outputs,
        )?;
        self.provider_files = provider_files;
        let rendered = self
            .set_fonts(&font_files)
            .and_then(|fonts| {
                self.provider_files.fonts = fonts;
                self.render_stills(
                    &project,
                    &resolved.paths,
                    &effect_assets,
                    effects,
                    &stills,
                    alpha,
                    existing_output(overwrite),
                )
            })
            .map(|mut result| {
                result["media"] = Value::Object(selections);
                result["idleBytesReleasedAtStart"] = json!(released);
                result["decodeDiagnostics"] = self.failure.decode_diagnostics();
                result
            });
        self.end_export();
        rendered.map_err(|message| {
            HostError::classify(crate::termination::cancelled_by_request_or_termination(
                message,
            ))
        })
    }

    /// Keys the render cache an export asked for. A cache this host cannot
    /// key is not used: the export runs without it, and the returned reason
    /// is what its result reports.
    #[allow(clippy::too_many_arguments)]
    fn open_render_cache(
        &mut self,
        request: crate::render_cache::RenderCacheRequest,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        providers: &crate::providers::ProviderFiles,
        effect_assets: &HashMap<String, PathBuf>,
        font_files: &[PathBuf],
        effects: &[EffectRequest],
    ) -> Option<String> {
        match self.render_cache_identity(
            project,
            assets,
            providers,
            effect_assets,
            font_files,
            effects,
        ) {
            Ok(identity) => {
                self.render_cache = Some((request, identity));
                None
            }
            Err(reason) => {
                alpha2_os::diagnostic!("alpha2 native export: render cache unused ({reason})");
                Some(reason)
            }
        }
    }

    /// What an export reads, and whether it can, before anything renders or
    /// any output exists.
    fn conform_export(
        &self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        proxies: &HashMap<String, Vec<ProxyRequest>>,
        frames: &HashMap<String, Vec<Option<PathBuf>>>,
        media: crate::protocol::ExportMedia,
    ) -> Result<ResolvedMedia, HostError> {
        let resolved = request_media(
            project,
            assets,
            proxies,
            frames,
            media.purpose(),
            &self.hardware_video_decode(),
        )
        .map_err(|error| self.attach_failure(error, None))?;
        if !resolved.report.ready {
            let error = HostError::new("CONFORM_FAILED", blocking_summary(&resolved.report))
                .with_details(json!(resolved.report));
            return Err(self.attach_failure(error, None));
        }
        Ok(resolved)
    }

    /// Forgets the settings of the export that just ran.
    /// Frees the hardware decode path's idle copy targets (D3D12VA's, the one
    /// backend that copies each surface) and returns their bytes, which a
    /// resident host would otherwise keep, at the frame sizes of its last
    /// export, through the next one.
    #[cfg_attr(
        not(all(target_os = "windows", feature = "hwdecode")),
        allow(clippy::unused_self)
    )]
    fn release_idle_decode_copies(&self) -> u64 {
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        if let Some(d3d12) = &self.shared_d3d12 {
            return d3d12.release_idle_copies();
        }
        0
    }

    fn end_export(&mut self) {
        self.conformed_video_pixels = None;
        self.render_cache = None;
        self.input_policy = crate::config::VideoInputPolicy::Auto;
        self.strategy = crate::config::ExportStrategy::Render;
        self.memory = crate::memory::MemoryPolicy::default();
    }

    /// Starts an export: its byte budgets, and its failure record with what
    /// it asked for (identifiers and settings only, never media bytes or
    /// project contents). Device events before this export are forgotten, a
    /// loss excepted.
    fn begin_export(
        &mut self,
        facts: &ExportRequestFacts<'_>,
        memory: crate::memory::MemoryRequest,
    ) -> Result<(), HostError> {
        self.begin_request(memory, Some(facts))
    }

    /// Starts a request that renders: its byte budgets, and a failure record
    /// with what an export asked for (`facts`). Device events before it are
    /// forgotten, a loss excepted.
    fn begin_request(
        &mut self,
        memory: crate::memory::MemoryRequest,
        facts: Option<&ExportRequestFacts<'_>>,
    ) -> Result<(), HostError> {
        self.failure.begin();
        self.gpu_log.clear();
        if let Some(facts) = facts {
            self.note_request(facts);
        }
        let decoding = crate::recovery::ExportDecoding::from_env()
            .map_err(|error| self.attach_failure(HostError::invalid_input(error), None))?;
        self.failure.begin_decoding(decoding);
        self.memory = crate::memory::MemoryPolicy::resolve(memory)
            .map_err(|error| self.attach_failure(HostError::invalid_input(error), None))?;
        self.failure.note("memoryPolicy", self.memory.to_json());
        Ok(())
    }

    fn note_request(&self, facts: &ExportRequestFacts<'_>) {
        let mut asset_ids = facts.assets.keys().cloned().collect::<Vec<_>>();
        asset_ids.sort();
        self.failure.note(
            "request",
            json!({
                "startTicks": facts.start_ticks.to_string(),
                "endTicks": facts.end_ticks.to_string(),
                "frameRate": { "num": facts.frame_rate.num, "den": facts.frame_rate.den },
                "includeAudio": facts.include_audio,
                "media": match facts.media {
                    crate::protocol::ExportMedia::Original => "original",
                    crate::protocol::ExportMedia::Proxies => "proxies",
                },
                "videoCodec": facts.video_codec.as_str(),
                "videoEncoder": facts.video_encoder,
                "inputPolicy": facts.input_policy.as_str(),
                "assets": asset_ids,
            }),
        );
        if let Ok(ProjectCanvas {
            settings: CanvasSize { width, height },
        }) = serde_json::from_str(facts.project)
        {
            self.failure
                .note("output", json!({ "width": width, "height": height }));
        }
    }

    /// `error`, carrying the export's failure report: its class, where it
    /// failed (`position` when the error was marked, otherwise where the main
    /// loop was), the device's diagnostics, and the renderer's and process's
    /// memory as they stand now.
    fn attach_failure(
        &self,
        error: HostError,
        position: Option<(crate::failure::ExportPhase, Option<u64>)>,
    ) -> HostError {
        let position = position.unwrap_or_else(|| self.failure.position());
        let mut facts = self.failure.facts();
        let mut error = error;
        if let RendererBackend::Gpu(renderer) = &self.renderer {
            // A loss the renderer's own long submission explains is hung, whichever step met it.
            if matches!(error.code, "GPU_DEVICE_LOST" | "GPU_TIMEOUT")
                && let hung @ alpha2_render::RenderError::GpuDeviceHung { .. } =
                    renderer.classify_loss(alpha2_render::RenderError::GpuDeviceLost)
            {
                error.code = "GPU_DEVICE_HUNG";
                error.message = format!("{hung} ({})", error.message);
            }
            if let Some(longest) = renderer.longest_submission() {
                facts.insert(
                    "gpuSubmission".to_owned(),
                    json!({
                        "maxMs": (longest.gpu_ms * 10.0).round() / 10.0,
                        "watchdogMs": alpha2_gpu::watchdog_ms(),
                        "frameIndex": longest.frame.and_then(|frame| frame.frame_index),
                        "effects": longest.effects,
                    }),
                );
            }
        }
        self.attach_failure_with(error, position, facts)
    }

    /// `error`, a refusal of an export request this host could not read
    /// (malformed, an unsupported protocol, a setting out of range),
    /// carrying its failure report: it failed before anything was prepared,
    /// so the report has no facts of an export (the last export's are not
    /// this one's), only the device's and the process's state.
    pub fn refuse_export(&self, error: HostError) -> HostError {
        self.attach_failure_with(
            error,
            (crate::failure::ExportPhase::Prepare, None),
            serde_json::Map::new(),
        )
    }

    fn attach_failure_with(
        &self,
        error: HostError,
        position: (crate::failure::ExportPhase, Option<u64>),
        mut diagnostics: serde_json::Map<String, Value>,
    ) -> HostError {
        let gpu = self.gpu_log.snapshot();
        diagnostics.insert("gpu".to_owned(), crate::failure::gpu_json(&gpu));
        diagnostics.insert("memory".to_owned(), self.memory_json());
        diagnostics.insert("environment".to_owned(), self.environment_json());
        let failure = crate::failure::failure_json(
            error.code,
            &error.message,
            error.evidence,
            position,
            &gpu,
            diagnostics,
        );
        error.with_failure(failure)
    }

    /// The memory this host can account for: the renderer's pooled GPU
    /// allocations, and the process's resident set where the OS reports it.
    /// Neither includes driver- or codec-internal memory.
    fn memory_json(&self) -> Value {
        let RendererBackend::Gpu(renderer) = &self.renderer else {
            let cpu = self.renderer.cpu().map(alpha2_render::CpuRenderer::stats);
            return json!({
                "cpu": {
                    "scope": "the CPU renderer's pictures; decoder and encoder memory not included",
                    "resultCacheBytes": self.renderer.result_cache_stats().bytes,
                    "imageHighWaterBytes": cpu.map_or(0, |stats| stats.image_high_water_bytes),
                },
                "process": crate::failure::process_memory_json(),
            });
        };
        let diagnostics = renderer.diagnostics();
        let pool = |active: u64, idle: u64, peak: u64| json!({ "activeBytes": active, "idleBytes": idle, "highWaterBytes": peak });
        json!({
            "gpu": {
                "scope": "renderer-pooled allocations; driver, decoder, and encoder memory not included",
                "textures": pool(
                    diagnostics.textures.active_bytes,
                    diagnostics.textures.available_bytes,
                    diagnostics.textures.high_water_bytes,
                ),
                "uploadTextures": pool(
                    diagnostics.upload_textures.active_bytes,
                    diagnostics.upload_textures.available_bytes,
                    diagnostics.upload_textures.high_water_bytes,
                ),
                "readbackBuffers": pool(
                    diagnostics.readback_buffers.active_bytes,
                    diagnostics.readback_buffers.available_bytes,
                    diagnostics.readback_buffers.high_water_bytes,
                ),
                "uniformBuffers": pool(
                    diagnostics.uniform_buffers.active_bytes,
                    diagnostics.uniform_buffers.available_bytes,
                    diagnostics.uniform_buffers.high_water_bytes,
                ),
                "resultCacheBytes": diagnostics.result_cache.bytes,
            },
            "process": crate::failure::process_memory_json(),
        })
    }

    /// The build and device this host runs on.
    fn environment_json(&self) -> Value {
        let adapter = self.renderer.adapter();
        let ffmpeg = ffmpeg_runtime_identity();
        json!({
            "hostVersion": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "features": {
                "hwdecode": cfg!(feature = "hwdecode"),
                "hwencode": cfg!(feature = "hwencode"),
                "extensions": cfg!(feature = "extensions"),
            },
            "adapter": {
                "name": adapter.name,
                "backend": adapter.backend,
                "driver": adapter.driver,
                "driverInfo": adapter.driver_info,
                "shaderCompiler": adapter.shader_compiler,
            },
            "ffmpeg": ffmpeg.version,
            "renderer": self.renderer.name(),
            "videoDecodeMode": self.decode_policy.as_str(),
            "hardwareDecodeUnavailable": self.hardware_decode_unavailable,
        })
    }

    /// What an export's baked frames depend on besides their time: this
    /// binary, the `FFmpeg` runtime and GPU, the project, and every file and
    /// definition the export reads. See [`crate::render_cache`].
    fn render_cache_identity(
        &self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        providers: &crate::providers::ProviderFiles,
        effect_assets: &HashMap<String, PathBuf>,
        font_files: &[PathBuf],
        effects: &[EffectRequest],
    ) -> Result<[u8; 32], String> {
        let mut identity = crate::render_cache::SnapshotIdentity::default();
        identity.add("binary", &host_binary_digest()?);
        let ffmpeg = ffmpeg_runtime_identity();
        identity.add("ffmpeg", ffmpeg.version.as_bytes());
        identity.add("ffmpeg-configuration", ffmpeg.configuration.as_bytes());
        // CPU pictures are the same on every machine and thread count: the key names the
        // renderer's version alone, so a cache can be shared.
        let adapter = match &self.renderer {
            RendererBackend::Cpu(_) => crate::renderer_backend::AdapterIdentity {
                name: "alpha2 CPU renderer".to_owned(),
                ..self.renderer.adapter()
            },
            RendererBackend::Gpu(_) => self.renderer.adapter(),
        };
        add_adapter_identity(&mut identity, &adapter);
        identity.add(
            "intermediate",
            format!("{:?}", self.renderer.intermediate_format()).as_bytes(),
        );
        identity.add("decode", self.decode_policy.as_str().as_bytes());
        identity.add("project", project.as_bytes());
        for (label, files) in [("asset", assets), ("effect-asset", effect_assets)] {
            let mut files = files.iter().collect::<Vec<_>>();
            files.sort();
            for (id, path) in files {
                identity.add(label, id.as_bytes());
                identity.add_file("file", path);
            }
        }
        let mut sequences = providers.frames.iter().collect::<Vec<_>>();
        sequences.sort_by(|a, b| a.0.cmp(b.0));
        for (id, files) in sequences {
            identity.add("sequence", id.as_bytes());
            for file in files {
                match file {
                    Some(path) => identity.add_file("frame", path),
                    None => identity.add("gap", &[]),
                }
            }
        }
        let mut resources = providers.resources.iter().collect::<Vec<_>>();
        resources.sort_by(|a, b| a.0.cmp(b.0));
        for (id, files) in resources {
            identity.add("resources", id.as_bytes());
            let mut files = files.iter().collect::<Vec<_>>();
            files.sort();
            for (locator, path) in files {
                identity.add("locator", locator.as_bytes());
                identity.add_file("resource", path);
            }
        }
        for font in font_files {
            identity.add_file("font", font);
        }
        // The request's runtime effects are the ones the export renders: nothing an earlier request
        // registered survives into it.
        identity.add("effects", format!("{effects:?}").as_bytes());
        Ok(identity.finish())
    }

    /// Replaces the fonts with the files at `paths`, in fallback
    /// order, and returns their bytes, read once. Each export request names
    /// its own; none is kept from another.
    ///
    /// # Errors
    /// Returns a message naming a font file that cannot be read or parsed.
    pub fn set_fonts(&mut self, paths: &[PathBuf]) -> Result<Vec<std::sync::Arc<[u8]>>, String> {
        let alpha2_layout::FontFiles {
            faces,
            bytes: fonts,
        } = alpha2_layout::fonts_from_files(paths).map_err(|error| match error {
            alpha2_layout::FontFileError::Read { path, error } => {
                code_tag("MISSING_INPUT", format!("font {}: {error}", path.display()))
            }
            alpha2_layout::FontFileError::Invalid { path, error, .. } => code_tag(
                "INVALID_INPUT",
                format!(
                    "font {}: {}",
                    path.display(),
                    alpha2_engine::TextError::Raster(error)
                ),
            ),
            error => code_tag("INVALID_INPUT", error.to_string()),
        })?;
        let mut texts = TextTextures::new();
        for face in faces {
            texts
                .add_font(face.family, face.bytes, face.face_index)
                .map_err(|error| code_tag("INVALID_INPUT", error.to_string()))?;
        }
        *self
            .texts
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = texts;
        Ok(fonts)
    }

    /// Registers the request's runtime effects and the project's effect
    /// resources, and checks that every effect the project draws is
    /// registered.
    ///
    /// # Errors
    /// An unreadable project, a missing or malformed effect resource, or an
    /// effect the project draws that is not registered.
    pub fn prepare(
        &mut self,
        project: &str,
        effect_assets: &HashMap<String, PathBuf>,
        effects: Vec<EffectRequest>,
    ) -> Result<(Project, EngineProject), String> {
        self.register_request_effects(effects)?;
        let project = Project::from_json(project).map_err(error_message)?;
        self.renderer.clear_effect_resources();
        for resource in &project.effect_resources {
            let path = effect_assets.get(resource.id.as_str()).ok_or_else(|| {
                code_tag(
                    "MISSING_INPUT",
                    format!("effect resource {} has no supplied asset path", resource.id),
                )
            })?;
            match resource.kind {
                alpha2_model::EffectResourceKind::Lut3d => {
                    let bytes = fs::read(path).map_err(|error| {
                        code_tag(
                            "MISSING_INPUT",
                            format!(
                                "failed to read effect resource {} ({}): {error}",
                                resource.id,
                                path.display()
                            ),
                        )
                    })?;
                    let cube = crate::text_file::decode_user_text(&bytes).map_err(|offset| {
                        code_tag(
                            "INVALID_INPUT",
                            crate::text_file::not_text(
                                &format!("effect resource {}", resource.id),
                                path,
                                offset,
                            ),
                        )
                    })?;
                    let data = Lut3dData::from_cube(&cube).map_err(|error| {
                        code_tag(
                            "INVALID_INPUT",
                            format!("effect resource {}: {error}", resource.id),
                        )
                    })?;
                    self.renderer
                        .register_lut3d(resource.id.as_str(), &data)
                        .map_err(error_message)?;
                }
            }
        }
        let snapshot = EngineProject::new(project.clone()).map_err(error_message)?;
        self.validate_project_effects(&snapshot)?;
        Ok((project, snapshot))
    }

    /// Registers the runtime effects an export request defines, and only
    /// those: each request names its own, as it names its fonts.
    ///
    /// A definition an earlier request on this host registered and this one
    /// does not repeat word for word is unregistered first, so an edited body
    /// renders as edited, a fixed one is registered again, and an effect only
    /// an earlier request defined is unregistered here as on a fresh host.
    /// One repeated unchanged keeps its compiled pipelines.
    ///
    /// A definition that fails to register is reported when the project
    /// draws it ([`Self::require_effect`]).
    ///
    /// # Errors
    /// `INVALID_INPUT` when the request defines one id@version twice, differently.
    pub fn register_request_effects(&mut self, effects: Vec<EffectRequest>) -> Result<(), String> {
        let mut requested = HashMap::<(String, u32), (String, EffectRequest)>::new();
        for effect in effects {
            let key = (effect.id.clone(), effect.version);
            let definition = format!("{effect:?}");
            match requested.get(&key) {
                Some((first, _)) if *first == definition => {}
                Some(_) => {
                    return Err(code_tag(
                        "INVALID_INPUT",
                        format!("effects defines {}@{} twice, differently", key.0, key.1),
                    ));
                }
                None => {
                    requested.insert(key, (definition, effect));
                }
            }
        }
        let stale = self
            .runtime_effects
            .iter()
            .filter(|(key, registered)| {
                requested
                    .get(*key)
                    .is_none_or(|(definition, _)| *definition != registered.definition)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(stale) = self.runtime_effects.remove(&key)
                && stale.rejected.is_none()
                && let Some(renderer) = self.renderer.gpu_mut()
            {
                renderer.unregister_effect(&EffectId::new(key.0, key.1));
            }
        }
        for (key, (definition, effect)) in requested {
            if self.runtime_effects.contains_key(&key) {
                continue;
            }
            let rejected = match (effect.into_definition(), self.renderer.gpu_mut()) {
                (Ok(effect), Some(renderer)) => {
                    pollster::block_on(renderer.register_effect(effect))
                        .err()
                        .map(|diagnostics| format!("{diagnostics:?}"))
                }
                // A runtime effect is WGSL, which only a GPU runs.
                (Ok(_), None) => Some(CPU_RUNTIME_EFFECT.to_owned()),
                (Err(error), _) => Some(error),
            };
            if let Some(reason) = &rejected {
                alpha2_os::diagnostic!(
                    "alpha2 native web host skipped runtime effect {}@{}: {reason}",
                    key.0,
                    key.1
                );
            }
            self.runtime_effects.insert(
                key,
                RuntimeEffect {
                    definition,
                    rejected,
                },
            );
        }
        Ok(())
    }

    /// Checks that every effect the project draws is registered
    /// ([`EngineProject::effects`]): those of each composition a clip shows
    /// or draws as its source, and each transition's. A composition that
    /// cannot draw its output is reported when a frame shows it.
    pub fn validate_project_effects(&self, snapshot: &EngineProject) -> Result<(), String> {
        for (id, version) in snapshot.effects() {
            self.require_effect(id, version)?;
        }
        Ok(())
    }

    /// Fails when `id@version` is neither built in nor a runtime effect this
    /// request registered successfully.
    pub fn require_effect(&self, id: &str, version: u32) -> Result<(), String> {
        let key = (id.to_owned(), version);
        if self.builtin_effects.contains(&key) {
            return Ok(());
        }
        if let Some(registered) = self.runtime_effects.get(&key) {
            return match &registered.rejected {
                None => Ok(()),
                Some(reason) if reason == CPU_RUNTIME_EFFECT => Err(crate::protocol::code_tag(
                    "EFFECT_ERROR",
                    format!(
                        "effect {id}@{version} is a runtime WGSL effect, which has no CPU \
                         implementation, and this host renders on the CPU (no hardware GPU, or \
                         --renderer cpu); remove the effect, or render on a GPU adapter with \
                         --renderer gpu (a software adapter such as lavapipe works, much slower)"
                    ),
                )),
                Some(_) => Err(crate::protocol::code_tag(
                    "EFFECT_ERROR",
                    format!(
                        "project requires effect {id}@{version}, but its shader registration failed"
                    ),
                )),
            };
        }
        Err(crate::protocol::code_tag(
            "EFFECT_ERROR",
            format!("project requires unregistered effect {id}@{version}"),
        ))
    }

    /// Exports a project, adding what the export cost in Vulkan import
    /// tickets (`vulkanImportTickets`) when frames can be imported.
    #[allow(clippy::too_many_arguments)]
    pub fn export(
        &mut self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        effect_assets: &HashMap<String, PathBuf>,
        effects: Vec<EffectRequest>,
        start_ticks: i64,
        end_ticks: i64,
        frame_rate: RateRequest,
        include_audio: bool,
        format: ResolvedFormat,
        requested_video_encoder: Option<&str>,
        video_quality: VideoQualityRequest,
        audio_quality: AudioQualityRequest,
        loudness: Option<LoudnessRequest>,
        output: impl Into<OutputTarget>,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, String> {
        let output = output.into();
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        let tickets_before = self
            .shared_vulkan
            .as_ref()
            .map(SharedVulkanContext::import_ticket_stats);
        let cache_before = self.renderer.result_cache_stats();
        // One record of the first failure, shared by every path and worker, so a cancellation that
        // failure caused elsewhere is never what is reported.
        let pipeline_failure = PipelineFailure::default();
        let mut result = self
            .export_once(
                project,
                assets,
                effect_assets,
                effects,
                start_ticks,
                end_ticks,
                frame_rate,
                include_audio,
                format,
                requested_video_encoder,
                video_quality,
                audio_quality,
                loudness,
                &output,
                &pipeline_failure,
                progress,
            )
            .map_err(|error| pipeline_failure.report(error))?;
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        if let Some((before, after)) = tickets_before.zip(
            self.shared_vulkan
                .as_ref()
                .map(SharedVulkanContext::import_ticket_stats),
        ) {
            result["vulkanImportTickets"] = json!({
                "created": after.created - before.created,
                "reused": after.reused - before.reused,
                "abandoned": after.abandoned - before.abandoned,
            });
        }
        result["resultCache"] =
            result_cache_json(&cache_before, &self.renderer.result_cache_stats());
        result["container"] = json!(format.container.as_str());
        result["output"] = json!(written_path(&output.path));
        Ok(result)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn export_once(
        &mut self,
        project: &str,
        assets: &HashMap<String, PathBuf>,
        effect_assets: &HashMap<String, PathBuf>,
        effects: Vec<EffectRequest>,
        start_ticks: i64,
        end_ticks: i64,
        frame_rate: RateRequest,
        include_audio: bool,
        format: ResolvedFormat,
        requested_video_encoder: Option<&str>,
        video_quality: VideoQualityRequest,
        audio_quality: AudioQualityRequest,
        loudness: Option<LoudnessRequest>,
        output: &OutputTarget,
        pipeline_failure: &PipelineFailure,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let ResolvedFormat {
            container,
            video: video_codec,
            audio: audio_codec,
            pixels,
        } = format;
        let (project, snapshot) = self.prepare(project, effect_assets, effects)?;
        let parked_leases = ParkedLeaseGuard {
            renderer: self.renderer.gpu(),
        };
        let rate = FrameRate::new(frame_rate.num, frame_rate.den).map_err(error_message)?;
        let range = TimeRange::new(
            MediaTime::from_ticks(start_ticks),
            MediaTime::from_ticks(end_ticks),
        )
        .map_err(error_message)?;
        let exporter = Exporter::new(
            &snapshot,
            range,
            rate,
            QualityProfile::export(),
            video_codec.model(),
        )
        .map_err(error_message)?
        .with_pixels(pixels);
        let size = RenderSize::new(project.settings.width, project.settings.height)
            .map_err(error_message)?;
        // A 10-bit output packs from the picture as stored, which an 8-bit intermediate holds at 8
        // bits only: refused rather than written as 10-bit data that is not.
        if pixels.bit_depth == alpha2_media::BitDepth::Ten
            && self.renderer.intermediate_format() == wgpu::TextureFormat::Rgba8Unorm
        {
            return Err(code_tag(
                "UNSUPPORTED",
                "10-bit output needs the rgba16float intermediate; this renderer stores 8 bits",
            ));
        }
        // This export's source keys (`export_source_key`) mean something else in any other.
        self.renderer.clear_result_cache();
        self.renderer.set_result_cache_budget(result_cache_budget(
            size,
            self.renderer.intermediate_format(),
        ));
        let video_rate = resolve_video_rate_control(
            video_quality,
            video_codec.model(),
            size.width,
            size.height,
            rate,
        )?;
        let audio_rate = ResolvedAudioRate::resolve(audio_quality, audio_codec.model());
        let cancel = crate::termination::export_cancel();
        let pipeline_enabled = configured_pipeline_enabled();
        let decode_lookahead = configured_decode_lookahead(pipeline_enabled);
        let has_audio = include_audio
            && project
                .sources
                .iter()
                .any(alpha2_model::MediaSource::is_audible);
        // Normalizing needs a mix to measure. A project that hears nothing has none on any path, so
        // this is decided once, before a copy is planned or any output opens.
        if loudness.is_some() && !has_audio {
            return Err(loudness_without_audio());
        }
        // Every path encodes the mix with the same encoder, so a runtime without one is refused
        // before any of them opens a device or an output.
        let audio_encoder_name = has_audio
            .then(|| {
                alpha2_native::audio_encoder_capabilities()
                    .iter()
                    .find(|capability| capability.codec == audio_codec)
                    .map(|capability| capability.encoder_name.clone())
                    .ok_or_else(|| {
                        code_tag(
                            "INVALID_INPUT",
                            format!(
                                "this FFmpeg runtime has no {} encoder",
                                audio_codec.as_str()
                            ),
                        )
                    })
            })
            .transpose()?;
        // A copy is taken only when asked for, and only when both the project and the file prove
        // the copied packets decode into exactly the frames a render would show.
        let strategy = self.strategy;
        if !pixels.is_yuv420p() && strategy == crate::config::ExportStrategy::RequireCopy {
            return Err(format!(
                "{}the export cannot be a copy: alpha and 10-bit pictures are rendered",
                crate::protocol::copy_ineligible_tag()
            ));
        }
        let copy_ineligible = if strategy == crate::config::ExportStrategy::Render {
            None
        } else if !pixels.is_yuv420p() {
            Some("alpha and 10-bit pictures are rendered".to_owned())
        } else {
            // A plan whose reading of the sources stalled fails the export: rendering would read
            // the same sources.
            let plan =
                crate::packet_copy::watched_packet_copy_plan(crate::packet_copy::CopyRequest {
                    project: project.clone(),
                    assets: assets.clone(),
                    effective_range: exporter.effective_range(),
                    rate,
                    frames: exporter.progress().total,
                    include_audio: has_audio,
                    video_codec,
                    container,
                    pinned_encoder: requested_video_encoder.map(str::to_owned),
                    loudness: loudness.is_some(),
                    cancel: cancel.clone(),
                })?;
            match plan {
                Ok(plan) => {
                    let mut result = self.export_packet_copy(
                        &crate::packet_copy::CopyExport {
                            plan: &plan,
                            snapshot: &snapshot,
                            project: &project,
                            assets,
                            effective_range: exporter.effective_range(),
                            container,
                            audio_codec,
                            audio_encoder: audio_encoder_name.clone(),
                            audio_rate,
                            loudness,
                            output,
                            started,
                        },
                        progress,
                    )?;
                    result["inputPolicy"] = json!(self.input_policy.as_str());
                    result["requestedStrategy"] = json!(strategy.as_str());
                    result["renderCache"] = self.render_cache.as_ref().map_or(Value::Null, |_| {
                        crate::render_cache::unavailable("a copy renders nothing to bake")
                    });
                    return Ok(result);
                }
                Err(reason) if strategy == crate::config::ExportStrategy::RequireCopy => {
                    return Err(format!(
                        "{}the export cannot be a copy: {reason}",
                        crate::protocol::copy_ineligible_tag()
                    ));
                }
                Err(reason) => {
                    alpha2_os::diagnostic!(
                        "alpha2 native export: rendering instead of copying ({reason})"
                    );
                    Some(reason)
                }
            }
        };
        // On Linux the hardware encoder is not in `codec_capabilities` (the software sinks cannot
        // feed it), so the GPU path is decided here from the device, codec, pinned encoder and
        // input policy.
        let input_policy = self.input_policy;
        #[cfg(feature = "hwencode")]
        let candidate = self.gpu_output_candidate(video_codec, requested_video_encoder);
        #[cfg(not(feature = "hwencode"))]
        let candidate: Result<
            std::convert::Infallible,
            crate::gpu_output::GpuOutputRefusal,
        > = Err(crate::gpu_output::GpuOutputRefusal::unavailable(
            "this host was built without hardware encode",
        ));
        // The CPU renderer draws into memory: only the readback-style sinks take its frames.
        let candidate = if self.renderer.cpu().is_some() {
            Err(crate::gpu_output::GpuOutputRefusal::unavailable(
                "the CPU renderer draws frames in memory; the GPU-resident path needs a GPU",
            ))
        } else if !pixels.is_yuv420p() {
            Err(crate::gpu_output::GpuOutputRefusal::unavailable(
                "alpha and 10-bit pictures are packed on the CPU; the GPU-resident path writes \
                 8-bit 4:2:0",
            ))
        } else {
            candidate
        };
        // The GPU-resident export the readback path started over from, when one did.
        let mut gpu_output_path_fallback = Value::Null;
        // The GPU-resident path's encoder, when it had no session free, and the smallest output
        // it takes: what a readback path that opens nothing either reports.
        let mut gpu_busy: Option<alpha2_native::EncoderRefusal> = None;
        #[cfg(feature = "hwencode")]
        let gpu_minimum = candidate
            .as_ref()
            .ok()
            .map(|(_, capability)| (capability.encoder, capability.minimum));
        #[cfg(not(feature = "hwencode"))]
        let gpu_minimum = None;
        let gpu_output_unavailable = match candidate {
            #[cfg(feature = "hwencode")]
            Ok((device, capability)) => {
                let attempt = self
                    .gpu_output_capability()
                    .and_then(|_| readback_only_switches(pipeline_enabled))
                    .map_err(GpuEncodeOutcome::Unavailable)
                    .and_then(|()| {
                        let transcode = crate::yuv_transcode::yuv_transcode_plan(
                            &project,
                            assets,
                            exporter.effective_range(),
                            exporter.encoding(),
                        );
                        self.export_gpu_encode(
                            &project,
                            &snapshot,
                            assets,
                            range,
                            rate,
                            has_audio,
                            video_rate,
                            container,
                            audio_codec,
                            audio_rate,
                            loudness,
                            output,
                            started,
                            (&device, &capability),
                            &transcode,
                            pipeline_failure,
                            progress,
                        )
                    });
                match attempt {
                    Ok(mut result) => {
                        result["renderCache"] =
                            self.render_cache.as_ref().map_or(Value::Null, |_| {
                                crate::render_cache::unavailable(
                                    "the GPU-resident path does not read frames back",
                                )
                            });
                        result["inputPolicy"] = json!(input_policy.as_str());
                        result["strategy"] = json!("render");
                        result["requestedStrategy"] = json!(strategy.as_str());
                        result["copyIneligible"] = json!(copy_ineligible);
                        return Ok(result);
                    }
                    Err(GpuEncodeOutcome::Unavailable(reason)) => {
                        // A pinned encoder that takes GPU frames only cannot be stood in for by
                        // another path; one the readback path also feeds (`h264_videotoolbox`)
                        // falls back with it.
                        let pinned_gpu_only =
                            requested_video_encoder.is_some() && capability.gpu_input_only;
                        if input_policy == crate::config::VideoInputPolicy::GpuRequired
                            || pinned_gpu_only
                        {
                            return Err(format!(
                                "{}GPU-resident output is required but unavailable: {reason}",
                                crate::protocol::gpu_output_unavailable_tag()
                            ));
                        }
                        alpha2_os::diagnostic!(
                            "alpha2 native export: GPU encode unavailable ({reason}); falling back"
                        );
                        reason
                    }
                    Err(GpuEncodeOutcome::Busy(reason)) => {
                        let busy = alpha2_native::EncoderRefusal {
                            encoder: capability.encoder.to_owned(),
                            hardware_accelerated: true,
                            kind: alpha2_native::OpenFailureKind::Busy,
                            reason: reason.clone(),
                        };
                        let pinned_gpu_only =
                            requested_video_encoder.is_some() && capability.gpu_input_only;
                        if input_policy == crate::config::VideoInputPolicy::GpuRequired
                            || pinned_gpu_only
                        {
                            return Err(crate::encoder_refusal::encoders_refused_error(
                                video_codec,
                                (size.width, size.height),
                                &[],
                                Some(&busy),
                                None,
                            )
                            .unwrap_or(reason));
                        }
                        alpha2_os::diagnostic!(
                            "alpha2 native export: GPU encoder busy ({reason}); falling back"
                        );
                        gpu_busy = Some(busy);
                        reason
                    }
                    Err(GpuEncodeOutcome::Failed(error)) => return Err(error),
                    Err(GpuEncodeOutcome::EncoderRejected { reason, frame }) => {
                        let pinned_gpu_only =
                            requested_video_encoder.is_some() && capability.gpu_input_only;
                        if input_policy == crate::config::VideoInputPolicy::GpuRequired
                            || pinned_gpu_only
                        {
                            // The worker's rejection is recorded as the export's cause; this
                            // refusal, which says why nothing started over, replaces it.
                            pipeline_failure.clear();
                            return Err(code_tag(
                                "ENCODE_ERROR",
                                format!(
                                    "{reason} (GPU-resident output is required, so the export \
                                     was not restarted on the readback path)"
                                ),
                            ));
                        }
                        // One restart: the readback path below never comes back here. The GPU
                        // path's sink is aborted and its partial removed already.
                        alpha2_os::diagnostic!(
                            "alpha2 native export: {}; restarting on the readback path",
                            crate::tag::untagged(&reason)
                        );
                        pipeline_failure.clear();
                        self.failure
                            .enter(crate::failure::ExportPhase::Prepare, None);
                        progress(crate::protocol::Progress::restarted(
                            exporter.progress().total,
                        ))
                        .map_err(|error| error.message)?;
                        let reason = crate::tag::untagged(&reason);
                        gpu_output_path_fallback = json!({
                            "encoder": capability.encoder,
                            "frame": frame,
                            "reason": reason,
                        });
                        format!(
                            "{reason} (the GPU-resident export failed at frame {frame} and was \
                             restarted on the readback path)"
                        )
                    }
                }
            }
            Err(refusal) => {
                if refusal.required || input_policy == crate::config::VideoInputPolicy::GpuRequired
                {
                    return Err(format!(
                        "{}GPU-resident output is required but unavailable: {}",
                        crate::protocol::gpu_output_unavailable_tag(),
                        refusal.reason
                    ));
                }
                refusal.reason
            }
        };
        // The synchronous comparison path decodes on the CPU, so it refuses `hardware-required`
        // before any output opens.
        if !pipeline_enabled && self.decode_policy.requires_hardware() {
            return Err(code_tag(
                "HARDWARE_DECODE_UNAVAILABLE",
                "hardware decode is required, but ALPHA2_EXPORT_PIPELINE is off, which runs the synchronous comparison path: it decodes in software",
            ));
        }
        if !pipeline_enabled && self.renderer.cpu().is_some() {
            return Err(code_tag(
                "INVALID_INPUT",
                "ALPHA2_EXPORT_PIPELINE is off, which runs the synchronous comparison path; that path renders on a GPU only",
            ));
        }
        // The capture and the render cache hold 8-bit 4:2:0 frames.
        let mut frame_capture = configured_frame_capture()
            .filter(|_| pixels.is_yuv420p())
            .map(|path| FrameCapture::create(&path, size, rate, exporter.encoding().chroma))
            .transpose()?;
        // A cache problem never fails an export: one that cannot be opened is not used, and the
        // result says why.
        let mut render_cache_unavailable = (!pixels.is_yuv420p() && self.render_cache.is_some())
            .then(|| "the render cache holds 8-bit 4:2:0 frames".to_owned());
        let mut render_cache = self
            .render_cache
            .as_ref()
            .filter(|_| pixels.is_yuv420p())
            .and_then(|(request, identity)| {
                crate::render_cache::RenderCache::open(
                    request,
                    *identity,
                    crate::render_cache::FrameFormat {
                        width: size.width,
                        height: size.height,
                        encoding: exporter.encoding(),
                    },
                    &cancel,
                )
                .map_err(|reason| {
                    alpha2_os::diagnostic!("alpha2 native export: render cache unused ({reason})");
                    render_cache_unavailable = Some(reason);
                })
                .ok()
            });
        let mut cache_reads = CacheReads::default();
        // Only this codec's encoders, and only now that the GPU-resident path was not taken: an
        // encoder that must be opened to prove it runs here (NVENC) is opened for an export that
        // can use it, and for no other.
        let inventory = alpha2_native::video_encoder_inventory(video_codec);
        let video_encoder = inventory
            .usable
            .into_iter()
            .find(|capability| {
                requested_video_encoder.is_none_or(|name| capability.encoder_name == name)
            })
            .ok_or_else(|| {
                // A busy encoder is a refusal that ends; the rest are this machine's setup.
                let busy = inventory
                    .refused
                    .iter()
                    .filter(|refusal| {
                        refusal.kind == alpha2_native::OpenFailureKind::Busy
                            && requested_video_encoder.is_none_or(|name| refusal.encoder == name)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(busy) = crate::encoder_refusal::encoders_refused_error(
                    video_codec,
                    (size.width, size.height),
                    &busy,
                    gpu_busy.as_ref(),
                    None,
                ) {
                    return busy;
                }
                code_tag(
                    "INVALID_INPUT",
                    match requested_video_encoder {
                        Some(name) if name == alpha2_native::OPENH264_ENCODER_NAME => {
                            let status = alpha2_native::openh264_status();
                            format!(
                                "video encoder {name} is unavailable for {}: OpenH264 is {} ({}){}",
                                video_codec.as_str(),
                                status.as_str(),
                                status.reason().unwrap_or_default(),
                                status
                                    .remedy()
                                    .map(|remedy| format!("; {remedy}"))
                                    .unwrap_or_default()
                            )
                        }
                        Some(name) => format!(
                            "video encoder {name} is unavailable for {}",
                            video_codec.as_str()
                        ),
                        None => alpha2_native::no_video_encoder_message(
                            video_codec,
                            &codec_capabilities().video,
                            Some(&gpu_output_unavailable),
                        ),
                    },
                )
            })?;
        let video_encoder_name = video_encoder.encoder_name.clone();
        let providers = self.export_providers(&project, size);
        let mut synchronous_frames = (!pipeline_enabled)
            .then(|| NativeFrames::new(size, &project, assets, self.failure.decoding()))
            .transpose()?;
        if let Some(frames) = synchronous_frames.as_mut() {
            frames.providers = std::sync::Arc::clone(&providers);
        }
        let mut gpu_frames = self.export_gpu_frames(size, &project);
        let mut cpu_frames = crate::cpu_frames::NativeCpuFrames::new(size);
        cpu_frames.color = gpu_frames.color;
        let packed_bytes_per_frame = packed_yuv420_bytes(size);
        let mut decode_scheduler = DecodeScheduler::new(decode_lookahead, &project);
        let mut audio_sources = has_audio
            .then(|| {
                WindowedAudioSources::new(
                    &project,
                    assets,
                    project.settings.sample_rate,
                    self.memory.audio_window.byte_budget(),
                    self.failure.decoding(),
                )
            })
            .transpose()?;
        let pipeline_limits = export_pipeline_limits(
            size,
            self.renderer.intermediate_format(),
            self.memory.in_flight_budget(),
            configured_frame_depth(),
            configured_encode_channel_depth(),
        )?;

        let video_config = VideoEncodeConfig {
            width: size.width,
            height: size.height,
            frame_rate: rate,
            color: ColorMetadata::EXPORT,
            rate_control: video_rate.control,
            codec: video_codec,
            display: alpha2_native::VideoDisplay::default(),
            container,
            pixels,
        };
        // The split follows the encoder that opens: a hardware encoder can fail `avcodec_open2` and
        // fall through to a software one, which needs a software encoder's share of the CPU.
        let software_split = std::cell::OnceCell::new();
        let decide_software_split = || {
            software_split.get_or_init(|| {
                self.export_codec_split(&project, assets, range, pipeline_enabled, true)
            })
        };
        let software_encoder_threads = || decide_software_split().codec_threads.encoder_threads;
        // Every candidate refused: busy, or this output is too small for each of them.
        let sink_refused = |error: alpha2_media::EncodeError| {
            if let alpha2_media::EncodeError::EncodersRefused { refusals, .. } = &error
                && let Some(refused) = crate::encoder_refusal::encoders_refused_error(
                    video_codec,
                    (size.width, size.height),
                    refusals,
                    gpu_busy.as_ref(),
                    gpu_minimum,
                )
            {
                return refused;
            }
            error_message(error)
        };
        let sink = if has_audio {
            NativeExportSink::AudioVideo(
                NativeAvEncodeSink::create_with_video_encoder_threads(
                    output,
                    AvEncodeConfig {
                        video: video_config,
                        audio_sample_rate: project.settings.sample_rate,
                        audio_channels: 2,
                        audio_codec,
                        audio_bit_rate: audio_rate.bitrate,
                    },
                    requested_video_encoder,
                    &software_encoder_threads,
                )
                .map_err(sink_refused)?,
            )
        } else {
            NativeExportSink::Video(
                NativeVideoEncodeSink::create_with_encoder_threads(
                    output,
                    video_config,
                    requested_video_encoder,
                    &software_encoder_threads,
                )
                .map_err(sink_refused)?,
            )
        };
        let video_encoder_hardware = sink.video_selection().hardware_accelerated;
        let split = if video_encoder_hardware {
            self.export_codec_split(&project, assets, range, pipeline_enabled, false)
        } else {
            decide_software_split().clone()
        };
        let plan = ExportDecodePlan::new(split, providers);
        let hardware_decode = plan.hardware_decode;
        let codec_threads = plan.codec_threads;
        let mut decode_worker = pipeline_enabled
            .then(|| {
                DecodeWorker::spawn(
                    &project,
                    assets,
                    &self.decode_worker_config(&plan),
                    cancel.clone(),
                    pipeline_failure.clone(),
                )
            })
            .transpose()?;
        // What opened, not what was advertised: a candidate can register and still fail to open.
        let selection = sink.video_selection().clone();
        // Frames in the pictures the encoder takes, which for 10-bit 4:2:0 can be `P010`.
        let frame_pool = FramePool::new(size.width, size.height, selection.pixels)?;
        let mut exporter = exporter.with_pixels(selection.pixels);
        self.failure
            .note("videoPipeline", json!(VideoPipeline::Readback.as_str()));
        self.failure
            .note("videoEncoder", json!(selection.encoder_name));
        self.failure.note("audioEncoder", json!(audio_encoder_name));
        self.failure
            .note("hardwareDecodeReady", json!(hardware_decode));
        self.failure.note(
            "pipeline",
            json!({
                "enabled": pipeline_enabled,
                "decodeLookahead": decode_lookahead,
                "readbackDepth": pipeline_limits.readback_depth,
                "encodeChannelDepth": pipeline_limits.encode_channel_depth,
            }),
        );
        alpha2_os::diagnostic!(
            "alpha2 native export encoder: {} ({}, {})",
            selection.encoder_name,
            selection.class.as_str(),
            if selection.hardware_accelerated {
                "on GPU"
            } else {
                "on CPU"
            }
        );
        for fallback in &selection.rejected {
            alpha2_os::diagnostic!(
                "alpha2 native export encoder: passed over {} ({}): {}",
                fallback.encoder_name,
                fallback.reason.as_str(),
                fallback.detail
            );
        }
        let mut encode_worker = EncodeWorker::spawn(
            sink,
            cancel.clone(),
            pipeline_failure.clone(),
            pipeline_limits.encode_channel_depth,
            started,
            crate::config::configured_stall_timeout(),
        )?;

        // The output's zero is the first frame's start, not the requested start, and the audio
        // begins there too.
        let effective_range = exporter.effective_range();
        let video_origin = effective_range.start();
        let sample_rate = project.settings.sample_rate;
        let (audio_start_sample, audio_frames) = if has_audio {
            let window = exporter.audio_window(sample_rate).map_err(error_message)?;
            (window.start_sample, window.frames)
        } else {
            (0, 0)
        };
        let mut audio_renderer = has_audio
            .then(|| AudioRenderer::new(&snapshot, 2, 4_096).map_err(error_message))
            .transpose()?;
        let loudness_report = match (loudness, audio_renderer.as_mut()) {
            (Some(request), Some(renderer)) => Some(normalize_export_audio(
                renderer,
                &snapshot,
                &project,
                assets,
                audio_start_sample,
                audio_frames,
                request,
                &cancel,
                self.memory.audio_window.byte_budget(),
                &self.failure.decoding(),
            )?),
            (Some(_), None) => return Err(loudness_without_audio()),
            (None, _) => None,
        };
        let mut audio_produced = 0_usize;
        let mut encode_state = EncodeProgressState::default();
        let cancel_at_frame = configured_cancel_at_frame();
        let fail_readback_at = crate::config::configured_fail_readback_at_frame();
        let lose_device_at = crate::config::configured_lose_device_at_frame();
        let total_frames = exporter.progress().total;
        let graph_before = self.renderer.gpu().map(Renderer::diagnostics);
        let cpu_before = self.renderer.cpu().map(CpuRenderer::stats);
        let mut pending_media = VecDeque::<PendingMedia>::new();
        let mut pending_videos = 0_usize;
        let mut pending_readback_bytes = 0_u64;
        let mut metrics = ExportMetrics {
            video_pipeline: VideoPipeline::Readback,
            pipeline_enabled,
            decode_lookahead,
            decode_concurrency: codec_threads.decode_concurrency,
            decoder_sessions: Some(plan.decoder_sessions),
            decoder_threads: codec_threads.decoder_threads,
            hardware_decoder_threads: codec_threads.hardware_decoder_threads,
            codec_thread_budget: codec_threads.total_threads,
            codec_threads_allocated: codec_threads.allocated_threads,
            encoder_threads: codec_threads.encoder_threads,
            source_frame_pixels: plan.source_frame_pixels,
            source_probe: plan.source_probe,
            trace: ExportTrace::from_env(started),
            ..ExportMetrics::default()
        };
        if let Some(worker) = decode_worker.as_ref() {
            decode_scheduler.refill(&exporter, &snapshot, worker, &mut metrics)?;
        }

        while exporter.pending_time().is_some()
            || audio_produced < audio_frames
            || !pending_media.is_empty()
        {
            'fill: while pending_videos < pipeline_limits.readback_depth
                && pending_media.len() < MAX_PENDING_MEDIA_EVENTS
                && (exporter.pending_time().is_some() || audio_produced < audio_frames)
            {
                let video_ticks = exporter
                    .pending_time()
                    .map(|time| time.ticks().saturating_sub(video_origin.ticks()));
                let audio_ticks = (audio_produced < audio_frames).then(|| {
                    i64::try_from(audio_produced)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(TICKS_PER_SECOND)
                        .div_euclid(i64::from(sample_rate))
                });
                let take_video = match (video_ticks, audio_ticks) {
                    (Some(video), Some(audio)) => video <= audio,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if take_video {
                    let pending = exporter
                        .pending_time()
                        .ok_or_else(|| "missing export cursor".to_owned())?;
                    let frame_index = exporter.progress().completed;
                    self.failure
                        .enter(crate::failure::ExportPhase::Decode, Some(frame_index));
                    let mut frame_trace = VideoFrameTrace::default();
                    let lookup;
                    if let Some(worker) = decode_worker.as_ref() {
                        let decode_wait_started = Instant::now();
                        if let Some(trace) = metrics.trace.as_ref() {
                            frame_trace.decode_wait_started = trace.now();
                        }
                        let decoded = loop {
                            if matches!(pending_media.front(), Some(PendingMedia::Audio(_))) {
                                break 'fill;
                            }
                            if let Some(decoded) = worker.try_result()? {
                                break decoded;
                            }
                            if let Some(is_video) = ready_pending_media(&mut pending_media)? {
                                metrics.ready_readback_dispatches = metrics
                                    .ready_readback_dispatches
                                    .saturating_add(u64::from(is_video));
                                break 'fill;
                            }
                            drain_encode_worker(
                                &encode_worker,
                                &mut encode_state,
                                total_frames,
                                started,
                                progress,
                                &mut metrics,
                            )?;
                            // This loop multiplexes a readback, encode completions and the decode
                            // pool, so it must not block on any one of them.
                            if let Some(decoded) = worker.result_timeout(WORKER_RETRY_INTERVAL)? {
                                break decoded;
                            }
                        };
                        metrics.decode_wait = metrics
                            .decode_wait
                            .saturating_add(decode_wait_started.elapsed());
                        if let Some(trace) = metrics.trace.as_ref() {
                            frame_trace.decoded = trace.now();
                        }
                        let decoded_bytes = decoded.bytes;
                        let decoded_time = decoded.decode_time;
                        metrics.reverse_cache_hits = metrics
                            .reverse_cache_hits
                            .saturating_add(decoded.reverse_cache_hits);
                        metrics.decoder_session_reopens = metrics
                            .decoder_session_reopens
                            .saturating_add(decoded.decoder_session_reopens);
                        metrics.software_decoded_frames = metrics
                            .software_decoded_frames
                            .saturating_add(decoded.software_decoded_frames);
                        metrics.hardware_decoded_frames = metrics
                            .hardware_decoded_frames
                            .saturating_add(decoded.hardware_decoded_frames);
                        metrics.hardware_fallbacks = metrics
                            .hardware_fallbacks
                            .merged(decoded.hardware_fallbacks);
                        metrics.reverse_windows_built = metrics
                            .reverse_windows_built
                            .saturating_add(decoded.reverse_windows_built);
                        metrics.peak_reverse_cache_bytes = metrics
                            .peak_reverse_cache_bytes
                            .max(decoded.reverse_cache_bytes);
                        if decoded.time != pending {
                            worker.release(decoded_bytes);
                            return Err(format!(
                                "decode worker returned timeline time {} while expecting {}",
                                decoded.time.ticks(),
                                pending.ticks()
                            ));
                        }
                        if let Err(error) =
                            decode_scheduler.accept(&decoded, exporter.progress().completed)
                        {
                            worker.release(decoded_bytes);
                            return Err(error);
                        }
                        metrics.decode = metrics.decode.saturating_add(decoded_time);
                        metrics.decode_stages = metrics.decode_stages.merged(decoded.stage_timings);
                        metrics.peak_decode_queue_bytes =
                            metrics.peak_decode_queue_bytes.max(worker.peak_bytes());
                        // A frame the cache holds is not rendered, so its sources are not uploaded:
                        // an upload is a queue write whose staging only a render's submission
                        // frees.
                        let lookup_started = Instant::now();
                        lookup = render_cache_lookup(
                            render_cache.as_mut(),
                            &mut cache_reads,
                            &exporter,
                            &snapshot,
                            pending,
                        )?;
                        metrics.render_cache_lookup = metrics
                            .render_cache_lookup
                            .saturating_add(lookup_started.elapsed());
                        if matches!(lookup, CacheLookup::Hit(_)) {
                            if self.renderer.cpu().is_some() {
                                cpu_frames.pass(decoded);
                            } else {
                                gpu_frames.pass(decoded);
                            }
                            worker.release(decoded_bytes);
                        } else {
                            self.failure
                                .enter(crate::failure::ExportPhase::Upload, Some(frame_index));
                            let upload = match &self.renderer {
                                RendererBackend::Gpu(renderer) => {
                                    gpu_frames.install(renderer, decoded)
                                }
                                RendererBackend::Cpu(renderer) => {
                                    cpu_frames.install(renderer, decoded)
                                }
                            };
                            worker.release(decoded_bytes);
                            let upload = upload?;
                            metrics.source_prepare =
                                metrics.source_prepare.saturating_add(upload.duration);
                            metrics.record_install(&upload);
                            if let Some(trace) = metrics.trace.as_ref() {
                                frame_trace.installed = trace.now();
                            }
                        }
                    } else {
                        let lookup_started = Instant::now();
                        lookup = render_cache_lookup(
                            render_cache.as_mut(),
                            &mut cache_reads,
                            &exporter,
                            &snapshot,
                            pending,
                        )?;
                        metrics.render_cache_lookup = metrics
                            .render_cache_lookup
                            .saturating_add(lookup_started.elapsed());
                    }
                    if decode_worker.is_none() && !matches!(lookup, CacheLookup::Hit(_)) {
                        let evaluate_started = Instant::now();
                        let needs = snapshot.frame_needs(pending).map_err(error_message)?;
                        metrics.evaluate =
                            metrics.evaluate.saturating_add(evaluate_started.elapsed());
                        let source_metrics = synchronous_frames
                            .as_mut()
                            .ok_or_else(|| "synchronous frame cache disappeared".to_owned())?
                            .supply(self.gpu_renderer(), &needs)?;
                        metrics.decode = metrics.decode.saturating_add(source_metrics.decode);
                        metrics.source_prepare = metrics
                            .source_prepare
                            .saturating_add(source_metrics.convert_upload);
                        metrics.direct_yuv_frames = metrics
                            .direct_yuv_frames
                            .saturating_add(source_metrics.direct_yuv_frames);
                        metrics.rgba_fallback_frames = metrics
                            .rgba_fallback_frames
                            .saturating_add(source_metrics.rgba_fallback_frames);
                        metrics.physical_source_reuses = metrics
                            .physical_source_reuses
                            .saturating_add(source_metrics.physical_reuses);
                        metrics.software_decoded_frames = metrics
                            .software_decoded_frames
                            .saturating_add(source_metrics.software_decoded_frames);
                    }
                    let bake = match lookup {
                        CacheLookup::Hit(frame) => {
                            let (index, time) = exporter
                                .take_next()
                                .map_err(error_message)?
                                .ok_or_else(|| "missing export cursor".to_owned())?;
                            frame_trace.index = index;
                            pending_media.push_back(PendingMedia::Baked {
                                index,
                                time,
                                frame,
                                trace: Box::new(frame_trace),
                            });
                            pending_videos = pending_videos.saturating_add(1);
                            if let Some(worker) = decode_worker.as_ref() {
                                decode_scheduler.refill(
                                    &exporter,
                                    &snapshot,
                                    worker,
                                    &mut metrics,
                                )?;
                            }
                            continue 'fill;
                        }
                        CacheLookup::Miss(bake) => bake,
                    };
                    self.failure
                        .enter(crate::failure::ExportPhase::Render, Some(frame_index));
                    let frame_provider: &dyn FrameProvider = if self.renderer.cpu().is_some() {
                        &cpu_frames
                    } else if pipeline_enabled {
                        &gpu_frames
                    } else {
                        synchronous_frames
                            .as_ref()
                            .ok_or_else(|| "synchronous frame cache disappeared".to_owned())?
                    };
                    let mut texts = self
                        .texts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(time) = exporter.pending_time() {
                        texts
                            .prepare_at(self.renderer.text_upload(), &snapshot, time, size)
                            .map_err(error_message)?;
                    }
                    let frame_provider = FramesWithText::new(frame_provider, &texts);
                    let render_started = Instant::now();
                    let mut step_timings = ExportStepTimings::default();
                    if let RendererBackend::Cpu(renderer) = &self.renderer {
                        // Drawn straight into the encoder frame: nothing to read back.
                        let mut frame = frame_pool.take()?;
                        let step = {
                            let buffer = frame.buffer_mut();
                            let mut planes = buffer.begin_write_planes().map_err(error_message)?;
                            let mut targets: Vec<alpha2_render::PlaneTarget<'_>> = planes
                                .iter_mut()
                                .map(|(bytes, stride)| {
                                    alpha2_render::PlaneTarget::new(bytes, *stride)
                                })
                                .collect();
                            exporter
                                .render_next_into_planes(
                                    &snapshot,
                                    &**renderer,
                                    &frame_provider,
                                    &mut targets,
                                    Some(&mut step_timings),
                                )
                                .map_err(error_message)?
                        };
                        metrics.frame_submit_stages =
                            metrics.frame_submit_stages.merged(step_timings);
                        metrics.frame_submit = metrics
                            .frame_submit
                            .saturating_add(render_started.elapsed());
                        match step {
                            alpha2_engine::ExportPlaneStep::Rendered { index, time } => {
                                let bake = bake.filter(|_| {
                                    render_cache
                                        .as_mut()
                                        .is_some_and(crate::render_cache::RenderCache::accepts)
                                });
                                let mut baked = bake.as_ref().map(|_| BakedFrame {
                                    y: Vec::new(),
                                    u: Vec::new(),
                                    v: Vec::new(),
                                });
                                crate::media::finish_drawn(
                                    frame.buffer_mut(),
                                    frame_capture.as_mut(),
                                    baked.as_mut(),
                                )?;
                                if let (Some(cache), Some((key, pinned)), Some(baked)) =
                                    (render_cache.as_mut(), bake, baked)
                                {
                                    cache.write(key, baked, pinned);
                                }
                                if let Some(trace) = metrics.trace.as_ref() {
                                    frame_trace.submitted = trace.now();
                                }
                                frame_trace.index = index;
                                pending_media.push_back(PendingMedia::Rendered {
                                    index,
                                    time,
                                    frame,
                                    trace: Box::new(frame_trace),
                                });
                                pending_videos = pending_videos.saturating_add(1);
                                if let Some(worker) = decode_worker.as_ref() {
                                    decode_scheduler.refill(
                                        &exporter,
                                        &snapshot,
                                        worker,
                                        &mut metrics,
                                    )?;
                                }
                            }
                            alpha2_engine::ExportPlaneStep::Waiting {
                                missing_sources, ..
                            } => {
                                return Err(crate::protocol::missing_input(format!(
                                    "native export is missing sources: {missing_sources:?}"
                                )));
                            }
                            alpha2_engine::ExportPlaneStep::Complete => {}
                        }
                        continue 'fill;
                    }
                    // Named in the report of a loss this frame's submission causes.
                    self.gpu_renderer()
                        .mark_frame(Some(alpha2_render::FrameMark {
                            time_ticks: exporter.pending_time().map_or(0, MediaTime::ticks),
                            frame_index: Some(frame_index),
                        }));
                    let step = exporter
                        .render_next_timed(
                            &snapshot,
                            self.gpu_renderer(),
                            &frame_provider,
                            &mut step_timings,
                        )
                        .map_err(error_message)?;
                    metrics.frame_submit_stages = metrics.frame_submit_stages.merged(step_timings);
                    match step {
                        ExportStep::Rendered {
                            index,
                            time,
                            readback,
                        } => {
                            metrics.frame_submit = metrics
                                .frame_submit
                                .saturating_add(render_started.elapsed());
                            pending_readback_bytes =
                                pending_readback_bytes.saturating_add(readback.staging_bytes());
                            metrics.peak_readback_bytes =
                                metrics.peak_readback_bytes.max(pending_readback_bytes);
                            if let Some(trace) = metrics.trace.as_ref() {
                                frame_trace.submitted = trace.now();
                            }
                            frame_trace.index = index;
                            pending_media.push_back(PendingMedia::Video {
                                index,
                                time,
                                readback: PendingVideoReadback::new(readback),
                                trace: Box::new(frame_trace),
                                bake,
                            });
                            pending_videos = pending_videos.saturating_add(1);
                            metrics.max_in_flight_readbacks =
                                metrics.max_in_flight_readbacks.max(pending_videos);
                            if let Some(worker) = decode_worker.as_ref() {
                                decode_scheduler.refill(
                                    &exporter,
                                    &snapshot,
                                    worker,
                                    &mut metrics,
                                )?;
                            }
                        }
                        ExportStep::Waiting {
                            missing_sources, ..
                        } => {
                            return Err(crate::protocol::missing_input(format!(
                                "native export is missing sources: {missing_sources:?}"
                            )));
                        }
                        ExportStep::Complete => {}
                    }
                } else {
                    if let Some(is_video) = ready_pending_media(&mut pending_media)? {
                        metrics.ready_readback_dispatches = metrics
                            .ready_readback_dispatches
                            .saturating_add(u64::from(is_video));
                        break 'fill;
                    }
                    self.failure
                        .enter(crate::failure::ExportPhase::Decode, None);
                    let count = (audio_frames - audio_produced).min(4_096);
                    let renderer = audio_renderer
                        .as_mut()
                        .ok_or_else(|| "audio renderer was not initialized".to_owned())?;
                    let sources = audio_sources
                        .as_mut()
                        .ok_or_else(|| "audio sources were not initialized".to_owned())?;
                    let absolute = audio_start_sample
                        .saturating_add(i64::try_from(audio_produced).unwrap_or(i64::MAX));
                    let needs = renderer
                        .audio_needs(&snapshot, absolute, count)
                        .map_err(error_message)?;
                    let decode_started = Instant::now();
                    sources.prepare(&needs, &cancel)?;
                    metrics.audio_decode = metrics
                        .audio_decode
                        .saturating_add(decode_started.elapsed());
                    metrics.peak_audio_window_bytes =
                        metrics.peak_audio_window_bytes.max(sources.peak_bytes());
                    let mix_started = Instant::now();
                    let status = renderer
                        .render(&snapshot, absolute, count, sources, TransportState::Playing)
                        .map_err(error_message)?;
                    metrics.audio_mix = metrics.audio_mix.saturating_add(mix_started.elapsed());
                    if !status.missing_sources.is_empty() {
                        return Err(crate::protocol::missing_input(format!(
                            "native audio mix is missing sources: {:?}",
                            status.missing_sources
                        )));
                    }
                    let mut block = renderer.block(&status);
                    block.start_sample = i64::try_from(audio_produced).unwrap_or(i64::MAX);
                    pending_media.push_back(PendingMedia::Audio(block));
                    audio_produced += count;
                    if exporter.pending_time().is_none() && pending_videos == 0 {
                        break;
                    }
                }
                metrics.max_pending_media = metrics.max_pending_media.max(pending_media.len());
                metrics.pending_depth_total = metrics
                    .pending_depth_total
                    .saturating_add(u64::try_from(pending_media.len()).unwrap_or(u64::MAX));
                metrics.pending_depth_samples = metrics.pending_depth_samples.saturating_add(1);
            }

            let pending = pending_media
                .pop_front()
                .ok_or_else(|| "export scheduler made no progress".to_owned())?;
            let mut prepared: Option<(PooledFrame, u64, MediaTime)> = None;
            let (event, video_index, mut frame_trace) = match pending {
                PendingMedia::Rendered {
                    index,
                    time,
                    frame,
                    trace,
                } => {
                    pending_videos = pending_videos.saturating_sub(1);
                    prepared = Some((
                        frame,
                        index,
                        MediaTime::from_ticks(time.ticks().saturating_sub(video_origin.ticks())),
                    ));
                    (None, Some(index), Some(*trace))
                }
                PendingMedia::Baked {
                    index,
                    time,
                    frame: baked,
                    trace,
                } => {
                    self.failure
                        .enter(crate::failure::ExportPhase::Readback, Some(index));
                    let mut frame = frame_pool.take()?;
                    unpack_baked(&baked, frame.buffer_mut(), frame_capture.as_mut())?;
                    metrics.output_cpu_pack_bytes = metrics
                        .output_cpu_pack_bytes
                        .saturating_add(packed_bytes_per_frame);
                    pending_videos = pending_videos.saturating_sub(1);
                    prepared = Some((
                        frame,
                        index,
                        MediaTime::from_ticks(time.ticks().saturating_sub(video_origin.ticks())),
                    ));
                    (None, Some(index), Some(*trace))
                }
                PendingMedia::Video {
                    index,
                    time,
                    mut readback,
                    trace,
                    bake,
                } => {
                    let mut trace = *trace;
                    self.failure
                        .enter(crate::failure::ExportPhase::Readback, Some(index));
                    if fail_readback_at == Some(index) {
                        return Err(error_message(alpha2_render::RenderError::TextureReadback(
                            alpha2_render::ReadbackFailure::MapFailed,
                        )));
                    }
                    let staging_bytes = readback.staging_bytes();
                    let readback_started = Instant::now();
                    if let Some(sink) = metrics.trace.as_ref() {
                        trace.readback_wait_started = sink.now();
                    }
                    readback.wait_ready()?;
                    if lose_device_at == Some(index) {
                        let renderer = self.gpu_renderer();
                        return Err(error_message(
                            renderer.classify_loss(alpha2_render::RenderError::GpuWorkDropped),
                        ));
                    }
                    metrics.readback_wait = metrics
                        .readback_wait
                        .saturating_add(readback_started.elapsed());
                    metrics.readback_map_latency = metrics
                        .readback_map_latency
                        .saturating_add(readback.mapping_latency());
                    let pack_started = Instant::now();
                    if let Some(sink) = metrics.trace.as_ref() {
                        trace.readback_ready = sink.now();
                    }
                    let mut frame = frame_pool.take()?;
                    let bake = bake.filter(|_| {
                        render_cache
                            .as_mut()
                            .is_some_and(crate::render_cache::RenderCache::accepts)
                    });
                    let mut baked = bake.as_ref().map(|_| BakedFrame {
                        y: Vec::new(),
                        u: Vec::new(),
                        v: Vec::new(),
                    });
                    readback.finish_into(
                        frame.buffer_mut(),
                        frame_capture.as_mut(),
                        baked.as_mut(),
                    )?;
                    if let (Some(cache), Some((key, pinned)), Some(baked)) =
                        (render_cache.as_mut(), bake, baked)
                    {
                        cache.write(key, baked, pinned);
                    }
                    metrics.readback_pack =
                        metrics.readback_pack.saturating_add(pack_started.elapsed());
                    metrics.output_readback_bytes =
                        metrics.output_readback_bytes.saturating_add(staging_bytes);
                    metrics.output_cpu_pack_bytes = metrics
                        .output_cpu_pack_bytes
                        .saturating_add(packed_bytes_per_frame);
                    if let Some(sink) = metrics.trace.as_ref() {
                        trace.packed = sink.now();
                    }
                    pending_readback_bytes = pending_readback_bytes.saturating_sub(staging_bytes);
                    pending_videos = pending_videos.saturating_sub(1);
                    prepared = Some((
                        frame,
                        index,
                        MediaTime::from_ticks(time.ticks().saturating_sub(video_origin.ticks())),
                    ));
                    (None, Some(index), Some(trace))
                }
                PendingMedia::Audio(block) => (Some(EncodeEvent::Audio(block)), None, None),
            };
            let event_bytes = event
                .as_ref()
                .map_or(packed_bytes_per_frame, encode_event_bytes);
            self.failure
                .enter(crate::failure::ExportPhase::Encode, video_index);
            let encode_started = Instant::now();
            let backpressured = match (event, prepared) {
                (Some(event), _) => {
                    encode_worker.send(EncodeCommand::Event { event, video_index })?
                }
                (None, Some((frame, video_index, pts))) => {
                    encode_worker.send(EncodeCommand::PreparedVideo {
                        frame,
                        video_index,
                        pts,
                    })?
                }
                (None, None) => return Err("export scheduler produced no media".to_owned()),
            };
            metrics.encode_queue = metrics
                .encode_queue
                .saturating_add(encode_started.elapsed());
            metrics.encode_backpressure = metrics
                .encode_backpressure
                .saturating_add(u64::from(backpressured));
            if let Some(sink) = metrics.trace.as_mut() {
                let queued = sink.now();
                match frame_trace.take() {
                    Some(mut row) => {
                        row.encode_queued = queued;
                        sink.push_video(row);
                    }
                    None => sink.push_audio(AudioBlockTrace {
                        queued,
                        ..AudioBlockTrace::default()
                    }),
                }
            }
            encode_state.pending_bytes = encode_state.pending_bytes.saturating_add(event_bytes);
            encode_state.pending_events = encode_state.pending_events.saturating_add(1);
            metrics.peak_encode_queue_bytes = metrics
                .peak_encode_queue_bytes
                .max(encode_state.pending_bytes);
            metrics.max_encode_queue = metrics.max_encode_queue.max(encode_state.pending_events);
            drain_encode_worker(
                &encode_worker,
                &mut encode_state,
                total_frames,
                started,
                progress,
                &mut metrics,
            )?;
            if cancel_at_frame.is_some_and(|frame| encode_state.completed_video >= frame)
                || crate::termination::requested().is_some()
            {
                cancel.cancel();
                return Err(cancelled_export(
                    self.renderer.gpu(),
                    encode_state.completed_video,
                ));
            }
        }

        if let Some(worker) = decode_worker.as_mut() {
            if decode_scheduler.outstanding != 0 {
                return Err(format!(
                    "decode worker still has {} outstanding frame sets",
                    decode_scheduler.outstanding
                ));
            }
            metrics.record_decode_pool(worker);
            worker.finish()?;
        }
        if let Some(capture) = frame_capture.take() {
            capture.finish()?;
        }
        self.failure.enter(crate::failure::ExportPhase::Mux, None);
        progress(crate::protocol::Progress::finalizing(
            encode_state.completed_video,
            total_frames,
        ))
        .map_err(|error| error.message)?;
        let finish_started = Instant::now();
        let _ = encode_worker.send(EncodeCommand::Finish)?;
        loop {
            let event = encode_worker.result()?;
            if handle_encode_worker_event(
                event,
                &mut encode_state,
                total_frames,
                started,
                &mut crate::protocol::finalizing(progress),
                &mut metrics,
            )? {
                break;
            }
        }
        encode_worker.join_success()?;
        metrics.encode_worker_idle = encode_worker.idle.total();
        metrics.encode_channel_depth_samples = encode_worker.depth.samples();
        metrics.mux_finish = finish_started.elapsed();
        // A profiling file must not fail an export whose output is already written.
        if let Some(trace) = metrics.trace.as_ref() {
            match trace.write() {
                Ok(path) => {
                    alpha2_os::diagnostic!("alpha2 native export trace: {}", path.display());
                }
                Err(error) => alpha2_os::diagnostic!("alpha2 native export trace failed: {error}"),
            }
        }
        // Strict mode must not pass on a run that silently decoded in software. Opening a hardware
        // session is not evidence that any frame came from one.
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        if self.decode_policy.requires_hardware() && metrics.software_decoded_frames > 0 {
            return Err(format!(
                "hardware decode is required but {} of {} video frames decoded in software ({})",
                metrics.software_decoded_frames,
                metrics
                    .software_decoded_frames
                    .saturating_add(metrics.hardware_decoded_frames),
                fallback_summary(&metrics)
            ));
        }
        metrics.retired_leases_at_exit =
            self.clean_up_after_output(&output.path, || parked_leases.release());
        metrics.encoder_frames_allocated = frame_pool.allocated();
        let mut result = self.response_metadata(started.elapsed().as_secs_f64() * 1_000.0);
        result["frames"] = json!(encode_state.completed_video);
        result["audio"] = json!(has_audio);
        result["videoCodec"] = json!(video_codec.as_str());
        result["videoEncoder"] = json!(selection.encoder_name);
        result["videoEncoderClass"] = json!(selection.class.as_str());
        result["videoEncoderHardware"] = json!(selection.hardware_accelerated);
        result["videoEncoderLibrary"] =
            crate::protocol::encoder_library_json(&selection.encoder_name);
        result["videoEncoderPreferred"] = json!(video_encoder_name);
        result["videoEncoderFallbacks"] = json!(
            selection
                .rejected
                .iter()
                .map(|fallback| json!({
                    "encoder": fallback.encoder_name,
                    "reason": fallback.reason.as_str(),
                    "detail": fallback.detail,
                }))
                .collect::<Vec<_>>()
        );
        result["videoPipeline"] = json!(VideoPipeline::Readback.as_str());
        result["gpuOutputPathUnavailable"] = json!(gpu_output_unavailable);
        result["gpuOutputPathFallback"] = gpu_output_path_fallback;
        result["publish"] = publish_json(metrics.publish);
        result["videoTranscode"] = Value::Null;
        result["yuvTranscodeIneligible"] = json!("only the GPU-resident path transcodes");
        result["inputPolicy"] = json!(input_policy.as_str());
        result["strategy"] = json!("render");
        result["requestedStrategy"] = json!(strategy.as_str());
        result["copyIneligible"] = json!(copy_ineligible);
        result["effectiveRange"] = effective_range_json(effective_range);
        result["color"] = self.color_pipeline_json(project.settings.color);
        result["renderCache"] = match (render_cache, render_cache_unavailable) {
            (Some(cache), _) => cache.finish(),
            (None, Some(reason)) => crate::render_cache::unavailable(&reason),
            (None, None) => Value::Null,
        };
        result["loudness"] = finish_loudness(loudness_report, audio_renderer.as_ref());
        result["videoRateControl"] = video_rate.to_json(&selection);
        result["videoDecodeMode"] = json!(self.decode_policy.as_str());
        result["videoDecodeBackend"] = json!(alpha2_native::hardware_decode_backend());
        result["hardwareDecodeReady"] = json!(hardware_decode);
        result["hardwareDecodeUnavailable"] = json!(self.hardware_decode_unavailable);
        result["videoThreadPolicy"] = json!(codec_thread_policy(video_encoder_hardware));
        result["decoderThreadPolicy"] = json!(decoder_thread_policy(
            hardware_decode,
            !video_encoder_hardware
        ));
        result["videoThreadCount"] = json!(selection.software_threads);
        result["audioCodec"] = json!(has_audio.then(|| audio_codec.as_str()));
        result["audioRateControl"] = if has_audio {
            audio_rate.to_json()
        } else {
            Value::Null
        };
        result["audioEncoder"] = json!(audio_encoder_name);
        result["fps"] = json!(frames_per_second(
            encode_state.completed_video,
            started.elapsed().as_secs_f64()
        ));
        let graph_after = self.renderer.gpu().map(Renderer::diagnostics);
        metrics.peak_provider_source_bytes = plan.providers.peak_bytes();
        metrics.provider_source_reopens = plan.providers.reopens();
        self.failure
            .decoding()
            .note_decoder_sessions(plan.decoder_sessions, metrics.decoder_session_reopens);
        result["performance"] = export_metrics_json(
            &metrics,
            graph_before.as_ref().zip(graph_after.as_ref()),
            &pipeline_limits,
        );
        if let (Some(before), Some(after)) =
            (cpu_before, self.renderer.cpu().map(CpuRenderer::stats))
        {
            result["performance"]["cpuRenderMs"] = json!(
                std::time::Duration::from_micros(after.render_micros - before.render_micros)
                    .as_secs_f64()
                    * 1_000.0
            );
            result["performance"]["cpuRenderNodes"] = json!(after.nodes - before.nodes);
            result["performance"]["cpuPresentedSources"] =
                json!(after.presented_sources - before.presented_sources);
            result["performance"]["cpuImageHighWaterBytes"] = json!(after.image_high_water_bytes);
        }

        Ok(result)
    }

    /// The color pipeline an export ran: the working space and the
    /// intermediate it was stored in, how HDR sources came in, and the output.
    pub(crate) fn color_pipeline_json(&self, color: alpha2_model::ColorPipeline) -> Value {
        let half = self.renderer.intermediate_format() == wgpu::TextureFormat::Rgba16Float;
        json!({
            "workingSpace": "linear_bt709_sdr",
            "intermediateFormat": if half { "rgba16float" } else { "rgba8unorm" },
            "hdrSources": color.hdr,
            "hdrPeakNits": color.hdr_peak_nits,
            "output": "bt709_sdr",
        })
    }

    /// The fields every response carries: render time, adapter, `FFmpeg`
    /// runtime, and the renderer's pool counters.
    pub fn response_metadata(&self, render_ms: f64) -> Value {
        let RendererBackend::Gpu(renderer) = &self.renderer else {
            let adapter = self.renderer.adapter();
            let ffmpeg = ffmpeg_runtime_identity();
            return json!({
                "renderMs": render_ms,
                "renderer": "cpu",
                "rendererReason": self.renderer_reason,
                "rendererThreads": self.renderer.cpu().map(CpuRenderer::threads),
                "adapter": adapter.name,
                "backend": adapter.backend,
                "driver": adapter.driver,
                "driverInfo": adapter.driver_info,
                "shaderCompiler": adapter.shader_compiler,
                "ffmpegVersion": ffmpeg.version,
                "ffmpegConfiguration": ffmpeg.configuration,
                "ffmpegPinnedSdkRelease": ffmpeg.pinned_sdk_release,
                "ffmpegPinnedSdkMismatch": ffmpeg.pinned_sdk_mismatch(),
                "timestampQueries": false,
                "tier": "A",
                "memory": self.memory_json(),
            });
        };
        let diagnostics = renderer.diagnostics();
        let ffmpeg = ffmpeg_runtime_identity();
        json!({
            "renderMs": render_ms,
            "renderer": "gpu",
            "rendererReason": self.renderer_reason,
            "adapter": diagnostics.adapter.name,
            "backend": diagnostics.adapter.backend,
            "driver": diagnostics.adapter.driver,
            "driverInfo": diagnostics.adapter.driver_info,
            "shaderCompiler": crate::renderer_backend::describe_shader_compiler(
                &diagnostics.adapter.shader_compiler
            ),
            "ffmpegVersion": ffmpeg.version,
            "ffmpegConfiguration": ffmpeg.configuration,
            "ffmpegPinnedSdkRelease": ffmpeg.pinned_sdk_release,
            "ffmpegPinnedSdkMismatch": ffmpeg.pinned_sdk_mismatch(),
            "timestampQueries": diagnostics.adapter.timestamp_queries,
            "tier": match diagnostics.adapter.tier {
                CapabilityTier::TierA => "A",
                CapabilityTier::TierB => "B",
                _ => "unknown",
            },
            "textures": {
                "created": diagnostics.textures.created,
                "reused": diagnostics.textures.reused,
                "active": diagnostics.textures.active,
                "activeBytes": diagnostics.textures.active_bytes,
                "highWaterBytes": diagnostics.textures.high_water_bytes,
                "available": diagnostics.textures.available,
                "availableBytes": diagnostics.textures.available_bytes,
                "budgetBytes": diagnostics.textures.budget_bytes,
                "evicted": diagnostics.textures.evicted,
            },
            "uploadTextures": {
                "created": diagnostics.upload_textures.created,
                "reused": diagnostics.upload_textures.reused,
                "discarded": diagnostics.upload_textures.discarded,
                "active": diagnostics.upload_textures.active,
                "activeBytes": diagnostics.upload_textures.active_bytes,
                "highWaterBytes": diagnostics.upload_textures.high_water_bytes,
                "available": diagnostics.upload_textures.available,
                "availableBytes": diagnostics.upload_textures.available_bytes,
                "budgetBytes": diagnostics.upload_textures.budget_bytes,
            },
            "readbackBuffers": {
                "created": diagnostics.readback_buffers.created,
                "reused": diagnostics.readback_buffers.reused,
                "discarded": diagnostics.readback_buffers.discarded,
                "active": diagnostics.readback_buffers.active,
                "activeBytes": diagnostics.readback_buffers.active_bytes,
                "highWaterBytes": diagnostics.readback_buffers.high_water_bytes,
                "available": diagnostics.readback_buffers.available,
                "availableBytes": diagnostics.readback_buffers.available_bytes,
            },
            "uniformBuffers": {
                "created": diagnostics.uniform_buffers.created,
                "reused": diagnostics.uniform_buffers.reused,
                "discarded": diagnostics.uniform_buffers.discarded,
                "active": diagnostics.uniform_buffers.active,
                "activeBytes": diagnostics.uniform_buffers.active_bytes,
                "highWaterBytes": diagnostics.uniform_buffers.high_water_bytes,
                "available": diagnostics.uniform_buffers.available,
                "availableBytes": diagnostics.uniform_buffers.available_bytes,
            },
            "framesPresented": diagnostics.presented_frames,
            "framesDropped": diagnostics.dropped_frames,
        })
    }
}

/// Releases decoder surfaces parked by the last submission, however an export
/// ends.
///
/// The queue callback parks a lease instead of dropping it, and only a later
/// submission drains the park. After the final frame there is no later
/// submission, so on success, on error and on cancel the surfaces, their
/// budget reservations and the `FFmpeg` frames context they pin would stay
/// resident until the next export's first submit, or forever if the device
/// was lost and the completion callbacks never ran.
struct ParkedLeaseGuard<'a> {
    renderer: Option<&'a Renderer>,
}

impl ParkedLeaseGuard<'_> {
    /// Drains now and reports how many leases were still parked.
    ///
    /// # Errors
    /// Returns an error if the device cannot be polled.
    fn release(&self) -> Result<usize, String> {
        self.renderer.map_or(Ok(0), |renderer| {
            renderer.reclaim_external_leases().map_err(error_message)
        })
    }
}

impl Drop for ParkedLeaseGuard<'_> {
    fn drop(&mut self) {
        // The success path has already drained; this is the error and unwind path, where there is
        // nothing useful to do with a failure.
        if self.release().is_err() {
            alpha2_os::diagnostic!(
                "alpha2 native export could not release decoder surfaces at exit"
            );
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Source files an export's decode plan opened again to read their
    /// frame size, on this thread.
    pub(crate) static SOURCE_REPROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Luma samples in the largest video source this project uses.
///
/// A software decode session is sized to the frames it will actually produce,
/// which the project model does not carry. `conformed` has them as the
/// export's conform probed them, which saves opening every file a second
/// time, one after another, before the first frame; without it each video
/// container is probed here. A source with no size is skipped rather than
/// failing here: opening its decoder later reports the real error, and a
/// missing size only costs that session the minimum worker count.
pub(crate) fn max_video_source_pixels(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    conformed: Option<&HashMap<String, u64>>,
) -> u64 {
    project
        .sources
        .iter()
        .filter(|source| source.kind == MediaKind::Video)
        .filter_map(|source| {
            if let Some(conformed) = conformed {
                return conformed.get(source.id.as_str()).copied();
            }
            let path = assets.get(source.id.as_str())?;
            #[cfg(test)]
            SOURCE_REPROBES.with(|count| count.set(count.get() + 1));
            let probed = alpha2_native::probe(path).ok()?;
            let (width, height) = probed.selected_video_stream()?.dimensions?;
            Some(u64::from(width).saturating_mul(u64::from(height)))
        })
        .max()
        .unwrap_or(0)
}

/// Why an export asked to normalize its loudness has nothing to normalize:
/// it carries no audio, because the request left it out (which the protocol
/// refuses first) or because no source the project uses is audible. The
/// caller's input, like a `loudness` without `includeAudio`.
pub(crate) fn loudness_without_audio() -> String {
    code_tag(
        "INVALID_INPUT",
        "loudness normalization needs the export's audio, and no source the project uses is audible",
    )
}

/// Declines the GPU-resident path for the diagnostic switches that need the
/// readback path, in their own words.
#[cfg(feature = "hwencode")]
fn readback_only_switches(pipeline_enabled: bool) -> Result<(), String> {
    // The capture reads what the readback path packs.
    if configured_frame_capture().is_some() {
        return Err("ALPHA2_EXPORT_FRAMES_Y4M captures the readback path's frames".to_owned());
    }
    // The synchronous comparison path is the readback path decoding on the coordinator. This path
    // always decodes ahead on its worker, so it declines rather than ignore the switch.
    if !pipeline_enabled {
        return Err(
            "ALPHA2_EXPORT_PIPELINE is off, which runs the readback path's synchronous comparison"
                .to_owned(),
        );
    }
    Ok(())
}

/// Reports what a cancelled export released, and names the frame it stopped at.
///
/// The transactional contract is kept by the sinks' own teardown; this is the
/// part a test can observe from outside the process.
pub(crate) fn cancelled_export(renderer: Option<&Renderer>, completed: u64) -> String {
    let retired = renderer.map_or(Some(0), |renderer| renderer.reclaim_external_leases().ok());
    alpha2_os::diagnostic!(
        "alpha2 native export stopped after {completed} frames: {} parked decoder surfaces released",
        retired.map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
    );
    crate::protocol::cancelled(format!("native export was cancelled at frame {completed}"))
}

/// The first pass of a normalized export: mixes the export's audio range
/// with `renderer` metering what reaches its limiter, plans the gain that
/// meets `request` without reaching the limiter, and leaves `renderer` reset
/// with that gain applied and its output metered for the export itself.
/// Sources are decoded by a pass of their own.
///
/// Returns the measurement and the plan as the export result's `loudness`,
/// which [`finish_loudness`] completes with what the export put out.
#[allow(clippy::too_many_arguments)]
pub(crate) fn normalize_export_audio(
    renderer: &mut AudioRenderer,
    snapshot: &EngineProject,
    project: &alpha2_model::Project,
    assets: &HashMap<String, PathBuf>,
    start_sample: i64,
    frames: usize,
    request: LoudnessRequest,
    cancel: &CancelToken,
    audio_window_bytes: crate::memory::ByteBudget,
    decoding: &crate::recovery::ExportDecoding,
) -> Result<Value, String> {
    let mut sources = WindowedAudioSources::new(
        project,
        assets,
        project.settings.sample_rate,
        audio_window_bytes,
        std::sync::Arc::new(decoding.unreported()),
    )?;
    renderer
        .set_loudness_metering(true)
        .map_err(error_message)?;
    let mut measured = 0_usize;
    while measured < frames {
        let count = (frames - measured).min(4_096);
        let absolute = start_sample.saturating_add(i64::try_from(measured).unwrap_or(i64::MAX));
        let needs = renderer
            .audio_needs(snapshot, absolute, count)
            .map_err(error_message)?;
        sources.prepare(&needs, cancel)?;
        let status = renderer
            .render(snapshot, absolute, count, &sources, TransportState::Playing)
            .map_err(error_message)?;
        if !status.missing_sources.is_empty() {
            return Err(crate::protocol::missing_input(format!(
                "loudness measurement is missing sources: {:?}",
                status.missing_sources
            )));
        }
        measured += count;
    }
    let report = renderer
        .loudness_report()
        .ok_or_else(|| "the loudness meter was not running".to_owned())?;
    renderer
        .set_loudness_metering(false)
        .map_err(error_message)?;
    renderer.reset();
    let limiter_threshold = renderer.limiter_threshold();
    let plan = alpha2_audio::LoudnessNormalization::plan(
        &report,
        request.target_lufs,
        request.true_peak_ceiling_dbtp,
        limiter_threshold,
    );
    if let Some(plan) = plan {
        renderer
            .set_normalization_gain_db(plan.gain_db)
            .map_err(error_message)?;
    }
    renderer
        .set_output_loudness_metering(true)
        .map_err(error_message)?;
    Ok(json!({
        "targetLufs": request.target_lufs,
        "truePeakCeilingDbtp": request.true_peak_ceiling_dbtp,
        "limiterThreshold": limiter_threshold,
        "measured": {
            "integratedLufs": report.integrated_lufs,
            "loudnessRangeLu": report.loudness_range_lu,
            "truePeakDbtp": report.true_peak_dbtp,
            "samplePeakDbfs": report.sample_peak_dbfs,
        },
        // Null when the mix is silent below the absolute gate: nothing to normalize.
        "applied": plan.map(|plan| json!({
            "gainDb": plan.gain_db,
            "integratedLufs": plan.integrated_lufs,
            "truePeakDbtp": plan.true_peak_dbtp,
            "peakLimited": plan.peak_limited,
            "limiterLimited": plan.limiter_limited,
        })),
        "output": Value::Null,
    }))
}

/// Completes a normalized export's `loudness` (from [`normalize_export_audio`])
/// with `output`: what `renderer` mixed for the export, measured after the
/// master limiter, the audio its encoder was given (for a copy, the renderer
/// of the feed that encoded its mix). `null` without a request.
pub(crate) fn finish_loudness(report: Option<Value>, renderer: Option<&AudioRenderer>) -> Value {
    let Some(mut report) = report else {
        return Value::Null;
    };
    report["output"] = renderer
        .and_then(AudioRenderer::output_loudness_report)
        .map_or(Value::Null, |output| {
            json!({
                "integratedLufs": output.integrated_lufs,
                "loudnessRangeLu": output.loudness_range_lu,
                "truePeakDbtp": output.true_peak_dbtp,
                "samplePeakDbfs": output.sample_peak_dbfs,
            })
        });
    report
}

/// Frames' worth of rendered results an export keeps. A frame that repeats
/// the last (a still, a held title) is then found whole; the clips under a
/// changing frame are found when only what is over them changed.
const EXPORT_RESULT_CACHE_FRAMES: u64 = 4;

pub(crate) fn result_cache_budget(size: RenderSize, format: wgpu::TextureFormat) -> u64 {
    let texel = u64::from(format.block_copy_size(None).unwrap_or(8));
    EXPORT_RESULT_CACHE_FRAMES * u64::from(size.width) * u64::from(size.height) * texel
}

/// What the result cache did during one export.
fn result_cache_json(
    before: &alpha2_render::ResultCacheStats,
    after: &alpha2_render::ResultCacheStats,
) -> Value {
    json!({
        "budgetBytes": after.budget_bytes,
        "hits": after.hits - before.hits,
        "misses": after.misses - before.misses,
        "ineligible": after.ineligible - before.ineligible,
        "inserts": after.inserts - before.inserts,
        "evictions": after.evictions - before.evictions,
        "skippedNodes": after.skipped_nodes - before.skipped_nodes,
    })
}

/// The timeline range an export covered, snapped to the frame grid.
///
/// The output's zero is `startTicks`, which can be up to one frame before the
/// requested start.
pub(crate) fn effective_range_json(range: TimeRange) -> Value {
    json!({
        "startTicks": range.start().ticks(),
        "endTicks": range.end().ticks(),
    })
}

#[cfg(test)]
thread_local! {
    /// Makes the next conform on this thread panic, as a provider or demuxer
    /// defect on a request's media does.
    pub static PANIC_IN_CONFORM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next cleanup after a published output on this thread fail,
    /// as a GPU wait that times out there does.
    pub static INJECT_CLEANUP_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

impl Host {
    /// Runs what an export does once its output is published: waiting on
    /// the GPU work that read its inputs and releasing what that work held.
    /// Returns the decoder surfaces still parked, or 0 when it failed.
    ///
    /// The file is complete and in place by then, so a failure here is
    /// reported and does not fail the export, as a profiling file's does: a
    /// client told the export failed would retry it, and every sink refuses
    /// an existing output. A device that stopped running work is
    /// retired after the answer ([`Renderer::gpu_lost`]), and one that is only
    /// slow fails the next export's own bounded wait.
    pub(crate) fn clean_up_after_output(
        &self,
        output: &Path,
        cleanup: impl FnOnce() -> Result<usize, String>,
    ) -> usize {
        self.failure
            .enter(crate::failure::ExportPhase::Cleanup, None);
        let cleaned = cleanup();
        #[cfg(test)]
        let cleaned = if INJECT_CLEANUP_FAILURE.with(std::cell::Cell::take) {
            cleaned.and(Err(format!(
                "{}injected cleanup timeout",
                crate::protocol::gpu_timeout_tag()
            )))
        } else {
            cleaned
        };
        cleaned.unwrap_or_else(|error| {
            alpha2_os::diagnostic!(
                "alpha2 native export: {} is written; cleaning up after it failed: {}",
                output.display(),
                crate::protocol::HostError::classify(error).message
            );
            0
        })
    }
}

/// The media a request reads, conformed, before anything renders
/// ([`Host::prepare_request`]).
struct PreparedRequest {
    resolved: ResolvedMedia,
    selections: serde_json::Map<String, Value>,
    released: u64,
    provider_files: crate::providers::ProviderFiles,
}

/// What an output asked to `overwrite` does with a file at its path.
const fn existing_output(overwrite: bool) -> ExistingOutput {
    if overwrite {
        ExistingOutput::Replace
    } else {
        ExistingOutput::Refuse
    }
}

/// The project a still request renders: its own, or, given a `size`, the
/// same document at that canvas size, as an export of it at that size would
/// render it.
///
/// # Errors
/// `INVALID_INPUT` when the document is not a JSON object with settings.
fn still_project(
    project: &str,
    size: Option<crate::protocol::StillSize>,
) -> Result<String, HostError> {
    let Some(size) = size else {
        return Ok(project.to_owned());
    };
    let mut document: Value = serde_json::from_str(project)
        .map_err(|error| HostError::invalid_input(format!("invalid project: {error}")))?;
    let settings = document
        .get_mut("settings")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| HostError::invalid_input("invalid project: settings is not an object"))?;
    settings.insert("width".to_owned(), json!(size.width));
    settings.insert("height".to_owned(), json!(size.height));
    Ok(document.to_string())
}

/// The absolute path of a file an export or a still has just published at
/// `path`, as text. The file is there, so it resolves; were it removed
/// meanwhile, the path is made absolute without resolving links. On Windows
/// the resolved path loses the `\\?\` prefix `canonicalize` adds, as every
/// other program writes paths without it.
/// How an output was published, as export and still results report it:
/// `{"method", "stagedLocally", "retries"}`, or `null` when it was not.
pub(crate) fn publish_json(report: Option<alpha2_native::PublishReport>) -> Value {
    report.map_or(Value::Null, |report| {
        json!({
            "method": report.method.as_str(),
            "stagedLocally": report.staged_locally,
            "retries": report.retries,
        })
    })
}

pub(crate) fn written_path(path: &Path) -> String {
    let text = std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_owned())
        .display()
        .to_string();
    if cfg!(windows) {
        if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{share}");
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    text
}

/// Answers a `conform` request: what each source the project reads resolves
/// to, and whether the media is ready for the purpose.
///
/// It reads media alone, so `serve` answers it without a GPU host; a request
/// that panics is answered `INTERNAL`, as one the host handles is. No device
/// is opened, so a mode that may decode in hardware is taken to reach every
/// stream a session might ([`conform_hardware`]), as the CLI does; an export
/// conforms again against its own device.
pub fn conform_media(
    request: crate::protocol::ConformRequest,
    policy: VideoDecodePolicy,
) -> Result<Value, HostError> {
    catch_unwind(AssertUnwindSafe(|| conform_answer(request, policy))).unwrap_or_else(|payload| {
        Err(HostError::new(
            "INTERNAL",
            format!(
                "the host panicked handling the request: {}",
                crate::failure::panic_message(&*payload)
            ),
        ))
    })
}

fn conform_answer(
    crate::protocol::ConformRequest {
        project,
        assets,
        proxies,
        frames,
        purpose,
        ..
    }: crate::protocol::ConformRequest,
    policy: VideoDecodePolicy,
) -> Result<Value, HostError> {
    let resolved = request_media(
        project.text(),
        &assets,
        &proxies,
        &frames,
        purpose.purpose(),
        &conform_hardware(policy),
    )?;
    Ok(json!({
        "catalog": resolved.catalog,
        "report": resolved.report,
    }))
}

/// The hardware decode a conform without a device assumes under `policy`:
/// every stream a session might decode where the build and mode allow one.
pub fn conform_hardware(policy: VideoDecodePolicy) -> HardwareVideoDecode {
    if cfg!(feature = "hwdecode") && policy.allows_hardware() {
        HardwareVideoDecode::PerStream
    } else {
        HardwareVideoDecode::None
    }
}

/// A request's media for `purpose`, from its `assets`, `proxies`, and
/// `frames` maps.
///
/// # Errors
/// A project that does not parse, or a proxy whose `madeFrom` is malformed.
fn request_media(
    project: &str,
    assets: &HashMap<String, PathBuf>,
    proxies: &HashMap<String, Vec<ProxyRequest>>,
    frames: &HashMap<String, Vec<Option<PathBuf>>>,
    purpose: MediaPurpose,
    hardware: &HardwareVideoDecode,
) -> Result<ResolvedMedia, HostError> {
    #[cfg(test)]
    assert!(
        !PANIC_IN_CONFORM.with(std::cell::Cell::take),
        "injected conform panic"
    );
    let project =
        Project::from_json(project).map_err(|error| HostError::classify(error_message(error)))?;
    // A caller's evidence that is not well formed is an input error, never
    // other media. Sources in order, so the error is the same each time.
    let mut by_source = proxies.iter().collect::<Vec<_>>();
    by_source.sort_by(|a, b| a.0.cmp(b.0));
    for (source, requests) in by_source {
        for (index, request) in requests.iter().enumerate() {
            if let Some(part) = request
                .made_from
                .as_ref()
                .and_then(MediaFingerprint::malformed_part)
            {
                return Err(HostError::invalid_input(format!(
                    "proxy {index} of source {source}: madeFrom has a malformed {part}"
                )));
            }
        }
    }
    let links = RequestMedia {
        originals: assets,
        proxies,
        frames,
    };
    Ok(resolve_media(&project, &links, &links, purpose, hardware))
}

/// What each source read, as the export result's `media`.
fn media_selections(report: &alpha2_assets::ConformReport) -> serde_json::Map<String, Value> {
    report
        .sources
        .iter()
        .map(|source| {
            (
                source.source_id.as_str().to_owned(),
                json!(source.selection),
            )
        })
        .collect()
}

/// Whether the render cache holds one frame.
enum CacheLookup {
    /// The baked frame, taken instead of rendered.
    Hit(Box<BakedFrame>),
    /// Not held: the frame is rendered, and baked under this key (pinned or
    /// not) when the cache takes it.
    Miss(Option<(String, bool)>),
}

/// The frames an export looks up in its render cache next, planned ahead so
/// the cache's reader reads them while the export handles the frames before:
/// each frame's cache intent, worked out once, and its key, `None` for a
/// frame a bypassed clip keeps live.
#[derive(Default)]
struct CacheReads {
    planned: std::collections::VecDeque<(MediaTime, Option<(String, bool)>)>,
}

impl CacheReads {
    /// Plans the frames the export reaches after `time`, up to
    /// [`crate::render_cache::READ_AHEAD_FRAMES`] in all, and asks the
    /// cache for each it may take. A frame whose intent cannot be worked out
    /// ends the plan: it fails when the export reaches it, as it would have
    /// unplanned.
    fn plan(
        &mut self,
        cache: &mut crate::render_cache::RenderCache,
        exporter: &Exporter,
        snapshot: &EngineProject,
        time: MediaTime,
    ) {
        let ahead = u32::try_from(crate::render_cache::READ_AHEAD_FRAMES).unwrap_or(u32::MAX);
        let Ok(upcoming) = exporter.upcoming_times(ahead.saturating_add(1)) else {
            return;
        };
        for upcoming in upcoming.into_iter().filter(|upcoming| *upcoming > time) {
            if self
                .planned
                .back()
                .is_some_and(|(planned, _)| *planned >= upcoming)
            {
                continue;
            }
            let Ok(intent) = frame_cache_intent(snapshot, upcoming) else {
                break;
            };
            self.planned
                .push_back((upcoming, cache_key(cache, intent, upcoming, exporter)));
        }
        for (_, key) in &self.planned {
            if let Some((key, _)) = key {
                cache.request(key);
            }
        }
    }
}

/// The key the frame at `time` is baked and taken under with `intent`, and
/// whether it is pinned; `None` for a frame kept live.
fn cache_key(
    cache: &crate::render_cache::RenderCache,
    intent: alpha2_model::CacheIntent,
    time: MediaTime,
    exporter: &Exporter,
) -> Option<(String, bool)> {
    (intent != alpha2_model::CacheIntent::Bypass).then(|| {
        (
            cache.key(time, exporter.frame_rate()),
            intent == alpha2_model::CacheIntent::Pin,
        )
    })
}

/// Looks the export's pending frame, at `time`, up in its render cache, if
/// it has one, and asks the cache's reader for the frames after it.
fn render_cache_lookup(
    cache: Option<&mut crate::render_cache::RenderCache>,
    reads: &mut CacheReads,
    exporter: &Exporter,
    snapshot: &EngineProject,
    time: MediaTime,
) -> Result<CacheLookup, String> {
    let Some(cache) = cache else {
        return Ok(CacheLookup::Miss(None));
    };
    if reads
        .planned
        .front()
        .is_some_and(|(planned, _)| *planned != time)
    {
        reads.planned.clear();
    }
    let key = match reads.planned.pop_front() {
        Some((_, key)) => key,
        None => cache_key(cache, frame_cache_intent(snapshot, time)?, time, exporter),
    };
    if let Some((key, _)) = &key {
        cache.request(key);
    }
    reads.plan(cache, exporter, snapshot, time);
    let Some((key, pinned)) = key else {
        cache.bypass();
        return Ok(CacheLookup::Miss(None));
    };
    Ok(match cache.read(&key) {
        Some(frame) => CacheLookup::Hit(Box::new(frame)),
        None => CacheLookup::Miss(Some((key, pinned))),
    })
}

/// Whether the frame at `time` may be baked or taken from the cache: a
/// bypassed clip keeps it live; a pinned one pins it.
fn frame_cache_intent(
    snapshot: &EngineProject,
    time: MediaTime,
) -> Result<alpha2_model::CacheIntent, String> {
    let frame = snapshot.evaluate(time).map_err(error_message)?;
    let intents = frame.visual.iter().map(|clip| clip.cache);
    Ok(
        if intents
            .clone()
            .any(|intent| intent == alpha2_model::CacheIntent::Bypass)
        {
            alpha2_model::CacheIntent::Bypass
        } else if intents
            .clone()
            .any(|intent| intent == alpha2_model::CacheIntent::Pin)
        {
            alpha2_model::CacheIntent::Pin
        } else {
            alpha2_model::CacheIntent::Auto
        },
    )
}

/// SHA-256 of the running host binary, read once: a rebuilt host may render
/// differently, so its frames never meet another build's.
///
/// `create_host` reads it at startup, before an upgrade can replace the file
/// under a resident process. On Linux it reads `/proc/self/exe`, the image
/// this process runs even once its path is unlinked or replaced. A binary
/// that cannot be read has no identity, and the render cache is unused: a
/// digest of nothing would be every unreadable build's.
///
/// # Errors
/// Why the running executable cannot be read.
pub(crate) fn host_binary_digest() -> Result<[u8; 32], String> {
    use sha2::Digest;
    static DIGEST: std::sync::OnceLock<Result<[u8; 32], String>> = std::sync::OnceLock::new();
    #[cfg(test)]
    if INJECT_UNREADABLE_BINARY.with(std::cell::Cell::get) {
        return Err("injected: the running executable cannot be read".to_owned());
    }
    DIGEST
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            let bytes = std::fs::read("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let bytes = std::env::current_exe().and_then(std::fs::read);
            bytes
                .map(|bytes| sha2::Sha256::digest(bytes).into())
                .map_err(|error| format!("the running executable cannot be read: {error}"))
        })
        .clone()
}

#[cfg(test)]
thread_local! {
    /// Makes this thread's exports find the running executable unreadable.
    pub(crate) static INJECT_UNREADABLE_BINARY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
