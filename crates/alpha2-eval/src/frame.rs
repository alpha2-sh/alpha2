use crate::{EffectParamValue, sample_effect_param};
use alpha2_media::SourceId;
use alpha2_model::{
    BlendSpace, ClipBlendMode, ClipEffect, ClipId, EffectResourceId, Generator, Matte, Rgb,
    ShapeMask, TextStyle, TrackId, Vec2,
};
use alpha2_time::MediaTime;

/// A timestamp-specific composition description with no GPU handles.
///
/// Identities and authored effects are borrowed from the [`crate::ActiveIndex`]
/// that produced the description, so evaluating a frame copies none of them.
/// `'a` is the index's borrow; clone what must outlive it.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDesc<'a> {
    /// Timestamp evaluated on the project timeline.
    pub time: MediaTime,
    /// Opaque color under every visual layer.
    pub background: Rgb,
    /// The space in which blend modes are evaluated.
    pub blend_space: BlendSpace,
    /// Active visual clips in composition order.
    pub visual: Vec<ActiveClip<'a>>,
    /// Active audio clips in mix order.
    pub audio: Vec<ActiveClip<'a>>,
    /// Active two-input transitions in track order.
    pub transitions: Vec<ActiveTransition<'a>>,
    /// The cue each caption track shows, in track order.
    pub captions: Vec<ActiveCaption<'a>>,
}

impl FrameDesc<'_> {
    /// Whether the frame draws `clip`'s picture anywhere: as itself, or,
    /// for another clip's matte ([`ActiveClip::is_matte`]), into an active
    /// clip that names it. A matte no active clip names is drawn nowhere, so
    /// nothing of it is needed.
    #[must_use]
    pub fn draws(&self, clip: &ActiveClip<'_>) -> bool {
        !clip.is_matte
            || self.visual.iter().any(|other| {
                other
                    .matte
                    .is_some_and(|matte| &matte.source == clip.clip_id)
            })
    }
}

/// The cue one caption track shows at a timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveCaption<'a> {
    /// The caption track.
    pub track_id: &'a TrackId,
    /// The cue's text.
    pub text: &'a str,
    /// The track's style.
    pub style: &'a TextStyle,
    /// How many of the frame's [`FrameDesc::visual`] clips lie under the
    /// caption, by track order: those on tracks before the caption's,
    /// whether or not the frame draws them ([`FrameDesc::draws`]). It is
    /// composited after the drawn clips among the first `layers_below` of
    /// `visual` and before every drawn clip after them, so a clip that is
    /// not drawn never moves a caption past a track.
    pub layers_below: usize,
}

/// A visible or audible clip mapped to its source timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveClip<'a> {
    /// Active clip identity.
    pub clip_id: &'a ClipId,
    /// Media source needed for this clip.
    pub source_id: &'a SourceId,
    /// Track containing the clip.
    pub track_id: &'a TrackId,
    /// Source timestamp the clip's time map selects (forward, reversed,
    /// frozen, or looped).
    pub source_time: MediaTime,
    /// Evaluated visual presentation, present only for visual tracks.
    pub visual: Option<EvaluatedVisual>,
    /// The composition that builds the clip's picture, at the clip's
    /// composition time; `None` shows the source as it is.
    pub composition: Option<crate::ActiveComposition<'a>>,
    /// Artistic blend operation used by visual composition.
    pub blend_mode: ClipBlendMode,
    /// Whether a runtime may keep and reuse the clip's rendered picture.
    pub cache: alpha2_model::CacheIntent,
    /// Shape masks, in order; their outlines are evaluated at the frame's
    /// time with [`crate::evaluate_geometry`], their feathers with
    /// [`crate::sample_scalar`].
    pub masks: &'a [ShapeMask],
    /// The clip whose picture sets where this one is visible.
    pub matte: Option<&'a Matte>,
    /// Whether this clip is another clip's matte, and so is drawn only into
    /// that matte, never composited itself.
    pub is_matte: bool,
    /// What the engine draws for a generated source, which needs no frame
    /// from the host. It is drawn at the clip's `source_time`.
    pub generator: Option<&'a Generator>,
    /// A shapes generator's tree, its fixed parts evaluated once: evaluate
    /// it at `source_time`.
    pub shapes: Option<&'a crate::ShapesPlan>,
    /// For a composition source, its composition at the clip's source time:
    /// the clip's picture, which the host supplies no frame for.
    pub source_composition: Option<crate::ActiveComposition<'a>>,
}

impl ActiveClip<'_> {
    /// Whether the clip's own source frame is drawn: it is a host-supplied
    /// source, and the clip shows it directly or its composition draws it.
    #[must_use]
    pub fn needs_source(&self) -> bool {
        self.generator.is_none()
            && self.source_composition.is_none()
            && self
                .composition
                .is_none_or(|composition| composition.plan().uses_clip())
    }
}

/// Visual properties evaluated at one timeline timestamp.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EvaluatedVisual {
    /// Normalized output translation.
    pub position: Vec2,
    /// Normalized source anchor.
    pub anchor: Vec2,
    /// Per-axis scale.
    pub scale: Vec2,
    /// Clockwise rotation in degrees.
    pub rotation_degrees: f32,
    /// Opacity from zero to one.
    pub opacity: f32,
}

/// A source-frame request emitted for host prefetch and decode scheduling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameNeed {
    /// Clip consumer that owns the sequential decoder cursor.
    ///
    /// Multiple clips may read the same source at unrelated timestamps. Hosts
    /// must not make those clips share one seek position.
    pub clip_id: ClipId,
    /// The composition step that reads the source, when a media step does
    /// rather than the clip itself: its [`crate::Step::node`] path, after a
    /// `/` for a step of the clip's composition source (a node ID has no `/`,
    /// so no path of the clip's own composition starts with one). With the
    /// clip it names the consumer: two steps reading one source at different
    /// times each keep their own cursor instead of making one seek back and
    /// forth.
    pub reader: Option<alpha2_model::NodeId>,
    /// Source to decode or retrieve.
    pub source_id: SourceId,
    /// Timestamp requested on the source timeline.
    pub media_time: MediaTime,
}

/// A transition evaluated at one timeline timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveTransition<'a> {
    /// Clip sampled at progress zero.
    pub from_clip: &'a ClipId,
    /// Clip sampled at progress one.
    pub to_clip: &'a ClipId,
    /// Versioned shader invocation, with parameters sampled at the frame's
    /// time.
    pub effect: ActiveEffect<'a>,
    /// Eased progress in the inclusive unit interval.
    pub progress: f32,
}

/// One effect invocation at one timestamp: timeline time for a timeline
/// transition, composition time for a composition step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveEffect<'a> {
    effect: &'a ClipEffect,
    time: MediaTime,
}

impl<'a> ActiveEffect<'a> {
    /// The invocation `effect` sampled at `time`.
    #[must_use]
    pub const fn new(effect: &'a ClipEffect, time: MediaTime) -> Self {
        Self { effect, time }
    }

    /// Stable effect definition identifier.
    #[must_use]
    pub fn id(&self) -> &'a str {
        &self.effect.id
    }

    /// Exact effect definition version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.effect.version
    }

    /// Every authored parameter, sampled at this timestamp, by name.
    /// Parameters the invocation leaves out take their definition defaults.
    pub fn params(&self) -> impl Iterator<Item = (&'a str, EffectParamValue)> + 'a {
        let time = self.time;
        self.effect
            .params
            .iter()
            .map(move |(name, animation)| (name.as_str(), sample_effect_param(animation, time)))
    }

    /// One parameter sampled at this timestamp, if the invocation sets it.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<EffectParamValue> {
        self.effect
            .params
            .get(name)
            .map(|animation| sample_effect_param(animation, self.time))
    }

    /// Named external resources matched against the effect definition.
    pub fn resources(&self) -> impl Iterator<Item = (&'a str, &'a EffectResourceId)> + 'a {
        self.effect
            .resources
            .iter()
            .map(|(name, id)| (name.as_str(), id))
    }

    /// The authored invocation, with its parameters unevaluated.
    #[must_use]
    pub const fn authored(&self) -> &'a ClipEffect {
        self.effect
    }
}
