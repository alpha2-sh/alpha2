//! The GPU-resident export: renderer-packed planes into a hardware encoder.
//!
//! The loop here names no platform type. It drives
//! [`alpha2_native::HardwareEncodeInput`] on the coordinator and
//! [`alpha2_native::HardwareEncodeSession`] on the encode worker, so a backend
//! for another platform plugs in below this file rather than beside it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::audio::WindowedAudioSources;
use crate::config::{ResolvedAudioRate, configured_encode_channel_depth};
use crate::config::{codec_thread_policy, configured_cancel_at_frame, decoder_thread_policy};
use crate::decode::{DecodeScheduler, DecodeWorker};
use crate::encode::{
    EncodeCommand, EncodeProgressState, EncodeWorker, GpuFrameBound, NativeExportSink,
    PipelineFailure, drain_encode_worker, encode_event_bytes, handle_encode_worker_event,
};
use crate::failure::ExportPhase;
use crate::gpu_output::GpuOutputRefusal;
use crate::host::{Host, cancelled_export, effective_range_json, normalize_export_audio};
use crate::metrics::{
    ExportMetrics, VideoOutputPath, export_metrics_json, frames_per_second, gpu_encode_frame_bound,
    gpu_encode_pipeline_limits,
};
use crate::protocol::{HostError, LoudnessRequest};
use crate::trace::{AudioBlockTrace, ExportTrace, VideoFrameTrace};
use crate::yuv_transcode::TranscodePlan;
use crate::{WORKER_RETRY_INTERVAL, error_message};
use alpha2_audio::TransportState;
use alpha2_engine::{
    AudioRenderer, EngineProject, ExportStepTimings, Exporter, FramesWithText, QualityProfile,
};
use alpha2_media::{ColorMetadata, EncodeEvent};
use alpha2_native::{
    AudioStreamConfig, HardwareEncodeCapability, HardwareEncodeDevice, HardwareEncodeOpenError,
    HardwareEncodeRequest, NativeAudioCodec,
};
use alpha2_render::RenderSize;
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use serde_json::{Value, json};

/// Why an export could not take the GPU encode path.
pub(crate) enum GpuEncodeOutcome {
    /// The path is not available here; the caller falls back silently.
    Unavailable(String),
    /// The export started on this path and then failed, so falling back would
    /// re-decode and re-render work the caller already paid for, and would
    /// hide a real failure.
    Failed(String),
}

impl Host {
    /// The renderer's device as a hardware encoder sees it, when this host
    /// has one it can encode on.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    pub(crate) fn hardware_encode_device(&self) -> Option<HardwareEncodeDevice> {
        #[cfg(target_os = "linux")]
        return self.shared_vulkan.clone().map(HardwareEncodeDevice::vulkan);
        #[cfg(target_os = "macos")]
        return self.shared_metal.clone().map(HardwareEncodeDevice::metal);
        #[cfg(target_os = "windows")]
        return self.shared_d3d12.clone().map(HardwareEncodeDevice::d3d12);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        None
    }

    /// Why this host has no device shared with a hardware encoder: on
    /// Windows, what the adapter it renders on answered, and an adapter that
    /// could encode, when one could.
    #[cfg_attr(not(target_os = "windows"), allow(clippy::unused_self))]
    fn no_encode_device(&self, codec: Option<alpha2_native::NativeVideoCodec>) -> String {
        let reason = "this host has no GPU device shared with a hardware encoder".to_owned();
        #[cfg(target_os = "windows")]
        return self.no_d3d12_encode_device(codec, reason);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = codec;
            reason
        }
    }

    /// [`Self::no_encode_device`] on Windows, from `reason`, the reason
    /// without what the adapter choice found.
    #[cfg(target_os = "windows")]
    fn no_d3d12_encode_device(
        &self,
        codec: Option<alpha2_native::NativeVideoCodec>,
        reason: String,
    ) -> String {
        // A switched-off path makes no device for encode alone, so the switch is the reason.
        if let Err(off) = self.gpu_encode_gate() {
            return off;
        }
        let reason = self.encode_adapters.no_device.clone().unwrap_or(reason);
        // A report names the adapter that could encode the default codec, or failing that any.
        let elsewhere = &self.encode_adapters.elsewhere;
        let codec = codec.or_else(|| {
            elsewhere
                .iter()
                .find(|(other, _)| *other == crate::adapter_choice::DEFAULT_ENCODE_CODEC)
                .or_else(|| elsewhere.first())
                .map(|(codec, _)| *codec)
        });
        match codec {
            Some(codec) => self.encode_adapters.explain(codec, reason),
            None => reason,
        }
    }

    /// What `device` encodes `codec` with (the capability a report leads with
    /// when `codec` is `None`), or why it does not.
    #[cfg_attr(not(target_os = "windows"), allow(clippy::unused_self))]
    fn device_capability(
        &self,
        device: &HardwareEncodeDevice,
        codec: Option<alpha2_native::NativeVideoCodec>,
    ) -> Result<HardwareEncodeCapability, String> {
        #[cfg(target_os = "windows")]
        return self.d3d12_capability(device, codec);
        #[cfg(not(target_os = "windows"))]
        match codec {
            Some(codec) => device.capability(codec),
            None => device.preferred_capability(),
        }
    }

    /// [`Self::device_capability`] on Windows. A codec the device says it
    /// encodes is held to a test encode, which runs only when the GPU path
    /// could be asked for, so the configuration switch is read first; and a
    /// refusal names another adapter that could encode, when one could.
    #[cfg(target_os = "windows")]
    fn d3d12_capability(
        &self,
        device: &HardwareEncodeDevice,
        codec: Option<alpha2_native::NativeVideoCodec>,
    ) -> Result<HardwareEncodeCapability, String> {
        self.gpu_encode_gate()?;
        let Some(codec) = codec else {
            let mut reasons = Vec::new();
            for codec in crate::adapter_choice::ENCODE_CODECS {
                match self.d3d12_capability(device, Some(codec)) {
                    Ok(capability) => return Ok(capability),
                    Err(reason) => reasons.push(reason),
                }
            }
            return Err(reasons.join("; "));
        };
        #[cfg(test)]
        if let Some(reason) = self.injected_encode_refusal(codec) {
            return Err(self.encode_adapters.explain(codec, reason));
        }
        device
            .capability(codec)
            .map_err(|reason| self.encode_adapters.explain(codec, reason))
    }

    /// The refusal a test forced for `codec` on this host's adapter.
    #[cfg(all(test, target_os = "windows"))]
    fn injected_encode_refusal(&self, codec: alpha2_native::NativeVideoCodec) -> Option<String> {
        let shared = self.shared_d3d12.as_ref()?;
        let luid = shared.adapter().map_or(0, |identity| identity.luid);
        crate::d3d12va_probe::injected_refusal(
            &shared.gpu().capabilities().adapter_name,
            luid,
            codec,
        )
    }

    /// Every capability the GPU-resident path has here, one per codec it
    /// takes: on Linux and macOS the one encoder there is, on Windows each
    /// of H.264, HEVC and AV1 the device encodes.
    pub(crate) fn gpu_output_codecs(&self) -> Vec<HardwareEncodeCapability> {
        let Some(device) = self.hardware_encode_device() else {
            return Vec::new();
        };
        if self.gpu_encode_gate().is_err() {
            return Vec::new();
        }
        #[cfg(target_os = "windows")]
        return crate::adapter_choice::ENCODE_CODECS
            .iter()
            .filter_map(|codec| self.device_capability(&device, Some(*codec)).ok())
            .collect();
        #[cfg(not(target_os = "windows"))]
        self.device_capability(&device, None).into_iter().collect()
    }

    /// Whether an export of `codec`, perhaps pinned to `pinned`, is a candidate
    /// for the GPU-resident path under this export's input policy, and if so
    /// on which device and with what its encoder for `codec` takes.
    ///
    /// # Errors
    /// Why it is not; `required` when the pin leaves no other path.
    pub(crate) fn gpu_output_candidate(
        &self,
        codec: alpha2_native::NativeVideoCodec,
        pinned: Option<&str>,
    ) -> Result<(HardwareEncodeDevice, HardwareEncodeCapability), GpuOutputRefusal> {
        // Windows asks about the export's own codec; elsewhere the one encoder's codec is checked
        // below.
        let asked = cfg!(target_os = "windows").then_some(codec);
        let device = self.hardware_encode_device();
        let capability = device
            .as_ref()
            .ok_or_else(|| self.no_encode_device(asked))
            .and_then(|device| self.device_capability(device, asked));
        // Pinning a GPU-frames-only encoder asks for this path, since readback cannot stand in. The
        // D3D12VA encoders are GPU-only everywhere, so pinning one asks for it even where no device
        // encodes.
        let pinned_d3d12va =
            cfg!(target_os = "windows") && pinned.is_some_and(|name| name.ends_with("_d3d12va"));
        let pinned_gpu_encoder = capability.as_ref().is_ok_and(|capability| {
            capability.gpu_input_only && pinned == Some(capability.encoder)
        }) || pinned_d3d12va;
        let refuse = |reason: String| GpuOutputRefusal {
            reason,
            required: pinned_gpu_encoder,
        };
        if self.input_policy == crate::config::VideoInputPolicy::CpuReadback {
            return Err(refuse(format!(
                "inputPolicy is cpu-readback{}",
                if pinned_gpu_encoder {
                    ", and the pinned encoder takes GPU frames only"
                } else {
                    ""
                }
            )));
        }
        // Without a capability only a pinned D3D12VA encoder makes the refusal required.
        let capability = capability.map_err(refuse)?;
        if capability.codec != codec {
            return Err(refuse(format!(
                "{} encodes {}, not {}",
                capability.encoder,
                capability.codec.as_str(),
                codec.as_str()
            )));
        }
        if let Some(name) = pinned
            && name != capability.encoder
        {
            return Err(refuse(if pinned_d3d12va {
                format!(
                    "the pinned encoder {name} does not encode {}; this host's GPU-input encoder \
                     for it is {}",
                    codec.as_str(),
                    capability.encoder
                )
            } else {
                format!(
                    "the pinned encoder {name} takes CPU frames; this host's GPU-input encoder is {}",
                    capability.encoder
                )
            }));
        }
        device
            .map(|device| (device, capability))
            .ok_or_else(|| GpuOutputRefusal::unavailable("the shared device disappeared"))
    }

    /// What the GPU-resident path can encode on this host, or why it cannot:
    /// the capability a report leads with.
    pub(crate) fn gpu_output_capability(&self) -> Result<HardwareEncodeCapability, String> {
        let device = self
            .hardware_encode_device()
            .ok_or_else(|| self.no_encode_device(None))?;
        let capability = self.device_capability(&device, None)?;
        self.gpu_encode_gate()?;
        Ok(capability)
    }

    /// The configuration switch, and on Linux whatever keeps `FFmpeg` from
    /// opening a video session on the shared device.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))]
    fn gpu_encode_gate(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        return crate::config::configured_gpu_encode(
            self.shared_vulkan
                .as_ref()
                .and_then(alpha2_native_gpu::SharedVulkanContext::video_session_hazard),
        );
        #[cfg(not(target_os = "linux"))]
        crate::config::configured_gpu_encode_switch()
    }

    /// Exports with the picture never leaving the GPU.
    ///
    /// Renders straight into the backend's NV12 plane textures, hands them to
    /// an encoder surface on the GPU, and muxes the packets. There is no
    /// readback buffer, no packed CPU frame and no CPU frame pool, so the
    /// readback stages of the software path are structurally absent rather
    /// than merely small.
    ///
    /// The pipeline is shallower than the readback path's on purpose: that
    /// path's depth exists to hide readback latency, and there is none here.
    /// Decode still runs ahead on its own worker.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn export_gpu_encode(
        &self,
        project: &alpha2_model::Project,
        snapshot: &EngineProject,
        assets: &HashMap<String, PathBuf>,
        range: TimeRange,
        rate: FrameRate,
        has_audio: bool,
        video_rate: crate::config::ResolvedVideoRate,
        container: alpha2_media::Container,
        audio_codec: NativeAudioCodec,
        audio_rate: ResolvedAudioRate,
        loudness: Option<LoudnessRequest>,
        output: &Path,
        started: Instant,
        (device, capability): (&HardwareEncodeDevice, &HardwareEncodeCapability),
        transcode: &Result<TranscodePlan, String>,
        pipeline_failure: &PipelineFailure,
        progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    ) -> Result<Value, GpuEncodeOutcome> {
        let transcode_plan = transcode.as_ref().ok();
        let size = RenderSize::new(project.settings.width, project.settings.height)
            .map_err(|error| GpuEncodeOutcome::Unavailable(format!("{error:?}")))?;
        capability
            .accepts(size.width, size.height)
            .map_err(GpuEncodeOutcome::Unavailable)?;
        let mut exporter = Exporter::new(
            snapshot,
            range,
            rate,
            QualityProfile::export(),
            capability.codec.model(),
        )
        .map_err(|error| GpuEncodeOutcome::Unavailable(format!("{error:?}")))?;
        let total_frames = exporter.progress().total;
        // The whole in-flight budget buys the plane ring and encoder surfaces: this path stages
        // nothing on the CPU. A budget too small refuses before the output exists.
        let pipeline_limits = gpu_encode_pipeline_limits(
            capability,
            size,
            self.memory.in_flight_budget(),
            configured_encode_channel_depth(),
        )
        .map_err(GpuEncodeOutcome::Unavailable)?;

        let (session, mut input) = device
            .open(&HardwareEncodeRequest {
                width: size.width,
                height: size.height,
                codec: capability.codec,
                frame_rate: rate,
                rate_control: video_rate.control,
                color: ColorMetadata::EXPORT,
                output,
                container,
                audio: has_audio.then_some(AudioStreamConfig {
                    codec: audio_codec,
                    sample_rate: project.settings.sample_rate,
                    channels: 2,
                    bit_rate: audio_rate.bitrate,
                }),
            })
            .map_err(|error| match error {
                HardwareEncodeOpenError::Refused(reason) => GpuEncodeOutcome::Unavailable(reason),
                HardwareEncodeOpenError::Failed(error) => {
                    GpuEncodeOutcome::Failed(error_message(error))
                }
            })?;
        let gpu_selection = session.selection().clone();
        self.failure.note(
            "videoOutputPath",
            json!(crate::metrics::VideoOutputPath::GpuResident.as_str()),
        );
        self.failure
            .note("videoEncoder", json!(gpu_selection.encoder_name));
        alpha2_os::diagnostic!(
            "alpha2 native export encoder: {} (hardware, GPU-resident)",
            gpu_selection.encoder_name
        );

        let graph_before = self.gpu_renderer().diagnostics();
        let cancel = crate::termination::export_cancel();
        let plan = crate::export_decode::ExportDecodePlan::new(
            self.export_codec_split(project, assets, range, true, false),
            self.export_providers(project, size),
        );
        let decode_lookahead = plan.decode_lookahead;
        let codec_threads = plan.codec_threads;
        self.failure
            .note("audioEncoder", json!(has_audio.then_some("aac")));
        self.failure
            .note("hardwareDecodeReady", json!(plan.hardware_decode));
        self.failure.note(
            "pipeline",
            json!({
                "enabled": true,
                "decodeLookahead": decode_lookahead,
                "encodeChannelDepth": pipeline_limits.encode_channel_depth,
            }),
        );
        let mut gpu_frames = self.export_gpu_frames(size, project);
        let mut decode_worker = DecodeWorker::spawn(
            project,
            assets,
            &self.decode_worker_config(&plan),
            cancel.clone(),
            pipeline_failure.clone(),
        )
        .map_err(GpuEncodeOutcome::Failed)?;
        let mut decode_scheduler = DecodeScheduler::new(decode_lookahead, project);
        let packed_bytes_per_frame = pipeline_limits.packed_bytes_per_frame;
        let frame_bound =
            GpuFrameBound::new(gpu_encode_frame_bound(pipeline_limits.encode_channel_depth));
        let encode_worker = EncodeWorker::spawn(
            NativeExportSink::GpuVideo(session),
            cancel.clone(),
            pipeline_failure.clone(),
            pipeline_limits.encode_channel_depth,
            started,
            crate::config::configured_stall_timeout(),
        )
        .map_err(GpuEncodeOutcome::Failed)?;
        let mut encode_state = EncodeProgressState::default();
        let cancel_at_frame = configured_cancel_at_frame();
        let mut metrics = ExportMetrics {
            video_output_path: VideoOutputPath::GpuResident,
            pipeline_enabled: true,
            decode_lookahead,
            decode_concurrency: codec_threads.decode_concurrency,
            decoder_threads: codec_threads.decoder_threads,
            codec_thread_budget: codec_threads.total_threads,
            codec_threads_allocated: codec_threads.allocated_threads,
            source_frame_pixels: plan.source_frame_pixels,
            source_probe: plan.source_probe,
            // Stage sums cannot express overlap, and this path is where overlap is the question,
            // so `ALPHA2_EXPORT_TRACE` must take effect here too.
            trace: ExportTrace::from_env(started),
            ..ExportMetrics::default()
        };
        let mut audio_sources = has_audio
            .then(|| {
                WindowedAudioSources::new(
                    project,
                    assets,
                    project.settings.sample_rate,
                    self.memory.audio_window.byte_budget(),
                    self.failure.decoding(),
                )
            })
            .transpose()
            .map_err(GpuEncodeOutcome::Failed)?;
        let mut audio_renderer = has_audio
            .then(|| AudioRenderer::new(snapshot, 2, 4_096).map_err(error_message))
            .transpose()
            .map_err(GpuEncodeOutcome::Failed)?;
        let effective_range = exporter.effective_range();
        let video_origin = effective_range.start();
        let sample_rate = project.settings.sample_rate;
        let (audio_start_sample, audio_frames) = if has_audio {
            let window = exporter
                .audio_window(sample_rate)
                .map_err(|error| GpuEncodeOutcome::Failed(error_message(error)))?;
            (window.start_sample, window.frames)
        } else {
            (0, 0)
        };
        let loudness_report = match (loudness, audio_renderer.as_mut()) {
            (Some(request), Some(renderer)) => Some(
                normalize_export_audio(
                    renderer,
                    snapshot,
                    project,
                    assets,
                    audio_start_sample,
                    audio_frames,
                    request,
                    &cancel,
                    self.memory.audio_window.byte_budget(),
                    &self.failure.decoding(),
                )
                .map_err(GpuEncodeOutcome::Failed)?,
            ),
            (Some(_), None) => {
                return Err(GpuEncodeOutcome::Failed(
                    crate::host::loudness_without_audio(),
                ));
            }
            (None, _) => None,
        };
        let mut audio_produced = 0_usize;

        // Set when the run stopped itself rather than failing, so the report names the cancel
        // instead of the encode worker's "cancelled" notice.
        let mut cancelled_at = None;
        let result = (|| -> Result<(), String> {
            decode_scheduler.refill(&exporter, snapshot, &decode_worker, &mut metrics)?;
            loop {
                let audio_ticks = (audio_produced < audio_frames).then(|| {
                    i64::try_from(audio_produced)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(TICKS_PER_SECOND)
                        .div_euclid(i64::from(sample_rate))
                });
                let video_ticks = exporter
                    .pending_time()
                    .map(|time| time.ticks().saturating_sub(video_origin.ticks()));
                let take_video = match (video_ticks, audio_ticks) {
                    (Some(video), Some(audio)) => video <= audio,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if !take_video {
                    self.failure.enter(ExportPhase::Decode, None);
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
                        .audio_needs(snapshot, absolute, count)
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
                        .render(snapshot, absolute, count, sources, TransportState::Playing)
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
                    let event = EncodeEvent::Audio(block);
                    let event_bytes = encode_event_bytes(&event);
                    let queue_started = Instant::now();
                    let backpressured = encode_worker.send(EncodeCommand::Event {
                        event,
                        video_index: None,
                    })?;
                    metrics.encode_queue =
                        metrics.encode_queue.saturating_add(queue_started.elapsed());
                    metrics.encode_backpressure = metrics
                        .encode_backpressure
                        .saturating_add(u64::from(backpressured));
                    if let Some(sink) = metrics.trace.as_mut() {
                        let queued = sink.now();
                        sink.push_audio(AudioBlockTrace {
                            queued,
                            ..AudioBlockTrace::default()
                        });
                    }
                    encode_state.pending_bytes =
                        encode_state.pending_bytes.saturating_add(event_bytes);
                    encode_state.pending_events = encode_state.pending_events.saturating_add(1);
                    metrics.peak_encode_queue_bytes = metrics
                        .peak_encode_queue_bytes
                        .max(encode_state.pending_bytes);
                    metrics.max_encode_queue =
                        metrics.max_encode_queue.max(encode_state.pending_events);
                    drain_encode_worker(
                        &encode_worker,
                        &mut encode_state,
                        total_frames,
                        started,
                        progress,
                        &mut metrics,
                    )?;
                    audio_produced += count;
                    continue;
                }

                let frame_index = exporter.progress().completed;
                self.failure.enter(ExportPhase::Decode, Some(frame_index));
                let mut frame_trace = VideoFrameTrace::default();
                let decode_wait = Instant::now();
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.decode_wait_started = sink.now();
                }
                let decoded = loop {
                    if let Some(decoded) = decode_worker.result_timeout(WORKER_RETRY_INTERVAL)? {
                        break decoded;
                    }
                };
                metrics.decode_wait = metrics.decode_wait.saturating_add(decode_wait.elapsed());
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.decoded = sink.now();
                }
                metrics.decode = metrics.decode.saturating_add(decoded.decode_time);
                metrics.decode_stages = metrics.decode_stages.merged(decoded.stage_timings);
                metrics.software_decoded_frames = metrics
                    .software_decoded_frames
                    .saturating_add(decoded.software_decoded_frames);
                metrics.hardware_decoded_frames = metrics
                    .hardware_decoded_frames
                    .saturating_add(decoded.hardware_decoded_frames);
                metrics.hardware_fallbacks = metrics
                    .hardware_fallbacks
                    .merged(decoded.hardware_fallbacks);
                metrics.reverse_cache_hits = metrics
                    .reverse_cache_hits
                    .saturating_add(decoded.reverse_cache_hits);
                metrics.reverse_windows_built = metrics
                    .reverse_windows_built
                    .saturating_add(decoded.reverse_windows_built);
                metrics.peak_reverse_cache_bytes = metrics
                    .peak_reverse_cache_bytes
                    .max(decoded.reverse_cache_bytes);
                // Without this the scheduler believes every set it sent is still outstanding, stops
                // refilling once it reaches its depth, and the wait above never ends.
                decode_scheduler.accept(&decoded, exporter.progress().completed)?;
                let bytes = decoded.bytes;
                // A transcoded frame is copied into the encoder's planes once acquired; anything
                // else is installed now. Either way the set's bytes are released after the host's
                // last use of it.
                let picture = if let Some(plan) = transcode_plan {
                    Some(plan.picture(decoded)?)
                } else {
                    let prepare = Instant::now();
                    self.failure.enter(ExportPhase::Upload, Some(frame_index));
                    let upload = gpu_frames.install(self.gpu_renderer(), decoded)?;
                    metrics.source_prepare =
                        metrics.source_prepare.saturating_add(prepare.elapsed());
                    metrics.record_install(&upload);
                    decode_worker.release(bytes);
                    None
                };
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.installed = sink.now();
                }

                // The surface comes first so a backend can hand out its own planes as the target;
                // begin then waits until those planes are no longer read. The handoff copy follows
                // the render on the same queue, which orders the two.
                self.failure.enter(ExportPhase::Upload, Some(frame_index));
                let mut lease =
                    frame_bound.acquire(&cancel, &|| encode_worker.check_stall(), || {
                        input.acquire_surface()
                    })?;
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.surface_acquired = sink.now();
                }
                let frame = input.begin_frame(lease.surface()).map_err(error_message)?;
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.planes_ready = sink.now();
                }
                let render_started = Instant::now();
                let mut source_in_submit = std::time::Duration::ZERO;
                let mut step_timings = ExportStepTimings::default();
                let (luma, chroma) = input.planes(&frame);
                let targets = alpha2_render::Nv12PlaneTargets { luma, chroma };
                let step = if let Some(picture) = picture {
                    self.failure.enter(ExportPhase::Render, Some(frame_index));
                    let repack = gpu_frames.repack(
                        self.gpu_renderer(),
                        &picture,
                        targets,
                        exporter.encoding(),
                        &mut step_timings.submit,
                    )?;
                    // Source preparation (surface import, budget wait) is the frame's upload; the
                    // rest is its
                    // submit. Timing the whole call as both would count each repack twice.
                    let upload = repack.timings;
                    source_in_submit = upload
                        .cpu_plane_upload
                        .saturating_add(upload.gpu_import)
                        .saturating_add(upload.surface_budget_wait);
                    metrics.source_prepare =
                        metrics.source_prepare.saturating_add(source_in_submit);
                    record_repack(&mut metrics, &repack);
                    // The picture and its surface are the GPU's now: its lease holds the surface
                    // until the copy completes.
                    drop(picture);
                    decode_worker.release(bytes);
                    match exporter.take_next().map_err(error_message)? {
                        Some((index, time)) => {
                            alpha2_engine::ExportPlaneStep::Rendered { index, time }
                        }
                        None => alpha2_engine::ExportPlaneStep::Complete,
                    }
                } else {
                    let mut texts = self
                        .texts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(time) = exporter.pending_time() {
                        texts
                            .prepare_at(self.gpu_renderer(), snapshot, time, size)
                            .map_err(error_message)?;
                    }
                    let frame_provider = FramesWithText::new(&gpu_frames, &texts);
                    self.failure.enter(ExportPhase::Render, Some(frame_index));
                    exporter
                        .render_next_nv12_timed(
                            snapshot,
                            self.gpu_renderer(),
                            &frame_provider,
                            targets,
                            &mut step_timings,
                        )
                        .map_err(error_message)?
                };
                metrics.frame_submit_stages = metrics.frame_submit_stages.merged(step_timings);
                metrics.frame_submit = metrics
                    .frame_submit
                    .saturating_add(render_started.elapsed().saturating_sub(source_in_submit));
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.submitted = sink.now();
                }
                let (index, time) = match step {
                    alpha2_engine::ExportPlaneStep::Rendered { index, time } => (index, time),
                    alpha2_engine::ExportPlaneStep::Waiting {
                        missing_sources, ..
                    } => {
                        return Err(crate::protocol::missing_input(format!(
                            "native export is missing sources: {missing_sources:?}"
                        )));
                    }
                    alpha2_engine::ExportPlaneStep::Complete => break,
                };

                input
                    .submit(frame, lease.surface_mut())
                    .map_err(error_message)?;
                input.set_time(
                    lease.surface_mut(),
                    MediaTime::from_ticks(time.ticks().saturating_sub(video_origin.ticks())),
                );
                if let Some(sink) = metrics.trace.as_ref() {
                    frame_trace.packed = sink.now();
                }
                frame_trace.index = index;
                let queue_started = Instant::now();
                self.failure.enter(ExportPhase::Encode, Some(index));
                let backpressured = encode_worker.send(EncodeCommand::GpuVideo {
                    frame: lease,
                    video_index: index,
                    bytes: packed_bytes_per_frame,
                })?;
                metrics.encode_queue = metrics.encode_queue.saturating_add(queue_started.elapsed());
                metrics.encode_backpressure = metrics
                    .encode_backpressure
                    .saturating_add(u64::from(backpressured));
                if let Some(sink) = metrics.trace.as_mut() {
                    frame_trace.encode_queued = sink.now();
                    sink.push_video(frame_trace);
                }
                encode_state.pending_bytes = encode_state
                    .pending_bytes
                    .saturating_add(packed_bytes_per_frame);
                encode_state.pending_events = encode_state.pending_events.saturating_add(1);
                metrics.peak_encode_queue_bytes = metrics
                    .peak_encode_queue_bytes
                    .max(encode_state.pending_bytes);
                metrics.max_encode_queue =
                    metrics.max_encode_queue.max(encode_state.pending_events);
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
                    cancelled_at = Some(encode_state.completed_video);
                    return Err(crate::protocol::cancelled("native export was cancelled"));
                }
                decode_scheduler.refill(&exporter, snapshot, &decode_worker, &mut metrics)?;
            }
            // No frame is read back, so each canary is checked only once a later frame is
            // submitted; the last ones are checked here, so an export the GPU did not render fails
            // before the file is finished.
            self.failure.enter(ExportPhase::Render, None);
            self.gpu_renderer()
                .confirm_gpu_work()
                .map_err(error_message)?;
            Ok(())
        })();
        if let Err(error) = result {
            // Record the coordinator's failure before the cancel: the cancel makes the workers fail
            // with "cancelled", and that must not be what the export reports.
            if cancelled_at.is_none() {
                pipeline_failure.record(&error);
            }
            // Release in reverse acquisition order: joining the encode worker tears the session
            // down and removes the partial file; dropping the input then waits on in-flight
            // handoffs.
            cancel.cancel();
            let _ = decode_worker.finish();
            drop(encode_worker);
            // After the session: dropping the input waits on the handoffs in flight. A backend-less
            // platform's input owns nothing to drop.
            #[allow(clippy::drop_non_drop)]
            drop(input);
            let reported =
                cancelled_export(Some(self.gpu_renderer()), encode_state.completed_video);
            return Err(GpuEncodeOutcome::Failed(match cancelled_at {
                // A cancel is not a failure, and the worker's own "cancelled" notice would
                // otherwise be reported as the cause of it.
                Some(_) => reported,
                None => pipeline_failure.cause_or(error),
            }));
        }

        metrics.record_decode_pool(&decode_worker);
        decode_worker.finish().map_err(GpuEncodeOutcome::Failed)?;
        let finish_started = Instant::now();
        self.failure.enter(ExportPhase::Mux, None);
        let mut finish = || -> Result<(), String> {
            progress(crate::protocol::Progress::finalizing(
                encode_state.completed_video,
                total_frames,
            ))
            .map_err(|error| error.message)?;
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
            Ok(())
        };
        finish().map_err(GpuEncodeOutcome::Failed)?;
        let mut encode_worker = encode_worker;
        encode_worker
            .join_success()
            .map_err(GpuEncodeOutcome::Failed)?;
        metrics.encode_worker_idle = encode_worker.idle.total();
        metrics.encode_channel_depth_samples = encode_worker.depth.samples();
        metrics.mux_finish = finish_started.elapsed();
        let pool = input.stats();
        metrics.encoder_frames_allocated = pool.allocated;
        metrics.encoder_surface_acquires = pool.acquired;
        metrics.encoder_surface_lease_peak = frame_bound.peak();
        metrics.encoder_input_copy_operations = pool.copy_operations;
        metrics.encoder_input_copy_bytes = pool.copy_bytes;
        // Same order as the cancel path: handoffs are waited on before the planes they read drop.
        metrics.retired_leases_at_exit = self.clean_up_after_output(output, || {
            input.finish().map_err(error_message)?;
            self.gpu_renderer()
                .reclaim_external_leases()
                .map_err(error_message)
        });
        let completed = encode_state.completed_video;
        // A profiling file must not fail an export whose output is already written.
        if let Some(trace) = metrics.trace.as_ref() {
            match trace.write() {
                Ok(path) => {
                    alpha2_os::diagnostic!("alpha2 native export trace: {}", path.display());
                }
                Err(error) => alpha2_os::diagnostic!("alpha2 native export trace failed: {error}"),
            }
        }

        let mut result = self.response_metadata(started.elapsed().as_secs_f64() * 1_000.0);
        result["frames"] = json!(completed);
        result["audio"] = json!(has_audio);
        result["videoCodec"] = json!(capability.codec.as_str());
        result["videoEncoder"] = json!(gpu_selection.encoder_name);
        result["videoEncoderClass"] = json!(gpu_selection.class.as_str());
        result["videoEncoderHardware"] = json!(gpu_selection.hardware_accelerated);
        result["videoEncoderLibrary"] = Value::Null;
        result["videoEncoderPreferred"] = json!(gpu_selection.encoder_name);
        result["videoEncoderFallbacks"] = json!([]);
        result["videoOutputPath"] = json!(VideoOutputPath::GpuResident.as_str());
        result["gpuOutputPathUnavailable"] = Value::Null;
        result["videoTranscode"] = json!(transcode_plan.map(|_| "yuv-repack"));
        result["yuvTranscodeIneligible"] = json!(transcode.as_ref().err());
        result["effectiveRange"] = effective_range_json(effective_range);
        result["color"] = self.color_pipeline_json(project.settings.color);
        result["loudness"] = crate::host::finish_loudness(loudness_report, audio_renderer.as_ref());
        result["videoRateControl"] = video_rate.to_json(&gpu_selection);
        result["videoDecodeMode"] = json!(self.decode_policy.as_str());
        result["videoDecodeBackend"] = json!(alpha2_native::hardware_decode_backend());
        result["hardwareDecodeReady"] = json!(plan.hardware_decode);
        result["hardwareDecodeUnavailable"] = json!(self.hardware_decode_unavailable);
        result["videoThreadPolicy"] = json!(codec_thread_policy(true));
        result["decoderThreadPolicy"] = json!(decoder_thread_policy(plan.hardware_decode, false));
        result["videoThreadCount"] = json!(Option::<i64>::None);
        result["audioCodec"] = json!(has_audio.then(|| audio_codec.as_str()));
        result["audioRateControl"] = if has_audio {
            audio_rate.to_json()
        } else {
            Value::Null
        };
        result["audioEncoder"] = json!(has_audio.then_some("aac"));
        result["fps"] = json!(frames_per_second(
            completed,
            started.elapsed().as_secs_f64()
        ));
        let after = self.gpu_renderer().diagnostics();
        metrics.peak_provider_source_bytes = plan.providers.peak_bytes();
        metrics.provider_source_reopens = plan.providers.reopens();
        result["performance"] =
            export_metrics_json(&metrics, Some((&graph_before, &after)), &pipeline_limits);
        Ok(result)
    }
}

/// Adds one transcoded picture's preparation to `metrics`.
fn record_repack(metrics: &mut ExportMetrics, repack: &crate::frames::NativeRepack) {
    metrics.direct_yuv_frames = metrics.direct_yuv_frames.saturating_add(1);
    metrics.record_native_surfaces(repack.native_surface_bytes, repack.native_surfaces);
    metrics.hardware_import_frames = metrics
        .hardware_import_frames
        .saturating_add(u64::from(repack.hardware_import));
    metrics.hardware_copy_frames = metrics
        .hardware_copy_frames
        .saturating_add(u64::from(repack.gpu_copy_bytes > 0));
    metrics.source_gpu_copy_bytes = metrics
        .source_gpu_copy_bytes
        .saturating_add(repack.gpu_copy_bytes);
    metrics.hardware_fallbacks.record_many(
        alpha2_native::HardwareDecodeFallback::ImportFailed,
        u64::from(repack.import_fallback),
    );
    metrics.source_upload_bytes = metrics
        .source_upload_bytes
        .saturating_add(repack.upload_bytes);
    metrics.surface_budget_stalls = metrics
        .surface_budget_stalls
        .saturating_add(u64::from(repack.surface_budget_stalled));
    metrics.source_upload_timings = metrics.source_upload_timings.merged(repack.timings);
}
