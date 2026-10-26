use alpha2_eval::FrameNeed;
use alpha2_media::SourceId;
use alpha2_render::{CacheEligibility, RenderCacheKey, RenderSize, RenderedTexture};
use alpha2_time::MediaTime;

use crate::schedule::frame_times;
use crate::{
    EngineError, EngineProject, FrameProvider, FrameRenderer, MissingFramePolicy, QualityProfile,
};

const MAX_PREFETCH_FRAMES: u32 = 120;

/// Result of one real-time presentation attempt, with `I` the picture the
/// renderer draws ([`FrameRenderer::Image`]).
#[derive(Debug)]
pub struct PlaybackFrame<'a, I = RenderedTexture> {
    /// Timeline time requested by the host clock.
    pub requested_time: MediaTime,
    /// Timeline time of the returned texture, absent before the first success.
    pub presented_time: Option<MediaTime>,
    /// Current or held picture. It remains owned by the player.
    pub texture: Option<&'a I>,
    /// Sources unavailable for the requested time.
    pub missing_sources: Vec<SourceId>,
    /// Whether the texture is the previous successful frame.
    pub repeated: bool,
    /// Whether the frame's output equals the one last returned, so nothing
    /// was rendered: a host that presented that texture need not present it
    /// again.
    pub unchanged: bool,
}

/// What the texture a player last returned shows: the size, the frame
/// graph's cache key and the renderer's content epoch it was rendered at.
type ShownKey = (RenderSize, RenderCacheKey, u64);

/// Stateful real-time runner, holding the last picture its renderer drew:
/// `I` is [`FrameRenderer::Image`], a [`RenderedTexture`] on the GPU and an
/// `Arc<CpuImage>` on the [`alpha2_render::CpuRenderer`].
///
/// [`MissingFramePolicy::HoldPrevious`] is the default; a host that would
/// rather show its own loading state than a stale frame selects
/// [`MissingFramePolicy::Wait`] through [`Player::with_missing_frame_policy`].
#[derive(Debug)]
pub struct Player<I = RenderedTexture> {
    quality: QualityProfile,
    prefetch_frames: u32,
    missing_frame_policy: MissingFramePolicy,
    last_frame: Option<(MediaTime, I)>,
    /// What `last_frame` shows, when its key names everything it depends on.
    shown: Option<ShownKey>,
}

impl<I> Default for Player<I> {
    fn default() -> Self {
        Self {
            quality: QualityProfile::preview(),
            prefetch_frames: 2,
            missing_frame_policy: MissingFramePolicy::HoldPrevious,
            last_frame: None,
            shown: None,
        }
    }
}

impl<I> Player<I> {
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

    /// Creates a player with explicit runtime quality.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidQualityProfile`] for invalid controls.
    pub fn new(quality: QualityProfile) -> Result<Self, EngineError> {
        quality.validate()?;
        Ok(Self {
            quality,
            ..Self::default()
        })
    }

    /// Sets how many future frame boundaries are included in prefetch requests.
    /// Values above 120 are clamped to keep one request bounded.
    #[must_use]
    pub const fn with_prefetch_frames(mut self, frames: u32) -> Self {
        self.prefetch_frames = if frames > MAX_PREFETCH_FRAMES {
            MAX_PREFETCH_FRAMES
        } else {
            frames
        };
        self
    }

    /// How many future frame boundaries prefetch requests include.
    #[must_use]
    pub const fn prefetch_frames(&self) -> u32 {
        self.prefetch_frames
    }

    /// Returns the active quality profile.
    #[must_use]
    pub const fn quality(&self) -> QualityProfile {
        self.quality
    }

    /// Sets what [`Player::render_at`] returns while source frames are missing.
    ///
    /// [`MissingFramePolicy::HoldPrevious`] repeats the last successful
    /// texture, which keeps real-time playback moving. [`MissingFramePolicy::Wait`]
    /// withholds it, so the host can render its own loading state instead of
    /// presenting a frame that belongs to another timestamp. The held frame is
    /// retained either way and is presented again as soon as a render succeeds.
    #[must_use]
    pub const fn with_missing_frame_policy(mut self, policy: MissingFramePolicy) -> Self {
        self.missing_frame_policy = policy;
        self
    }

    /// Returns the policy used when source frames are unavailable.
    #[must_use]
    pub const fn missing_frame_policy(&self) -> MissingFramePolicy {
        self.missing_frame_policy
    }

    /// Collects deduplicated decode requests for the current and future frames.
    ///
    /// # Errors
    /// Returns evaluation or timeline overflow errors.
    pub fn prefetch_needs(
        &self,
        project: &EngineProject,
        time: MediaTime,
    ) -> Result<Vec<FrameNeed>, EngineError> {
        let rate = project.project().settings.frame_rate;
        let first = rate
            .frame_at_or_before(time)
            .ok_or(EngineError::OutOfRange)?;
        let mut needs = Vec::new();
        for (_, frame_time) in
            frame_times(rate, first, self.prefetch_frames.saturating_add(1), None)?
        {
            for need in project.frame_needs(frame_time)? {
                if !needs.iter().any(|existing: &FrameNeed| {
                    existing.source_id == need.source_id && existing.media_time == need.media_time
                }) {
                    needs.push(need);
                }
            }
        }
        Ok(needs)
    }

    /// Evaluates and attempts to present the host clock's current frame.
    ///
    /// When inputs are missing, the most recent successful texture is returned
    /// with `repeated = true` under [`MissingFramePolicy::HoldPrevious`], and
    /// withheld under [`MissingFramePolicy::Wait`]. Before the first successful
    /// frame, `texture` is absent and the host can display its own loading
    /// state.
    ///
    /// A frame whose output equals the texture last returned (the same render
    /// size, frame-graph cache key and renderer content epoch, with every
    /// source keyed) is not rendered: that texture is returned again with
    /// `unchanged = true`, and no GPU work is encoded. A frame drawn from a
    /// source without a stable key always renders.
    ///
    /// # Errors
    /// Returns timeline evaluation, quality, or rendering failures, and
    /// [`EngineError::UnregisteredEffect`] naming an effect the frame draws
    /// that `renderer` has not registered.
    pub fn render_at<'a, R: FrameRenderer<Image = I> + ?Sized>(
        &'a mut self,
        project: &EngineProject,
        renderer: &R,
        frames: &dyn FrameProvider,
        time: MediaTime,
    ) -> Result<PlaybackFrame<'a, I>, EngineError> {
        let frame = project.evaluate(time)?;
        crate::frame::check_frame_effects(renderer, &frame)?;
        let size = self.quality.render_size(&project.project().settings)?;
        renderer.set_texture_budget_bytes(self.quality.texture_budget_bytes);
        renderer.set_quality_controls(
            self.quality.max_filter_samples,
            self.quality.motion_blur_subframes,
        );
        let built = renderer.build_frame_graph(size, &frame, frames)?;
        if let Some(graph) = built.graph {
            let key = match graph.output().map(|output| graph.cache_eligibility(output)) {
                Some(Ok(CacheEligibility::Eligible)) => Some(
                    renderer
                        .graph_cache_key(&graph)
                        .map_err(alpha2_render::GraphError::into_render_error)?,
                ),
                _ => None,
            };
            let unchanged = self.last_frame.is_some()
                && key.is_some_and(|key| self.shown == Some((size, key, renderer.content_epoch())));
            if !unchanged {
                let texture = renderer.render_frame_graph(&graph, &built.pending)?;
                self.last_frame = Some((time, texture));
                // Read after the render, which may itself have cleared the result cache.
                self.shown = key.map(|key| (size, key, renderer.content_epoch()));
            }
            let (presented_time, texture) =
                self.last_frame.as_mut().ok_or(EngineError::OutOfRange)?;
            // The held texture is this frame's too.
            *presented_time = time;
            renderer.record_frame_outcome(true, false);
            return Ok(PlaybackFrame {
                requested_time: time,
                presented_time: Some(*presented_time),
                texture: Some(texture),
                missing_sources: Vec::new(),
                repeated: false,
                unchanged,
            });
        }
        // A hold is not this frame: the next complete one is presented, whatever its key.
        self.shown = None;

        let (presented_time, texture) = match self.missing_frame_policy {
            MissingFramePolicy::HoldPrevious => self
                .last_frame
                .as_ref()
                .map_or((None, None), |(time, texture)| (Some(*time), Some(texture))),
            // The held frame stays owned by the player so the next successful render, or a policy
            // change, can present it again.
            MissingFramePolicy::Wait => (None, None),
        };
        renderer.record_frame_outcome(texture.is_some(), texture.is_some());
        Ok(PlaybackFrame {
            requested_time: time,
            presented_time,
            texture,
            missing_sources: built.report.missing_sources,
            repeated: texture.is_some(),
            unchanged: false,
        })
    }

    /// Drops the held frame, for example after a seek or project swap.
    pub fn reset(&mut self) {
        self.last_frame = None;
        self.shown = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_configuration_is_bounded() {
        let player = Player::<RenderedTexture>::default().with_prefetch_frames(u32::MAX);
        assert_eq!(player.prefetch_frames, MAX_PREFETCH_FRAMES);
    }

    #[test]
    fn hold_previous_is_the_default_missing_frame_policy() {
        assert_eq!(
            Player::<RenderedTexture>::default().missing_frame_policy(),
            MissingFramePolicy::HoldPrevious
        );
    }

    #[test]
    fn the_missing_frame_policy_is_configurable() {
        let player = Player::<RenderedTexture>::default()
            .with_missing_frame_policy(MissingFramePolicy::Wait);
        assert_eq!(player.missing_frame_policy(), MissingFramePolicy::Wait);
        assert_eq!(
            player
                .with_missing_frame_policy(MissingFramePolicy::HoldPrevious)
                .missing_frame_policy(),
            MissingFramePolicy::HoldPrevious
        );
    }

    #[test]
    fn configuring_the_policy_preserves_quality_and_prefetch() {
        let player = Player::<RenderedTexture>::new(QualityProfile::export())
            .expect("valid quality")
            .with_prefetch_frames(7)
            .with_missing_frame_policy(MissingFramePolicy::Wait);
        assert_eq!(player.quality(), QualityProfile::export());
        assert_eq!(player.prefetch_frames, 7);
        assert_eq!(player.missing_frame_policy(), MissingFramePolicy::Wait);
    }
}
