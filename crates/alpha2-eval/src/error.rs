use alpha2_media::SourceId;
use alpha2_model::{
    ClipBlendMode, ClipEffect, ClipId, CompositionId, ProjectId, TrackId, TrackKind,
    VisualProperties,
};
use alpha2_time::TimeRange;
use std::fmt;

/// An evaluator failure.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalError {
    /// The input project violated a model invariant.
    InvalidProject(alpha2_model::ModelError),
    /// A valid range could not be represented during integer retiming.
    UnmappableClip {
        /// Clip whose source timestamp could not be calculated.
        clip: ClipId,
    },
    /// A visible clip's composition cannot draw its output: it selects none,
    /// a node the output needs has an unconnected input, a group input no
    /// group node feeds is read, or a composition source reads a clip node
    /// ([`crate::PlanError`]'s incomplete reasons), or the composition nests
    /// or draws more than the planner holds.
    IncompleteComposition {
        /// The clip showing the composition.
        clip: ClipId,
        /// The composition.
        composition: CompositionId,
        /// What is missing.
        reason: crate::PlanError,
    },
    /// The reusable index belongs to a different project.
    IndexProjectMismatch {
        /// Project identity captured when the index was built.
        index_project: ProjectId,
        /// Project identity passed to evaluation.
        evaluated_project: ProjectId,
    },
}

impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidProject(error) => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProject(error) => {
                write!(formatter, "cannot index invalid project: {error}")
            }
            Self::UnmappableClip { clip } => {
                write!(
                    formatter,
                    "clip {clip} cannot be mapped to a source timestamp"
                )
            }
            Self::IncompleteComposition {
                clip,
                composition,
                reason,
            } => match reason {
                crate::PlanError::NoOutput => write!(
                    formatter,
                    "composition {composition} of clip {clip} has no output"
                ),
                crate::PlanError::MissingInput { node, input } => write!(
                    formatter,
                    "input {input} of node {node} in composition {composition} of clip {clip} is not connected"
                ),
                crate::PlanError::UnboundGroupInput { node } => write!(
                    formatter,
                    "group input {node} in composition {composition} of clip {clip} is fed by no group node"
                ),
                crate::PlanError::ClipWithoutClip { node } => write!(
                    formatter,
                    "clip node {node} in composition {composition}, drawn as clip {clip}'s source, shows no clip"
                ),
                crate::PlanError::NestedTooDeep { node } => write!(
                    formatter,
                    "node {node} in composition {composition} of clip {clip} nests compositions more than {} deep",
                    alpha2_model::MAX_COMPOSITION_DEPTH
                ),
                crate::PlanError::TooLarge => write!(
                    formatter,
                    "composition {composition} of clip {clip} draws more than {} nodes, or the project's compositions hold more than {} bytes once inlined",
                    alpha2_model::MAX_INLINED_NODES,
                    crate::MAX_PLAN_BYTES
                ),
            },
            Self::IndexProjectMismatch {
                index_project,
                evaluated_project,
            } => {
                write!(
                    formatter,
                    "index belongs to project {index_project}, not {evaluated_project}"
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedClip {
    pub(crate) clip_id: ClipId,
    pub(crate) source_id: SourceId,
    pub(crate) track_id: TrackId,
    pub(crate) track_kind: TrackKind,
    pub(crate) timeline_range: TimeRange,
    pub(crate) source_range: TimeRange,
    pub(crate) time_map: alpha2_model::TimeMap,
    pub(crate) visual: VisualProperties,
    pub(crate) composition: Option<crate::IndexedComposition>,
    pub(crate) blend_mode: ClipBlendMode,
    pub(crate) cache: alpha2_model::CacheIntent,
    pub(crate) masks: Vec<alpha2_model::ShapeMask>,
    pub(crate) matte: Option<alpha2_model::Matte>,
    /// Whether another clip names this one as its matte.
    pub(crate) is_matte: bool,
    pub(crate) audio: alpha2_model::AudioProperties,
    pub(crate) source_is_audible: bool,
    pub(crate) generator: Option<alpha2_model::Generator>,
    /// A shapes or layout generator's tree, planned once per source.
    pub(crate) plan: Option<std::sync::Arc<crate::GeneratorPlan>>,
    /// For a composition source, its composition compiled to draw it.
    pub(crate) source_composition: Option<(alpha2_model::CompositionId, crate::index::PlanSlot)>,
    pub(crate) track_order: usize,
    pub(crate) clip_order: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedTransition {
    pub(crate) from_clip: ClipId,
    pub(crate) to_clip: ClipId,
    pub(crate) timeline_range: TimeRange,
    pub(crate) easing: alpha2_model::Easing,
    pub(crate) effect: ClipEffect,
    pub(crate) track_order: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedCaptions {
    pub(crate) track_id: TrackId,
    pub(crate) track_order: usize,
    pub(crate) captions: alpha2_model::Captions,
    /// Every style the track's cues draw in, the track's own first, each
    /// patch applied once.
    pub(crate) styles: Vec<alpha2_model::TextStyle>,
    /// Each cue's style in `styles`.
    pub(crate) cue_styles: Vec<usize>,
}

impl IndexedCaptions {
    /// `captions` of track `track_id`, its cues' styles resolved.
    pub(crate) fn new(
        track_id: TrackId,
        track_order: usize,
        captions: alpha2_model::Captions,
    ) -> Self {
        let mut styles = vec![captions.style.clone()];
        let mut by_name = std::collections::BTreeMap::<&str, usize>::new();
        let mut cue_styles = Vec::with_capacity(captions.cues.len());
        for cue in &captions.cues {
            // The name a cue's patch is found by: its own style, else its speaker's.
            let name = cue.style.as_deref().or_else(|| {
                cue.speaker
                    .as_deref()
                    .filter(|speaker| captions.styles.contains_key(*speaker))
            });
            let index = match name.and_then(|name| captions.styles.get_key_value(name)) {
                Some((name, patch)) => *by_name.entry(name.as_str()).or_insert_with(|| {
                    styles.push(patch.apply(&captions.style));
                    styles.len() - 1
                }),
                None => 0,
            };
            cue_styles.push(index);
        }
        Self {
            track_id,
            track_order,
            captions,
            styles,
            cue_styles,
        }
    }
}
