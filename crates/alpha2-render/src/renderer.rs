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
    pub(crate) readback_buffers: crate::Shared<crate::pool::ReadbackBufferPool>,
    pub(crate) uniform_buffers: crate::Shared<crate::uniform::UniformBufferPool>,
    /// Destroys what the pools let go of, once no encode uses it.
    pub(crate) retirement: crate::Shared<crate::retire::Retirement>,
    pub(crate) pipelines: crate::pipeline::PipelineCache,
    pub(crate) diagnostics: crate::diagnostics::SharedDiagnostics,
    pub(crate) quality: Mutex<QualityState>,
    pub(crate) result_cache: Mutex<crate::result_cache::ResultCache>,
    pub(crate) timestamp_probe: std::sync::OnceLock<Option<crate::runtime_effect::TimestampProbe>>,
    pub(crate) pass_timing: std::sync::atomic::AtomicBool,
    /// Test-only: composite every layer as a full-frame pass, culling
    /// nothing, as the reference the bounded passes must match.
    #[cfg(test)]
    pub(crate) full_frame_layers: std::sync::atomic::AtomicBool,
    /// Test-only: the most layers a composite pass takes when nonzero, so
    /// one layer per pass is the reference batched passes must match. A
    /// nonzero limit composites as a GPU does on any device, small layers
    /// with the loop pipeline.
    #[cfg(test)]
    pub(crate) composite_batch_limit: std::sync::atomic::AtomicUsize,
    /// Test-only: the pipeline each composite pass drew with, and how many
    /// composite pipelines were created.
    #[cfg(test)]
    pub(crate) composite_pipelines: Mutex<crate::composite::CompositePipelineLog>,
    pub(crate) retired_leases: Arc<crate::yuv::RetiredLeases>,
    pub(crate) gpu_wait_timeout: Duration,
    /// The device's limits, read once: they never change for a device, and
    /// on the browser backend every read crosses into JavaScript per field.
    pub(crate) limits: wgpu::Limits,
    /// The device's enabled features, read once for the same reason.
    pub(crate) features: wgpu::Features,
    /// The texture bound to a generator's unused image inputs.
    pub(crate) empty_input: wgpu::TextureView,
    /// What each YUV readback's staging buffer must end in, and whether one
    /// did not.
    pub(crate) canaries: crate::canary::Canaries,
    /// The canaries of submitted NV12 packs and repacks not checked yet.
    pub(crate) submission_canaries: crate::canary::SubmissionCanaries,
    /// GPU errors the embedding saw outside the renderer's own scopes.
    pub(crate) gpu_errors: crate::GpuErrorReports,
    /// Held by each renderer encoder until it is submitted.
    pub(crate) frame_lock: crate::Shared<FrameLock>,
}

/// A command encoder of this renderer's, open until its commands are
/// submitted. Pooled resources it uses that are let go of meanwhile are
/// destroyed only after that (see [`crate::retire`]), so every encoder the
/// renderer records into is one of these.
///
/// It also holds the validation and out-of-memory scopes that its
/// submission closes (see [`crate::gpu_errors`]), so it stays on the thread
/// that created it.
///
/// And it holds the renderer's frame lock (see [`FrameLock`]) from creation
/// until that submission.
#[derive(Debug)]
pub(crate) struct RenderEncoder {
    encoder: wgpu::CommandEncoder,
    errors: crate::gpu_errors::FrameErrorScope,
    scope: crate::retire::EncodeScope,
    frame: FrameGuard,
}

/// Lets one thread at a time record and submit renderer work.
///
/// A graph hands each intermediate back to the texture pool as soon as its
/// last reader is encoded, so a later pass of the same frame can reuse it.
/// The passes that wrote it are still in the unsubmitted encoder. A renderer
/// is `Sync` natively, and another thread that took that texture, drew into
/// it and submitted first would have its kept picture overwritten when the
/// first frame's submission ran. Holding this lock from an encoder's
/// creation until its submission keeps every other thread's submission
/// after it, so the reuse stays within the frame that released the texture.
///
/// It is re-entrant: the thread that holds it may open another renderer
/// encoder. Natively renders on one renderer from several threads queue
/// here; they share one queue, so their GPU work was ordered anyway.
#[derive(Debug, Default)]
pub(crate) struct FrameLock {
    owner: Mutex<Option<(std::thread::ThreadId, usize)>>,
    released: std::sync::Condvar,
}

/// One hold of a [`FrameLock`], released when dropped.
#[derive(Debug)]
pub(crate) struct FrameGuard {
    lock: crate::Shared<FrameLock>,
}

impl FrameLock {
    /// Waits until no other thread holds the lock, then holds it.
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
    /// Finishes recording; the encode stays open until [`Renderer::submit`].
    pub(crate) fn finish(self) -> RenderCommands {
        RenderCommands {
            buffer: self.encoder.finish(),
            errors: self.errors,
            scope: self.scope,
            frame: self.frame,
        }
    }
}

/// A finished [`RenderEncoder`] waiting to be submitted.
#[derive(Debug)]
pub(crate) struct RenderCommands {
    buffer: wgpu::CommandBuffer,
    errors: crate::gpu_errors::FrameErrorScope,
    scope: crate::retire::EncodeScope,
    frame: FrameGuard,
}

/// One queue submission and the spans a caller asked to be measured.
pub(crate) struct MeasuredSubmit {
    pub(crate) index: wgpu::SubmissionIndex,
    pub(crate) timings: SubmitTimings,
    pub(crate) lease_retire: Duration,
}

/// Quality controls consumed by graph compilation and pass execution.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RenderQualityControls {
    /// Maximum samples available to filter effects and layer minification.
    /// Analytic edge coverage remains enabled even with a one-sample budget.
    pub max_filter_samples: u32,
    /// Deterministic temporal samples used by motion-aware runners.
    pub motion_blur_subframes: u32,
}

impl RenderQualityControls {
    /// The most filter samples any pass takes: the runtime passes, layer
    /// minification and the blur shaders all stop there, so a larger budget
    /// renders the same pixels. [`Renderer::set_quality_controls`] applies at
    /// most this, or less where the device's tier takes less
    /// ([`Renderer::filter_sample_limit`]).
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

/// The quality controls a renderer applies, and the filter sample budget it
/// was asked for, which the limit may have reduced.
#[derive(Copy, Clone, Debug)]
pub(crate) struct QualityState {
    applied: RenderQualityControls,
    requested_filter_samples: u32,
}

impl QualityState {
    /// The default controls, under `limit`.
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
        // The one read of the device's limits; everything else uses the copy.
        #[allow(clippy::disallowed_methods)]
        let limits = gpu.device().limits();
        let uniform_alignment = u64::from(limits.min_uniform_buffer_offset_alignment.max(1));
        let features = gpu.device().features();
        let filter_sample_limit = crate::filter_sample_limit(gpu.capabilities().tier);
        let retirement = crate::Shared::new(crate::retire::Retirement::default());
        let gpu_errors = crate::GpuErrorReports::default();
        let canaries = crate::canary::Canaries::new(gpu_errors.clone(), &retirement);
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
            timestamp_probe: std::sync::OnceLock::new(),
            pass_timing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            full_frame_layers: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            composite_batch_limit: std::sync::atomic::AtomicUsize::new(0),
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
        }
    }

    /// Where an embedding reports the GPU errors its device's
    /// uncaptured-error handler receives, on a backend whose error scopes
    /// resolve only after a frame has returned: the browser. The renderer
    /// then drops the results it kept and the resources it pooled before
    /// its next frame, since the error may have come from any of them, and
    /// reads a readback canary missing after such a report as that error
    /// rather than as a device that drops work.
    ///
    /// A native embedding does not need it: there every frame's own scopes
    /// resolve before the frame returns, and fail it
    /// ([`RenderError::Gpu`]).
    #[must_use]
    pub fn gpu_error_reports(&self) -> crate::GpuErrorReports {
        self.gpu_errors.clone()
    }

    /// This renderer, hearing of GPU errors through `reports` (see
    /// [`Renderer::gpu_error_reports`]): for an embedding that installs its
    /// device's error handler before the renderer exists.
    #[must_use]
    pub fn with_gpu_error_reports(mut self, reports: crate::GpuErrorReports) -> Self {
        self.canaries.clear();
        self.canaries = crate::canary::Canaries::new(reports.clone(), &self.retirement);
        self.gpu_errors = reports;
        self
    }

    /// Stops trusting what the pools hold, after the GPU reported an error
    /// for work that may have used it, and destroys what they keep idle.
    fn forget_untrusted_resources(&self) {
        self.retirement.doubt();
        self.textures.clear();
        self.upload_textures.clear();
        self.readback_buffers.clear();
        self.uniform_buffers.clear();
        self.canaries.clear();
    }

    /// What the GPU this renderer drives offers: tier, intermediate format,
    /// adapter, and limits.
    #[must_use]
    pub fn gpu_capabilities(&self) -> &alpha2_gpu::Capabilities {
        self.gpu.capabilities()
    }

    /// The device's limits, as read when the renderer was created.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// Sets how long a native wait on GPU work may take before it fails with
    /// [`RenderError::GpuTimeout`]. Readbacks begun afterwards use it.
    pub fn set_gpu_wait_timeout(&mut self, timeout: Duration) {
        self.gpu_wait_timeout = timeout;
    }

    /// Why this renderer's device can no longer run work, once it found
    /// out: it completed work without running it, which a readback's missing
    /// canary showed ([`RenderError::GpuWorkDropped`]), or it could not make
    /// the resources a frame needs any more ([`RenderError::GpuDeviceLost`]).
    /// From then on this renderer's readbacks and NV12 packs fail with that
    /// error; a host should answer what it was doing and start a new process.
    #[must_use]
    pub fn gpu_lost(&self) -> Option<RenderError> {
        self.canaries.lost()
    }

    /// Fails once the device has been found unable to run work.
    pub(crate) fn ensure_gpu_runs_work(&self) -> Result<(), RenderError> {
        self.canaries.lost().map_or(Ok(()), Err)
    }

    /// Checks the canaries of earlier NV12 packs and repacks that have
    /// mapped, without waiting for the rest, and fails once the device has
    /// been found dropping work.
    pub(crate) fn check_submitted_work(&self) -> Result<(), RenderError> {
        // Mapping callbacks run only when the device is polled; a frame loop fed by a
        // hardware encoder may not poll it otherwise.
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        self.submission_canaries.check_mapped()?;
        self.ensure_gpu_runs_work()
    }

    /// Waits for every NV12 pack and repack submitted so far and checks that
    /// the GPU ran each of them.
    ///
    /// Those submissions read nothing back, so a device that completes work
    /// without running it (see [`RenderError::GpuWorkDropped`]) hands the
    /// encoder planes it never wrote. Each frame checks the canaries of the
    /// ones before it that have finished; an export calls this after its
    /// last frame, so that the last few are checked before it reports
    /// success.
    ///
    /// It may run while another thread polls the same device, as the macOS
    /// `VideoToolbox` encode worker does: a canary's mapping then reports on
    /// that thread, and this waits for the report.
    ///
    /// # Errors
    /// Returns [`RenderError::GpuWorkDropped`] when a canary is missing, or
    /// [`RenderError::GpuTimeout`] when the work does not complete, or a
    /// canary's mapping does not report, within the wait timeout.
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

    /// Releases every native lease whose GPU work has finished.
    ///
    /// Waiting on the queue is what makes progress possible: a decoder surface
    /// is only returned once the submission sampling it completes, so a caller
    /// that has run out of surface budget can call this and retry rather than
    /// failing the export. Surfaces held by work that has not been submitted
    /// yet are unaffected, so this never waits for something that cannot
    /// happen.
    ///
    /// A completed submission's leases are parked by its queue callback,
    /// which runs on whichever thread's poll collected it; this waits, up to
    /// the wait timeout, for every submitted one to be parked, then drops
    /// them.
    ///
    /// Returns how many leases it released.
    ///
    /// # Errors
    /// Returns a readback error if the device cannot be polled.
    ///
    /// Native only: a browser cannot block on the queue, and its external
    /// frames are released by the event loop.
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
        // Whatever is still awaited at the deadline stays parked for a later reclaim.
        self.settle(&self.retired_leases.parks, deadline, || {
            Ok(self.retired_leases.awaited() == 0)
        })?;
        Ok(self.retired_leases.drain())
    }

    /// Submits command buffers and then releases every completed native lease.
    ///
    /// A decoder surface must not be handed back to `FFmpeg` from inside
    /// `wgpu`'s submission, because a shared-queue backend holds the decoder's
    /// queue lock for the whole call while freeing a frame takes `FFmpeg`'s
    /// frame lock. `FFmpeg`'s own decode threads take those two in the
    /// opposite order, so doing it inside the submission deadlocks the export.
    /// Completed leases are therefore parked by the queue callback and dropped
    /// here, once the queue lock is released.
    ///
    /// # Errors
    /// Returns [`RenderError::Gpu`] when the GPU reported an error for the
    /// work recorded under the commands' scopes, which rejects the whole
    /// submission: nothing in it ran.
    pub(crate) fn submit<I>(&self, commands: I) -> Result<wgpu::SubmissionIndex, RenderError>
    where
        I: IntoIterator<Item = RenderCommands>,
    {
        self.submit_measured(commands, false)
            .map(|submitted| submitted.index)
    }

    /// Opens a command encoder whose encode lasts until it is submitted, and
    /// the error scopes that submission closes.
    ///
    /// Open it before acquiring what the frame draws into, so an allocation
    /// that fails is the frame's error.
    pub(crate) fn command_encoder(&self, label: &'static str) -> RenderEncoder {
        if self.gpu_errors.take_unhandled() {
            // An error the browser reported for earlier work: whatever it came from may be kept.
            self.clear_result_cache_for(crate::CacheInvalidation::GpuError);
            self.forget_untrusted_resources();
        }
        // Before anything else: waiting for another thread's frame holds no scope of this one.
        let frame = self.frame_lock.hold();
        // The scope opens first, so nothing the encoder uses can be destroyed under it.
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
            errors,
            scope,
            frame,
        }
    }

    /// Keeps whatever the pools let go of from being destroyed until the
    /// returned scope is dropped: for renderer work recorded into an encoder
    /// the caller submits.
    pub(crate) fn open_encode(&self) -> crate::retire::EncodeScope {
        self.retirement.open()
    }

    /// Submits and reports the queue wait, the submit, and the lease drain.
    ///
    /// With `measure` false no clock is read and every reported span is zero,
    /// which is what [`Renderer::submit`] relies on.
    ///
    /// # Errors
    /// Returns [`RenderError::Gpu`] when a scope of the commands caught an
    /// error: the GPU rejected the submission, and nothing the renderer
    /// pooled before it is trusted any more. The caller returns it before
    /// keeping a result or tracking a canary of the rejected work.
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
        for commands in commands {
            command_buffers.push(commands.buffer);
            errors.push(commands.errors);
            scopes.push(commands.scope);
            frames.push(commands.frame);
        }
        let (index, timings) = if measure {
            self.gpu.submit_timed(command_buffers)
        } else {
            (self.gpu.submit(command_buffers), SubmitTimings::default())
        };
        // The submission reports its own errors, so its scopes close after it, in reverse order.
        let mut caught = None;
        for scope in errors.into_iter().rev() {
            caught = match (caught, scope.close()) {
                (Some(alpha2_gpu::GpuErrorKind::OutOfMemory), _) | (_, None) => caught,
                (_, kind) => kind,
            };
        }
        // Submitted: what these encodes kept alive may be destroyed, and another frame may go.
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
            timings,
            lease_retire: drain_started.map_or(Duration::ZERO, |started| started.elapsed()),
        })
    }

    /// Returns whether per-pass timing is being collected.
    #[must_use]
    pub fn pass_timing_enabled(&self) -> bool {
        self.pass_timing.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enables or disables per-pass timing collection. Disabled by default.
    ///
    /// Timing is a profiling aid, not part of rendering. While it is off no
    /// clock is read, no pass record is taken, and no GPU timestamp resources
    /// are created, so the render path carries none of its cost. Turning it off
    /// also drops the retained history that [`Renderer::diagnostics`] reports,
    /// and the GPU timestamp of a pass encoded before then, whose read lands
    /// later, is dropped with it.
    ///
    /// Every other diagnostic (adapter identity, texture pool and pipeline
    /// counters, and frame outcomes) stays available at all times because it
    /// costs nothing per pass.
    pub fn set_pass_timing_enabled(&self, enabled: bool) {
        self.pass_timing
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        if !enabled {
            crate::diagnostics::end_timing_run(&self.diagnostics);
        }
    }

    /// The timing run a pass encoded now records into, or `None` while pass
    /// timing is off. Read before the flag: a run that ends after this read
    /// drops the pass's record even if the flag was still on.
    pub(crate) fn pass_timing_epoch(&self) -> Option<u64> {
        let epoch = crate::diagnostics::timing_epoch(&self.diagnostics);
        self.pass_timing_enabled().then_some(epoch)
    }

    /// Capability-negotiated storage precision. Render graph images contain
    /// premultiplied sRGB values; layer blend arithmetic may use linear light.
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

    /// Acquires a source-upload texture with RGBA8 storage.
    ///
    /// The texture and its default view return to a bounded renderer-owned pool
    /// when the handle is dropped. Queue submissions that use it must be issued
    /// before dropping the handle so later uploads remain queue-ordered.
    ///
    /// # Errors
    /// Returns an error when the requested dimensions exceed device limits.
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

    /// Gives back the GPU memory the renderer holds only for reuse: idle
    /// intermediate and upload textures, idle readback, uniform and canary
    /// buffers, and kept graph results. Each is destroyed, not only dropped, so the
    /// browser's WebGPU frees it now rather than at garbage collection.
    /// Nothing a render, a readback, or a caller still holds is released (a
    /// kept result the caller also holds stays with the caller), and work
    /// already submitted completes first, so this is safe at any point
    /// between calls. Later work allocates again what it needs.
    ///
    /// Returns the bytes this destroyed. If another thread is recording
    /// renderer work at that moment, the destruction waits for that work to
    /// be submitted, and is counted here all the same.
    pub fn release_idle_memory(&self) -> u64 {
        let before = self.retirement.retired_bytes();
        self.clear_result_cache();
        self.textures.clear();
        self.upload_textures.clear();
        self.readback_buffers.clear();
        self.uniform_buffers.clear();
        self.canaries.clear();
        self.retirement.retired_bytes().saturating_sub(before)
    }

    /// Sets the soft byte budget for idle reusable intermediate textures.
    ///
    /// Existing idle textures are evicted immediately until the pool is within
    /// the new budget. In-flight textures are considered when they return.
    pub fn set_texture_budget_bytes(&self, budget_bytes: u64) {
        self.textures.set_budget(budget_bytes);
    }

    /// Applies filter and temporal sampling controls.
    ///
    /// The filter sample budget is applied up to
    /// [`Self::filter_sample_limit`], since no pass takes more; a reduced
    /// budget is reported in [`Self::diagnostics`] as
    /// [`crate::CapabilityDiagnostic::FilterSamplesReduced`], and the plan,
    /// the diagnostics and the result cache all see the applied one. Kept
    /// results are dropped only when what is applied changes.
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

    /// The most filter samples a pass of this renderer takes:
    /// [`RenderQualityControls::MAX_FILTER_SAMPLES`], or 16 on a Tier-B
    /// device (see [`crate::filter_sample_limit`]).
    #[must_use]
    pub fn filter_sample_limit(&self) -> u32 {
        crate::filter_sample_limit(self.gpu.capabilities().tier)
    }

    /// Lets kept graph results occupy up to `budget_bytes` (see
    /// [`crate::RenderGraph::set_cache_intent`]); zero, the default, turns
    /// the cache off and drops what it holds. Kept results come out of the
    /// same GPU memory as the texture pool, which does not count them.
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

    /// The fallbacks this renderer applies to what its caller asked for.
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
