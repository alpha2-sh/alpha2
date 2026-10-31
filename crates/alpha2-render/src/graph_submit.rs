use std::time::Duration;
use web_time::Instant;

use crate::yuv::SourceWorkTicket;
use crate::{
    GraphError, PendingSourceWork, RenderError, RenderGraph, RenderedTexture, Renderer,
    Yuv420pReadback,
};
use alpha2_media::YuvEncoding;

/// Where one frame's graph construction and submission spent its time.
///
/// Fields are consecutive spans on one thread, separating a slow graph from a contended queue.
/// `graph_build` belongs to whoever built the [`RenderGraph`]; timed submissions write only the
/// later spans.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameSubmitTimings {
    /// Effect validation, source availability, and render-graph construction.
    pub graph_build: Duration,
    /// Encoding source conversion, graph, and readback commands.
    pub command_encode: Duration,
    /// Native acquire and layout transition for imported decoder surfaces.
    pub source_acquire: Duration,
    /// Blocked on the queue lock shared with the native decoder.
    pub queue_lock_wait: Duration,
    /// Inside `wgpu`'s queue submit.
    pub queue_submit: Duration,
    /// Releasing decoder surfaces whose submission has completed.
    pub lease_retire: Duration,
    /// Building the readback handle and returning pooled textures.
    pub complete: Duration,
}

impl FrameSubmitTimings {
    /// Adds another call's spans to these, saturating on overflow.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            graph_build: self.graph_build.saturating_add(other.graph_build),
            command_encode: self.command_encode.saturating_add(other.command_encode),
            source_acquire: self.source_acquire.saturating_add(other.source_acquire),
            queue_lock_wait: self.queue_lock_wait.saturating_add(other.queue_lock_wait),
            queue_submit: self.queue_submit.saturating_add(other.queue_submit),
            lease_retire: self.lease_retire.saturating_add(other.lease_retire),
            complete: self.complete.saturating_add(other.complete),
        }
    }

    /// Replaces every span after graph construction with `later`'s.
    const fn with_submission(self, later: Self) -> Self {
        Self {
            graph_build: self.graph_build,
            ..later
        }
    }
}

/// A lap timer that reads no clock unless a caller asked for timings.
#[derive(Copy, Clone)]
struct StageClock(Option<Instant>);

impl StageClock {
    fn start(enabled: bool) -> Self {
        Self(enabled.then(Instant::now))
    }

    /// Returns the span since the previous lap and starts a new one.
    fn lap(&mut self) -> Duration {
        let Some(mark) = self.0 else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        self.0 = Some(now);
        now.duration_since(mark)
    }
}

impl Renderer {
    /// Renders a graph, folding its sources' pending presentation work into the same submission.
    ///
    /// `sources` are in encode order; work another consumer already submitted is skipped.
    ///
    /// # Errors
    /// Returns [`RenderError`] when the graph or a render target is invalid.
    pub fn render_graph(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[&PendingSourceWork],
    ) -> Result<RenderedTexture, RenderError> {
        // Held past the tickets, which revert to pending if the frame fails.
        let frame = self.frame_lock.hold();
        let mut command_encoder = self.command_encoder("alpha2 frame encoder");
        let source_work = begin_source_work(&frame, sources);
        for work in &source_work {
            work.encode(&mut command_encoder);
        }
        let graph_work = self
            .encode_graph(graph, &mut command_encoder)
            .map_err(GraphError::into_render_error)?;
        stage_source_submissions(self.gpu.queue(), &source_work)?;
        graph_work.flush_uniforms(self.gpu.queue());
        // A rejected submission returns before anything of the frame is kept.
        self.submit([command_encoder.finish()])?;
        let combined_submission = !source_work.is_empty();
        for work in source_work {
            work.complete(self, false);
        }
        let texture = self.complete_graph_work(graph_work, !combined_submission);
        if combined_submission {
            crate::diagnostics::record_frame_submission(&self.diagnostics);
        }
        Ok(texture)
    }

    /// Renders a graph, converts it to YUV420, and submits it with its sources' pending work in one
    /// command buffer. The output size must be even.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph, target, or YUV layout data.
    pub fn render_graph_yuv420p(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[&PendingSourceWork],
        encoding: YuvEncoding,
    ) -> Result<Yuv420pReadback, RenderError> {
        self.render_graph_yuv420p_measured(graph, sources, encoding, None)
    }

    /// [`Renderer::render_graph_yuv420p`] that also writes every span after `graph_build` to
    /// `timings`.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph, target, or YUV layout data.
    pub fn render_graph_yuv420p_timed(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[&PendingSourceWork],
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<Yuv420pReadback, RenderError> {
        self.render_graph_yuv420p_measured(graph, sources, encoding, Some(timings))
    }

    /// [`Renderer::render_graph_yuv420p_timed`] packing into caller-owned NV12 plane textures
    /// instead of a staging buffer, for feeding a hardware encoder.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph or plane-target data.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn render_graph_nv12_timed(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[&PendingSourceWork],
        planes: crate::Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<(), RenderError> {
        let mut clock = StageClock::start(true);
        let mut collected = FrameSubmitTimings::default();
        let frame = self.frame_lock.hold();
        let mut command_encoder = self.command_encoder("alpha2 render and NV12 pack encoder");
        self.check_submitted_work()?;
        let source_work = begin_source_work(&frame, sources);
        for work in &source_work {
            work.encode(&mut command_encoder);
        }
        let sample = self
            .frame_gpu_timer()
            .and_then(crate::frame_gpu_time::FrameGpuTimer::begin);
        if let Some(sample) = &sample {
            sample.graph_start(&mut command_encoder);
        }
        let graph_work = self
            .encode_graph(graph, &mut command_encoder)
            .map_err(GraphError::into_render_error)?;
        if let Some(sample) = &sample {
            sample.graph_end(&mut command_encoder);
        }
        let pack =
            self.encode_nv12_pack(graph_work.texture(), planes, encoding, &mut command_encoder)?;
        if let Some(sample) = &sample {
            sample.pack_end(&mut command_encoder);
        }
        // No readback, so the canary goes in a buffer of its own.
        let (canary_buffer, canary) = self.canaries.encode_own(&self.gpu, &mut command_encoder)?;
        collected.command_encode = clock.lap();
        stage_source_submissions(self.gpu.queue(), &source_work)?;
        collected.source_acquire = clock.lap();
        graph_work.flush_uniforms(self.gpu.queue());
        let command_buffer = command_encoder.finish();
        collected.command_encode = collected.command_encode.saturating_add(clock.lap());
        // A rejected submission wrote nothing, so the frame fails here untracked.
        let submitted = self.submit_measured([command_buffer], true)?;
        if let Some(sample) = sample {
            sample.submitted(&self.gpu);
        }
        self.submission_canaries
            .track(&self.gpu, canary_buffer, canary);
        drop(pack);
        collected.queue_lock_wait = submitted.timings.queue_lock_wait;
        collected.queue_submit = submitted.timings.queue_submit;
        collected.lease_retire = submitted.lease_retire;
        let _ = clock.lap();
        for work in source_work {
            work.complete(self, false);
        }
        drop(self.complete_graph_work(graph_work, false));
        crate::diagnostics::record_frame_submission(&self.diagnostics);
        collected.complete = clock.lap();
        *timings = timings.with_submission(collected);
        Ok(())
    }

    fn render_graph_yuv420p_measured(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[&PendingSourceWork],
        encoding: YuvEncoding,
        timings: Option<&mut FrameSubmitTimings>,
    ) -> Result<Yuv420pReadback, RenderError> {
        let measure = timings.is_some();
        let mut clock = StageClock::start(measure);
        let mut collected = FrameSubmitTimings::default();
        let frame = self.frame_lock.hold();
        let mut command_encoder =
            self.command_encoder("alpha2 render and YUV420P readback encoder");
        let source_work = begin_source_work(&frame, sources);
        for work in &source_work {
            work.encode(&mut command_encoder);
        }
        // Only an export's timed frames are sampled.
        let sample = measure
            .then(|| self.frame_gpu_timer())
            .flatten()
            .and_then(crate::frame_gpu_time::FrameGpuTimer::begin);
        if let Some(sample) = &sample {
            sample.graph_start(&mut command_encoder);
        }
        let graph_work = self
            .encode_graph(graph, &mut command_encoder)
            .map_err(GraphError::into_render_error)?;
        if let Some(sample) = &sample {
            sample.graph_end(&mut command_encoder);
        }
        let readback_work =
            self.encode_yuv420p_readback(graph_work.texture(), encoding, &mut command_encoder)?;
        if let Some(sample) = &sample {
            sample.pack_end(&mut command_encoder);
        }
        collected.command_encode = clock.lap();
        stage_source_submissions(self.gpu.queue(), &source_work)?;
        collected.source_acquire = clock.lap();
        graph_work.flush_uniforms(self.gpu.queue());
        readback_work.flush_uniforms(self.gpu.queue());
        let command_buffer = command_encoder.finish();
        // Finishing the encoder and flushing uniforms count as command encoding.
        collected.command_encode = collected.command_encode.saturating_add(clock.lap());
        let submitted = self.submit_measured([command_buffer], measure)?;
        if let Some(sample) = sample {
            sample.submitted(&self.gpu);
        }
        collected.queue_lock_wait = submitted.timings.queue_lock_wait;
        collected.queue_submit = submitted.timings.queue_submit;
        collected.lease_retire = submitted.lease_retire;
        // The submission timed itself; discard this lap.
        let _ = clock.lap();
        let readback = self.complete_yuv420p_readback(
            readback_work,
            #[cfg(not(target_arch = "wasm32"))]
            submitted.index,
            false,
        );
        for work in source_work {
            work.complete(self, false);
        }
        drop(self.complete_graph_work(graph_work, false));
        crate::diagnostics::record_frame_submission(&self.diagnostics);
        collected.complete = clock.lap();
        if let Some(timings) = timings {
            *timings = timings.with_submission(collected);
        }
        Ok(readback)
    }
}

/// Flushes source uniforms and stages external textures' native waits and signals on the
/// submission about to sample them.
///
/// Every graph submission must run this before `queue.submit`, or a decoder surface could be
/// sampled before it is waited on and transitioned out of its decode layout.
pub(crate) fn stage_source_submissions(
    queue: &wgpu::Queue,
    source_work: &[SourceWorkTicket<'_>],
) -> Result<(), RenderError> {
    debug_assert!(
        source_work
            .iter()
            .all(SourceWorkTicket::has_no_staged_signals),
        "a source lease carried a staged native signal into the next frame"
    );
    for work in source_work {
        work.flush_uniforms(queue);
        if !work.prepare_submission() {
            return Err(RenderError::ExternalTextureSync(None));
        }
    }
    // Staging is separate from flushing so one native submission carries every source.
    for work in source_work {
        work.flush_submission()?;
    }
    Ok(())
}

/// Takes the pending work of `sources` no other frame has taken.
///
/// Call only under the frame lock, held until the tickets complete or drop, so "taken" always
/// means "submitted ahead of this frame", never "being encoded".
pub(crate) fn begin_source_work<'a>(
    frame: &crate::renderer::FrameGuard,
    sources: &[&'a PendingSourceWork],
) -> Vec<SourceWorkTicket<'a>> {
    sources
        .iter()
        .filter_map(|pending| pending.begin(frame))
        .collect()
}
