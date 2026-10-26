use alpha2_eval::FrameNeed;
use alpha2_media::{SourceId, VideoCodec, YuvEncoding};
use alpha2_render::{FrameSubmitTimings, Renderer, Yuv420pReadback, Yuv420pTargets};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use std::time::Duration;
use web_time::Instant;

use crate::schedule::frame_times;
use crate::{
    EngineError, EngineProject, FrameProvider, FrameRenderer, GpuFrameRenderer, MissingFramePolicy,
    QualityProfile,
};

/// Largest frame schedule accepted by one export operation.
pub const MAX_EXPORT_FRAMES: u64 = 10_000_000;
const MAX_PREFETCH_FRAMES: u32 = 120;

/// Decode requirements for one scheduled export frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportFrameNeeds {
    /// Zero-based frame position inside the export operation.
    pub index: u64,
    /// Exact rational frame timestamp on the project timeline.
    pub time: MediaTime,
    /// Deduplicated source frames required before rendering this timestamp.
    pub needs: Vec<FrameNeed>,
}

/// Completed and total frame counts for deterministic export.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ExportProgress {
    /// Successfully rendered frames.
    pub completed: u64,
    /// Frame intervals that intersect the export range.
    pub total: u64,
}

impl ExportProgress {
    /// Completion ratio in the inclusive range zero to one.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.completed as f64 / self.total as f64
        }
    }
}

/// Outcome of one non-blocking exporter step.
#[derive(Debug)]
pub enum ExportStep {
    /// Required decoded frames are not available; retry without advancing.
    Waiting {
        /// Timeline timestamp that remains pending.
        time: MediaTime,
        /// Sources the host must provide before retrying.
        missing_sources: Vec<SourceId>,
    },
    /// One frame was rendered and the cursor advanced exactly once.
    Rendered {
        /// Zero-based position inside this export operation.
        index: u64,
        /// Exact rational-frame timestamp.
        time: MediaTime,
        /// Submitted planar YUV420 readback for the encoder.
        readback: Yuv420pReadback,
    },
    /// Every scheduled frame has been produced.
    Complete,
}

/// One step of an export that leaves its picture as a rendered texture.
#[derive(Debug)]
pub enum ExportTextureStep {
    /// Required decoded frames are not available; retry without advancing.
    Waiting {
        /// Timeline timestamp that remains pending.
        time: MediaTime,
        /// Sources the host must provide before retrying.
        missing_sources: Vec<SourceId>,
    },
    /// One frame was rendered and the cursor advanced exactly once.
    Rendered {
        /// Zero-based position inside this export operation.
        index: u64,
        /// Exact rational-frame timestamp.
        time: MediaTime,
        /// The frame at export quality, in the renderer's intermediate
        /// format; its work is submitted.
        texture: alpha2_render::RenderedTexture,
    },
    /// Every scheduled frame has been produced.
    Complete,
}

/// One step of an export that packs its picture straight into caller-owned
/// planes: an encoder's frame in memory, or GPU encoder planes.
#[derive(Debug)]
pub enum ExportPlaneStep {
    /// Required decoded frames are not available; retry without advancing.
    Waiting {
        /// Timeline timestamp that remains pending.
        time: MediaTime,
        /// Sources the host must provide before retrying.
        missing_sources: Vec<SourceId>,
    },
    /// One frame was packed into the caller's planes and the cursor advanced.
    Rendered {
        /// Zero-based position inside this export operation.
        index: u64,
        /// Exact rational-frame timestamp.
        time: MediaTime,
    },
    /// Every scheduled frame has been produced.
    Complete,
}

/// Where one [`Exporter::render_next_timed`] step spent its time.
///
/// `frameSubmitMs` is one number covering timeline evaluation, graph
/// construction, and the queue submission; these spans split it so a
/// regression can be attributed to one of them.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportStepTimings {
    /// Evaluating the timeline at this frame's timestamp.
    pub evaluate: Duration,
    /// Applying the quality profile, which also trims the texture pool to its
    /// budget and so is not the constant-time setter it reads as.
    pub prepare: Duration,
    /// Everything the renderer did with the evaluated frame.
    pub submit: FrameSubmitTimings,
}

impl ExportStepTimings {
    /// Adds another step's spans to these, saturating on overflow.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            evaluate: self.evaluate.saturating_add(other.evaluate),
            prepare: self.prepare.saturating_add(other.prepare),
            submit: self.submit.merged(other.submit),
        }
    }
}

/// Deterministic, retryable frame-sequence runner.
#[derive(Clone, Debug)]
pub struct Exporter {
    /// The [`EngineProject::generation`] it was made for, and renders.
    snapshot: u64,
    quality: QualityProfile,
    encoding: YuvEncoding,
    frame_rate: FrameRate,
    effective_range: TimeRange,
    next_frame: i64,
    end_frame: i64,
    completed: u64,
    total: u64,
}

impl Exporter {
    /// Creates an exporter for frame intervals intersecting a half-open timeline range.
    ///
    /// Everything an export can be refused for is refused here, before a
    /// host opens an encoder: the output 4:2:0 frames are the project size at
    /// `quality`, and they must be even in both dimensions. They are packed
    /// for `codec`: [`YuvEncoding::export`]. Every later step takes this
    /// snapshot (or a clone of it) and refuses any other with
    /// [`EngineError::ExportProjectChanged`], so an export renders one
    /// project, at the size checked here.
    ///
    /// # Errors
    /// Returns an error for an empty range, invalid quality, timeline overflow,
    /// or [`EngineError::OddExportDimensions`].
    pub fn new(
        snapshot: &EngineProject,
        range: TimeRange,
        frame_rate: FrameRate,
        quality: QualityProfile,
        codec: VideoCodec,
    ) -> Result<Self, EngineError> {
        quality.export_size(&snapshot.project().settings)?;
        if range.is_empty() {
            return Err(EngineError::EmptyExportRange);
        }
        let next_frame = frame_rate
            .frame_at_or_before(range.start())
            .ok_or(EngineError::OutOfRange)?;
        let end_frame = first_frame_at_or_after(frame_rate, range.end())?;
        let count = end_frame
            .checked_sub(next_frame)
            .ok_or(EngineError::OutOfRange)?;
        let total = u64::try_from(count).map_err(|_| EngineError::OutOfRange)?;
        if total == 0 {
            return Err(EngineError::EmptyExportRange);
        }
        if total > MAX_EXPORT_FRAMES {
            return Err(EngineError::ExportTooLarge {
                frames: total,
                maximum: MAX_EXPORT_FRAMES,
            });
        }
        let effective_range = TimeRange::new(
            frame_rate
                .frame_start(next_frame)
                .ok_or(EngineError::OutOfRange)?,
            frame_rate
                .frame_start(end_frame)
                .ok_or(EngineError::OutOfRange)?,
        )
        .map_err(|_| EngineError::OutOfRange)?;
        Ok(Self {
            snapshot: snapshot.generation(),
            quality,
            encoding: YuvEncoding::export(codec),
            frame_rate,
            effective_range,
            next_frame,
            end_frame,
            completed: 0,
            total,
        })
    }

    /// How this export's frames are packed into 4:2:0 YUV.
    #[must_use]
    pub const fn encoding(&self) -> YuvEncoding {
        self.encoding
    }

    /// The timeline range this export covers: the requested range widened to
    /// the frame grid, `[floor(start), ceil(end))`.
    ///
    /// A video export renders whole frames, so its first frame can start up to
    /// one frame before the requested start. That frame's start is the
    /// output's zero, and a host places audio, and reports the export, from
    /// this range rather than from the request.
    #[must_use]
    pub const fn effective_range(&self) -> TimeRange {
        self.effective_range
    }

    /// The samples this export's audio covers at `sample_rate`: the
    /// [`export_audio_window`] of [`Self::effective_range`].
    ///
    /// # Errors
    /// Returns [`EngineError::OutOfRange`] when the range does not fit in
    /// memory-addressable samples.
    pub fn audio_window(&self, sample_rate: u32) -> Result<ExportAudioWindow, EngineError> {
        export_audio_window(self.effective_range, sample_rate)
    }

    /// Creates an exporter spanning everything a project shows, its clips
    /// and caption cues ([`EngineProject::timeline_range`]), packed for
    /// `codec`.
    ///
    /// # Errors
    /// Returns an error for an empty project or invalid schedule.
    pub fn for_project(
        project: &EngineProject,
        quality: QualityProfile,
        codec: VideoCodec,
    ) -> Result<Self, EngineError> {
        let range = project
            .timeline_range()
            .ok_or(EngineError::EmptyExportRange)?;
        Self::new(
            project,
            range,
            project.project().settings.frame_rate,
            quality,
            codec,
        )
    }

    /// The only policy an export can apply.
    ///
    /// Export output must not depend on decode timing: a pipelined export and
    /// a synchronous one are required to be byte-identical, which
    /// `scripts/test-host-cli-parity.mjs` asserts. Repeating the previous
    /// frame when a source is late would make the result a function of how
    /// fast the host decoded, so this is a constant rather than a setting.
    /// [`Player`] configures the same policy because a dropped real-time frame
    /// has no such guarantee to break.
    ///
    /// [`Player`]: crate::Player
    pub const MISSING_FRAME_POLICY: MissingFramePolicy = MissingFramePolicy::Wait;

    /// Returns the policy used when source frames are unavailable.
    #[must_use]
    pub const fn missing_frame_policy(&self) -> MissingFramePolicy {
        Self::MISSING_FRAME_POLICY
    }

    /// Returns current deterministic export progress.
    #[must_use]
    pub const fn progress(&self) -> ExportProgress {
        ExportProgress {
            completed: self.completed,
            total: self.total,
        }
    }

    /// The size frames of `project` render at: where a host prepares their
    /// captions.
    ///
    /// # Errors
    /// Returns an error when the quality profile cannot size the project.
    pub fn render_size(
        &self,
        project: &EngineProject,
    ) -> Result<alpha2_render::RenderSize, EngineError> {
        self.quality.render_size(&project.project().settings)
    }

    /// Refuses a snapshot other than the one this export was made for.
    fn check(&self, project: &EngineProject) -> Result<(), EngineError> {
        if project.generation() == self.snapshot {
            Ok(())
        } else {
            Err(EngineError::ExportProjectChanged)
        }
    }

    /// Returns the pending frame timestamp, or `None` after completion.
    #[must_use]
    pub fn pending_time(&self) -> Option<MediaTime> {
        (self.next_frame < self.end_frame)
            .then(|| self.frame_rate.frame_start(self.next_frame))
            .flatten()
    }

    /// Takes the pending frame as supplied from elsewhere (a copy baked by
    /// an earlier export of the same snapshot) and advances the cursor
    /// exactly as a render does. Returns its index and time, or `None` after
    /// completion.
    ///
    /// # Errors
    /// Returns [`EngineError::OutOfRange`] if the cursor would overflow.
    pub fn take_next(&mut self) -> Result<Option<(u64, MediaTime)>, EngineError> {
        let Some(time) = self.pending_time() else {
            return Ok(None);
        };
        let index = self.completed;
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or(EngineError::OutOfRange)?;
        self.completed += 1;
        Ok(Some((index, time)))
    }

    /// The frame grid the export renders on.
    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    /// The times of the pending frame and up to `frames - 1` after it, in
    /// order, as [`Self::take_next`] and the renders after it will reach
    /// them: fewer near the end of the range, none after completion. The
    /// cursor is not changed, and `frames` is clamped as in
    /// [`Self::prefetch_needs`].
    ///
    /// # Errors
    /// Returns [`EngineError::OutOfRange`] if a time overflows.
    pub fn upcoming_times(&self, frames: u32) -> Result<Vec<MediaTime>, EngineError> {
        Ok(frame_times(
            self.frame_rate,
            self.next_frame,
            frames.min(MAX_PREFETCH_FRAMES),
            Some(self.end_frame),
        )?
        .into_iter()
        .map(|(_, time)| time)
        .collect())
    }

    /// Returns decode requirements for the pending frame and bounded lookahead.
    ///
    /// The exporter cursor is not changed. `frames` is the total number of
    /// scheduled frames requested, including the current frame, and is clamped
    /// to 120 to keep one request bounded.
    ///
    /// # Errors
    /// Returns evaluation or timeline overflow errors, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    pub fn prefetch_needs(
        &self,
        project: &EngineProject,
        frames: u32,
    ) -> Result<Vec<ExportFrameNeeds>, EngineError> {
        self.check(project)?;
        let count = frames.min(MAX_PREFETCH_FRAMES);
        let times = frame_times(
            self.frame_rate,
            self.next_frame,
            count,
            Some(self.end_frame),
        )?;
        times
            .into_iter()
            .enumerate()
            .map(|(offset, (_, time))| {
                let offset = u64::try_from(offset).map_err(|_| EngineError::OutOfRange)?;
                let index = self
                    .completed
                    .checked_add(offset)
                    .ok_or(EngineError::OutOfRange)?;
                Ok(ExportFrameNeeds {
                    index,
                    time,
                    needs: project.frame_needs(time)?,
                })
            })
            .collect()
    }

    /// Attempts one frame without blocking on host decode.
    ///
    /// Waiting does not change progress. A successful render advances the
    /// exact rational frame cursor once, so retries cannot skip timestamps.
    ///
    /// # Errors
    /// Returns timeline evaluation, quality, or rendering failures, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    pub fn render_next(
        &mut self,
        project: &EngineProject,
        renderer: &Renderer,
        frames: &dyn FrameProvider,
    ) -> Result<ExportStep, EngineError> {
        self.render_next_measured(project, renderer, frames, None)
    }

    /// Attempts one frame and reports where the step spent its time.
    ///
    /// This renders exactly what [`Exporter::render_next`] renders. A step
    /// that only waits leaves `timings` holding the stages that ran.
    ///
    /// # Errors
    /// Returns timeline evaluation, quality, or rendering failures, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    pub fn render_next_timed(
        &mut self,
        project: &EngineProject,
        renderer: &Renderer,
        frames: &dyn FrameProvider,
        timings: &mut ExportStepTimings,
    ) -> Result<ExportStep, EngineError> {
        self.render_next_measured(project, renderer, frames, Some(timings))
    }

    /// Renders the next frame straight into caller-owned NV12 plane textures.
    ///
    /// The readback-free counterpart of [`Self::render_next_timed`]: the
    /// cursor advances identically, so an export can be driven through either
    /// without its frame accounting changing. `Rendered` carries no readback
    /// because the picture stays on the GPU.
    ///
    /// # Errors
    /// Returns an engine error when evaluation or rendering fails, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn render_next_nv12_timed(
        &mut self,
        project: &EngineProject,
        renderer: &Renderer,
        frames: &dyn FrameProvider,
        planes: alpha2_render::Nv12PlaneTargets<'_>,
        timings: &mut ExportStepTimings,
    ) -> Result<ExportPlaneStep, EngineError> {
        self.check(project)?;
        let Some(time) = self.pending_time() else {
            return Ok(ExportPlaneStep::Complete);
        };
        let evaluate_started = Instant::now();
        let frame = project.evaluate(time)?;
        crate::frame::check_frame_effects(renderer, &frame)?;
        let prepare_started = Instant::now();
        let evaluate = prepare_started.duration_since(evaluate_started);
        let size = self.quality.render_size(&project.project().settings)?;
        renderer.set_texture_budget_bytes(self.quality.texture_budget_bytes);
        renderer.set_quality_controls(
            self.quality.max_filter_samples,
            self.quality.motion_blur_subframes,
        );
        let prepare = prepare_started.elapsed();
        let mut submit = FrameSubmitTimings::default();
        let attempt = renderer.render_frame_nv12_timed(
            size,
            &frame,
            frames,
            planes,
            self.encoding,
            &mut submit,
        )?;
        *timings = ExportStepTimings {
            evaluate,
            prepare,
            submit,
        };
        if !attempt.submitted {
            return Ok(ExportPlaneStep::Waiting {
                time,
                missing_sources: attempt.report.missing_sources,
            });
        }
        let index = self.completed;
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or(EngineError::OutOfRange)?;
        self.completed += 1;
        Ok(ExportPlaneStep::Rendered { index, time })
    }

    /// Renders the next frame straight into caller-owned YUV420 planes, on
    /// whichever renderer draws it.
    ///
    /// The cursor advances exactly as it does for [`Self::render_next`]; a
    /// step that waits writes nothing. `timings`, when given, receives the
    /// evaluation and preparation spans, and the whole draw as its submit
    /// span.
    ///
    /// # Errors
    /// Returns an engine error when evaluation or rendering fails, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    pub fn render_next_into_yuv420p<R: FrameRenderer + ?Sized>(
        &mut self,
        project: &EngineProject,
        renderer: &R,
        frames: &dyn FrameProvider,
        targets: &mut Yuv420pTargets<'_>,
        timings: Option<&mut ExportStepTimings>,
    ) -> Result<ExportPlaneStep, EngineError> {
        self.check(project)?;
        let Some(time) = self.pending_time() else {
            return Ok(ExportPlaneStep::Complete);
        };
        let evaluate_started = Instant::now();
        let frame = project.evaluate(time)?;
        crate::frame::check_frame_effects(renderer, &frame)?;
        let prepare_started = Instant::now();
        let evaluate = prepare_started.duration_since(evaluate_started);
        let size = self.quality.render_size(&project.project().settings)?;
        renderer.set_texture_budget_bytes(self.quality.texture_budget_bytes);
        renderer.set_quality_controls(
            self.quality.max_filter_samples,
            self.quality.motion_blur_subframes,
        );
        let draw_started = Instant::now();
        let prepare = draw_started.duration_since(prepare_started);
        let report =
            renderer.render_frame_into_yuv420p(size, &frame, frames, self.encoding, targets)?;
        if let Some(timings) = timings {
            *timings = ExportStepTimings {
                evaluate,
                prepare,
                submit: FrameSubmitTimings {
                    command_encode: draw_started.elapsed(),
                    ..FrameSubmitTimings::default()
                },
            };
        }
        if !report.is_complete() {
            return Ok(ExportPlaneStep::Waiting {
                time,
                missing_sources: report.missing_sources,
            });
        }
        let index = self.completed;
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or(EngineError::OutOfRange)?;
        self.completed += 1;
        Ok(ExportPlaneStep::Rendered { index, time })
    }

    /// Renders the next frame and hands its texture over, with no readback
    /// and no pack: for a host that moves the picture on by other means, such
    /// as presenting it to a canvas an encoder captures.
    ///
    /// The cursor advances exactly as it does for [`Self::render_next`].
    ///
    /// # Errors
    /// Returns an engine error when evaluation or rendering fails, and
    /// [`EngineError::ExportProjectChanged`] for another snapshot.
    pub fn render_next_texture(
        &mut self,
        project: &EngineProject,
        renderer: &Renderer,
        frames: &dyn FrameProvider,
    ) -> Result<ExportTextureStep, EngineError> {
        self.check(project)?;
        let Some(time) = self.pending_time() else {
            return Ok(ExportTextureStep::Complete);
        };
        let frame = project.evaluate(time)?;
        crate::frame::check_frame_effects(renderer, &frame)?;
        let size = self.quality.render_size(&project.project().settings)?;
        renderer.set_texture_budget_bytes(self.quality.texture_budget_bytes);
        renderer.set_quality_controls(
            self.quality.max_filter_samples,
            self.quality.motion_blur_subframes,
        );
        let render = crate::FrameRenderer::render_frame(renderer, size, &frame, frames)?;
        let Some(texture) = render.texture else {
            return Ok(ExportTextureStep::Waiting {
                time,
                missing_sources: render.report.missing_sources,
            });
        };
        let index = self.completed;
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or(EngineError::OutOfRange)?;
        self.completed += 1;
        Ok(ExportTextureStep::Rendered {
            index,
            time,
            texture,
        })
    }

    fn render_next_measured(
        &mut self,
        project: &EngineProject,
        renderer: &Renderer,
        frames: &dyn FrameProvider,
        timings: Option<&mut ExportStepTimings>,
    ) -> Result<ExportStep, EngineError> {
        self.check(project)?;
        let Some(time) = self.pending_time() else {
            return Ok(ExportStep::Complete);
        };
        let evaluate_started = timings.as_ref().map(|_| Instant::now());
        let frame = project.evaluate(time)?;
        crate::frame::check_frame_effects(renderer, &frame)?;
        let prepare_started = evaluate_started.map(|_| Instant::now());
        let evaluate = evaluate_started
            .zip(prepare_started)
            .map_or(Duration::ZERO, |(started, ended)| {
                ended.duration_since(started)
            });
        let size = self.quality.render_size(&project.project().settings)?;
        renderer.set_texture_budget_bytes(self.quality.texture_budget_bytes);
        renderer.set_quality_controls(
            self.quality.max_filter_samples,
            self.quality.motion_blur_subframes,
        );
        let prepare = prepare_started.map_or(Duration::ZERO, |started| started.elapsed());
        let attempt = match timings {
            Some(timings) => {
                let mut submit = FrameSubmitTimings::default();
                let attempt = renderer.render_frame_yuv420p_timed(
                    size,
                    &frame,
                    frames,
                    self.encoding,
                    &mut submit,
                )?;
                *timings = ExportStepTimings {
                    evaluate,
                    prepare,
                    submit,
                };
                attempt
            }
            None => renderer.render_frame_yuv420p(size, &frame, frames, self.encoding)?,
        };
        let Some(readback) = attempt.readback else {
            return Ok(ExportStep::Waiting {
                time,
                missing_sources: attempt.report.missing_sources,
            });
        };
        let index = self.completed;
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or(EngineError::OutOfRange)?;
        self.completed += 1;
        Ok(ExportStep::Rendered {
            index,
            time,
            readback,
        })
    }
}

/// The project-rate samples an export's audio covers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ExportAudioWindow {
    /// First sample, at the project sample rate.
    pub start_sample: i64,
    /// Sample count.
    pub frames: usize,
}

/// The samples an export over `range` writes as audio: both ends are the
/// samples nearest the range's ends, half a sample rounding up.
///
/// Every host places export audio through this, from the export's
/// [`Exporter::effective_range`], so the audio starts and ends within half a
/// sample of the video's first frame and last frame end on every path that
/// writes the file.
///
/// # Errors
/// Returns [`EngineError::OutOfRange`] when the range does not fit in
/// memory-addressable samples.
pub fn export_audio_window(
    range: TimeRange,
    sample_rate: u32,
) -> Result<ExportAudioWindow, EngineError> {
    let nearest = |ticks: i64| {
        let doubled =
            i128::from(ticks) * i128::from(sample_rate) * 2 + i128::from(TICKS_PER_SECOND);
        i64::try_from(doubled.div_euclid(i128::from(TICKS_PER_SECOND) * 2))
            .map_err(|_| EngineError::OutOfRange)
    };
    let start_sample = nearest(range.start().ticks())?;
    let end = nearest(range.end().ticks())?;
    let frames = end
        .checked_sub(start_sample)
        .and_then(|frames| usize::try_from(frames).ok())
        .ok_or(EngineError::OutOfRange)?;
    Ok(ExportAudioWindow {
        start_sample,
        frames,
    })
}

fn first_frame_at_or_after(rate: FrameRate, time: MediaTime) -> Result<i64, EngineError> {
    let floor = rate
        .frame_at_or_before(time)
        .ok_or(EngineError::OutOfRange)?;
    let floor_time = rate.frame_start(floor).ok_or(EngineError::OutOfRange)?;
    if floor_time < time {
        floor.checked_add(1).ok_or(EngineError::OutOfRange)
    } else {
        Ok(floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Player;
    use alpha2_model::{Project, ProjectId, ProjectSettings};

    fn snapshot(width: u32, height: u32) -> EngineProject {
        EngineProject::new(Project::new(
            ProjectId::new("export".to_owned()).expect("id"),
            "export",
            ProjectSettings::new(width, height, FrameRate::new(30, 1).expect("rate"), 48_000)
                .expect("settings"),
        ))
        .expect("valid project")
    }

    #[test]
    fn an_odd_4_2_0_output_is_refused_before_anything_is_encoded() {
        let range =
            TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(1)).expect("valid range");
        let rate = FrameRate::new(30, 1).expect("valid rate");
        for (size, suggested) in [((65, 49), (64, 48)), ((64, 49), (64, 48)), ((1, 3), (2, 2))] {
            let error = Exporter::new(
                &snapshot(size.0, size.1),
                range,
                rate,
                QualityProfile::export(),
                VideoCodec::H264,
            )
            .expect_err("odd output");
            assert_eq!(
                error,
                EngineError::OddExportDimensions {
                    width: size.0,
                    height: size.1,
                    suggested,
                }
            );
            assert!(
                error
                    .to_string()
                    .contains(&format!("{}x{}", suggested.0, suggested.1))
            );
        }
        assert!(
            Exporter::new(
                &snapshot(64, 48),
                range,
                rate,
                QualityProfile::export(),
                VideoCodec::H264
            )
            .is_ok()
        );
    }

    #[test]
    fn runners_expose_distinct_missing_frame_policies() {
        let rate = FrameRate::new(30, 1).expect("valid rate");
        let range =
            TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(1)).expect("valid range");
        let exporter = Exporter::new(
            &snapshot(16, 16),
            range,
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("valid exporter");
        assert_eq!(
            Player::<alpha2_render::RenderedTexture>::default().missing_frame_policy(),
            MissingFramePolicy::HoldPrevious
        );
        assert_eq!(exporter.missing_frame_policy(), MissingFramePolicy::Wait);
    }

    #[test]
    fn schedule_uses_exact_rational_frame_boundaries() {
        let rate = FrameRate::new(30_000, 1_001).expect("valid rate");
        let start = rate.frame_start(10).expect("representable");
        let end = rate.frame_start(20).expect("representable");
        let exporter = Exporter::new(
            &snapshot(16, 16),
            TimeRange::new(start, end).expect("valid range"),
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("valid exporter");
        assert_eq!(exporter.pending_time(), Some(start));
        assert_eq!(
            exporter.progress(),
            ExportProgress {
                completed: 0,
                total: 10,
            }
        );
    }

    #[test]
    fn schedule_includes_every_frame_intersecting_an_unaligned_range() {
        let rate = FrameRate::new(30, 1).expect("valid rate");
        let subframe = Exporter::new(
            &snapshot(16, 16),
            TimeRange::new(MediaTime(2_000), MediaTime(4_000)).expect("non-empty range"),
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("one intersecting frame");
        assert_eq!(subframe.pending_time(), Some(MediaTime::ZERO));
        assert_eq!(subframe.progress().total, 1);

        let unaligned = Exporter::new(
            &snapshot(16, 16),
            TimeRange::new(MediaTime(2_000), MediaTime(6_000)).expect("non-empty range"),
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("two intersecting frames");
        assert_eq!(unaligned.pending_time(), Some(MediaTime::ZERO));
        assert_eq!(unaligned.progress().total, 2);
        assert_eq!(
            unaligned.effective_range(),
            TimeRange::new(MediaTime::ZERO, MediaTime(8_000)).expect("frame grid")
        );
    }

    #[test]
    fn effective_range_is_the_request_widened_to_the_frame_grid() {
        let rate = FrameRate::new(30_000, 1_001).expect("valid rate");
        let frame = |index| rate.frame_start(index).expect("representable").ticks();
        for (start, end) in [
            (frame(3), frame(9)),
            (frame(3) + 1, frame(9) - 1),
            (frame(4) - 1, frame(8) + 1),
        ] {
            let exporter = Exporter::new(
                &snapshot(16, 16),
                TimeRange::new(MediaTime(start), MediaTime(end)).expect("non-empty range"),
                rate,
                QualityProfile::export(),
                VideoCodec::H264,
            )
            .expect("valid exporter");
            let effective = exporter.effective_range();
            assert_eq!(
                (effective.start(), effective.end()),
                (MediaTime(frame(3)), MediaTime(frame(9))),
                "{start}..{end}"
            );
            assert_eq!(exporter.pending_time(), Some(effective.start()));
        }
    }

    #[test]
    fn schedule_rejects_unbounded_frame_counts() {
        let rate = FrameRate::new(30, 1).expect("valid rate");
        let range = TimeRange::new(MediaTime::ZERO, MediaTime(i64::MAX)).expect("ordered range");
        assert!(matches!(
            Exporter::new(
                &snapshot(16, 16),
                range,
                rate,
                QualityProfile::export(),
                VideoCodec::H264
            ),
            Err(EngineError::ExportTooLarge { .. })
        ));
    }

    #[test]
    fn prefetch_reports_future_indices_without_advancing_the_cursor() {
        let rate = FrameRate::new(30, 1).expect("valid rate");
        let project = Project::new(
            ProjectId::new("prefetch").expect("project ID"),
            "prefetch",
            ProjectSettings::new(16, 16, rate, 48_000).expect("settings"),
        );
        let snapshot = EngineProject::new(project).expect("snapshot");
        let range =
            TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(1)).expect("non-empty range");
        let exporter = Exporter::new(
            &snapshot,
            range,
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("valid exporter");

        let prefetched = exporter.prefetch_needs(&snapshot, 2).expect("prefetch");
        assert_eq!(
            prefetched
                .iter()
                .map(|frame| (frame.index, frame.time))
                .collect::<Vec<_>>(),
            vec![(0, MediaTime::ZERO), (1, MediaTime::from_ticks(4_000))]
        );
        assert!(prefetched.iter().all(|frame| frame.needs.is_empty()));
        assert_eq!(exporter.pending_time(), Some(MediaTime::ZERO));
        assert_eq!(exporter.progress().completed, 0);
    }

    #[test]
    fn upcoming_times_are_the_frames_the_cursor_reaches_next() {
        let rate = FrameRate::new(30, 1).expect("valid rate");
        let snapshot = snapshot(16, 16);
        // Three frames: 0, 1/30 s, and 2/30 s.
        let range = TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(12_000))
            .expect("non-empty range");
        let mut exporter = Exporter::new(
            &snapshot,
            range,
            rate,
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("valid exporter");
        let frame = MediaTime::from_ticks;
        assert_eq!(
            exporter.upcoming_times(2).expect("times"),
            vec![frame(0), frame(4_000)]
        );
        let (_, taken) = exporter.take_next().expect("take").expect("a frame");
        assert_eq!(taken, frame(0));
        // Fewer near the end; the cursor is not moved.
        assert_eq!(
            exporter.upcoming_times(5).expect("times"),
            vec![frame(4_000), frame(8_000)]
        );
        assert_eq!(exporter.pending_time(), Some(frame(4_000)));
        exporter.take_next().expect("take");
        exporter.take_next().expect("take");
        assert_eq!(exporter.upcoming_times(2).expect("times"), Vec::new());
    }

    #[test]
    fn a_whole_project_export_covers_its_caption_cues() {
        let project = |clips: &str| {
            let json = format!(
                r#"{{"schema_version": 1, "id": "p", "name": "p",
                  "settings": {{"width": 16, "height": 16, "frame_rate": {{"num": 30, "den": 1}}, "sample_rate": 48000}},
                  "sources": [{{"id": "v", "kind": "video", "locator": "v.mp4"}}],
                  "timeline": {{"tracks": [
                    {{"id": "t", "name": "T", "kind": "video", "clips": [{clips}]}},
                    {{"id": "c", "name": "C", "kind": "caption", "captions": {{"cues": [
                      {{"timeline_range": {{"start": 1080000, "end": 1440000}}, "text": "last line"}}]}}}}]}}}}"#
            );
            EngineProject::new(Project::from_json(&json).expect("valid")).expect("valid project")
        };
        let clip = r#"{"id": "a", "source_id": "v", "timeline_range": {"start": 0, "end": 1200000}, "source_range": {"start": 0, "end": 1200000}}"#;
        for (snapshot, start) in [(project(clip), 0), (project(""), 1_080_000)] {
            let range = snapshot.timeline_range().expect("something is shown");
            assert_eq!(
                (range.start().ticks(), range.end().ticks()),
                (start, 1_440_000)
            );
            let exporter =
                Exporter::for_project(&snapshot, QualityProfile::export(), VideoCodec::H264)
                    .expect("an export of the whole project");
            assert_eq!(exporter.effective_range(), range);
        }
    }
}
