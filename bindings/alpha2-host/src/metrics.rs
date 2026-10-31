use std::time::Duration;

use crate::decode::HardwareFallbackCounts;
use crate::frames::{estimated_staging_bytes, packed_yuv420_bytes};
use crate::{MAX_ENCODE_CHANNEL_DEPTH, MAX_IN_FLIGHT_VIDEO_FRAMES, MIN_ENCODE_CHANNEL_DEPTH};
use alpha2_render::RenderSize;
use serde_json::{Value, json};

/// `videoPipeline` in every result and metrics file.
///
/// Several counters (encode queue, channel depth, readback stages and so on) read zero on a path
/// where the structure they measure does not exist; naming the path lets a reader tell "zero
/// because idle" from "zero because absent".
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum VideoPipeline {
    /// Rendered into a staging buffer, mapped, packed, and encoded from CPU memory.
    #[default]
    Readback,
    /// Rendered straight into encoder-input images. Nothing crosses the CPU.
    #[cfg(feature = "hwencode")]
    GpuResident,
    /// The source's compressed packets copied; nothing rendered or encoded.
    PacketCopy,
}

impl VideoPipeline {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Readback => "readback",
            #[cfg(feature = "hwencode")]
            Self::GpuResident => "gpu-resident",
            Self::PacketCopy => "packet-copy",
        }
    }
}

#[derive(Default)]
pub struct ExportMetrics {
    pub video_pipeline: VideoPipeline,
    pub pipeline_enabled: bool,
    pub decode_lookahead: usize,
    pub decode: Duration,
    pub decode_wait: Duration,
    /// The export thread's render-cache lookups, hit or miss.
    pub render_cache_lookup: Duration,
    pub evaluate: Duration,
    pub audio_decode: Duration,
    pub audio_mix: Duration,
    pub audio_encode: Duration,
    pub source_prepare: Duration,
    pub source_upload_timings: crate::frames::SourceUploadTimings,
    pub decode_stages: alpha2_native::DecodeStageTimings,
    pub decode_budget_wait: Duration,
    pub decode_handoff_wait: Duration,
    pub encode_worker_idle: Duration,
    pub trace: Option<crate::trace::ExportTrace>,
    /// `pipelineCompileMs`: the first frame that draws a built-in compiles its shaders.
    pub pipeline_compile: Duration,
    pub frame_submit: Duration,
    pub frame_submit_stages: alpha2_engine::ExportStepTimings,
    pub readback_map_latency: Duration,
    pub readback_wait: Duration,
    pub readback_pack: Duration,
    /// `encodeQueueWaitMs`: waiting on a full channel, or on a rendezvous channel until the worker
    /// took the command.
    pub encode_queue: Duration,
    pub video_encode: Duration,
    /// Split out of `video_encode`.
    pub encode_stages: alpha2_native::EncodeStageTimings,
    pub publish: Option<alpha2_native::PublishReport>,
    /// Indexed by the depth left behind when the worker took a command.
    pub encode_channel_depth_samples: Vec<u64>,
    pub mux_finish: Duration,
    pub peak_readback_bytes: u64,
    pub peak_audio_window_bytes: u64,
    /// Peak between frames, against `memoryPolicy.providerSourceBytes`.
    pub peak_provider_source_bytes: u64,
    /// Provider sources the budget closed that a later frame opened again.
    pub provider_source_reopens: u64,
    pub peak_encode_queue_bytes: u64,
    pub max_pending_media: usize,
    pub max_in_flight_readbacks: usize,
    pub max_encode_queue: usize,
    /// `encodeBackpressureCount`: on a queued channel the channel was full; on a rendezvous
    /// channel the worker was not waiting when the frame was ready.
    pub encode_backpressure: u64,
    pub pending_depth_total: u64,
    pub pending_depth_samples: u64,
    pub peak_decode_queue_bytes: u64,
    pub max_decode_queue: usize,
    pub decode_backpressure: u64,
    pub ready_readback_dispatches: u64,
    pub decode_concurrency: usize,
    pub decoder_sessions: Option<crate::config::DecoderSessionCount>,
    /// Sessions opened again for a cursor that had one before.
    pub decoder_session_reopens: u64,
    pub decoder_threads: i64,
    pub hardware_decoder_threads: i64,
    pub codec_thread_budget: usize,
    pub codec_threads_allocated: usize,
    pub encoder_threads: i64,
    /// Luma samples in the largest video source, which sizes software decoders.
    pub source_frame_pixels: u64,
    pub source_probe: Duration,
    pub reverse_cache_hits: u64,
    pub reverse_windows_built: u64,
    pub peak_reverse_cache_bytes: u64,
    pub direct_yuv_frames: u64,
    pub rgba_fallback_frames: u64,
    pub physical_source_reuses: u64,
    pub hardware_import_frames: u64,
    pub hardware_copy_frames: u64,
    pub software_decoded_frames: u64,
    pub hardware_decoded_frames: u64,
    pub hardware_fallbacks: HardwareFallbackCounts,
    pub source_upload_bytes: u64,
    pub source_gpu_copy_bytes: u64,
    /// Raw video the readback path maps for the CPU; zero on the GPU-resident path.
    pub output_readback_bytes: u64,
    /// From a readback or from the baked frame cache.
    pub output_cpu_pack_bytes: u64,
    /// One copy per plane.
    pub encoder_input_copy_operations: u64,
    pub encoder_input_copy_bytes: u64,
    pub peak_native_surface_bytes: u64,
    pub peak_native_surfaces: u32,
    pub peak_resident_stills: usize,
    pub surface_budget_stalls: u64,
    /// Nonzero is normal: the last submission's leases are released here, not by a submission.
    pub retired_leases_at_exit: usize,
    /// On the readback path, CPU frames; on the GPU-resident path, the distinct surfaces the
    /// hardware pool allocated, including what the handoff and codec held beyond the host's
    /// leases.
    pub encoder_frames_allocated: u64,
    pub encoder_surface_acquires: u64,
    /// Capped by the lease bound; not what the pool allocated (`encoder_frames_allocated`).
    pub encoder_surface_lease_peak: u64,
    /// GPU-resident path: waits for an encoder surface after a frame's sources were installed
    /// (`encoderSurfaceWaitMs`), and for the plane pair the handoff last read
    /// (`encoderPlaneWaitMs`).
    pub encoder_surface_wait: Duration,
    pub encoder_plane_wait: Duration,
    /// Surfaces the renderer could not import (`sourceDownloadBytes`, `sourceDownloadMs`).
    pub source_download_bytes: u64,
    pub source_download: Duration,
}

impl ExportMetrics {
    /// Both paths install through this, so neither reports a counter the other leaves at zero.
    /// Durations are timed by each path.
    pub fn record_install(&mut self, upload: &crate::frames::FrameInstallMetrics) {
        self.peak_resident_stills = self.peak_resident_stills.max(upload.resident_stills);
        self.direct_yuv_frames = self
            .direct_yuv_frames
            .saturating_add(upload.direct_yuv_frames);
        self.rgba_fallback_frames = self
            .rgba_fallback_frames
            .saturating_add(upload.rgba_fallback_frames);
        self.physical_source_reuses = self
            .physical_source_reuses
            .saturating_add(upload.physical_reuses);
        self.hardware_import_frames = self
            .hardware_import_frames
            .saturating_add(upload.hardware_import_frames);
        self.hardware_fallbacks.record_many(
            alpha2_native::HardwareDecodeFallback::ImportFailed,
            upload.import_fallbacks,
        );
        self.source_download_bytes = self
            .source_download_bytes
            .saturating_add(upload.source_download_bytes);
        self.source_download = self.source_download.saturating_add(upload.source_download);
        self.hardware_copy_frames = self
            .hardware_copy_frames
            .saturating_add(upload.hardware_copy_frames);
        self.source_upload_bytes = self
            .source_upload_bytes
            .saturating_add(upload.source_upload_bytes);
        self.source_gpu_copy_bytes = self
            .source_gpu_copy_bytes
            .saturating_add(upload.source_gpu_copy_bytes);
        self.surface_budget_stalls = self
            .surface_budget_stalls
            .saturating_add(upload.surface_budget_stalls);
        self.source_upload_timings = self.source_upload_timings.merged(upload.timings);
        self.record_native_surfaces(upload.native_surface_bytes, upload.native_surfaces);
    }

    pub fn record_native_surfaces(&mut self, bytes: u64, surfaces: u32) {
        self.peak_native_surface_bytes = self.peak_native_surface_bytes.max(bytes);
        self.peak_native_surfaces = self.peak_native_surfaces.max(surfaces);
    }

    pub fn record_pipelines(&mut self, frame: u64, creations: &[alpha2_render::PipelineCreation]) {
        for creation in creations {
            self.pipeline_compile = self
                .pipeline_compile
                .saturating_add(creation.finished.duration_since(creation.started));
        }
        if let Some(trace) = self.trace.as_mut() {
            trace.push_pipelines(frame, creations);
        }
    }

    pub fn record_decode_pool(&mut self, worker: &crate::decode::DecodeWorker) {
        self.peak_decode_queue_bytes = self.peak_decode_queue_bytes.max(worker.peak_bytes());
        self.decode_budget_wait = worker.waits.budget();
        self.decode_handoff_wait = worker.waits.handoff();
    }
}

#[derive(Debug)]
pub struct ExportPipelineLimits {
    pub byte_budget: u64,
    pub readback_depth: usize,
    pub encode_channel_depth: usize,
    pub staging_bytes_per_frame: u64,
    pub packed_bytes_per_frame: u64,
    /// `None` on the readback path.
    #[cfg(feature = "hwencode")]
    pub gpu: Option<GpuEncodeBudget>,
}

/// Divides the in-flight byte budget between readbacks and the encode channel.
///
/// `requested_encode_depth` pins the depth; `None` derives it. Readbacks are capped at
/// [`MAX_IN_FLIGHT_VIDEO_FRAMES`], then the encode channel takes what is left within
/// [`MIN_ENCODE_CHANNEL_DEPTH`]..=[`MAX_ENCODE_CHANNEL_DEPTH`], reduced until both fit, so a small
/// budget degrades instead of failing.
///
/// # Errors
/// When the budget cannot hold one readback and two packed frames (a rendezvous channel's), which
/// is a configuration error rather than backpressure.
pub fn export_pipeline_limits(
    size: RenderSize,
    format: wgpu::TextureFormat,
    budget: crate::memory::InFlightBudget,
    requested_depth: usize,
    requested_encode_depth: Option<usize>,
) -> Result<ExportPipelineLimits, String> {
    let staging_bytes_per_frame = estimated_staging_bytes(size, format);
    let packed_bytes_per_frame = packed_yuv420_bytes(size);
    // A retry's scale never takes the budget below what a rendezvous channel needs.
    let byte_budget = budget.holding(
        staging_bytes_per_frame.saturating_add(
            packed_bytes_per_frame
                .saturating_mul(u64::try_from(readback_encode_frame_bound(0)).unwrap_or(u64::MAX)),
        ),
    );
    let start = requested_encode_depth.map_or_else(
        || {
            derived_encode_channel_depth(
                byte_budget,
                staging_bytes_per_frame,
                packed_bytes_per_frame,
                requested_depth,
            )
        },
        |depth| depth.min(MAX_ENCODE_CHANNEL_DEPTH),
    );
    for encode_channel_depth in (0..=start).rev() {
        let encode_slots =
            u64::try_from(readback_encode_frame_bound(encode_channel_depth)).unwrap_or(u64::MAX);
        let encode_reserve = packed_bytes_per_frame.saturating_mul(encode_slots);
        let available = byte_budget.saturating_sub(encode_reserve);
        let possible_depth = available / staging_bytes_per_frame.max(1);
        let readback_depth = usize::try_from(possible_depth)
            .unwrap_or(MAX_IN_FLIGHT_VIDEO_FRAMES)
            .min(requested_depth)
            .min(MAX_IN_FLIGHT_VIDEO_FRAMES);
        if readback_depth > 0
            && encode_reserve.saturating_add(
                staging_bytes_per_frame
                    .saturating_mul(u64::try_from(readback_depth).unwrap_or(u64::MAX)),
            ) <= byte_budget
        {
            return Ok(ExportPipelineLimits {
                byte_budget,
                readback_depth,
                encode_channel_depth,
                staging_bytes_per_frame,
                packed_bytes_per_frame,
                #[cfg(feature = "hwencode")]
                gpu: None,
            });
        }
    }
    Err(crate::memory::resource_limit(format!(
        "{} cannot hold one readback and two packed {}x{} YUV420P frames",
        budget.describe(),
        size.width,
        size.height
    )))
}

/// Counts the renderer's plane ring, the surfaces the host holds, and those the handoff still
/// holds after the host lets go. The codec's own reference pictures are the driver's and are not
/// counted.
#[cfg(feature = "hwencode")]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GpuEncodeBudget {
    /// One encoder surface at the coded size.
    pub surface_bytes: u64,
    pub ring_bytes: u64,
    /// The channel, plus one surface being handed a frame and one being encoded.
    pub lease_bound: usize,
    /// Surfaces the handoff and the encoder may still hold on top of those.
    pub retained_surfaces: usize,
}

#[cfg(feature = "hwencode")]
impl GpuEncodeBudget {
    #[must_use]
    pub const fn surface_ceiling(&self) -> usize {
        self.lease_bound + self.retained_surfaces
    }

    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.ring_bytes.saturating_add(
            self.surface_bytes
                .saturating_mul(u64::try_from(self.surface_ceiling()).unwrap_or(u64::MAX)),
        )
    }
}

/// Encoder surfaces the GPU-resident path may hold, from the readback path's byte budget.
///
/// The budget pays for the plane ring, then for surfaces. Two surfaces are never in the channel
/// (being handed a frame, being encoded), so the depth is capped by what is left after those and
/// the handoff's retention; below that it is the backend's measured default unless pinned, and a
/// pinned depth is clamped too.
///
/// # Errors
/// A budget that cannot hold the ring and the minimum refuses before anything opens, so the export
/// falls back (or fails when the GPU path is required) instead of exceeding its budget.
#[cfg(feature = "hwencode")]
pub fn gpu_encode_pipeline_limits(
    capability: &alpha2_native::HardwareEncodeCapability,
    size: RenderSize,
    budget: crate::memory::InFlightBudget,
    requested_encode_depth: Option<usize>,
) -> Result<ExportPipelineLimits, String> {
    let layout = capability.input_layout;
    let coded = capability.coded(size.width, size.height);
    let surface_bytes = layout.frame_bytes(coded.0, coded.1);
    let frame_bytes = layout.frame_bytes(size.width, size.height);
    let ring_bytes =
        frame_bytes.saturating_mul(u64::try_from(capability.plane_ring_slots).unwrap_or(u64::MAX));
    let retained_surfaces = capability.retained_surfaces();
    let minimum = retained_surfaces.saturating_add(2);
    // A retry's scale never takes the budget below the plane ring and the least surfaces.
    let byte_budget =
        budget.holding(ring_bytes.saturating_add(
            surface_bytes.saturating_mul(u64::try_from(minimum).unwrap_or(u64::MAX)),
        ));
    let affordable = usize::try_from(byte_budget.saturating_sub(ring_bytes) / surface_bytes.max(1))
        .unwrap_or(usize::MAX);
    if affordable < minimum {
        return Err(format!(
            "{} cannot hold the GPU path's plane ring ({} slots, {ring_bytes} B) and its \
             minimum of {minimum} encoder surfaces ({surface_bytes} B each)",
            budget.describe(),
            capability.plane_ring_slots
        ));
    }
    let ceiling = (affordable - minimum).min(MAX_ENCODE_CHANNEL_DEPTH);
    let encode_channel_depth = requested_encode_depth
        .map_or(ceiling.min(capability.default_channel_depth), |depth| {
            depth.min(ceiling)
        });
    Ok(ExportPipelineLimits {
        byte_budget,
        readback_depth: 0,
        encode_channel_depth,
        staging_bytes_per_frame: 0,
        packed_bytes_per_frame: frame_bytes,
        gpu: Some(GpuEncodeBudget {
            surface_bytes,
            ring_bytes,
            lease_bound: gpu_encode_frame_bound(encode_channel_depth),
            retained_surfaces,
        }),
    })
}

/// One being handed a frame, one being encoded, and the rest queued between them.
#[cfg(feature = "hwencode")]
#[must_use]
pub const fn gpu_encode_frame_bound(encode_channel_depth: usize) -> usize {
    encode_channel_depth.saturating_add(2)
}

/// As [`gpu_encode_frame_bound`]: the coordinator takes a frame from the pool before its send
/// blocks and the worker returns one only after encoding, so both count.
#[must_use]
pub const fn readback_encode_frame_bound(encode_channel_depth: usize) -> usize {
    encode_channel_depth.saturating_add(2)
}

/// Packed frames the budget still holds after the readbacks, minus the two
/// [`readback_encode_frame_bound`] counts.
///
/// Counted in bytes, not slots: a 4K and a 720p frame differ 18x in size. At 4K with the 256 MiB
/// default the count cap binds first.
fn derived_encode_channel_depth(
    byte_budget: u64,
    staging_bytes_per_frame: u64,
    packed_bytes_per_frame: u64,
    requested_depth: usize,
) -> usize {
    let readback_depth = requested_depth.min(MAX_IN_FLIGHT_VIDEO_FRAMES);
    let readback_reserve =
        staging_bytes_per_frame.saturating_mul(u64::try_from(readback_depth).unwrap_or(u64::MAX));
    let slots = byte_budget.saturating_sub(readback_reserve) / packed_bytes_per_frame.max(1);
    usize::try_from(slots.saturating_sub(2))
        .unwrap_or(MAX_ENCODE_CHANNEL_DEPTH)
        .clamp(MIN_ENCODE_CHANNEL_DEPTH, MAX_ENCODE_CHANNEL_DEPTH)
}

fn add_decode_path_metrics(value: &mut Value, metrics: &ExportMetrics) {
    value["softwareDecodedFrames"] = json!(metrics.software_decoded_frames);
    value["hardwareDecodedFrames"] = json!(metrics.hardware_decoded_frames);
    value["hardwareFallbackReasons"] = Value::Object(
        metrics
            .hardware_fallbacks
            .entries()
            .map(|(reason, count)| (reason.to_owned(), json!(count)))
            .collect(),
    );
    value["hardwareFallbackMessages"] = Value::Object(
        metrics
            .hardware_fallbacks
            .messages()
            .map(|(reason, message)| (reason.to_owned(), json!(message)))
            .collect(),
    );
    value["hardwareImportFrames"] = json!(metrics.hardware_import_frames);
    value["hardwareCopyFrames"] = json!(metrics.hardware_copy_frames);
    value["hardwareDirectFrames"] = json!(
        metrics
            .hardware_import_frames
            .saturating_sub(metrics.hardware_copy_frames)
    );
    value["sourceUploads"] = json!(
        metrics
            .direct_yuv_frames
            .saturating_add(metrics.rgba_fallback_frames)
            .saturating_sub(metrics.hardware_import_frames)
    );
    value["sourceUploadBytes"] = json!(metrics.source_upload_bytes);
    // Software planes are written once and uploaded: one set of bytes at two stages. A hardware
    // surface the renderer could not import is downloaded first, and counted.
    value["sourceCpuPlaneCopyBytes"] = json!(metrics.source_upload_bytes);
    value["sourceDownloadBytes"] = json!(metrics.source_download_bytes);
    value["sourceDownloadMs"] = milliseconds(metrics.source_download);
    value["sourceGpuCopyBytes"] = json!(metrics.source_gpu_copy_bytes);
    value["outputReadbackBytes"] = json!(metrics.output_readback_bytes);
    value["outputCpuPackBytes"] = json!(metrics.output_cpu_pack_bytes);
    value["encoderInputCopyOperations"] = json!(metrics.encoder_input_copy_operations);
    value["encoderInputCopyBytes"] = json!(metrics.encoder_input_copy_bytes);
    value["peakNativeSurfaceBytes"] = json!(metrics.peak_native_surface_bytes);
    value["peakNativeSurfaces"] = json!(metrics.peak_native_surfaces);
    value["peakResidentStills"] = json!(metrics.peak_resident_stills);
    // A nonzero count means the surface budget, not the decoder, paced the run.
    value["surfaceBudgetStalls"] = json!(metrics.surface_budget_stalls);
    value["retiredLeasesAtExit"] = json!(metrics.retired_leases_at_exit);
    value["encoderFramesAllocated"] = json!(metrics.encoder_frames_allocated);
    value["encoderSurfaceAcquires"] = json!(metrics.encoder_surface_acquires);
    value["encoderSurfaceLeasePeak"] = json!(metrics.encoder_surface_lease_peak);
    value["encoderSurfaceWaitMs"] = milliseconds(metrics.encoder_surface_wait);
    value["encoderPlaneWaitMs"] = milliseconds(metrics.encoder_plane_wait);
}

/// Adds `gpuRenderMsPerFrame` (the graph) and `gpuPackMsPerFrame` (graph end through the pack),
/// means over `gpuTimeSampledFrames`, or `gpuTimeUnavailable` saying why there are none.
/// `gpuSubmissions` counts how frames were cut into submissions to stay under the GPU watchdog.
pub fn add_gpu_time_metrics(value: &mut Value, renderer: Option<&alpha2_render::Renderer>) {
    let times = renderer.map_or(Err("the CPU renderer runs no GPU work"), |renderer| {
        renderer.take_frame_gpu_times()
    });
    let (render, pack, sampled, unavailable) = match times {
        Ok(times) if times.sampled_frames > 0 => (
            times.render_ms_per_frame(),
            times.pack_ms_per_frame(),
            times.sampled_frames,
            None,
        ),
        Ok(times) if times.flat_frames > 0 => (
            None,
            None,
            0,
            Some("the device's timestamps did not advance across the frame's passes"),
        ),
        Ok(_) => (
            None,
            None,
            0,
            Some("no sampled frame's timestamps were read back before the export ended"),
        ),
        Err(reason) => (None, None, 0, Some(reason)),
    };
    value["gpuRenderMsPerFrame"] = json!(render);
    value["gpuPackMsPerFrame"] = json!(pack);
    value["gpuTimeSampledFrames"] = json!(sampled);
    value["gpuTimeUnavailable"] = json!(unavailable);
    value["gpuSubmissions"] = renderer.map_or(Value::Null, |renderer| {
        let stats = renderer.take_gpu_submission_stats();
        json!({
            "frames": stats.frames,
            "splitFrames": stats.split_frames,
            "maxPerFrame": stats.max_per_frame,
            "maxPredictedMs": stats.max_predicted_ms.map(|ms| (ms * 10.0).round() / 10.0),
        })
    });
}

fn milliseconds(duration: Duration) -> Value {
    json!(duration.as_secs_f64() * 1_000.0)
}

/// Splits the stage totals a single number cannot attribute, reporting each total beside its
/// parts.
///
/// The frame-submit and source-upload parts sum to their totals (one thread's consecutive spans).
/// The decode parts do not: they accumulate in concurrent shard threads while `decodeMs` measures
/// the pool thread's service time.
pub fn add_stage_split_metrics(value: &mut Value, metrics: &ExportMetrics) {
    let decode = &metrics.decode_stages;
    value["decodeDemuxMs"] = milliseconds(decode.demux);
    value["decodeSendPacketMs"] = milliseconds(decode.send_packet);
    value["decodeReceiveFrameMs"] = milliseconds(decode.receive_frame);
    value["decodeConvertMs"] = milliseconds(decode.convert);
    value["decodeSeekMs"] = milliseconds(decode.seek);
    // Both are zero while the host keeps up with the decoder.
    value["decodeBudgetWaitMs"] = milliseconds(metrics.decode_budget_wait);
    value["decodeHandoffWaitMs"] = milliseconds(metrics.decode_handoff_wait);

    let upload = &metrics.source_upload_timings;
    value["sourceCpuPlaneUploadMs"] = milliseconds(upload.cpu_plane_upload);
    value["sourceGpuImportMs"] = milliseconds(upload.gpu_import);
    value["surfaceBudgetWaitMs"] = milliseconds(upload.surface_budget_wait);

    let submit = &metrics.frame_submit_stages.submit;
    value["frameEvaluateMs"] = milliseconds(metrics.frame_submit_stages.evaluate);
    value["framePrepareMs"] = milliseconds(metrics.frame_submit_stages.prepare);
    value["frameGraphBuildMs"] = milliseconds(submit.graph_build);
    value["frameCommandEncodeMs"] = milliseconds(submit.command_encode);
    value["frameSourceAcquireMs"] = milliseconds(submit.source_acquire);
    value["frameQueueLockWaitMs"] = milliseconds(submit.queue_lock_wait);
    value["frameQueueSubmitMs"] = milliseconds(submit.queue_submit);
    value["frameLeaseRetireMs"] = milliseconds(submit.lease_retire);
    value["frameCompleteMs"] = milliseconds(submit.complete);

    // Encoder occupancy is `videoEncodeMs / (videoEncodeMs + this)`; without it a saturated and a
    // starved encoder report the same work.
    value["encodeWorkerIdleMs"] = milliseconds(metrics.encode_worker_idle);

    // The parts sum to the total.
    let encode = &metrics.encode_stages;
    // Waiting for the render that packed a surface; not in `encoderSendFrameMs`.
    value["encoderInputReadyWaitMs"] = milliseconds(encode.video_input_ready_wait);
    value["encoderSendFrameMs"] = milliseconds(encode.video_send_frame);
    value["encoderReceivePacketMs"] = milliseconds(encode.video_receive_packet);
    value["muxWriteMs"] = milliseconds(encode.video_mux_write);
    value["audioEncoderSendFrameMs"] = milliseconds(encode.audio_send_frame);
    value["audioEncoderReceivePacketMs"] = milliseconds(encode.audio_receive_packet);
    value["audioMuxWriteMs"] = milliseconds(encode.audio_mux_write);
    // Mass at zero is a starved encoder; mass at the configured depth is a saturated one.
    value["encodeChannelDepthSamples"] = json!(metrics.encode_channel_depth_samples);
}

fn append_render_work_metrics(
    value: &mut Value,
    before: &alpha2_render::RenderDiagnostics,
    after: &alpha2_render::RenderDiagnostics,
) {
    value["graphNodes"] = json!(after.graph_nodes.saturating_sub(before.graph_nodes));
    value["graphPasses"] = json!(after.graph_passes.saturating_sub(before.graph_passes));
    value["sourceConversions"] = json!(
        after
            .source_conversions
            .saturating_sub(before.source_conversions)
    );
    value["yuvRepacks"] = json!(after.yuv_repacks.saturating_sub(before.yuv_repacks));
    value["renderPasses"] = json!(after.render_passes.saturating_sub(before.render_passes));
    value["sourceSubmissions"] = json!(
        after
            .source_submissions
            .saturating_sub(before.source_submissions)
    );
    value["graphSubmissions"] = json!(
        after
            .graph_submissions
            .saturating_sub(before.graph_submissions)
    );
    value["readbackSubmissions"] = json!(
        after
            .readback_submissions
            .saturating_sub(before.readback_submissions)
    );
    value["frameSubmissions"] = json!(
        after
            .frame_submissions
            .saturating_sub(before.frame_submissions)
    );
    // Only the readback's texture-to-buffer copy; source uploads and imports are reported apart.
    value["readbackCopyOperations"] =
        json!(after.copy_operations.saturating_sub(before.copy_operations));
    value["readbackCopyBytes"] = json!(after.copy_bytes.saturating_sub(before.copy_bytes));
}

/// Renderer counters are the difference between `before` and `after`, so they cover this export.
#[allow(clippy::cast_precision_loss)]
pub fn export_metrics_json(
    metrics: &ExportMetrics,
    gpu: Option<(
        &alpha2_render::RenderDiagnostics,
        &alpha2_render::RenderDiagnostics,
    )>,
    limits: &ExportPipelineLimits,
) -> Value {
    let mut value = json!({
        "videoPipeline": metrics.video_pipeline.as_str(),
        "pipelineEnabled": metrics.pipeline_enabled,
        "decodeLookahead": metrics.decode_lookahead,
        "decodeMs": metrics.decode.as_secs_f64() * 1_000.0,
        "decodeWaitMs": metrics.decode_wait.as_secs_f64() * 1_000.0,
        "renderCacheLookupMs": metrics.render_cache_lookup.as_secs_f64() * 1_000.0,
        "evaluateMs": metrics.evaluate.as_secs_f64() * 1_000.0,
        "audioDecodeMs": metrics.audio_decode.as_secs_f64() * 1_000.0,
        "audioMixMs": metrics.audio_mix.as_secs_f64() * 1_000.0,
        "audioEncodeMs": metrics.audio_encode.as_secs_f64() * 1_000.0,
        "sourceConvertUploadMs": metrics.source_prepare.as_secs_f64() * 1_000.0,
        "pipelineCompileMs": metrics.pipeline_compile.as_secs_f64() * 1_000.0,
        "frameSubmitMs": metrics.frame_submit.as_secs_f64() * 1_000.0,
        "readbackMapLatencyMs": metrics.readback_map_latency.as_secs_f64() * 1_000.0,
        "readbackWaitMs": metrics.readback_wait.as_secs_f64() * 1_000.0,
        "readbackPackMs": metrics.readback_pack.as_secs_f64() * 1_000.0,
        "encodeQueueWaitMs": metrics.encode_queue.as_secs_f64() * 1_000.0,
        "videoEncodeMs": metrics.video_encode.as_secs_f64() * 1_000.0,
        "muxFinishMs": metrics.mux_finish.as_secs_f64() * 1_000.0,
        "inFlightDepth": limits.readback_depth,
        "encodeChannelDepth": limits.encode_channel_depth,
        "inFlightByteBudget": limits.byte_budget,
        "stagingBytesPerFrame": limits.staging_bytes_per_frame,
        "packedBytesPerFrame": limits.packed_bytes_per_frame,
        "peakReadbackBytes": metrics.peak_readback_bytes,
        "peakAudioWindowBytes": metrics.peak_audio_window_bytes,
        "peakProviderSourceBytes": metrics.peak_provider_source_bytes,
        "providerSourceReopens": metrics.provider_source_reopens,
        "peakEncodeQueueBytes": metrics.peak_encode_queue_bytes,
        "maxPendingMediaEvents": metrics.max_pending_media,
        "averagePendingMediaEvents": if metrics.pending_depth_samples == 0 {
            0.0
        } else {
            metrics.pending_depth_total as f64 / metrics.pending_depth_samples as f64
        },
        "maxInFlightReadbacks": metrics.max_in_flight_readbacks,
        "maxEncodeQueueEvents": metrics.max_encode_queue,
        "encodeBackpressureCount": metrics.encode_backpressure,
        "decodeBackpressureCount": metrics.decode_backpressure,
        "maxDecodeQueueFrames": metrics.max_decode_queue,
        "peakDecodeQueueBytes": metrics.peak_decode_queue_bytes,
        "readyReadbackDispatches": metrics.ready_readback_dispatches,
        "directYuvFrames": metrics.direct_yuv_frames,
        "rgbaFallbackFrames": metrics.rgba_fallback_frames,
        "physicalSourceReuses": metrics.physical_source_reuses,
    });
    add_decode_path_metrics(&mut value, metrics);
    add_stage_split_metrics(&mut value, metrics);
    value["decodeConcurrency"] = json!(metrics.decode_concurrency);
    value["decoderSessions"] = json!(metrics.decoder_sessions.map(|count| count.sessions));
    value["decoderSessionLimit"] =
        json!(metrics.decoder_sessions.map(|count| count.limit.as_str()));
    value["decoderSessionPeak"] = json!(metrics.decoder_sessions.map(|count| count.peak));
    value["decoderSessionReopens"] = json!(metrics.decoder_session_reopens);
    value["decoderThreads"] = json!(metrics.decoder_threads);
    value["hardwareDecoderThreads"] = json!(metrics.hardware_decoder_threads);
    value["codecThreadBudget"] = json!(metrics.codec_thread_budget);
    value["codecThreadsAllocated"] = json!(metrics.codec_threads_allocated);
    value["encoderThreads"] = json!(metrics.encoder_threads);
    value["maxSourceFramePixels"] = json!(metrics.source_frame_pixels);
    value["sourceProbeMs"] = milliseconds(metrics.source_probe);
    value["reverseDecodeCacheHits"] = json!(metrics.reverse_cache_hits);
    value["reverseDecodeWindowsBuilt"] = json!(metrics.reverse_windows_built);
    value["peakReverseCacheBytes"] = json!(metrics.peak_reverse_cache_bytes);
    // The GPU renderer's pools and passes; the CPU renderer reports its own.
    if let Some((before, after)) = gpu {
        append_render_work_metrics(&mut value, before, after);
        append_buffer_pool_metrics(&mut value, before, after);
    }
    #[cfg(feature = "hwencode")]
    let gpu = limits.gpu;
    #[cfg(not(feature = "hwencode"))]
    let gpu: Option<std::convert::Infallible> = None;
    value["gpuEncodeBudget"] = gpu.map_or(Value::Null, |budget| {
        #[cfg(feature = "hwencode")]
        return json!({
            "surfaceBytes": budget.surface_bytes,
            "planeRingBytes": budget.ring_bytes,
            "leaseBound": budget.lease_bound,
            "retainedSurfaces": budget.retained_surfaces,
            "surfaceCeiling": budget.surface_ceiling(),
            "reservedBytes": budget.reserved_bytes(),
        });
        #[cfg(not(feature = "hwencode"))]
        match budget {}
    });
    value
}

pub fn append_buffer_pool_metrics(
    value: &mut Value,
    before: &alpha2_render::RenderDiagnostics,
    after: &alpha2_render::RenderDiagnostics,
) {
    value["texturesCreated"] = json!(
        after
            .textures
            .created
            .saturating_sub(before.textures.created)
    );
    value["texturesReused"] = json!(after.textures.reused.saturating_sub(before.textures.reused));
    value["texturesEvicted"] = json!(
        after
            .textures
            .evicted
            .saturating_sub(before.textures.evicted)
    );
    value["textureHighWaterBytes"] = json!(after.textures.high_water_bytes);
    value["uploadTexturesCreated"] = json!(
        after
            .upload_textures
            .created
            .saturating_sub(before.upload_textures.created)
    );
    value["uploadTexturesReused"] = json!(
        after
            .upload_textures
            .reused
            .saturating_sub(before.upload_textures.reused)
    );
    value["uploadTexturesDiscarded"] = json!(
        after
            .upload_textures
            .discarded
            .saturating_sub(before.upload_textures.discarded)
    );
    value["uploadTextureHighWaterBytes"] = json!(after.upload_textures.high_water_bytes);
    value["readbackBuffersCreated"] = json!(
        after
            .readback_buffers
            .created
            .saturating_sub(before.readback_buffers.created)
    );
    value["readbackBuffersReused"] = json!(
        after
            .readback_buffers
            .reused
            .saturating_sub(before.readback_buffers.reused)
    );
    value["readbackBuffersDiscarded"] = json!(
        after
            .readback_buffers
            .discarded
            .saturating_sub(before.readback_buffers.discarded)
    );
    value["readbackBufferHighWaterBytes"] = json!(after.readback_buffers.high_water_bytes);
    value["uniformBuffersCreated"] = json!(
        after
            .uniform_buffers
            .created
            .saturating_sub(before.uniform_buffers.created)
    );
    value["uniformBuffersReused"] = json!(
        after
            .uniform_buffers
            .reused
            .saturating_sub(before.uniform_buffers.reused)
    );
    value["uniformBuffersDiscarded"] = json!(
        after
            .uniform_buffers
            .discarded
            .saturating_sub(before.uniform_buffers.discarded)
    );
    value["uniformBufferHighWaterBytes"] = json!(after.uniform_buffers.high_water_bytes);
}

/// A zero elapsed time is treated as `f64::EPSILON`.
#[allow(clippy::cast_precision_loss)]
pub fn frames_per_second(frames: u64, elapsed_seconds: f64) -> f64 {
    frames as f64 / elapsed_seconds.max(f64::EPSILON)
}
