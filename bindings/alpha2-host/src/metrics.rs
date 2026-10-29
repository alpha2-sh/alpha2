//! Export metrics collection and their JSON projection.

use std::time::Duration;

use crate::decode::HardwareFallbackCounts;
use crate::frames::{estimated_staging_bytes, packed_yuv420_bytes};
use crate::{MAX_ENCODE_CHANNEL_DEPTH, MAX_IN_FLIGHT_VIDEO_FRAMES, MIN_ENCODE_CHANNEL_DEPTH};
use alpha2_render::RenderSize;
use serde_json::{Value, json};

/// How an export's frames reached the encoder, which every result and
/// metrics file names as `videoPipeline`.
///
/// Seven counters (`encodeQueueWaitMs`, `encodeWorkerIdleMs`,
/// `encodeChannelDepth`, `encoderFramesAllocated`, `inFlightDepth`,
/// `encodeBackpressureCount` and `maxPendingMediaEvents`) read zero on one
/// path because the structures they measure do not exist there, and the
/// readback stages read zero for the same reason. A reader comparing two
/// metrics files cannot otherwise tell "zero because idle" from "zero because
/// absent", so every file says which path wrote it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum VideoPipeline {
    /// Rendered into a staging buffer, mapped, packed, and encoded from CPU
    /// memory.
    #[default]
    Readback,
    /// Rendered straight into encoder-input images. Nothing crosses the CPU.
    #[cfg(feature = "hwencode")]
    GpuResident,
    /// Nothing rendered or encoded: the source's compressed packets copied.
    PacketCopy,
}

impl VideoPipeline {
    /// Stable identifier used by the export result and the metrics file.
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

/// Timings and counters one export accumulates, written to its result by
/// [`export_metrics_json`].
#[derive(Default)]
pub struct ExportMetrics {
    /// How this export's frames reached the encoder.
    pub video_pipeline: VideoPipeline,
    pub pipeline_enabled: bool,
    pub decode_lookahead: usize,
    pub decode: Duration,
    pub decode_wait: Duration,
    /// Time the export's own thread spent looking frames up in the render
    /// cache: what a frame the cache holds costs the export, or a miss before
    /// it renders.
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
    /// Per-frame handoff timestamps, when `ALPHA2_EXPORT_TRACE` asked for them.
    pub trace: Option<crate::trace::ExportTrace>,
    pub frame_submit: Duration,
    pub frame_submit_stages: alpha2_engine::ExportStepTimings,
    pub readback_map_latency: Duration,
    pub readback_wait: Duration,
    pub readback_pack: Duration,
    pub encode_queue: Duration,
    pub video_encode: Duration,
    /// Where the encode sink's own time went, split out of `video_encode`.
    pub encode_stages: alpha2_native::EncodeStageTimings,
    /// Encode-channel occupancy the worker observed, indexed by the depth
    /// left behind when it took a command.
    pub encode_channel_depth_samples: Vec<u64>,
    pub mux_finish: Duration,
    pub peak_readback_bytes: u64,
    pub peak_audio_window_bytes: u64,
    /// The most the open extension-provider sources held together between
    /// frames, against `memoryPolicy.providerSourceBytes`.
    pub peak_provider_source_bytes: u64,
    /// Provider sources the budget closed that a later frame opened again.
    pub provider_source_reopens: u64,
    pub peak_encode_queue_bytes: u64,
    pub max_pending_media: usize,
    pub max_in_flight_readbacks: usize,
    pub max_encode_queue: usize,
    pub encode_backpressure: u64,
    pub pending_depth_total: u64,
    pub pending_depth_samples: u64,
    pub peak_decode_queue_bytes: u64,
    pub max_decode_queue: usize,
    pub decode_backpressure: u64,
    pub ready_readback_dispatches: u64,
    pub decode_concurrency: usize,
    /// The decoder sessions the export kept open, and why that many.
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
    /// Time spent reading source stream parameters before the export started.
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
    /// Staging bytes of output frames mapped for the CPU: the raw video the
    /// readback path downloads. Structurally zero on the GPU-resident path.
    pub output_readback_bytes: u64,
    /// Bytes the CPU wrote into encoder-input frames, from a readback or from
    /// the baked frame cache.
    pub output_cpu_pack_bytes: u64,
    /// GPU copies that moved a packed output into an encoder-input image, one
    /// per plane, and the bytes they moved.
    pub encoder_input_copy_operations: u64,
    pub encoder_input_copy_bytes: u64,
    pub peak_native_surface_bytes: u64,
    pub peak_native_surfaces: u32,
    /// The most stills held on the GPU at once: those one frame shows.
    pub peak_resident_stills: usize,
    pub surface_budget_stalls: u64,
    /// Decoder surfaces still parked when the export finished. Nonzero is
    /// normal: the last submission's leases are released here rather than by
    /// a submission that never comes.
    pub retired_leases_at_exit: usize,
    /// Encoder frames the pool had to allocate. On the readback path these
    /// are the CPU frames; on the GPU-resident path, the distinct surfaces the
    /// hardware pool allocated, which counts what the handoff and the codec
    /// held beyond the host's own leases.
    pub encoder_frames_allocated: u64,
    /// Surfaces the GPU-resident path took from its pool.
    pub encoder_surface_acquires: u64,
    /// The most surfaces the host held at once, which its bound caps. Not
    /// what the pool allocated: that is `encoder_frames_allocated`.
    pub encoder_surface_lease_peak: u64,
}

impl ExportMetrics {
    /// Adds one installed frame set's source preparation, all but its
    /// duration, which each path times as it lays its spans out. Both
    /// paths install through this, so neither reports a counter the other
    /// leaves at zero.
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

    /// Raises the decoder-surface peaks to what is retained now.
    pub fn record_native_surfaces(&mut self, bytes: u64, surfaces: u32) {
        self.peak_native_surface_bytes = self.peak_native_surface_bytes.max(bytes);
        self.peak_native_surfaces = self.peak_native_surfaces.max(surfaces);
    }

    /// Takes the decode pool's byte peak and its waits, which only it saw.
    pub fn record_decode_pool(&mut self, worker: &crate::decode::DecodeWorker) {
        self.peak_decode_queue_bytes = self.peak_decode_queue_bytes.max(worker.peak_bytes());
        self.decode_budget_wait = worker.waits.budget();
        self.decode_handoff_wait = worker.waits.handoff();
    }
}

/// How an export's in-flight byte budget was divided, from
/// [`export_pipeline_limits`] or [`gpu_encode_pipeline_limits`].
#[derive(Debug)]
pub struct ExportPipelineLimits {
    pub byte_budget: u64,
    pub readback_depth: usize,
    pub encode_channel_depth: usize,
    pub staging_bytes_per_frame: u64,
    pub packed_bytes_per_frame: u64,
    /// What the GPU-resident path reserved; `None` on the readback path.
    #[cfg(feature = "hwencode")]
    pub gpu: Option<GpuEncodeBudget>,
}

/// Divides the in-flight byte budget between readbacks and the encode channel.
///
/// `requested_encode_depth` is what a caller pinned; `None` derives it. The
/// readback depth is settled first because it is capped at three frames by
/// [`MAX_IN_FLIGHT_VIDEO_FRAMES`], and then the encode channel takes what the
/// budget still holds, clamped to [`MIN_ENCODE_CHANNEL_DEPTH`] and
/// [`MAX_ENCODE_CHANNEL_DEPTH`]. Both reservations are then checked together,
/// and the encode depth is reduced until they fit, so a budget too small for
/// the derived depth degrades instead of failing.
///
/// # Errors
/// Returns an error when the budget cannot hold one readback and two packed
/// frames (a rendezvous channel's), which is a configuration error rather
/// than backpressure.
pub fn export_pipeline_limits(
    size: RenderSize,
    format: wgpu::TextureFormat,
    budget: crate::memory::InFlightBudget,
    requested_depth: usize,
    requested_encode_depth: Option<usize>,
) -> Result<ExportPipelineLimits, String> {
    let staging_bytes_per_frame = estimated_staging_bytes(size, format);
    let packed_bytes_per_frame = packed_yuv420_bytes(size);
    // A retry's scale never takes the budget below one readback and the packed frames a rendezvous
    // channel holds.
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

/// What the GPU-resident path reserves out of the in-flight byte budget.
///
/// Three things hold device memory for the export's whole length, and all
/// three are counted: the renderer's plane ring, which is allocated whatever
/// the depth; the encoder surfaces the host holds at once; and the surfaces
/// the handoff still holds after the host has let go of them. Memory the codec
/// keeps for its own reference pictures is the driver's and is not counted.
#[cfg(feature = "hwencode")]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GpuEncodeBudget {
    /// One encoder surface at the coded size.
    pub surface_bytes: u64,
    /// The renderer-owned plane ring.
    pub ring_bytes: u64,
    /// Surfaces the host may hold: the channel, plus one being handed a
    /// frame and one being encoded.
    pub lease_bound: usize,
    /// Surfaces the handoff and the encoder may still hold on top of those.
    pub retained_surfaces: usize,
}

#[cfg(feature = "hwencode")]
impl GpuEncodeBudget {
    /// The most surfaces the pool can be asked to allocate.
    #[must_use]
    pub const fn surface_ceiling(&self) -> usize {
        self.lease_bound + self.retained_surfaces
    }

    /// Everything this reserves, in bytes.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.ring_bytes.saturating_add(
            self.surface_bytes
                .saturating_mul(u64::try_from(self.surface_ceiling()).unwrap_or(u64::MAX)),
        )
    }
}

/// The GPU-resident path's encode channel depth when none is pinned.
///
/// Measured, not inherited from the readback path's derivation. On the RTX
/// 4090 (`h264_vulkan`, 1080p and 4K testsrc2, and the host-parity complex
/// composition at 4K 240 fps) depths 0, 1, 2, 4 and 8 ran within the run-to-run
/// noise of each other (the worker was busy 88-99% of every run and the
/// coordinator's only wait was the full channel), while each step reserved
/// another surface: 142 MiB at depth 2 against 214 MiB at 8 for 4K. Two keeps
/// a frame queued behind the one being encoded, so the encoder never waits on
/// the coordinator's next submission, for the least memory that does.
#[cfg(feature = "hwencode")]
pub const GPU_DEFAULT_ENCODE_CHANNEL_DEPTH: usize = 2;

/// Encoder surfaces the GPU-resident path may hold, from the same byte budget
/// the readback path divides between readbacks and packed frames.
///
/// There is no readback here and no packed CPU frame; the budget pays for the
/// plane ring and then for surfaces (see [`GpuEncodeBudget`]). Two surfaces are
/// never in the channel (the one being handed a frame and the one being
/// encoded), so the channel depth can be at most what is left after those two
/// and the handoff's retention, and is [`GPU_DEFAULT_ENCODE_CHANNEL_DEPTH`]
/// below that ceiling unless pinned. A pinned depth is clamped by the same
/// ceiling rather than trusted.
///
/// # Errors
/// A budget that cannot hold the ring and the minimum (a rendezvous channel's
/// two surfaces plus the handoff's retention) refuses before anything opens,
/// so an export falls back (or, when the GPU path is required, fails) instead
/// of silently exceeding its budget.
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
        .map_or(ceiling.min(GPU_DEFAULT_ENCODE_CHANNEL_DEPTH), |depth| {
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

/// Encoder surfaces the host holds at once for a given channel depth.
///
/// One is being handed a frame by the coordinator, one is being encoded by
/// the worker, and the rest are queued between them.
#[cfg(feature = "hwencode")]
#[must_use]
pub const fn gpu_encode_frame_bound(encode_channel_depth: usize) -> usize {
    encode_channel_depth.saturating_add(2)
}

/// Packed encoder frames the readback path holds at once for a given
/// channel depth.
///
/// As on the GPU-resident path ([`gpu_encode_frame_bound`]): one is being
/// packed by the coordinator, which takes it from the pool before its send
/// blocks, one is being encoded by the worker, which returns it only after
/// encoding, and the rest are queued between them. Counting only the first
/// would let a video-only export whose encoder is the bottleneck hold one packed
/// frame more than its budget reserved.
#[must_use]
pub const fn readback_encode_frame_bound(encode_channel_depth: usize) -> usize {
    encode_channel_depth.saturating_add(2)
}

/// How many packed frames the budget still holds once the readbacks are paid
/// for, minus the one the coordinator is packing into and the one the worker
/// is encoding ([`readback_encode_frame_bound`]).
///
/// Counting slots rather than bytes would pin this at two for every
/// output size: a 4K frame and a 720p frame cost the same *slot* and 18x
/// different *bytes*, and the budget is bytes. At 4K with the 256 MiB default
/// the count caps bound long before the bytes do, so the derived depth is the
/// ceiling; at a resolution where bytes bind first it is smaller.
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

/// Adds the counters that distinguish software decode from a resident
/// hardware surface, and the input transfer they each imply.
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
    // The decoder writes software planes once and that buffer is uploaded: one set of bytes at two
    // stages. No path reads pixels back, so the download count is zero.
    value["sourceCpuPlaneCopyBytes"] = json!(metrics.source_upload_bytes);
    value["sourceDownloadBytes"] = json!(0);
    value["sourceGpuCopyBytes"] = json!(metrics.source_gpu_copy_bytes);
    // The output side: what crossed to the CPU and what the CPU wrote into encoder frames, or on
    // the GPU-resident path the plane copies into the encoder's input image.
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
}

fn milliseconds(duration: Duration) -> Value {
    json!(duration.as_secs_f64() * 1_000.0)
}

/// Splits the three stage totals whose single number cannot be attributed.
///
/// `decodeMs` covers demux, the codec, and the conversion that only a software
/// session does. `sourceConvertUploadMs` covers a plane copy on one path and a
/// surface import on the other. `frameSubmitMs` covers evaluation, command
/// encoding, and a queue submission shared with the decoder. Each total stays
/// reported beside its parts so the split can be checked against it.
///
/// The frame-submit and source-upload parts sum to their total: both are one
/// thread's consecutive spans. The decode parts do not, because they are
/// accumulated inside shard threads that run concurrently while `decodeMs`
/// measures the pool thread's service time for the whole set.
pub fn add_stage_split_metrics(value: &mut Value, metrics: &ExportMetrics) {
    let decode = &metrics.decode_stages;
    value["decodeDemuxMs"] = milliseconds(decode.demux);
    value["decodeSendPacketMs"] = milliseconds(decode.send_packet);
    value["decodeReceiveFrameMs"] = milliseconds(decode.receive_frame);
    value["decodeConvertMs"] = milliseconds(decode.convert);
    value["decodeSeekMs"] = milliseconds(decode.seek);
    // Both are structurally zero while the host keeps up with the decoder.
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

    // Encoder-thread occupancy is `videoEncodeMs / (videoEncodeMs + this)`. Without it a saturated
    // encoder and a starved one report the same work.
    value["encodeWorkerIdleMs"] = milliseconds(metrics.encode_worker_idle);

    // Each covers three calls that move for different reasons; the parts sum to the total.
    let encode = &metrics.encode_stages;
    // The encode worker waiting for the render that packed a surface; not in `encoderSendFrameMs`.
    value["encoderInputReadyWaitMs"] = milliseconds(encode.video_input_ready_wait);
    value["encoderSendFrameMs"] = milliseconds(encode.video_send_frame);
    value["encoderReceivePacketMs"] = milliseconds(encode.video_receive_packet);
    value["muxWriteMs"] = milliseconds(encode.video_mux_write);
    value["audioEncoderSendFrameMs"] = milliseconds(encode.audio_send_frame);
    value["audioEncoderReceivePacketMs"] = milliseconds(encode.audio_receive_packet);
    value["audioMuxWriteMs"] = milliseconds(encode.audio_mux_write);
    // Mass at zero is a starved encoder; mass at the configured depth is a saturated one.
    // `encodeWorkerIdleMs` alone cannot tell them apart.
    value["encodeChannelDepthSamples"] = json!(metrics.encode_channel_depth_samples);
}

/// The renderer's work over the export: graph, source, and readback passes
/// and submissions, and how many source pictures were converted or repacked.
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
    // Only the readback's texture-to-buffer copy; source uploads and imports are reported
    // separately.
    value["readbackCopyOperations"] =
        json!(after.copy_operations.saturating_sub(before.copy_operations));
    value["readbackCopyBytes"] = json!(after.copy_bytes.saturating_sub(before.copy_bytes));
}

/// The export result's metrics object. Renderer counters are reported as the
/// difference between the `before` and `after` snapshots, so they cover this
/// export alone.
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

/// Adds the renderer's texture and buffer pool counters between `before` and
/// `after`, and the uniform buffers' high-water mark.
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

/// Throughput; an elapsed time of zero is treated as `f64::EPSILON` rather
/// than dividing by zero.
#[allow(clippy::cast_precision_loss)]
pub fn frames_per_second(frames: u64, elapsed_seconds: f64) -> f64 {
    frames as f64 / elapsed_seconds.max(f64::EPSILON)
}
