use alpha2_eval::{
    ActiveIndex, AudioSpan, AudioSpanDesc, AudioSpanScratch, EvalError, FrameDesc, FrameNeed,
    evaluate, evaluate_audio, evaluate_audio_in, frame_needs,
};
use alpha2_model::Project;
use alpha2_time::{MediaTime, TimeRange};

/// The next [`EngineProject::generation`].
static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An immutable project snapshot and its reusable evaluation index.
#[derive(Clone, Debug)]
pub struct EngineProject {
    project: Project,
    index: ActiveIndex,
    generation: u64,
    /// [`Project::draws_text`], read once.
    draws_text: bool,
}

impl EngineProject {
    /// Validates a project and builds its active interval index.
    ///
    /// # Errors
    /// Returns an evaluation error when the project is structurally invalid.
    pub fn new(project: Project) -> Result<Self, EvalError> {
        let index = ActiveIndex::build(&project)?;
        Ok(Self {
            draws_text: project.draws_text(),
            project,
            index,
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        })
    }

    /// Process-unique snapshot identity, shared by clones. An [`crate::Exporter`] renders only
    /// its own snapshot.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether any frame may draw text, which the host must prepare
    /// ([`crate::TextTextures::prepare_at`]).
    #[must_use]
    pub const fn draws_text(&self) -> bool {
        self.draws_text
    }

    /// Evaluates a deterministic frame description.
    ///
    /// # Errors
    /// Returns an error if the immutable index and project disagree.
    pub fn evaluate(&self, time: MediaTime) -> Result<FrameDesc<'_>, EvalError> {
        evaluate(&self.project, &self.index, time)
    }

    /// Returns the host frames needed at a timeline timestamp.
    ///
    /// # Errors
    /// Returns an error if the immutable index and project disagree.
    pub fn frame_needs(&self, time: MediaTime) -> Result<Vec<FrameNeed>, EvalError> {
        frame_needs(&self.project, &self.index, time)
    }

    /// Evaluates every audio clip overlapping a half-open timeline span.
    ///
    /// # Errors
    /// Returns an error if the immutable index and project disagree.
    pub fn evaluate_audio(&self, range: TimeRange) -> Result<AudioSpanDesc<'_>, EvalError> {
        evaluate_audio(&self.project, &self.index, range)
    }

    /// [`Self::evaluate_audio`] into reusable `scratch`.
    ///
    /// # Errors
    /// Returns an error if the immutable index and project disagree.
    pub fn evaluate_audio_in<'s>(
        &self,
        range: TimeRange,
        scratch: &'s mut AudioSpanScratch,
    ) -> Result<AudioSpan<'_, 's>, EvalError> {
        evaluate_audio_in(&self.project, &self.index, range, scratch)
    }

    /// The smallest interval holding everything the timeline shows, captions included
    /// ([`alpha2_model::Timeline::extent`]).
    #[must_use]
    pub fn timeline_range(&self) -> Option<TimeRange> {
        self.project.extent()
    }

    /// Every effect the project can draw, by id and version, each once. Compositions that cannot
    /// draw are reported when a frame shows them.
    #[must_use]
    pub fn effects(&self) -> Vec<(&str, u32)> {
        let mut effects = self
            .index
            .effects()
            .map(|effect| (effect.id.as_str(), effect.version))
            .collect::<Vec<_>>();
        effects.sort_unstable();
        effects.dedup();
        effects
    }

    /// Checks that `renderer` can draw every effect the project uses
    /// ([`Self::effects`]).
    ///
    /// # Errors
    /// Returns [`crate::EngineError::UnregisteredEffect`] naming the first missing one.
    pub fn check_effects(
        &self,
        renderer: &alpha2_render::Renderer,
    ) -> Result<(), crate::EngineError> {
        crate::frame::require_effects(renderer, self.effects())
    }

    /// Returns the immutable project document.
    #[must_use]
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// What the project's timeline-time values read their data bindings through.
    #[must_use]
    pub fn sample_context(&self) -> alpha2_eval::SampleContext<'_> {
        self.index.sample_context(&self.project)
    }
}
