use crate::{BlendSpace, Color, RenderError, RenderSize, RenderedTexture};
use alpha2_gpu::GpuContext;
use alpha2_gpu::IntermediateFormat;
use alpha2_gpu::SubmitTimings;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Renders graphs of sources, effects, transitions and composites on one GPU
/// device, and owns the pipelines, pools and caches they use.
#[derive(Debug)]
pub struct Renderer {
    pub(crate) gpu: GpuContext,
    pub(crate) blend_space: BlendSpace,
    pub(crate) effects: crate::registry::EffectRegistry,
    pub(crate) effect_resources: crate::effect_resource::EffectResourceRegistry,
    pub(crate) textures: crate::Shared<crate::pool::TexturePool>,
    pub(crate) upload_textures: crate::Shared<crate::pool::UploadTexturePool>,
    /// Device memory budget in bytes, 0 when unknown.
    pub(crate) gpu_memory_budget: std::sync::atomic::AtomicU64,
    pub(crate) readback_buffers: crate::Shared<crate::pool::ReadbackBufferPool>,
    pub(crate) uniform_buffers: crate::Shared<crate::uniform::UniformBufferPool>,
    pub(crate) retirement: crate::Shared<crate::retire::Retirement>,
    pub(crate) pipelines: crate::pipeline::PipelineCache,
    pub(crate) diagnostics: crate::diagnostics::SharedDiagnostics,
    pub(crate) quality: Mutex<QualityState>,
    pub(crate) result_cache: Mutex<crate::result_cache::ResultCache>,
    pub(crate) vector_state: Mutex<crate::vector::VectorState>,
    pub(crate) timestamp_probe: std::sync::OnceLock<Option<crate::runtime_effect::TimestampProbe>>,
    /// Timestamp queries for sampled export frames, created on first use.
    pub(crate) frame_gpu_timer: std::sync::OnceLock<Option<crate::frame_gpu_time::FrameGpuTimer>>,
    pub(crate) pass_timing: std::sync::atomic::AtomicBool,
    /// Test-only: composite every layer full-frame as the culling reference.
    #[cfg(test)]
    pub(crate) full_frame_layers: std::sync::atomic::AtomicBool,
    /// Test-only: max layers per composite pass when nonzero (1 = batching reference).
    #[cfg(test)]
    pub(crate) composite_batch_limit: std::sync::atomic::AtomicUsize,
    /// Test-only: tile rows per vector atlas page when nonzero.
    #[cfg(test)]
    pub(crate) vector_page_rows: std::sync::atomic::AtomicU32,
    /// Test-only: composite pipeline log.
    #[cfg(test)]
    pub(crate) composite_pipelines: Mutex<crate::composite::CompositePipelineLog>,
    pub(crate) retired_leases: Arc<crate::yuv::RetiredLeases>,
    pub(crate) gpu_wait_timeout: Duration,
    /// Read once because every field read crosses into JavaScript on the browser backend.
    pub(crate) limits: wgpu::Limits,
    pub(crate) features: wgpu::Features,
    /// Bound to a generator's unused image inputs.
    pub(crate) empty_input: wgpu::TextureView,
    pub(crate) canaries: crate::canary::Canaries,
    pub(crate) submission_canaries: crate::canary::SubmissionCanaries,
    /// GPU errors the embedding saw outside the renderer's own scopes.
    pub(crate) gpu_errors: crate::GpuErrorReports,
    pub(crate) frame_lock: crate::Shared<FrameLock>,
    pub(crate) submission_clock: Arc<crate::submission_clock::SubmissionClock>,
    pub(crate) last_frame_submissions: Mutex<Option<crate::FrameSubmissions>>,
    pub(crate) calibration: std::sync::OnceLock<crate::registry::RegisteredEffect>,
}

/// A command encoder open until submitted.
///
/// Pooled resources released meanwhile are destroyed only after submission (see
/// [`crate::retire`]). It also holds the frame's error scopes, so it must stay on the creating
/// thread, and the [`FrameLock`]. A heavy frame may be split into several command buffers
/// ([`crate::submission_planner`]); finished ones wait here for the submission.
#[derive(Debug)]
pub(crate) struct RenderEncoder {
    encoder: wgpu::CommandEncoder,
    finished: Vec<wgpu::CommandBuffer>,
    planner: crate::submission_planner::SubmissionPlanner,
    device: wgpu::Device,
    label: &'static str,
    errors: crate::gpu_errors::FrameErrorScope,
    scope: crate::retire::EncodeScope,
    frame: FrameGuard,
}

/// Lets one thread at a time record and submit renderer work.
///
/// A graph returns intermediates to the pool once their last reader is encoded, while the
/// passes that wrote them are still unsubmitted. Holding this lock from encoder creation to
/// submission stops another thread from reusing such a texture and submitting first.
/// Re-entrant for the holding thread.
#[derive(Debug, Default)]
pub(crate) struct FrameLock {
    owner: Mutex<Option<(std::thread::ThreadId, usize)>>,
    released: std::sync::Condvar,
}

#[derive(Debug)]
pub(crate) struct FrameGuard {
    lock: crate::Shared<FrameLock>,
}

impl FrameLock {
    pub(crate) fn hold(self: &crate::Shared<Self>) -> FrameGuard {
        let me = std::thread::current().id();
        let mut owner = self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match owner.as_mut() {
                None => {
                    *owner = Some((me, 1));
                    break;
                }
                Some((thread, depth)) if *thread == me => {
                    *depth += 1;
                    break;
                }
                Some(_) => {
                    owner = self
                        .released
                        .wait(owner)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
        drop(owner);
        FrameGuard {
            lock: crate::Shared::clone(self),
        }
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        let mut owner = self
            .lock
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, depth)) = owner.as_mut() {
            *depth -= 1;
            if *depth == 0 {
                *owner = None;
                drop(owner);
                self.lock.released.notify_all();
            }
        }
    }
}

impl std::ops::Deref for RenderEncoder {
    type Target = wgpu::CommandEncoder;

    fn deref(&self) -> &Self::Target {
        &self.encoder
    }
}

impl std::ops::DerefMut for RenderEncoder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.encoder
    }
}

impl RenderEncoder {
    pub(crate) fn finish(self) -> RenderCommands {
        let mut buffers = self.finished;
        buffers.push(self.encoder.finish());
        RenderCommands {
            buffers,
            planner: self.planner,
            errors: self.errors,
            scope: self.scope,
            frame: self.frame,
        }
    }

    pub(crate) const fn planner(&mut self) -> &mut crate::submission_planner::SubmissionPlanner {
        &mut self.planner
    }

    /// Charges a pass to the plan and returns its band count, finishing the command buffer first
    /// when the plan says so.
    pub(crate) fn place_pass(
        &mut self,
        cost: crate::effect_cost::EffectCost,
        pixels: u64,
        bandable: bool,
    ) -> u32 {
        let placement = self.planner.pass(cost, pixels, bandable);
        if placement.cut_before {
            self.cut();
        }
        placement.bands
    }

    pub(crate) fn next_band(&mut self) {
        self.planner.next_band();
        self.cut();
    }

    fn cut(&mut self) {
        #[allow(clippy::disallowed_methods)]
        let next = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some(self.label),
            });
        let done = std::mem::replace(&mut self.encoder, next);
        self.finished.push(done.finish());
    }
}

#[derive(Debug)]
pub(crate) struct RenderCommands {
    buffers: Vec<wgpu::CommandBuffer>,
    planner: crate::submission_planner::SubmissionPlanner,
    errors: crate::gpu_errors::FrameErrorScope,
    scope: crate::retire::EncodeScope,
    frame: FrameGuard,
}

/// Classifies a loss by the submission times of its renderer.
#[derive(Clone, Debug)]
pub struct LossClassifier(Arc<crate::submission_clock::SubmissionClock>);

impl LossClassifier {
    /// Classifies `error` as [`Renderer::classify_loss`] does.
    #[must_use]
    pub fn classify(&self, error: RenderError) -> RenderError {
        match error {
            RenderError::GpuWorkDropped
            | RenderError::GpuDeviceLost
            | RenderError::GpuTimeout
            | RenderError::ExternalTextureSync(Some(alpha2_media::GpuFault::DeviceLost)) => self
                .0
                .hung()
                .map_or(error, |report| crate::canary::hung_error(&report)),
            error => error,
        }
    }

    /// The recent submission that kept the GPU busy longest.
    #[must_use]
    pub fn longest_submission(&self) -> Option<crate::SubmissionReport> {
        self.0.longest()
    }
}

struct SubmittedBuffers {
    index: wgpu::SubmissionIndex,
    timings: SubmitTimings,
    sequences: std::ops::Range<u64>,
}

pub(crate) struct MeasuredSubmit {
    pub(crate) index: wgpu::SubmissionIndex,
    pub(crate) sequences: std::ops::Range<u64>,
    pub(crate) timings: SubmitTimings,
    pub(crate) lease_retire: Duration,
}

/// Quality controls consumed by graph compilation and pass execution.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RenderQualityControls {
    /// Maximum samples for filter effects and layer minification. Analytic edge coverage stays
    /// on even with a one-sample budget.
    pub max_filter_samples: u32,
    /// Temporal samples used by motion-aware runners.
    pub motion_blur_subframes: u32,
}

impl RenderQualityControls {
    /// Upper bound on filter samples; every pass caps there, so a larger budget changes nothing.
    pub const MAX_FILTER_SAMPLES: u32 = 64;
}

impl Default for RenderQualityControls {
    fn default() -> Self {
        Self {
            max_filter_samples: Self::MAX_FILTER_SAMPLES,
            motion_blur_subframes: 1,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct QualityState {
    applied: RenderQualityControls,
    requested_filter_samples: u32,
}

impl QualityState {
    fn new(limit: u32) -> Self {
        let requested = RenderQualityControls::default();
        Self {
            applied: RenderQualityControls {
                max_filter_samples: requested.max_filter_samples.min(limit),
                ..requested
            },
            requested_filter_samples: requested.max_filter_samples,
        }
    }
}

impl Renderer {
    pub(crate) fn validate_size(&self, size: RenderSize) -> Result<(), RenderError> {
        RenderSize::new(size.width, size.height)?;
        let maximum = self.limits.max_texture_dimension_2d;
        if size.width > maximum || size.height > maximum {
            return Err(RenderError::DimensionTooLarge);
        }
        Ok(())
    }

    /// Creates a renderer that owns the supplied GPU context.
    #[must_use]
    pub fn new(gpu: GpuContext) -> Self {
        #[allow(clippy::disallowed_methods)]
        let limits = gpu.device().limits();
        let uniform_alignment = u64::from(limits.min_uniform_buffer_offset_alignment.max(1));
        let features = gpu.device().features();
        let filter_sample_limit = crate::filter_sample_limit(gpu.capabilities().tier);
        let retirement = crate::Shared::new(crate::retire::Retirement::default());
        let gpu_errors = crate::GpuErrorReports::default();
        let submission_clock = Arc::new(crate::submission_clock::SubmissionClock::default());
        let canaries = crate::canary::Canaries::new(
            gpu_errors.clone(),
            &retirement,
            Arc::clone(&submission_clock),
        );
        let empty_input =
            crate::pool::default_view(&gpu.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("alpha2 generator empty input"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }));
        Self {
            gpu,
            blend_space: BlendSpace::default(),
            effects: crate::registry::EffectRegistry::default(),
            effect_resources: crate::effect_resource::EffectResourceRegistry::default(),
            textures: crate::Shared::new(crate::pool::TexturePool::new(crate::Shared::clone(
                &retirement,
            ))),
            upload_textures: crate::Shared::new(crate::pool::UploadTexturePool::new(
                crate::Shared::clone(&retirement),
            )),
            gpu_memory_budget: std::sync::atomic::AtomicU64::new(0),
            readback_buffers: crate::Shared::new(crate::pool::ReadbackBufferPool::new(
                crate::Shared::clone(&retirement),
            )),
            uniform_buffers: crate::Shared::new(crate::uniform::UniformBufferPool::new(
                uniform_alignment,
                crate::Shared::clone(&retirement),
            )),
            retirement,
            pipelines: crate::pipeline::PipelineCache::default(),
            diagnostics: Arc::new(Mutex::new(crate::diagnostics::DiagnosticsState::default())),
            quality: Mutex::new(QualityState::new(filter_sample_limit)),
            result_cache: Mutex::new(crate::result_cache::ResultCache::default()),
            vector_state: Mutex::default(),
            timestamp_probe: std::sync::OnceLock::new(),
            frame_gpu_timer: std::sync::OnceLock::new(),
            pass_timing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            full_frame_layers: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            composite_batch_limit: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            vector_page_rows: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            composite_pipelines: Mutex::new(crate::composite::CompositePipelineLog::default()),
            retired_leases: Arc::new(crate::yuv::RetiredLeases::default()),
            gpu_wait_timeout: crate::error::DEFAULT_GPU_WAIT_TIMEOUT,
            limits,
            features,
            empty_input,
            canaries,
            submission_canaries: crate::canary::SubmissionCanaries::default(),
            gpu_errors,
            frame_lock: crate::Shared::default(),
            submission_clock,
            last_frame_submissions: Mutex::new(None),
            calibration: std::sync::OnceLock::new(),
        }
    }

    /// `None` on a device without timestamp queries, and in a browser.
    pub(crate) fn frame_gpu_timer(&self) -> Option<&crate::frame_gpu_time::FrameGpuTimer> {
        self.frame_gpu_timer
            .get_or_init(|| {
                (cfg!(not(target_arch = "wasm32"))
                    && self.features.contains(wgpu::Features::TIMESTAMP_QUERY))
                .then(|| {
                    crate::frame_gpu_time::FrameGpuTimer::new(self.gpu.device(), self.gpu.queue())
                })
            })
            .as_ref()
    }

    /// GPU time of the export frames sampled since the last call, then resets the totals.
    ///
    /// Every eighth frame of [`Self::render_graph_nv12_timed`] or
    /// [`Self::render_graph_yuv420p_timed`] is timed and read back later without waiting.
    ///
    /// # Errors
    /// The device has no timestamp queries.
    pub fn take_frame_gpu_times(&self) -> Result<crate::FrameGpuTimes, &'static str> {
        let timer = self
            .frame_gpu_timer()
            .ok_or("the device has no timestamp queries")?;
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        Ok(timer.take())
    }

    /// Where an embedding reports errors from its uncaptured-error handler.
    ///
    /// Needed only in a browser, where error scopes resolve after the frame returns. After a
    /// report the renderer drops its cached results and pooled resources, and treats a missing
    /// readback canary as that error rather than as dropped work.
    #[must_use]
    pub fn gpu_error_reports(&self) -> crate::GpuErrorReports {
        self.gpu_errors.clone()
    }

    /// This renderer, using `reports` for GPU errors the embedding saw before it existed.
    #[must_use]
    pub fn with_gpu_error_reports(mut self, reports: crate::GpuErrorReports) -> Self {
        self.canaries.clear();
        self.canaries = crate::canary::Canaries::new(
            reports.clone(),
            &self.retirement,
            Arc::clone(&self.submission_clock),
        );
        self.gpu_errors = reports;
        self
    }

    /// Drops pooled resources after the GPU reported an error that may involve them.
    fn forget_untrusted_resources(&self) {
        self.retirement.doubt();
        self.textures.clear();
        self.upload_textures.clear();
        self.readback_buffers.clear();
        self.uniform_buffers.clear();
        self.canaries.clear();
    }

    /// Tier, intermediate format, adapter and limits of the GPU.
    #[must_use]
    pub fn gpu_capabilities(&self) -> &alpha2_gpu::Capabilities {
        self.gpu.capabilities()
    }

    /// The device's limits.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// Sets how long a native GPU wait may take before [`RenderError::GpuTimeout`].
    pub fn set_gpu_wait_timeout(&mut self, timeout: Duration) {
        self.gpu_wait_timeout = timeout;
    }

    /// Why the device can no longer run work, once known.
    ///
    /// Either a readback canary showed dropped work ([`RenderError::GpuWorkDropped`]) or resources
    /// could no longer be created ([`RenderError::GpuDeviceLost`]). Later readbacks and NV12 packs
    /// fail with this error; a host should report it and restart the process.
    #[must_use]
    pub fn gpu_lost(&self) -> Option<RenderError> {
        self.canaries.lost()
    }

    /// Marks the frame later submissions render, for [`RenderError::GpuDeviceHung`] reports.
    pub fn mark_frame(&self, frame: Option<crate::FrameMark>) {
        self.submission_clock.mark_frame(frame);
    }

    /// The recent submission, possibly still running, that kept the GPU busy longest.
    #[must_use]
    pub fn longest_submission(&self) -> Option<crate::SubmissionReport> {
        self.submission_clock.longest()
    }

    /// Classifies `error` using this renderer's submission times.
    ///
    /// A device loss or timeout becomes [`RenderError::GpuDeviceHung`] when a recent submission
    /// ran for at least half the GPU watchdog ([`alpha2_gpu::hung_threshold_ms`]). Other errors
    /// pass through.
    #[must_use]
    pub fn classify_loss(&self, error: RenderError) -> RenderError {
        self.loss_classifier().classify(error)
    }

    /// [`Renderer::classify_loss`] as an owned handle, for readbacks finished on an event loop.
    #[must_use]
    pub fn loss_classifier(&self) -> LossClassifier {
        LossClassifier(Arc::clone(&self.submission_clock))
    }

    pub(crate) fn ensure_gpu_runs_work(&self) -> Result<(), RenderError> {
        self.canaries.lost().map_or(Ok(()), Err)
    }

    /// Checks canaries of earlier NV12 packs and repacks that have mapped, without waiting.
    pub(crate) fn check_submitted_work(&self) -> Result<(), RenderError> {
        // Map callbacks run only on poll; a hardware-encoder frame loop may not poll otherwise.
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        self.submission_canaries.check_mapped()?;
        self.ensure_gpu_runs_work()
    }

    /// Waits for every submitted NV12 pack and repack and checks that the GPU ran it.
    ///
    /// These submissions read nothing back, so a canary is the only proof the planes were written.
    /// Exports call this after the last frame. Safe while another thread polls the device (the
    /// macOS `VideoToolbox` worker does).
    ///
    /// # Errors
    /// [`RenderError::GpuWorkDropped`] when a canary is missing, or [`RenderError::GpuTimeout`]
    /// when work or a mapping report does not arrive in time.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn confirm_gpu_work(&self) -> Result<(), RenderError> {
        let deadline = web_time::Instant::now() + self.gpu_wait_timeout;
        self.gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(self.gpu_wait_timeout),
            })
            .map_err(crate::error::poll_error)?;
        let settled = self.settle(&self.submission_canaries.reports, deadline, || {
            self.check_submitted_work()?;
            Ok(self.submission_canaries.outstanding() == 0)
        })?;
        if settled {
            Ok(())
        } else {
            Err(RenderError::GpuTimeout)
        }
    }

    /// How long a native wait on GPU work may take.
    #[must_use]
    pub const fn gpu_wait_timeout(&self) -> Duration {
        self.gpu_wait_timeout
    }

    /// Releases every native lease whose GPU work has finished, returning how many.
    ///
    /// A decoder surface returns only after the submission sampling it completes, so a caller out
    /// of surface budget can call this and retry. Unsubmitted work is not waited on.
    ///
    /// # Errors
    /// A readback error if the device cannot be polled.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reclaim_external_leases(&self) -> Result<usize, RenderError> {
        let deadline = web_time::Instant::now() + self.gpu_wait_timeout;
        self.gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(self.gpu_wait_timeout),
            })
            .map_err(crate::error::poll_error)?;
        self.settle(&self.retired_leases.parks, deadline, || {
            Ok(self.retired_leases.awaited() == 0)
        })?;
        Ok(self.retired_leases.drain())
    }

    /// Submits command buffers, then releases completed native leases.
    ///
    /// Leases must not be released inside `wgpu`'s submit: a shared-queue backend holds the decoder
    /// queue lock there, and freeing a frame takes `FFmpeg`'s frame lock, which `FFmpeg` decode
    /// threads take in the opposite order.
    ///
    /// # Errors
    /// [`RenderError::Gpu`] when a scope caught an error; then nothing in the submission ran.
    pub(crate) fn submit<I>(&self, commands: I) -> Result<wgpu::SubmissionIndex, RenderError>
    where
        I: IntoIterator<Item = RenderCommands>,
    {
        self.submit_measured(commands, false)
            .map(|submitted| submitted.index)
    }

    /// Opens a command encoder and the error scopes its submission closes.
    ///
    /// Open it before acquiring the frame's targets so a failed allocation is the frame's error.
    pub(crate) fn command_encoder(&self, label: &'static str) -> RenderEncoder {
        if self.gpu_errors.take_unhandled() {
            // The browser reported an error for earlier work; cached results may be bad.
            self.clear_result_cache_for(crate::CacheInvalidation::GpuError);
            self.forget_untrusted_resources();
        }
        // Wait for other frames before opening any scope of this one.
        let frame = self.frame_lock.hold();
        // Open first so nothing the encoder uses can be destroyed under it.
        let scope = self.retirement.open();
        let errors = crate::gpu_errors::FrameErrorScope::open(
            self.gpu.device(),
            crate::Shared::clone(&self.retirement),
        );
        #[allow(clippy::disallowed_methods)] // The one place a renderer encoder is created.
        let encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) });
        RenderEncoder {
            encoder,
            finished: Vec::new(),
            planner: crate::submission_planner::SubmissionPlanner::default(),
            device: self.gpu.device().clone(),
            label,
            errors,
            scope,
            frame,
        }
    }

    /// Defers destruction of released pool resources until the returned scope is dropped.
    pub(crate) fn open_encode(&self) -> crate::retire::EncodeScope {
        self.retirement.open()
    }

    /// Submits and reports the queue wait, submit and lease drain times.
    ///
    /// With `measure` false every span is zero; the submission clock still records it.
    ///
    /// # Errors
    /// [`RenderError::Gpu`] when a scope caught an error; pooled resources are no longer trusted.
    pub(crate) fn submit_measured<I>(
        &self,
        commands: I,
        measure: bool,
    ) -> Result<MeasuredSubmit, RenderError>
    where
        I: IntoIterator<Item = RenderCommands>,
    {
        let mut command_buffers = Vec::new();
        let mut errors = Vec::new();
        let mut scopes = Vec::new();
        let mut frames = Vec::new();
        let mut planners = Vec::new();
        for commands in commands {
            command_buffers.extend(commands.buffers.into_iter().map(|buffer| (buffer, None)));
            if let Some(buffer_ms) = commands.planner.buffer_ms() {
                let first = command_buffers.len() - buffer_ms.len();
                for (slot, ms) in command_buffers[first..].iter_mut().zip(buffer_ms) {
                    slot.1 = Some(ms);
                }
            }
            planners.push(commands.planner);
            errors.push(commands.errors);
            scopes.push(commands.scope);
            frames.push(commands.frame);
        }
        let submitted = self.submit_buffers(command_buffers, measure);
        self.note_frame_submissions(&planners, &submitted);
        let SubmittedBuffers {
            index,
            timings,
            sequences,
        } = submitted;
        // Scopes close after the submit, in reverse order.
        let mut caught = None;
        for scope in errors.into_iter().rev() {
            caught = match (caught, scope.close()) {
                (Some(alpha2_gpu::GpuErrorKind::OutOfMemory), _) | (_, None) => caught,
                (_, kind) => kind,
            };
        }
        drop(scopes);
        drop(frames);
        let drain_started = measure.then(web_time::Instant::now);
        self.retired_leases.drain();
        if let Some(kind) = caught {
            self.forget_untrusted_resources();
            return Err(RenderError::Gpu { kind });
        }
        Ok(MeasuredSubmit {
            index,
            sequences,
            timings,
            lease_retire: drain_started.map_or(Duration::ZERO, |started| started.elapsed()),
        })
    }

    /// Submits `buffers` in one submission, or one per buffer when the frame was split so the GPU
    /// can be preempted between them. Every submission is timed for loss classification.
    fn submit_buffers(
        &self,
        buffers: Vec<(wgpu::CommandBuffer, Option<f64>)>,
        measure: bool,
    ) -> SubmittedBuffers {
        let submit = |buffers: Vec<wgpu::CommandBuffer>, predicted_ms: Option<f64>| {
            let (index, timings) = if measure {
                self.gpu.submit_timed(buffers)
            } else {
                (self.gpu.submit(buffers), SubmitTimings::default())
            };
            let sequence = self
                .submission_clock
                .record(web_time::Instant::now(), predicted_ms);
            let clock = Arc::clone(&self.submission_clock);
            self.gpu.on_submitted_work_done(move || {
                clock.complete(sequence, web_time::Instant::now());
            });
            (index, timings, sequence)
        };
        if buffers.len() <= 1 || buffers.iter().all(|(_, ms)| ms.is_none()) {
            let predicted_ms = buffers
                .iter()
                .filter_map(|(_, ms)| *ms)
                .reduce(|a, b| a + b);
            let (index, timings, sequence) = submit(
                buffers.into_iter().map(|(buffer, _)| buffer).collect(),
                predicted_ms,
            );
            return SubmittedBuffers {
                index,
                timings,
                sequences: sequence..sequence + 1,
            };
        }
        let mut total = SubmitTimings::default();
        let mut last = None;
        let mut first_sequence = None;
        for (buffer, predicted_ms) in buffers {
            let (index, timings, sequence) = submit(vec![buffer], predicted_ms);
            total.queue_lock_wait = total
                .queue_lock_wait
                .saturating_add(timings.queue_lock_wait);
            total.queue_submit = total.queue_submit.saturating_add(timings.queue_submit);
            first_sequence.get_or_insert(sequence);
            last = Some((index, sequence));
        }
        let (index, last_sequence) = last.expect("at least two buffers");
        SubmittedBuffers {
            index,
            timings: total,
            sequences: first_sequence.unwrap_or(last_sequence)..last_sequence + 1,
        }
    }

    /// Records frame submissions and folds measured GPU time into the device rate on completion.
    fn note_frame_submissions(
        &self,
        planners: &[crate::submission_planner::SubmissionPlanner],
        submitted: &SubmittedBuffers,
    ) {
        let Some(planner) = planners.iter().find(|planner| planner.is_frame()) else {
            return;
        };
        let submissions =
            u32::try_from(submitted.sequences.end - submitted.sequences.start).unwrap_or(u32::MAX);
        let buffer_ms = planner.buffer_ms();
        let report = crate::FrameSubmissions {
            submissions,
            predicted_ms: buffer_ms.as_ref().map(|buffers| buffers.iter().sum()),
            cost: planner.cost(),
        };
        crate::diagnostics::record_frame_plan(
            &self.diagnostics,
            &report,
            buffer_ms
                .iter()
                .flatten()
                .copied()
                .fold(None, |most: Option<f64>, ms| {
                    Some(most.map_or(ms, |most| most.max(ms)))
                }),
        );
        *self
            .last_frame_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
        if planner.is_planned() {
            let clock = Arc::clone(&self.submission_clock);
            let rate = Arc::clone(self.gpu.rate());
            let sequences = submitted.sequences.clone();
            let unit_pixels = report.cost.unit_pixels();
            self.gpu.on_submitted_work_done(move || {
                if let Some(measured_ms) = clock.gpu_ms(sequences) {
                    rate.observe(measured_ms, unit_pixels);
                }
            });
        }
    }

    /// How the last frame was submitted; each frame's is returned once.
    pub fn take_frame_submissions(&self) -> Option<crate::FrameSubmissions> {
        self.last_frame_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Submission split counts since the last call, then resets them.
    pub fn take_gpu_submission_stats(&self) -> crate::GpuSubmissionStats {
        crate::diagnostics::take_gpu_submission_stats(&self.diagnostics)
    }

    /// Returns whether per-pass timing is being collected.
    #[must_use]
    pub fn pass_timing_enabled(&self) -> bool {
        self.pass_timing.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enables or disables per-pass timing collection. Disabled by default.
    ///
    /// While off, no clock is read and no timestamp resources are created. Turning it off drops
    /// the retained timing history. Other diagnostics are always available.
    pub fn set_pass_timing_enabled(&self, enabled: bool) {
        self.pass_timing
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        if !enabled {
            crate::diagnostics::end_timing_run(&self.diagnostics);
        }
    }

    /// The timing run a pass encoded now records into, or `None` while timing is off. Read before
    /// the flag: a run that ends after this read drops the pass's record.
    pub(crate) fn pass_timing_epoch(&self) -> Option<u64> {
        let epoch = crate::diagnostics::timing_epoch(&self.diagnostics);
        self.pass_timing_enabled().then_some(epoch)
    }

    /// Capability-negotiated storage precision. Graph images hold premultiplied sRGB; layer blends
    /// may use linear light.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn intermediate_format(&self) -> wgpu::TextureFormat {
        match self.gpu.capabilities().intermediate_format {
            IntermediateFormat::Rgba16Float => wgpu::TextureFormat::Rgba16Float,
            _ => wgpu::TextureFormat::Rgba8Unorm,
        }
    }

    /// Returns allocation and reuse counters for intermediate render targets.
    #[must_use]
    pub fn texture_pool_stats(&self) -> crate::TexturePoolStats {
        self.textures.stats()
    }

    /// Acquires a pooled RGBA8 source-upload texture.
    ///
    /// Submissions using it must be issued before the handle is dropped so later uploads stay
    /// queue-ordered.
    ///
    /// # Errors
    /// The dimensions exceed device limits.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn acquire_rgba8_upload_texture(
        &self,
        size: RenderSize,
    ) -> Result<crate::UploadTexture, RenderError> {
        self.validate_size(size)?;
        Ok(self.upload_textures.acquire(
            self.gpu.device(),
            size,
            wgpu::TextureFormat::Rgba8Unorm,
            "alpha2 RGBA upload texture",
        ))
    }

    /// Destroys all currently idle intermediate textures.
    pub fn clear_texture_pool(&self) {
        self.textures.clear();
    }

    /// Destroys GPU memory held only for reuse (idle pools and kept graph results) and returns the
    /// bytes freed.
    ///
    /// Resources are destroyed rather than dropped so browser `WebGPU` frees them immediately.
    /// Nothing still in use is released, and submitted work completes first, so this is safe
    /// between calls.
    pub fn release_idle_memory(&self) -> u64 {
        let before = self.retirement.retired_bytes();
        self.clear_result_cache();
        self.textures.clear();
        self.upload_textures.clear();
        self.readback_buffers.clear();
        self.uniform_buffers.clear();
        self.canaries.clear();
        self.vector_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cache
            .clear();
        self.retirement.retired_bytes().saturating_sub(before)
    }

    /// Sets the soft byte budget for idle intermediate textures, evicting down to it now.
    pub fn set_texture_budget_bytes(&self, budget_bytes: u64) {
        self.textures
            .set_budget(budget_bytes.min(self.pool_budget_cap()));
    }

    /// Records the adapter's memory budget (`None` if unreported) and caps idle pools at a quarter
    /// of it. Returns whether a pool budget was lowered.
    pub fn set_gpu_memory_budget(&self, budget_bytes: Option<u64>) -> bool {
        self.gpu_memory_budget.store(
            budget_bytes.unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
        let cap = self.pool_budget_cap();
        let textures = self.textures.stats().budget_bytes;
        let uploads = self.upload_textures.stats().budget_bytes;
        if textures > cap {
            self.textures.set_budget(cap);
        }
        if uploads > cap {
            self.upload_textures.set_budget(cap);
        }
        textures > cap || uploads > cap
    }

    /// The adapter's device memory budget, when reported.
    #[must_use]
    pub fn gpu_memory_budget(&self) -> Option<u64> {
        match self
            .gpu_memory_budget
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            bytes => Some(bytes),
        }
    }

    /// A quarter of the device memory budget, or unlimited when unknown.
    fn pool_budget_cap(&self) -> u64 {
        self.gpu_memory_budget().map_or(u64::MAX, |bytes| bytes / 4)
    }

    /// Applies filter and temporal sampling controls.
    ///
    /// The filter budget is capped at [`Self::filter_sample_limit`]; a reduction is reported as
    /// [`crate::CapabilityDiagnostic::FilterSamplesReduced`]. Kept results are dropped only when
    /// the applied values change.
    pub fn set_quality_controls(&self, max_filter_samples: u32, motion_blur_subframes: u32) {
        let requested = max_filter_samples.max(1);
        let next = RenderQualityControls {
            max_filter_samples: requested.min(self.filter_sample_limit()),
            motion_blur_subframes: motion_blur_subframes.max(1),
        };
        let mut quality = self
            .quality
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        quality.requested_filter_samples = requested;
        if quality.applied != next {
            quality.applied = next;
            self.clear_result_cache_for(crate::CacheInvalidation::Quality);
        }
    }

    /// [`RenderQualityControls::MAX_FILTER_SAMPLES`], or 16 on a Tier-B device.
    #[must_use]
    pub fn filter_sample_limit(&self) -> u32 {
        crate::filter_sample_limit(self.gpu.capabilities().tier)
    }

    /// Lets kept graph results use up to `budget_bytes` (see
    /// [`crate::RenderGraph::set_cache_intent`]); zero, the default, disables the cache. The
    /// texture pool budget does not count these results.
    pub fn set_result_cache_budget(&self, budget_bytes: u64) {
        self.lock_result_cache().set_budget(budget_bytes);
    }

    /// Drops every kept graph result.
    pub fn clear_result_cache(&self) {
        self.clear_result_cache_for(crate::CacheInvalidation::Explicit);
    }

    /// The result cache's counters.
    #[must_use]
    pub fn result_cache_stats(&self) -> crate::ResultCacheStats {
        self.lock_result_cache().stats()
    }

    /// Changes whenever a graph with an unchanged [`Self::graph_cache_key`] may render differently
    /// (any result-cache clear or reported GPU error). Equal keys under an equal epoch mean equal
    /// pixels.
    #[must_use]
    pub fn content_epoch(&self) -> u64 {
        self.lock_result_cache()
            .clears()
            .wrapping_add(self.gpu_errors.count())
    }

    pub(crate) fn clear_result_cache_for(&self, reason: crate::CacheInvalidation) {
        self.lock_result_cache().clear(reason);
    }

    pub(crate) fn lock_result_cache(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::result_cache::ResultCache> {
        self.result_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns the active render quality controls.
    #[must_use]
    pub fn quality_controls(&self) -> RenderQualityControls {
        self.quality_state().applied
    }

    fn quality_state(&self) -> QualityState {
        *self
            .quality
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn degradations(&self) -> Vec<crate::CapabilityDiagnostic> {
        let quality = self.quality_state();
        let (requested, applied) = (
            quality.requested_filter_samples,
            quality.applied.max_filter_samples,
        );
        if requested > applied {
            vec![crate::CapabilityDiagnostic::FilterSamplesReduced { requested, applied }]
        } else {
            Vec::new()
        }
    }

    /// Returns a stable snapshot for telemetry or benchmark output.
    #[must_use]
    pub fn diagnostics(&self) -> crate::RenderDiagnostics {
        let quality = self.quality_controls();
        crate::diagnostics::snapshot(
            &self.diagnostics,
            self.gpu.capabilities(),
            &crate::diagnostics::ResourceDiagnostics {
                textures: self.texture_pool_stats(),
                upload_textures: self.upload_textures.stats(),
                readback_buffers: self.readback_buffers.stats(),
                uniform_buffers: self.uniform_buffers.stats(),
                pipelines: self.pipeline_cache_stats(),
                result_cache: self.result_cache_stats(),
            },
            quality.max_filter_samples,
            quality.motion_blur_subframes,
            self.degradations(),
        )
    }

    /// Pipeline creations since the last call, oldest first (at most 256), with start and end
    /// times.
    pub fn take_pipeline_creations(&self) -> Vec<crate::PipelineCreation> {
        crate::diagnostics::take_pipeline_creations(&self.diagnostics)
    }

    /// Records a host presentation outcome without imposing a product clock.
    pub fn record_frame_outcome(&self, presented: bool, repeated: bool) {
        crate::diagnostics::record_frame(&self.diagnostics, presented, repeated);
    }

    /// Adds time spent waiting for a host decoder or refill operation.
    pub fn record_decoder_wait(&self, duration: Duration) {
        crate::diagnostics::record_decoder_wait(&self.diagnostics, duration);
    }

    /// Returns the number of cached runtime and built-in pipelines.
    #[must_use]
    pub fn pipeline_cache_stats(&self) -> crate::PipelineCacheStats {
        crate::PipelineCacheStats {
            runtime_effects: self.effects.len(),
            builtins: self.pipelines.builtin_count(),
        }
    }

    /// Selects the single color space used by all layer blending.
    #[must_use]
    pub const fn with_blend_space(mut self, blend_space: BlendSpace) -> Self {
        self.blend_space = blend_space;
        self
    }

    /// Returns the configured layer-compositing color space.
    #[must_use]
    pub const fn blend_space(&self) -> BlendSpace {
        self.blend_space
    }

    /// Clears an offscreen RGBA texture and submits it to the GPU queue.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError`] for an invalid size or color.
    pub fn render_solid(
        &self,
        size: RenderSize,
        color: Color,
    ) -> Result<RenderedTexture, RenderError> {
        self.validate_size(size)?;
        if !color.is_valid() {
            return Err(RenderError::InvalidColor);
        }
        let format = self.intermediate_format();
        let mut encoder = self.command_encoder("alpha2 solid encoder");
        let texture = self.textures.acquire(
            self.gpu.device(),
            size,
            format,
            "alpha2 solid render target",
        );
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("alpha2 solid clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: texture.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: color.red,
                            g: color.green,
                            b: color.blue,
                            a: color.alpha,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.submit([encoder.finish()])?;
        Ok(RenderedTexture::pooled(size, format, texture))
    }
}
