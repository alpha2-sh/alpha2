use crate::{EffectParamValue, SampleContext, sample_effect_param};
use alpha2_media::SourceId;
use alpha2_model::{
    BlendSpace, ClipBlendMode, ClipEffect, ClipId, EffectResourceId, Generator, Matte, Rgb,
    ShapeMask, TextStyle, TrackId, Vec2,
};
use alpha2_time::MediaTime;

/// A timestamp-specific composition description with no GPU handles.
///
/// Identities and authored effects are borrowed from the [`crate::ActiveIndex`] (`'a`), so
/// evaluating a frame copies none of them.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDesc<'a> {
    /// Timestamp evaluated on the project timeline.
    pub time: MediaTime,
    /// Opaque color under every visual layer, or `None` for transparent black (alpha output).
    pub background: Option<Rgb>,
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
    /// Whether the frame draws `clip`'s picture. A matte ([`ActiveClip::is_matte`]) is drawn
    /// only while an active clip names it.
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
    pub text: &'a alpha2_model::TextContent,
    /// Time since the cue started, which keys its style's animators.
    pub local_time: alpha2_time::MediaTime,
    /// The track style with the cue's (or else its speaker's) named style patch applied.
    pub style: &'a TextStyle,
    /// How many [`FrameDesc::visual`] clips lie under the caption by track order, counting clips
    /// the frame does not draw ([`FrameDesc::draws`]) so an undrawn clip never shifts a caption.
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
    /// Source timestamp selected by the clip's time map.
    pub source_time: MediaTime,
    /// Evaluated visual presentation, present only for visual tracks.
    pub visual: Option<EvaluatedVisual>,
    /// The composition that builds the clip's picture; `None` shows the source as is.
    pub composition: Option<crate::ActiveComposition<'a>>,
    /// Artistic blend operation used by visual composition.
    pub blend_mode: ClipBlendMode,
    /// Whether a runtime may keep and reuse the clip's rendered picture.
    pub cache: alpha2_model::CacheIntent,
    /// Shape masks in order. Evaluate outlines with [`crate::evaluate_geometry`] and feathers with
    /// [`crate::sample_scalar`] at the frame's time.
    pub masks: &'a [ShapeMask],
    /// The clip whose picture sets where this one is visible.
    pub matte: Option<&'a Matte>,
    /// Whether this clip is another clip's matte, drawn only into that matte.
    pub is_matte: bool,
    /// Generated source content, drawn at `source_time` with no host frame.
    pub generator: Option<&'a Generator>,
    /// Planned shapes or layout generator tree, to evaluate at `source_time`.
    pub plan: Option<&'a crate::GeneratorPlan>,
    /// For a composition source, its composition at the clip's source time (no host frame).
    pub source_composition: Option<crate::ActiveComposition<'a>>,
    /// What the clip's timeline-time values (its masks) read data through.
    pub data: SampleContext<'a>,
    /// The correction drawn between the clip's finished picture and its visual, when it has one
    /// with a value at this time.
    pub correction: Option<crate::Correction>,
}

impl ActiveClip<'_> {
    /// Whether the clip draws its own host-supplied source frame.
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
    /// The composition step reading the source, if not the clip itself: its
    /// [`crate::Step::node`] path, prefixed with `/` for steps of a composition source. Each
    /// (clip, reader) pair keeps its own decoder cursor.
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
    /// Versioned shader invocation, with parameters sampled at the frame's time.
    pub effect: ActiveEffect<'a>,
    /// Eased progress in the inclusive unit interval.
    pub progress: f32,
}

/// One effect invocation at one timestamp (timeline time for transitions, composition time for
/// composition steps).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveEffect<'a> {
    effect: &'a ClipEffect,
    time: MediaTime,
    context: SampleContext<'a>,
}

impl<'a> ActiveEffect<'a> {
    /// The invocation `effect` sampled at `time`, with data bindings read through `context`.
    #[must_use]
    pub const fn new(effect: &'a ClipEffect, time: MediaTime, context: SampleContext<'a>) -> Self {
        Self {
            effect,
            time,
            context,
        }
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

    /// Every authored parameter sampled at this timestamp. Omitted ones use definition defaults.
    pub fn params(&self) -> impl Iterator<Item = (&'a str, EffectParamValue)> + 'a {
        let time = self.time;
        let context = self.context;
        self.effect.params.iter().map(move |(name, animation)| {
            (
                name.as_str(),
                sample_effect_param(animation, time, &context),
            )
        })
    }

    /// One parameter sampled at this timestamp, if the invocation sets it.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<EffectParamValue> {
        self.effect
            .params
            .get(name)
            .map(|animation| sample_effect_param(animation, self.time, &self.context))
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
